//! `FolderIndex` — deterministic folder scanner + incremental `index.md`
//! builder. The mirror image of `FetchImages`: the model supplies only the
//! judgement (a one-line summary per file), everything else — walking,
//! fingerprinting, deciding what actually changed, laying out the tables — is
//! done here so re-indexing a folder costs nothing when nothing moved.
//!
//! Two actions:
//!
//! - `plan` walks the tree, fingerprints every file (sha256 of the content),
//!   diffs against `.thclaws-index.json`, and hands back only the files whose
//!   bytes changed since their cached summary — with the reader tool to use for
//!   each. It rewrites `index.md` on the way out, so a folder where nothing
//!   changed is fully re-indexed in one call with zero file reads.
//! - `commit` merges a batch of summaries back into the cache and rebuilds
//!   `index.md`.
//!
//! The cache lives beside the index (`<folder>/.thclaws-index.json`) so it
//! travels with the folder when it's moved or copied, and dies with it.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const CACHE_FILE: &str = ".thclaws-index.json";
pub const INDEX_FILE: &str = "index.md";
const CACHE_VERSION: u32 = 1;
const DEFAULT_MAX_DEPTH: usize = 6;
const DEFAULT_BATCH: usize = 40;
/// How many child summaries a subfolder rollup is condensed from. A folder
/// with 900 files doesn't need all 900 in the prompt to be described.
const FOLDER_ROLLUP_SAMPLE: usize = 60;
const DEFAULT_MAX_READ_MB: u64 = 20;
const MAX_FILES: usize = 5000;
/// Files at or below this size are hashed whole; larger ones are hashed
/// head+tail+size (a 4 GB video re-hashed on every scan would dominate the
/// runtime, and head/tail/size catches every realistic edit).
const HASH_FULL_LIMIT: u64 = 8 * 1024 * 1024;
const HASH_EDGE: usize = 1024 * 1024;

// ── Cache ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct CachedFile {
    /// Content fingerprint at the time `summary` was written. A file whose
    /// fingerprint still matches is never re-read.
    pub fingerprint: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Set for files that are listed but deliberately not read (archives,
    /// video, oversized). Holds the reason; keeps them out of every later
    /// batch instead of being re-proposed forever.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub skipped: String,
    #[serde(default)]
    pub indexed: String,
}

/// A rollup written for one immediate subfolder in `report_depth: 1` mode —
/// what's in there, in a sentence, instead of a table of its files.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct CachedFolder {
    /// Digest of every file fingerprint under this subfolder. Any add,
    /// delete or edit anywhere in the subtree changes it, which is what
    /// makes the rollup stale.
    pub digest: String,
    /// Short label for what the folder is about — the `Title` column.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub indexed: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct IndexCache {
    pub version: u32,
    #[serde(default)]
    pub generated: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub overview: String,
    #[serde(default)]
    pub files: BTreeMap<String, CachedFile>,
    /// Keyed by immediate subfolder name. Only populated in `report_depth: 1`
    /// mode; harmless (and preserved) in the others.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub folders: BTreeMap<String, CachedFolder>,
}

impl IndexCache {
    fn load(dir: &Path) -> Self {
        let path = dir.join(CACHE_FILE);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Self {
                version: CACHE_VERSION,
                ..Default::default()
            };
        };
        // A corrupt or older-version cache is not an error: the worst case
        // is a full re-summarize, which is exactly what a fresh index does.
        match serde_json::from_str::<Self>(&raw) {
            Ok(c) if c.version == CACHE_VERSION => c,
            _ => Self {
                version: CACHE_VERSION,
                ..Default::default()
            },
        }
    }

    fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(CACHE_FILE);
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Tool(format!("FolderIndex: cannot serialize cache: {e}")))?;
        std::fs::write(&path, body)
            .map_err(|e| Error::Tool(format!("FolderIndex: cannot write {}: {e}", path.display())))
    }
}

// ── Scan ───────────────────────────────────────────────────────────

struct Entry {
    /// Path relative to the indexed folder, forward slashes.
    rel: String,
    /// Parent directory relative to the indexed folder (`""` = the folder itself).
    dir: String,
    name: String,
    ext: String,
    size: u64,
    modified: Option<DateTime<Utc>>,
    /// Tool the summarizing agent should use, or `None` when the format
    /// carries no text/pixels a model can read.
    reader: Option<&'static str>,
}

/// The engine tool that can actually read this extension. `None` → the file
/// is listed from its metadata alone (archives, video, fonts, binaries).
fn reader_for(ext: &str) -> Option<&'static str> {
    match ext {
        "pdf" => Some("PdfRead"),
        "docx" => Some("DocxRead"),
        "pptx" => Some("PptxRead"),
        "xlsx" | "xlsm" => Some("XlsxRead"),
        // What `Read` returns as inline image blocks — other image formats
        // (heic/tiff/bmp/avif) reach the model as bytes it can't decode.
        "png" | "jpg" | "jpeg" | "webp" | "gif" => Some("Read"),
        _ if is_text_ext(ext) => Some("Read"),
        _ => None,
    }
}

fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        // documents / prose
        "md" | "markdown"
            | "txt"
            | "text"
            | "rst"
            | "adoc"
            | "org"
            | "tex"
            | "csv"
            | "tsv"
            | "log"
            | "srt"
            | "vtt"
            // markup / config
            | "html"
            | "htm"
            | "xml"
            // SVG is markup — the model reads the source, which describes the
            // drawing better than the (undecodable) rasterless bytes would.
            | "svg"
            | "json"
            | "jsonl"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "cfg"
            | "conf"
            | "env"
            | "properties"
            // source
            | "rs"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "go"
            | "java"
            | "kt"
            | "kts"
            | "swift"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "hpp"
            | "cs"
            | "rb"
            | "php"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "ps1"
            | "sql"
            | "graphql"
            | "proto"
            | "vue"
            | "svelte"
            | "lua"
            | "r"
            | "scala"
            | "dart"
            | "pl"
            | "ex"
            | "exs"
            | "erl"
            | "hs"
            | "clj"
            | "css"
            | "scss"
            | "less"
            | "dockerfile"
            | "makefile"
            | "gradle"
            | "ipynb"
    )
}

/// Coarse bucket used for the index's `Type` column and the run summary.
fn kind_for(ext: &str, reader: Option<&str>) -> &'static str {
    match ext {
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "heic" | "heif" | "tiff" | "tif" | "bmp"
        | "avif" | "svg" | "ico" => "image",
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" => {
            "media"
        }
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "dmg" | "iso" => "archive",
        "pdf" | "docx" | "doc" | "pptx" | "ppt" | "xlsx" | "xls" | "xlsm" | "epub" | "rtf"
        | "odt" | "ods" | "odp" | "pages" | "key" | "numbers" => "document",
        _ if is_text_ext(ext) => "text",
        _ if reader.is_some() => "text",
        _ => "binary",
    }
}

fn ext_of(name: &str) -> String {
    // `Makefile` / `Dockerfile` carry their type in the stem, not an extension.
    let lower = name.to_ascii_lowercase();
    if !lower.contains('.') {
        return lower;
    }
    lower.rsplit('.').next().unwrap_or("").to_string()
}

fn scan(dir: &Path, recursive: bool, max_depth: usize) -> Result<(Vec<Entry>, bool)> {
    let mut walker = ignore::WalkBuilder::new(dir);
    walker
        .hidden(true)
        .follow_links(false)
        .max_depth(Some(if recursive { max_depth.max(1) } else { 1 }));
    let mut out = Vec::new();
    let mut truncated = false;
    for entry in walker.build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(rel_path) = path.strip_prefix(dir) else {
            continue;
        };
        let rel = rel_path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/");
        if rel.is_empty() {
            continue;
        }
        // Our own artifacts are never part of what we describe.
        if rel == INDEX_FILE || rel.ends_with(CACHE_FILE) {
            continue;
        }
        if out.len() >= MAX_FILES {
            truncated = true;
            break;
        }
        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        let parent = match rel.rfind('/') {
            Some(i) => rel[..i].to_string(),
            None => String::new(),
        };
        let meta = entry.metadata().ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(DateTime::<Utc>::from);
        let ext = ext_of(&name);
        let reader = reader_for(&ext);
        out.push(Entry {
            rel,
            dir: parent,
            name,
            ext,
            size,
            modified,
            reader,
        });
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir).then(a.name.cmp(&b.name)));
    Ok((out, truncated))
}

fn fingerprint(path: &Path, size: u64) -> Result<String> {
    use std::io::{Read as _, Seek, SeekFrom};
    let mut hasher = Sha256::new();
    hasher.update(size.to_le_bytes());
    if size <= HASH_FULL_LIMIT {
        let bytes = std::fs::read(path).map_err(|e| {
            Error::Tool(format!("FolderIndex: cannot read {}: {e}", path.display()))
        })?;
        hasher.update(&bytes);
    } else {
        let mut f = std::fs::File::open(path).map_err(|e| {
            Error::Tool(format!("FolderIndex: cannot open {}: {e}", path.display()))
        })?;
        let mut buf = vec![0u8; HASH_EDGE];
        f.read_exact(&mut buf).map_err(|e| {
            Error::Tool(format!("FolderIndex: cannot read {}: {e}", path.display()))
        })?;
        hasher.update(&buf);
        f.seek(SeekFrom::End(-(HASH_EDGE as i64)))
            .and_then(|_| f.read_exact(&mut buf))
            .map_err(|e| {
                Error::Tool(format!("FolderIndex: cannot read {}: {e}", path.display()))
            })?;
        hasher.update(&buf);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

// ── Rendering ──────────────────────────────────────────────────────

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Markdown table cells can't hold pipes or newlines; a summary that does
/// would shear the row in half.
fn cell(text: &str) -> String {
    let flat = text
        .replace(['\r', '\n'], " ")
        .replace('|', "\\|")
        .trim()
        .to_string();
    if flat.is_empty() {
        "—".to_string()
    } else {
        flat
    }
}

/// Percent-encode the characters that break a markdown link target.
fn link_target(rel: &str) -> String {
    rel.replace('%', "%25")
        .replace(' ', "%20")
        .replace('(', "%28")
        .replace(')', "%29")
}

/// The description cell: the title in bold on its own line, then the summary
/// under it. A separate title column would be squeezed to a few characters —
/// the summary is the widest column and the two belong together anyway.
/// `<br>` is the only way a markdown table cell can hold two lines; every
/// viewer renders it, including the Files tab (see `file_preview`).
fn described(title: &str, body: &str) -> String {
    match (title.trim(), body.trim()) {
        ("", b) => cell(b),
        (t, "") => format!("**{}**", cell(t)),
        (t, b) => format!("**{}**<br>{}", cell(t), cell(b)),
    }
}

/// One markdown table row for a file: link, type, size, date, description
/// (or the reason it was listed without being read).
fn file_row(e: &Entry, cache: &IndexCache) -> String {
    let c = cache.files.get(&e.rel);
    let (title, summary) = match c {
        Some(c) if !c.summary.is_empty() => (c.title.as_str(), c.summary.clone()),
        Some(c) if !c.skipped.is_empty() => ("", format!("_{}_", c.skipped)),
        _ => ("", String::new()),
    };
    let modified = e
        .modified
        .map(|m| m.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "—".into());
    format!(
        "| [{}]({}) | {} | {} | {} | {} |\n",
        cell(&e.name),
        link_target(&e.rel),
        cell(&e.ext),
        human_size(e.size),
        modified,
        described(title, &summary)
    )
}

/// Header + separator for the file table, matching [`file_row`].
fn file_table_head() -> &'static str {
    "| File | Type | Size | Modified | Summary |\n|---|---|---|---|---|\n"
}

/// The immediate subfolder an entry belongs to (`""` = the indexed folder
/// itself). `docs/img/a.png` rolls up under `docs`.
fn top_level(dir: &str) -> &str {
    match dir.find('/') {
        Some(i) => &dir[..i],
        None => dir,
    }
}

/// Fingerprint of a whole subtree: every file's path + content hash, in a
/// stable order. Changes on any add, delete or edit inside it, which is
/// exactly when its rollup summary needs rewriting.
///
/// Reads the fingerprints taken during THIS scan, never the cached ones —
/// `plan` deliberately leaves a changed file's cached fingerprint stale until
/// its summary lands, so a cache-derived digest would call the folder fresh
/// while its contents had already moved on.
fn folder_digest(entries: &[&Entry], live: &HashMap<String, String>) -> String {
    let mut rows: Vec<String> = entries
        .iter()
        .map(|e| {
            let fp = live.get(&e.rel).map(String::as_str).unwrap_or("");
            format!("{}:{fp}", e.rel)
        })
        .collect();
    rows.sort();
    let mut h = Sha256::new();
    h.update(rows.join("\n").as_bytes());
    format!("sha256:{:x}", h.finalize())
}

/// Immediate subfolders, in name order, each with everything beneath it.
fn subfolders(entries: &[Entry]) -> Vec<(String, Vec<&Entry>)> {
    let mut names: Vec<&str> = entries
        .iter()
        .map(|e| top_level(&e.dir))
        .filter(|d| !d.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .map(|n| {
            (
                n.to_string(),
                entries
                    .iter()
                    .filter(|e| top_level(&e.dir) == n)
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn render_index(
    name: &str,
    folder: &str,
    entries: &[Entry],
    cache: &IndexCache,
    truncated: bool,
    one_level: bool,
) -> String {
    let total_size: u64 = entries.iter().map(|e| e.size).sum();
    let dirs: Vec<String> = {
        let mut d: Vec<String> = entries.iter().map(|e| e.dir.clone()).collect();
        d.sort();
        d.dedup();
        d
    };
    let described_count = entries
        .iter()
        .filter(|e| {
            cache
                .files
                .get(&e.rel)
                .is_some_and(|c| !c.summary.is_empty())
        })
        .count();

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("title: \"Index — {}\"\n", name.replace('"', "'")));
    out.push_str(&format!("folder: \"{}\"\n", folder.replace('"', "'")));
    out.push_str(&format!(
        "generated: {}\n",
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    ));
    out.push_str(&format!("files: {}\n", entries.len()));
    out.push_str(&format!(
        "subfolders: {}\n",
        dirs.iter().filter(|d| !d.is_empty()).count()
    ));
    out.push_str(&format!("total_size: \"{}\"\n", human_size(total_size)));
    if !cache.language.is_empty() {
        out.push_str(&format!("language: {}\n", cache.language));
    }
    out.push_str("generator: thClaws FolderIndex\n");
    out.push_str("---\n\n");

    out.push_str(&format!("# Index — {name}\n\n"));
    if !cache.overview.trim().is_empty() {
        out.push_str(cache.overview.trim());
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "{} file{} · {} · {} described\n\n",
        entries.len(),
        if entries.len() == 1 { "" } else { "s" },
        human_size(total_size),
        described_count
    ));
    if truncated {
        out.push_str(&format!(
            "> Listing capped at {MAX_FILES} files — deeper/newer entries are not shown.\n\n"
        ));
    }

    // `report_depth: 1`: the indexed folder's own files, then ONE row per
    // immediate subfolder describing what's inside it. Everything deeper was
    // still scanned and summarized (it feeds those rollups and stays in the
    // cache) — it just isn't listed here.
    if one_level {
        let own: Vec<&Entry> = entries.iter().filter(|e| e.dir.is_empty()).collect();
        let own_size: u64 = own.iter().map(|e| e.size).sum();
        out.push_str(&format!(
            "## [./](./) — {} file{} ({})\n\n",
            own.len(),
            if own.len() == 1 { "" } else { "s" },
            human_size(own_size)
        ));
        if own.is_empty() {
            out.push_str("_No files directly in this folder._\n\n");
        } else {
            out.push_str(file_table_head());
            for e in own {
                out.push_str(&file_row(e, cache));
            }
            out.push('\n');
        }

        let subs = subfolders(entries);
        if !subs.is_empty() {
            out.push_str(&format!("## Folders ({})\n\n", subs.len()));
            out.push_str("| Folder | Files | Size | Contents |\n");
            out.push_str("|---|---|---|---|\n");
            for (dir_name, files) in &subs {
                let size: u64 = files.iter().map(|e| e.size).sum();
                let nested = files.iter().filter(|e| e.dir != *dir_name).count();
                let rollup = cache.folders.get(dir_name);
                let summary = rollup.map(|f| f.summary.clone()).unwrap_or_default();
                let title = rollup.map(|f| f.title.as_str()).unwrap_or_default();
                out.push_str(&format!(
                    "| [{}/]({}/) | {}{} | {} | {} |\n",
                    cell(dir_name),
                    link_target(dir_name),
                    files.len(),
                    if nested > 0 {
                        format!(" ({nested} nested)")
                    } else {
                        String::new()
                    },
                    human_size(size),
                    described(title, &summary)
                ));
            }
            out.push('\n');
        }
        out.push_str(&format!(
            "<!-- Generated by thClaws FolderIndex (report_depth: 1 — subfolders are \
             summarized, not listed). Per-file summaries are cached in `{CACHE_FILE}` keyed by \
             content hash; re-indexing only re-reads files that changed. Hand edits to this file \
             are overwritten on the next run. -->\n"
        ));
        return out;
    }

    for d in &dirs {
        // Linked so the Files tab can walk into the folder from the
        // heading; the trailing slash is what marks it as a directory.
        let section = if d.is_empty() {
            "[./](./)".to_string()
        } else {
            format!("[{d}/]({}/)", link_target(d))
        };
        let files: Vec<&Entry> = entries.iter().filter(|e| &e.dir == d).collect();
        let size: u64 = files.iter().map(|e| e.size).sum();
        out.push_str(&format!(
            "## {section} — {} file{} ({})\n\n",
            files.len(),
            if files.len() == 1 { "" } else { "s" },
            human_size(size)
        ));
        out.push_str(file_table_head());
        for e in files {
            out.push_str(&file_row(e, cache));
        }
        out.push('\n');
    }

    out.push_str(&format!(
        "<!-- Generated by thClaws FolderIndex. Per-file summaries are cached in \
         `{CACHE_FILE}` keyed by content hash — re-indexing only re-reads files that changed. \
         Hand edits to this file are overwritten on the next run. -->\n"
    ));
    out
}

// ── Tool ───────────────────────────────────────────────────────────

pub struct FolderIndexTool;

/// Options shared by both actions, so `plan` and `commit` always see the
/// same tree (a `commit` with a different depth would prune half the cache).
struct Opts {
    recursive: bool,
    max_depth: usize,
    max_read_bytes: u64,
    /// `1` = list this folder's own files and describe each immediate
    /// subfolder in one row instead of listing its contents. Anything else
    /// (the default) lists every scanned file, grouped by directory. Note
    /// this is a *reporting* depth: the scan is unchanged, so the rollups
    /// are written from summaries of everything underneath.
    report_depth: u64,
}

fn opts_from(input: &Value) -> Opts {
    Opts {
        recursive: input
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        max_depth: input
            .get("max_depth")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_DEPTH as u64) as usize,
        max_read_bytes: input
            .get("max_read_mb")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_READ_MB)
            * 1024
            * 1024,
        report_depth: input
            .get("report_depth")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

/// The indexed folder as the user knows it: relative to the workspace root
/// when it's inside one (`articles/thai`), otherwise whatever the caller
/// typed. Never an absolute machine path — those end up in the index title.
fn workspace_relative(dir: &Path, folder_arg: &str) -> String {
    if let Some(root) = crate::sandbox::Sandbox::root() {
        if let Ok(rel) = dir.strip_prefix(&root) {
            let s = rel.to_string_lossy().replace('\\', "/");
            return if s.is_empty() { ".".to_string() } else { s };
        }
    }
    let typed = folder_arg.trim().trim_end_matches('/');
    if typed.is_empty() || typed == "." || Path::new(typed).is_absolute() {
        return dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
    }
    typed.trim_start_matches("./").to_string()
}

/// Resolve a caller-supplied file path to a cache key relative to the indexed
/// folder. Tolerates `./x`, `folder/x` (the path as the agent saw it in its
/// own prompt), and an absolute path inside the folder.
fn normalize_rel(dir: &Path, folder_arg: &str, raw: &str) -> String {
    let raw = raw.trim().trim_start_matches("./");
    if let Ok(stripped) = Path::new(raw).strip_prefix(dir) {
        return stripped.to_string_lossy().replace('\\', "/");
    }
    let folder = folder_arg
        .trim()
        .trim_start_matches("./")
        .trim_end_matches('/');
    if !folder.is_empty() {
        if let Some(rest) = raw.strip_prefix(&format!("{folder}/")) {
            return rest.to_string();
        }
    }
    raw.replace('\\', "/")
}

#[async_trait]
impl Tool for FolderIndexTool {
    fn name(&self) -> &'static str {
        "FolderIndex"
    }

    fn description(&self) -> &'static str {
        "Build or refresh an `index.md` that catalogues a folder — the \
         deterministic half of folder indexing. `action:\"plan\"` walks the tree, \
         fingerprints every file (sha256), rewrites `index.md` from the cached \
         summaries, and returns ONLY the files whose content changed since they \
         were last summarized, each with the reader tool to use (Read / PdfRead / \
         DocxRead / PptxRead / XlsxRead). Read those files, then send one-line \
         summaries back with `action:\"commit\"` — they're merged into the cache \
         (`.thclaws-index.json`, next to index.md) and `index.md` is rebuilt. \
         Files that never changed are never re-read, so re-indexing a settled \
         folder costs one call and zero tokens. Archives, video and oversized \
         files are listed from metadata alone. `report_depth:1` changes the \
         SHAPE of the index only: this folder's own files are listed, and each \
         immediate subfolder gets one row describing what's inside — write those \
         with `folder_summaries` on commit (the plan hands you the child \
         summaries to condense; nothing extra needs reading). Never hand-edit \
         index.md — it is regenerated from the cache on every run."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Folder to index. index.md and .thclaws-index.json are written inside it."
                },
                "action": {
                    "type": "string",
                    "enum": ["plan", "commit"],
                    "description": "plan (default): scan + report what needs summarizing. commit: merge summaries and rebuild index.md."
                },
                "summaries": {
                    "type": "array",
                    "description": "commit only. One entry per file you read.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file": {"type": "string", "description": "Path exactly as `plan` reported it (relative to the folder)."},
                            "summary": {"type": "string", "description": "1-2 sentences: what this file IS and what it contains. No filler."},
                            "title": {"type": "string", "description": "Short label (2-6 words) for what this file IS about — its real title when it has one, otherwise a topic label. Shown as its own column."},
                            "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional short topic tags."}
                        },
                        "required": ["file", "summary"]
                    }
                },
                "overview": {
                    "type": "string",
                    "description": "commit only. 2-4 sentence description of the folder as a whole; rendered under the index title and cached."
                },
                "language": {"type": "string", "description": "Language the summaries are written in (e.g. th, en). Stored in the cache and the index frontmatter."},
                "recursive": {"type": "boolean", "description": "Descend into subfolders (default true). Must match between plan and commit."},
                "max_depth": {"type": "integer", "description": "Max directory depth when recursive (default 6)."},
                "max_read_mb": {"type": "integer", "description": "Files larger than this are listed but never proposed for reading (default 20)."},
                "report_depth": {
                    "type": "integer",
                    "description": "1 = list only this folder's files plus a one-row summary per immediate subfolder (everything deeper is still scanned and feeds those rollups). Omit for the full per-directory listing. Must match between plan and commit."
                },
                "folder_summaries": {
                    "type": "array",
                    "description": "commit + report_depth:1 only. One entry per subfolder the plan listed in `folders_to_summarize`.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "folder": {"type": "string", "description": "Subfolder name exactly as `plan` reported it."},
                            "title": {"type": "string", "description": "Short label (2-6 words) for what this folder is about."},
                            "summary": {"type": "string", "description": "1-2 sentences: what this folder holds, condensed from its files' summaries."}
                        },
                        "required": ["folder", "summary"]
                    }
                },
                "batch": {"type": "integer", "description": "plan only: max files to return per call (default 40)."},
                "force": {"type": "boolean", "description": "plan only: re-summarize everything, ignoring cached fingerprints."}
            },
            "required": ["path"]
        })
    }

    /// Gated behind the `folder-indexer` subagent, which allow-lists it (and
    /// so opens the gate). Same deal as `FetchImages`/content-extractor: the
    /// main agent never carries this schema in its tool list, and folder
    /// indexing always runs in the isolated context that was built for it.
    fn requires_gate(&self) -> Option<&'static str> {
        Some("folder-indexer")
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let folder_arg = req_str(&input, "path")?.to_string();
        let dir = crate::sandbox::Sandbox::check_write(&folder_arg)?;
        if !dir.is_dir() {
            return Err(Error::Tool(format!(
                "FolderIndex: {} is not a folder",
                dir.display()
            )));
        }
        let opts = opts_from(&input);
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("plan")
            .to_ascii_lowercase();

        let (entries, truncated) = scan(&dir, opts.recursive, opts.max_depth)?;
        let mut cache = IndexCache::load(&dir);
        if let Some(lang) = input.get("language").and_then(Value::as_str) {
            if !lang.trim().is_empty() {
                cache.language = lang.trim().to_string();
            }
        }
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        // Drop cache rows for files that no longer exist — the whole point of
        // a folder index is that it doesn't describe things that are gone.
        let present: std::collections::HashSet<&str> =
            entries.iter().map(|e| e.rel.as_str()).collect();
        let before = cache.files.len();
        cache.files.retain(|k, _| present.contains(k.as_str()));
        let removed = before - cache.files.len();

        // Titles use the folder's own name, and every reported path is
        // workspace-relative: the caller may hand us an absolute path (the
        // Files tab passes what the tree gave it), and "Index —
        // /private/var/folders/…/demo" is nobody's idea of a title.
        let folder_rel = workspace_relative(&dir, &folder_arg);
        let folder_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| folder_rel.clone());
        let index_path = if folder_rel == "." {
            INDEX_FILE.to_string()
        } else {
            format!("{folder_rel}/{INDEX_FILE}")
        };

        // Hash every file once per run; both actions (and the subfolder
        // digests) read from here instead of re-hashing.
        let mut live_fp: HashMap<String, String> = HashMap::new();
        let mut unreadable_paths: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for e in &entries {
            match fingerprint(&dir.join(&e.rel), e.size) {
                Ok(fp) => {
                    live_fp.insert(e.rel.clone(), fp);
                }
                Err(_) => {
                    unreadable_paths.insert(e.rel.clone());
                }
            }
        }

        let one_level = opts.report_depth == 1;
        // In one-level mode the cache also carries a rollup per immediate
        // subfolder; drop rows for folders that are gone.
        if one_level {
            let live: std::collections::HashSet<&str> =
                entries.iter().map(|e| top_level(&e.dir)).collect();
            cache.folders.retain(|k, _| live.contains(k.as_str()));
        }

        let result = match action.as_str() {
            "plan" => {
                let force = input.get("force").and_then(Value::as_bool).unwrap_or(false);
                let batch = input
                    .get("batch")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_BATCH as u64) as usize;

                let mut stale: Vec<Value> = Vec::new();
                let mut stale_total = 0usize;
                let mut unchanged = 0usize;
                let mut listed_only = 0usize;
                let mut unreadable = 0usize;

                for e in &entries {
                    let Some(fp) = live_fp.get(&e.rel).cloned() else {
                        unreadable += 1;
                        cache.files.insert(
                            e.rel.clone(),
                            CachedFile {
                                fingerprint: String::new(),
                                skipped: "unreadable".into(),
                                indexed: now.clone(),
                                ..Default::default()
                            },
                        );
                        continue;
                    };
                    let skip_reason = match e.reader {
                        None => Some(format!(
                            "{} file — listed, not read",
                            kind_for(&e.ext, None)
                        )),
                        Some(_) if e.size > opts.max_read_bytes => Some(format!(
                            "larger than {} — not read",
                            human_size(opts.max_read_bytes)
                        )),
                        Some(_) => None,
                    };
                    if let Some(reason) = skip_reason {
                        listed_only += 1;
                        cache.files.insert(
                            e.rel.clone(),
                            CachedFile {
                                fingerprint: fp,
                                skipped: reason,
                                indexed: now.clone(),
                                ..Default::default()
                            },
                        );
                        continue;
                    }
                    let fresh = !force
                        && cache
                            .files
                            .get(&e.rel)
                            .is_some_and(|c| c.fingerprint == fp && !c.summary.is_empty());
                    if fresh {
                        unchanged += 1;
                        continue;
                    }
                    // Deliberately NOT storing the new fingerprint here: it's
                    // written by `commit` along with the summary, so a plan
                    // that's never committed doesn't mark the file as done.
                    stale_total += 1;
                    if stale.len() < batch {
                        stale.push(json!({
                            "file": e.rel,
                            "reader": e.reader,
                            "kind": kind_for(&e.ext, e.reader),
                            "size": human_size(e.size),
                        }));
                    }
                }

                // Subfolder rollups (one-level mode). A folder is stale when
                // anything under it changed; the agent condenses it from the
                // child summaries we hand over, so no file is read twice.
                let mut folders_todo: Vec<Value> = Vec::new();
                if one_level {
                    for (dir_name, files) in subfolders(&entries) {
                        let digest = folder_digest(&files, &live_fp);
                        let fresh = !force
                            && cache
                                .folders
                                .get(&dir_name)
                                .is_some_and(|f| f.digest == digest && !f.summary.is_empty());
                        if fresh {
                            continue;
                        }
                        // Only summarized children are worth sending; a folder
                        // whose files are still pending gets its rollup on a
                        // later pass, once they have summaries.
                        let child: Vec<Value> = files
                            .iter()
                            .filter_map(|e| {
                                cache.files.get(&e.rel).and_then(|c| {
                                    let text = if !c.summary.is_empty() {
                                        c.summary.clone()
                                    } else if !c.skipped.is_empty() {
                                        c.skipped.clone()
                                    } else {
                                        return None;
                                    };
                                    Some(json!({
                                        "file": e.rel,
                                        "title": c.title,
                                        "summary": text,
                                    }))
                                })
                            })
                            .take(FOLDER_ROLLUP_SAMPLE)
                            .collect();
                        folders_todo.push(json!({
                            "folder": dir_name,
                            "files": files.len(),
                            "size": human_size(files.iter().map(|e| e.size).sum::<u64>()),
                            "described": child.len(),
                            "file_summaries": child,
                        }));
                    }
                }

                cache.generated = now.clone();
                cache.save(&dir)?;
                let index = render_index(
                    &folder_name,
                    &folder_rel,
                    &entries,
                    &cache,
                    truncated,
                    one_level,
                );
                std::fs::write(dir.join(INDEX_FILE), &index)
                    .map_err(|e| Error::Tool(format!("FolderIndex: cannot write index.md: {e}")))?;

                json!({
                    "action": "plan",
                    "folder": folder_rel,
                    "index_path": index_path,
                    "files": entries.len(),
                    "unchanged": unchanged,
                    "listed_only": listed_only,
                    "unreadable": unreadable,
                    "removed_from_cache": removed,
                    "truncated": truncated,
                    "to_summarize": stale_total,
                    "batch": stale,
                    "remaining_after_batch": stale_total.saturating_sub(stale.len()),
                    "report_depth": opts.report_depth,
                    "folders_to_summarize": folders_todo,
                    "next": match (stale_total, folders_todo.len()) {
                        (0, 0) => "index.md is up to date — nothing to read.".to_string(),
                        (0, f) => format!(
                            "Files are all described. Condense each of the {f} entries in `folders_to_summarize` from its `file_summaries` (no reading needed) and send them back as `folder_summaries` with action:\"commit\"."
                        ),
                        (_, f) => format!(
                            "Read the {} file(s) in `batch` with the listed reader tool, then call FolderIndex again with action:\"commit\" and one summary each.{}",
                            stale.len(),
                            if f > 0 { " Subfolder rollups follow on the next plan, once their files are described." } else { "" }
                        ),
                    },
                })
            }
            "commit" => {
                let mut applied = 0usize;
                let mut unknown: Vec<String> = Vec::new();
                if let Some(items) = input.get("summaries").and_then(Value::as_array) {
                    for it in items {
                        let Some(file) = it.get("file").and_then(Value::as_str) else {
                            continue;
                        };
                        let rel = normalize_rel(&dir, &folder_arg, file);
                        let Some(entry) = entries.iter().find(|e| e.rel == rel) else {
                            unknown.push(file.to_string());
                            continue;
                        };
                        let summary = it
                            .get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if summary.is_empty() {
                            unknown.push(file.to_string());
                            continue;
                        }
                        let fp = live_fp.get(&entry.rel).cloned().unwrap_or_default();
                        cache.files.insert(
                            entry.rel.clone(),
                            CachedFile {
                                fingerprint: fp,
                                summary,
                                title: it
                                    .get("title")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .trim()
                                    .to_string(),
                                tags: it
                                    .get("tags")
                                    .and_then(Value::as_array)
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(Value::as_str)
                                            .map(|s| s.trim().to_string())
                                            .filter(|s| !s.is_empty())
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                skipped: String::new(),
                                indexed: now.clone(),
                            },
                        );
                        applied += 1;
                    }
                }
                if let Some(items) = input.get("folder_summaries").and_then(Value::as_array) {
                    let live: std::collections::HashSet<&str> =
                        entries.iter().map(|e| top_level(&e.dir)).collect();
                    for it in items {
                        let Some(name) = it.get("folder").and_then(Value::as_str) else {
                            continue;
                        };
                        let key = name.trim().trim_end_matches('/').to_string();
                        let summary = it
                            .get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if summary.is_empty() || !live.contains(key.as_str()) {
                            unknown.push(name.to_string());
                            continue;
                        }
                        let files: Vec<&Entry> = entries
                            .iter()
                            .filter(|e| top_level(&e.dir) == key)
                            .collect();
                        cache.folders.insert(
                            key,
                            CachedFolder {
                                digest: folder_digest(&files, &live_fp),
                                title: it
                                    .get("title")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .trim()
                                    .to_string(),
                                summary,
                                indexed: now.clone(),
                            },
                        );
                        applied += 1;
                    }
                }
                if let Some(ov) = input.get("overview").and_then(Value::as_str) {
                    if !ov.trim().is_empty() {
                        cache.overview = ov.trim().to_string();
                    }
                }

                let pending = entries
                    .iter()
                    .filter(|e| {
                        e.reader.is_some()
                            && e.size <= opts.max_read_bytes
                            && !cache
                                .files
                                .get(&e.rel)
                                .is_some_and(|c| !c.summary.is_empty() || !c.skipped.is_empty())
                    })
                    .count();

                let folders_pending = if one_level {
                    subfolders(&entries)
                        .into_iter()
                        .filter(|(name, files)| {
                            let digest = folder_digest(files, &live_fp);
                            !cache
                                .folders
                                .get(name)
                                .is_some_and(|f| f.digest == digest && !f.summary.is_empty())
                        })
                        .count()
                } else {
                    0
                };

                cache.generated = now.clone();
                cache.save(&dir)?;
                let index = render_index(
                    &folder_name,
                    &folder_rel,
                    &entries,
                    &cache,
                    truncated,
                    one_level,
                );
                std::fs::write(dir.join(INDEX_FILE), &index)
                    .map_err(|e| Error::Tool(format!("FolderIndex: cannot write index.md: {e}")))?;

                json!({
                    "action": "commit",
                    "folder": folder_rel,
                    "index_path": index_path,
                    "applied": applied,
                    "unknown_files": unknown,
                    "still_pending": pending,
                    "folders_pending": folders_pending,
                    "files": entries.len(),
                    "next": match (pending, folders_pending) {
                        (0, 0) => "index.md rebuilt — folder fully described.".to_string(),
                        (0, f) => format!("{f} subfolder rollup(s) still missing — call action:\"plan\" to get their `file_summaries`."),
                        (p, _) => format!("{p} file(s) still unsummarized — call action:\"plan\" for the next batch."),
                    },
                })
            }
            other => {
                return Err(Error::Tool(format!(
                    "FolderIndex: unknown action '{other}' (expected plan or commit)"
                )))
            }
        };

        serde_json::to_string_pretty(&result)
            .map_err(|e| Error::Tool(format!("FolderIndex: cannot serialize result: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    async fn call(input: Value) -> Value {
        let out = FolderIndexTool.call(input).await.unwrap();
        serde_json::from_str(&out).unwrap()
    }

    #[tokio::test]
    async fn plan_lists_readable_files_then_commit_makes_them_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "notes.md", "# Notes\nhello");
        write(root, "docs/spec.txt", "spec body");
        write(root, "clip.mp4", "not really a video");

        let plan = call(json!({"path": root.to_str().unwrap()})).await;
        assert_eq!(plan["to_summarize"], 2, "md + txt are readable, mp4 is not");
        assert_eq!(plan["listed_only"], 1);
        // index.md exists even before a single summary lands.
        let index = std::fs::read_to_string(root.join(INDEX_FILE)).unwrap();
        assert!(index.contains("notes.md"), "index lists files: {index}");
        assert!(index.contains("clip.mp4"));

        let commit = call(json!({
            "path": root.to_str().unwrap(),
            "action": "commit",
            "overview": "A tiny folder.",
            "summaries": [
                {"file": "notes.md", "summary": "Short note file."},
                {"file": "docs/spec.txt", "summary": "The spec."}
            ]
        }))
        .await;
        assert_eq!(commit["applied"], 2);
        assert_eq!(commit["still_pending"], 0);
        let index = std::fs::read_to_string(root.join(INDEX_FILE)).unwrap();
        assert!(index.contains("Short note file."));
        assert!(index.contains("A tiny folder."));

        // Second plan re-reads nothing.
        let plan2 = call(json!({"path": root.to_str().unwrap()})).await;
        assert_eq!(plan2["to_summarize"], 0);
        assert_eq!(plan2["unchanged"], 2);
    }

    #[tokio::test]
    async fn changed_content_goes_stale_again() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "a.md", "one");
        call(json!({"path": root.to_str().unwrap()})).await;
        call(json!({
            "path": root.to_str().unwrap(),
            "action": "commit",
            "summaries": [{"file": "a.md", "summary": "first version"}]
        }))
        .await;
        assert_eq!(
            call(json!({"path": root.to_str().unwrap()})).await["to_summarize"],
            0
        );

        write(root, "a.md", "two — different bytes");
        let plan = call(json!({"path": root.to_str().unwrap()})).await;
        assert_eq!(plan["to_summarize"], 1);
        assert_eq!(plan["batch"][0]["file"], "a.md");
        assert_eq!(plan["batch"][0]["reader"], "Read");
    }

    #[tokio::test]
    async fn force_reindexes_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "a.md", "one");
        call(json!({"path": root.to_str().unwrap()})).await;
        call(json!({
            "path": root.to_str().unwrap(),
            "action": "commit",
            "summaries": [{"file": "a.md", "summary": "cached"}]
        }))
        .await;
        let plan = call(json!({"path": root.to_str().unwrap(), "force": true})).await;
        assert_eq!(plan["to_summarize"], 1);
    }

    #[tokio::test]
    async fn deleted_file_is_pruned_from_cache_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "a.md", "one");
        write(root, "b.md", "two");
        call(json!({"path": root.to_str().unwrap()})).await;
        call(json!({
            "path": root.to_str().unwrap(),
            "action": "commit",
            "summaries": [
                {"file": "a.md", "summary": "alpha"},
                {"file": "b.md", "summary": "beta"}
            ]
        }))
        .await;
        std::fs::remove_file(root.join("b.md")).unwrap();
        let plan = call(json!({"path": root.to_str().unwrap()})).await;
        assert_eq!(plan["removed_from_cache"], 1);
        let index = std::fs::read_to_string(root.join(INDEX_FILE)).unwrap();
        assert!(!index.contains("beta"), "stale row survived: {index}");
        let cache: IndexCache =
            serde_json::from_str(&std::fs::read_to_string(root.join(CACHE_FILE)).unwrap()).unwrap();
        assert!(!cache.files.contains_key("b.md"));
    }

    #[tokio::test]
    async fn non_recursive_stops_at_the_top_level() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "a.md", "one");
        write(root, "sub/b.md", "two");
        let plan = call(json!({"path": root.to_str().unwrap(), "recursive": false})).await;
        assert_eq!(plan["files"], 1);
    }

    #[tokio::test]
    async fn index_and_cache_never_index_themselves() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "a.md", "one");
        call(json!({"path": root.to_str().unwrap()})).await;
        let plan = call(json!({"path": root.to_str().unwrap()})).await;
        assert_eq!(plan["files"], 1, "index.md/.thclaws-index.json excluded");
    }

    #[tokio::test]
    async fn commit_reports_paths_it_cannot_place() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "a.md", "one");
        let out = call(json!({
            "path": root.to_str().unwrap(),
            "action": "commit",
            "summaries": [{"file": "ghost.md", "summary": "nope"}]
        }))
        .await;
        assert_eq!(out["applied"], 0);
        assert_eq!(out["unknown_files"][0], "ghost.md");
    }

    #[tokio::test]
    async fn commit_accepts_folder_prefixed_and_dot_slash_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "docs/a.md", "one");
        let folder = root.to_str().unwrap();
        call(json!({"path": folder})).await;
        let out = call(json!({
            "path": folder,
            "action": "commit",
            "summaries": [{"file": format!("{folder}/docs/a.md"), "summary": "absolute path"}]
        }))
        .await;
        assert_eq!(out["applied"], 1);
    }

    /// The Files tab turns links inside a rendered preview into in-app
    /// navigation: a plain relative link opens the file in the viewer, and a
    /// trailing slash marks a folder to walk into. Both shapes must survive
    /// rendering, and paths with spaces must stay clickable.
    #[tokio::test]
    async fn index_links_are_relative_and_folders_keep_their_slash() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "a.md", "one");
        write(root, "my docs/b (draft).md", "two");
        call(json!({"path": root.to_str().unwrap()})).await;
        let index = std::fs::read_to_string(root.join(INDEX_FILE)).unwrap();
        assert!(index.contains("[a.md](a.md)"), "{index}");
        assert!(
            index.contains("[b (draft).md](my%20docs/b%20%28draft%29.md)"),
            "spaces/parens must be encoded or the link breaks: {index}"
        );
        assert!(index.contains("## [./](./)"), "{index}");
        assert!(index.contains("## [my docs/](my%20docs/)"), "{index}");
    }

    /// The Files tab hands the tool whatever path the tree gave it — often
    /// absolute. That must not become the index's title.
    #[tokio::test]
    async fn absolute_path_is_titled_by_folder_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("demo");
        std::fs::create_dir_all(&root).unwrap();
        write(&root, "a.md", "one");
        let out = call(json!({"path": root.to_str().unwrap()})).await;
        assert_eq!(out["folder"], "demo");
        assert_eq!(out["index_path"], "demo/index.md");
        let index = std::fs::read_to_string(root.join(INDEX_FILE)).unwrap();
        assert!(index.contains("# Index — demo"), "{index}");
        assert!(
            !index.contains(root.to_str().unwrap()),
            "absolute path leaked into the index: {index}"
        );
    }

    /// `report_depth: 1` still scans (and summarizes) everything — it only
    /// changes what the index lists: own files, then one row per immediate
    /// subfolder. Nothing from the second level appears.
    #[tokio::test]
    async fn one_level_rolls_subfolders_into_a_single_row() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "readme.md", "top level");
        write(root, "docs/a.md", "alpha");
        write(root, "docs/deep/b.md", "beta");
        let path = root.to_str().unwrap();

        let plan = call(json!({"path": path, "report_depth": 1})).await;
        // Every file is scanned, including the second level.
        assert_eq!(plan["files"], 3);
        assert_eq!(plan["to_summarize"], 3);
        // Rollups wait until their children have summaries.
        assert_eq!(plan["folders_to_summarize"][0]["described"], 0);

        call(json!({
            "path": path, "action": "commit", "report_depth": 1,
            "summaries": [
                {"file": "readme.md", "summary": "Top-level readme."},
                {"file": "docs/a.md", "summary": "Alpha note."},
                {"file": "docs/deep/b.md", "summary": "Beta note."}
            ]
        }))
        .await;

        // Now the folder rollup is offered, seeded with the child summaries.
        let plan2 = call(json!({"path": path, "report_depth": 1})).await;
        assert_eq!(plan2["to_summarize"], 0);
        let todo = &plan2["folders_to_summarize"];
        assert_eq!(todo[0]["folder"], "docs");
        assert_eq!(todo[0]["files"], 2, "nested file counts toward its parent");
        assert_eq!(todo[0]["file_summaries"][1]["summary"], "Beta note.");

        let commit = call(json!({
            "path": path, "action": "commit", "report_depth": 1,
            "folder_summaries": [{"folder": "docs", "summary": "Notes and a deeper draft."}]
        }))
        .await;
        assert_eq!(commit["folders_pending"], 0);

        let index = std::fs::read_to_string(root.join(INDEX_FILE)).unwrap();
        assert!(index.contains("## Folders (1)"), "{index}");
        assert!(index.contains("Notes and a deeper draft."), "{index}");
        assert!(
            index.contains("(1 nested)"),
            "nested count missing: {index}"
        );
        assert!(index.contains("readme.md"), "{index}");
        // Second level never surfaces in the listing.
        assert!(!index.contains("deep/b.md"), "{index}");
        assert!(!index.contains("Beta note."), "{index}");
        assert!(!index.contains("Alpha note."), "{index}");
    }

    /// The rollup is keyed by a digest of everything underneath, so editing a
    /// second-level file makes its top-level folder stale again.
    #[tokio::test]
    async fn one_level_rollup_goes_stale_when_a_nested_file_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "docs/deep/b.md", "beta");
        let path = root.to_str().unwrap();
        call(json!({"path": path, "report_depth": 1})).await;
        call(json!({
            "path": path, "action": "commit", "report_depth": 1,
            "summaries": [{"file": "docs/deep/b.md", "summary": "Beta note."}]
        }))
        .await;
        call(json!({
            "path": path, "action": "commit", "report_depth": 1,
            "folder_summaries": [{"folder": "docs", "summary": "One draft."}]
        }))
        .await;
        assert_eq!(
            call(json!({"path": path, "report_depth": 1})).await["folders_to_summarize"]
                .as_array()
                .unwrap()
                .len(),
            0
        );

        write(root, "docs/deep/b.md", "beta, revised and longer");
        let plan = call(json!({"path": path, "report_depth": 1})).await;
        assert_eq!(plan["to_summarize"], 1);
        assert_eq!(
            plan["folders_to_summarize"][0]["folder"], "docs",
            "rollup must follow its subtree"
        );
    }

    #[tokio::test]
    async fn commit_rejects_a_folder_summary_for_a_folder_that_is_not_there() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "docs/a.md", "alpha");
        let path = root.to_str().unwrap();
        call(json!({"path": path, "report_depth": 1})).await;
        let out = call(json!({
            "path": path, "action": "commit", "report_depth": 1,
            "folder_summaries": [{"folder": "ghost", "summary": "nope"}]
        }))
        .await;
        assert_eq!(out["applied"], 0);
        assert_eq!(out["unknown_files"][0], "ghost");
    }

    /// The title rides in the description cell — bold, on its own line above
    /// the summary — so it can't be squeezed into an unreadable column.
    #[tokio::test]
    async fn title_renders_bold_above_the_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "a.md", "one");
        write(root, "docs/b.md", "two");
        let path = root.to_str().unwrap();

        call(json!({"path": path})).await;
        call(json!({
            "path": path, "action": "commit",
            "summaries": [
                {"file": "a.md", "summary": "First note."},
                {"file": "docs/b.md", "summary": "Second note."}
            ]
        }))
        .await;
        let index = std::fs::read_to_string(root.join(INDEX_FILE)).unwrap();
        assert!(
            index.contains("| File | Type |"),
            "header is unchanged: {index}"
        );
        assert!(!index.contains("<br>"), "no title yet, no break: {index}");

        call(json!({
            "path": path, "action": "commit",
            "summaries": [{
                "file": "a.md", "title": "ADR 001 — double entry",
                "summary": "First note."
            }]
        }))
        .await;
        let index = std::fs::read_to_string(root.join(INDEX_FILE)).unwrap();
        assert!(
            index.contains("**ADR 001 — double entry**<br>First note."),
            "{index}"
        );
        // Header stays five columns, and the untitled sibling still renders.
        assert!(index.contains("| File | Type |"), "{index}");
        assert!(index.contains("| Second note. |"), "{index}");
    }

    #[tokio::test]
    async fn folder_rollups_carry_a_title_too() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "docs/b.md", "two");
        let path = root.to_str().unwrap();
        call(json!({"path": path, "report_depth": 1})).await;
        call(json!({
            "path": path, "action": "commit", "report_depth": 1,
            "summaries": [{"file": "docs/b.md", "title": "Setup guide", "summary": "How to install."}]
        }))
        .await;
        // The rollup prompt carries the child titles, so it can reuse them.
        let plan = call(json!({"path": path, "report_depth": 1})).await;
        assert_eq!(
            plan["folders_to_summarize"][0]["file_summaries"][0]["title"],
            "Setup guide"
        );
        call(json!({
            "path": path, "action": "commit", "report_depth": 1,
            "folder_summaries": [{
                "folder": "docs", "title": "คู่มือติดตั้ง",
                "summary": "Install and run instructions."
            }]
        }))
        .await;
        let index = std::fs::read_to_string(root.join(INDEX_FILE)).unwrap();
        assert!(index.contains("| Folder | Files |"), "{index}");
        assert!(
            index.contains("**คู่มือติดตั้ง**<br>Install and run instructions."),
            "{index}"
        );
    }

    #[test]
    fn table_cells_survive_pipes_and_newlines() {
        assert_eq!(cell("a | b\nc"), "a \\| b c");
        assert_eq!(cell("  "), "—");
    }

    #[test]
    fn readers_match_the_engine_tools() {
        assert_eq!(reader_for("pdf"), Some("PdfRead"));
        assert_eq!(reader_for("xlsx"), Some("XlsxRead"));
        assert_eq!(reader_for("png"), Some("Read"));
        assert_eq!(reader_for("rs"), Some("Read"));
        // Vision can't decode these, so they're listed, not read.
        assert_eq!(reader_for("heic"), None);
        assert_eq!(reader_for("zip"), None);
    }
}
