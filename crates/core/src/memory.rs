//! Persistent memory: a directory of markdown files the agent can read as
//! long-lived context.
//!
//! Shape:
//! - `<root>/MEMORY.md` — index. Free-form markdown; the typical pattern is
//!   one line per entry pointing at a topic file. Auto-maintained by
//!   `write_entry` / `delete_entry` so it stays consistent with on-disk
//!   entries (M6.26).
//! - `<root>/<slug>.md` — individual entries with optional YAML-ish
//!   frontmatter (`name`, `description`, `type`, `category`, `created`,
//!   `updated`) followed by a body. Frontmatter is parsed loosely: `---`
//!   fences, `key: value` lines inside. Anything outside the fences goes
//!   in `body`.
//!
//! M6.26 makes memory **LLM-maintainable**:
//! - `MemoryWrite` / `MemoryAppend` tools (sandbox carve-out for the
//!   resolved memory root)
//! - `MemoryRead` tool for on-demand entry fetch
//! - `/memory write|append|delete|edit` slash commands for direct user
//!   authoring
//! - Auto-maintained `MEMORY.md` index
//! - Categorized index in system prompt (frontmatter `category:`)
//! - Total prompt-budget cap on inlined bodies; overflow entries become
//!   "use MemoryRead to fetch" pointers
//!
//! Sandbox carve-out: `MemoryWrite`/`MemoryAppend` bypass `Sandbox::check_write`
//! to land inside the resolved memory root (project-scope `.thclaws/memory/`
//! is otherwise blocked). Path safety enforced at finer grain via
//! `writable_entry_path` (no `..` / separators / control chars / reserved
//! `MEMORY` stem; canonicalized inside the memory root). Same intentional
//! carve-out pattern as `TodoWrite` and `KmsWrite`.

use crate::error::{Error, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    /// File stem, e.g. `user_role` for `user_role.md`.
    pub name: String,
    pub description: String,
    /// Frontmatter `type` field, if present (e.g. `user`, `feedback`, `project`).
    pub memory_type: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct MemoryStore {
    pub root: PathBuf,
}

impl MemoryStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Preferred path (project-scoped): `./.thclaws/memory/`.
    /// Falls back — in order — to the old user-level Claude-Code-compatible
    /// per-project dirs, then to the user-global `~/.local/share/thclaws/memory/`.
    pub fn default_path() -> Option<PathBuf> {
        // Project-scoped: if we're inside a thClaws project (./.thclaws/
        // exists), keep memory with the project. Create the memory/ dir as
        // needed. Use the task-local working dir (not the process cwd) so
        // that under the multi-tenant "workspace per user" model each
        // user's turn resolves memory in THEIR workspace_root — process
        // cwd is shared across the one worker and would leak memory across
        // tenants. `current_workdir()` falls back to process cwd outside a
        // task scope, so single-tenant / desktop behaviour is unchanged.
        {
            let cwd = crate::workdir::current_workdir();
            let project_root = cwd.join(".thclaws");
            if project_root.is_dir() {
                return Some(project_root.join("memory"));
            }
        }

        let home = crate::util::home_dir()?;

        // Legacy user-level per-project paths (read-only fallback).
        if let Ok(cwd) = std::env::current_dir() {
            let sanitized = cwd
                .to_string_lossy()
                .replace('/', "-")
                .trim_start_matches('-')
                .to_string();
            let claude_project = home
                .join(".claude/projects")
                .join(&sanitized)
                .join("memory");
            if claude_project.exists() {
                return Some(claude_project);
            }
            let thclaws_project = home
                .join(".thclaws/projects")
                .join(&sanitized)
                .join("memory");
            if thclaws_project.exists() {
                return Some(thclaws_project);
            }
        }

        // Global fallback.
        let thclaws = home.join(".local/share/thclaws/memory");
        if thclaws.exists() {
            return Some(thclaws);
        }
        Some(thclaws)
    }

    /// Free-form contents of `MEMORY.md` (the index file), or `None` if missing.
    /// The index is a pointer sheet, not an archive — runaway growth silently
    /// burns tokens on every turn. Same caps as Claude Code:
    ///   * 200 lines (line-truncated at a natural newline boundary).
    ///   * 25 KB (byte-truncated at the last newline under cap after the line
    ///     pass).
    ///
    /// When either fires, a one-line notice is appended so the user sees
    /// *why* older entries stopped reaching the model.
    pub fn index(&self) -> Option<String> {
        let raw = std::fs::read_to_string(self.root.join("MEMORY.md")).ok()?;
        Some(truncate_index(&raw))
    }

    /// List all `*.md` files in the root (excluding `MEMORY.md`), parsed as
    /// `MemoryEntry`. Sorted by name. Returns empty vec if the root is missing.
    pub fn list(&self) -> Result<Vec<MemoryEntry>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root)?.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name == "MEMORY.md" || !name.ends_with(".md") {
                continue;
            }
            if let Some(parsed) = parse_entry_file(&path) {
                out.push(parsed);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Read one entry by file-stem name (e.g. `user_role`).
    pub fn get(&self, name: &str) -> Option<MemoryEntry> {
        parse_entry_file(&self.root.join(format!("{name}.md")))
    }

    /// Produce the memory section for the system prompt.
    ///
    /// M6.26 BUG #4 + #5: categorize entries by frontmatter `category:`
    /// (group bullets in the index when at least one entry uses it) AND
    /// cap total inlined body bytes (`MEMORY_TOTAL_INLINE_BYTES`).
    /// Entries that don't fit the budget are reduced to one-line
    /// pointers (`- <name> [<type>] — <description>. Use MemoryRead to
    /// fetch.`); the model can pull them in on demand via the
    /// `MemoryRead` tool.
    ///
    /// Inlining order: alphabetical by name (deterministic). The budget
    /// fills greedily; small entries early in the alphabet have an
    /// advantage. Users who want priority control should keep their
    /// always-needed entries small enough to fit.
    pub fn system_prompt_section(&self) -> Option<String> {
        let entries = self.list().ok()?;
        let index = self.index().and_then(|s| {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        });
        if entries.is_empty() && index.is_none() {
            return None;
        }

        let mut parts: Vec<String> = Vec::new();

        // Index — either categorized (when frontmatter `category:` is in
        // use) or the raw MEMORY.md (legacy). Categorized form scales
        // better for many entries.
        let any_category = entries.iter().any(|e| {
            // Re-read frontmatter to check for `category:` (MemoryEntry
            // doesn't store arbitrary frontmatter).
            std::fs::read_to_string(self.root.join(format!("{}.md", e.name)))
                .ok()
                .map(|raw| parse_frontmatter(&raw).0.contains_key("category"))
                .unwrap_or(false)
        });
        if any_category {
            parts.push(self.render_categorized_index(&entries));
        } else if let Some(index_text) = index {
            parts.push(format!("## Index\n{index_text}"));
        }

        // Inline entry bodies up to a total byte budget. Anything past
        // the budget becomes a one-line pointer.
        let mut used = 0usize;
        for e in &entries {
            let mut section = format!("## {}", e.name);
            if let Some(ty) = &e.memory_type {
                section.push_str(&format!(" ({ty})"));
            }
            if !e.description.is_empty() {
                section.push_str(&format!("\n_{}_", e.description));
            }
            let body = e.body.trim();

            if body.is_empty() {
                used = used.saturating_add(section.len());
                parts.push(section);
                continue;
            }

            // Per-entry cap (M6.18 BUG M5) still applies — protects
            // against a single runaway entry even within the total
            // budget.
            let bounded = truncate_for_prompt(
                body,
                MEMORY_ENTRY_MAX_LINES,
                MEMORY_ENTRY_MAX_BYTES,
                &format!("memory entry `{}`", e.name),
            );

            if used + bounded.len() > MEMORY_TOTAL_INLINE_BYTES {
                // Out of budget — replace body with on-demand pointer.
                section.push_str(&format!(
                    "\n\n_(body deferred — {} bytes; call `MemoryRead(name: \"{}\")` to fetch.)_",
                    bounded.len(),
                    e.name,
                ));
            } else {
                used = used.saturating_add(bounded.len());
                section.push_str("\n\n");
                section.push_str(&bounded);
            }
            parts.push(section);
        }

        // Tool affordances — the model knows it can write/append/read.
        parts.push(
            "## Tools\n\
             - `MemoryRead(name: \"<entry>\")` — read full body of a deferred entry\n\
             - `MemoryWrite(name: \"<entry>\", content: \"...\")` — create or replace an entry\n\
             - `MemoryAppend(name: \"<entry>\", content: \"...\")` — append to an entry\n\
             Entries may carry YAML frontmatter (`name`, `description`, `type`, `category`, \
             `created`, `updated`). `MEMORY.md` index is auto-maintained on write/delete."
                .to_string(),
        );

        Some(parts.join("\n\n"))
    }

    /// Render a categorized index from per-entry frontmatter `category:`
    /// fields. Pages without a category go under `**uncategorized**`.
    /// Mirrors `kms::render_index_section` from M6.25.
    fn render_categorized_index(&self, entries: &[MemoryEntry]) -> String {
        use std::collections::BTreeMap;
        let mut by_category: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for e in entries {
            let raw = std::fs::read_to_string(self.root.join(format!("{}.md", e.name)))
                .unwrap_or_default();
            let (fm, _) = parse_frontmatter(&raw);
            let category = fm
                .get("category")
                .cloned()
                .unwrap_or_else(|| "uncategorized".into());
            by_category
                .entry(category)
                .or_default()
                .push((e.name.clone(), e.description.clone()));
        }
        let mut out = String::from("## Index\n");
        for (cat, mut items) in by_category {
            items.sort();
            out.push_str(&format!("\n**{cat}**\n"));
            for (name, desc) in items {
                if desc.is_empty() {
                    out.push_str(&format!("- {name}\n"));
                } else {
                    out.push_str(&format!("- {name} — {desc}\n"));
                }
            }
        }
        out
    }
}

// ────────────────────────────────────────────────────────────────────────
// M6.26 BUG #1 + #3: write helpers + auto-maintained MEMORY.md index.

/// Names that must not be used as entry stems. `MEMORY` would clobber
/// the index file; an empty stem produces `<root>/.md` which is hidden
/// and confusing.
const RESERVED_ENTRY_STEMS: &[&str] = &["MEMORY"];

/// Resolve `name` to a writable path inside the memory root. Refuses
/// `..`, path separators, control chars, absolute paths, and the
/// `MEMORY` reserved stem. Canonicalizes the parent inside the memory
/// root so symlink escapes are caught. Mirrors `kms::writable_page_path`.
pub fn writable_entry_path(store: &MemoryStore, name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.chars().any(|c| c.is_control())
        || Path::new(name).is_absolute()
    {
        return Err(Error::Tool(format!(
            "invalid memory name '{name}' — no '..', path separators, or control chars"
        )));
    }
    let stem = name.trim_end_matches(".md");
    if RESERVED_ENTRY_STEMS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(stem))
    {
        return Err(Error::Tool(format!(
            "memory name '{name}' is reserved — pick another stem"
        )));
    }
    let filename = if name.ends_with(".md") {
        name.to_string()
    } else {
        format!("{name}.md")
    };

    std::fs::create_dir_all(&store.root)
        .map_err(|e| Error::Tool(format!("ensure memory root: {e}")))?;
    if let Ok(md) = std::fs::symlink_metadata(&store.root) {
        if md.file_type().is_symlink() {
            return Err(Error::Tool(
                "memory root is a symlink — refusing to write".into(),
            ));
        }
    }
    let canon_root = std::fs::canonicalize(&store.root)
        .map_err(|e| Error::Tool(format!("canonicalize memory root: {e}")))?;
    let candidate = canon_root.join(&filename);
    if let Ok(canon_existing) = std::fs::canonicalize(&candidate) {
        if !canon_existing.starts_with(&canon_root) {
            return Err(Error::Tool(format!(
                "memory '{name}' resolves outside the memory root — symlink escape rejected"
            )));
        }
    }
    Ok(candidate)
}

/// Write an entry (create-or-replace). Preserves user-supplied
/// frontmatter and merges with auto-stamped `name`, `created` (new only),
/// `updated` (always today). Updates `MEMORY.md` to add/dedupe the
/// bullet for this entry.
pub fn write_entry(store: &MemoryStore, name: &str, content: &str) -> Result<PathBuf> {
    let path = writable_entry_path(store, name)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string();
    let existed = path.exists();

    // Parse user-supplied frontmatter; merge with stamps. On replace,
    // preserve `created:` from the existing file if the new content
    // doesn't override it (treating `created:` as immutable history).
    let (mut fm, body) = parse_frontmatter(content);
    let today = crate::usage::today_str();
    fm.entry("name".into()).or_insert_with(|| stem.clone());
    fm.entry("updated".into()).or_insert_with(|| today.clone());
    if existed {
        let raw_existing = std::fs::read_to_string(&path).unwrap_or_default();
        let (existing_fm, _) = parse_frontmatter(&raw_existing);
        if let Some(prior_created) = existing_fm.get("created") {
            fm.entry("created".into())
                .or_insert_with(|| prior_created.clone());
        }
    } else {
        fm.entry("created".into()).or_insert(today.clone());
    }
    let serialized = write_frontmatter_map(&fm, &body);
    std::fs::write(&path, serialized.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;

    // Auto-update MEMORY.md so the index stays consistent.
    let description = fm.get("description").cloned().unwrap_or_default();
    update_memory_index_bullet(store, &stem, &description)?;
    Ok(path)
}

/// Append a chunk to an entry. Creates with bare body if missing.
/// Bumps `updated:` if frontmatter present.
pub fn append_to_entry(store: &MemoryStore, name: &str, chunk: &str) -> Result<PathBuf> {
    use std::io::Write;
    let path = writable_entry_path(store, name)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string();
    let existed = path.exists();
    if existed {
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let (mut fm, body) = parse_frontmatter(&raw);
        if !fm.is_empty() {
            fm.insert("updated".into(), crate::usage::today_str());
            let mut new_body = body;
            if !new_body.ends_with('\n') {
                new_body.push('\n');
            }
            new_body.push_str(chunk);
            let serialized = write_frontmatter_map(&fm, &new_body);
            std::fs::write(&path, serialized.as_bytes())
                .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        } else {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
            if !raw.ends_with('\n') {
                writeln!(f).ok();
            }
            f.write_all(chunk.as_bytes())
                .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        }
    } else {
        std::fs::write(&path, chunk.as_bytes())
            .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        // Add an index bullet for the new entry.
        update_memory_index_bullet(store, &stem, "")?;
    }
    Ok(path)
}

/// Delete an entry: removes the file AND drops the matching bullet from
/// `MEMORY.md`. Returns `Ok(path)` even when already missing
/// (idempotent — same posture as `SessionStore::delete`).
pub fn delete_entry(store: &MemoryStore, name: &str) -> Result<PathBuf> {
    // Validate the name (same rules as write — refuses traversal).
    let path = writable_entry_path(store, name)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string();
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| Error::Tool(format!("remove {}: {e}", path.display())))?;
    }
    remove_memory_index_bullet(store, &stem)?;
    Ok(path)
}

/// Rewrite `MEMORY.md` to add/replace a bullet for `name` with
/// `description`. Drops any existing bullet matching `(<name>.md)` first
/// so re-writes don't produce duplicates. Idempotent on identical input.
fn update_memory_index_bullet(store: &MemoryStore, name: &str, description: &str) -> Result<()> {
    use std::io::Write;
    let path = store.root.join("MEMORY.md");
    let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
    let needle = format!("({name}.md)");
    existing = existing
        .lines()
        .filter(|l| !l.contains(&needle))
        .collect::<Vec<_>>()
        .join("\n");
    if !existing.ends_with('\n') && !existing.is_empty() {
        existing.push('\n');
    }
    let bullet = if description.is_empty() {
        format!("- [{name}]({name}.md)\n")
    } else {
        format!("- [{name}]({name}.md) — {description}\n")
    };
    existing.push_str(&bullet);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
    f.write_all(existing.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// Drop the bullet for `name` from `MEMORY.md` if present. No-op if the
/// index file doesn't exist.
fn remove_memory_index_bullet(store: &MemoryStore, name: &str) -> Result<()> {
    use std::io::Write;
    let path = store.root.join("MEMORY.md");
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let needle = format!("({name}.md)");
    let filtered = existing
        .lines()
        .filter(|l| !l.contains(&needle))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = filtered;
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
    f.write_all(out.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// Serialize a frontmatter map + body into a memory entry. Same shape
/// as `kms::write_frontmatter` (auto-quotes `:` / `#` / leading space /
/// `"` / `\n` values). Empty map → just the body.
pub fn write_frontmatter_map(map: &HashMap<String, String>, body: &str) -> String {
    if map.is_empty() {
        return body.to_string();
    }
    // Sort keys for deterministic output (matters for prompt-cache stability).
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let mut out = String::from("---\n");
    for k in keys {
        let v = &map[k];
        let needs_quote = v.contains(':')
            || v.contains('#')
            || v.starts_with(' ')
            || v.contains('"')
            || v.contains('\n');
        if needs_quote {
            let escaped = v.replace('"', "\\\"");
            out.push_str(&format!("{k}: \"{escaped}\"\n"));
        } else {
            out.push_str(&format!("{k}: {v}\n"));
        }
    }
    out.push_str("---\n");
    out.push_str(body);
    out
}

/// Byte sizes of `MEMORY.md` (index) plus per-topic entry files, used
/// by `/context` to break down memory's contribution to the system
/// prompt. Returns `(index_bytes, Vec<(name, bytes)> per entry)`. Any
/// file that can't be stat'd is silently skipped — callers treat
/// "missing" and "size 0" the same way.
pub fn memory_sizes(store: &MemoryStore) -> (u64, Vec<(String, u64)>) {
    let index_bytes = std::fs::metadata(store.root.join("MEMORY.md"))
        .map(|m| m.len())
        .unwrap_or(0);
    let mut entries: Vec<(String, u64)> = Vec::new();
    if let Ok(read) = std::fs::read_dir(&store.root) {
        for entry in read.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name == "MEMORY.md" || !name.ends_with(".md") {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    entries.push((name.to_string(), meta.len()));
                }
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    (index_bytes, entries)
}

/// Line cap for `MEMORY.md` — matches Claude Code's `MAX_ENTRYPOINT_LINES`.
pub const MEMORY_INDEX_MAX_LINES: usize = 200;
/// Byte cap for `MEMORY.md`. ~125 chars/line at the line cap; this catches
/// long-line indexes that slip past the line count but still bloat the
/// prompt. Matches Claude Code's `MAX_ENTRYPOINT_BYTES`.
pub const MEMORY_INDEX_MAX_BYTES: usize = 25_000;

/// Per-entry body cap (M6.18 BUG M5). Memory entries are meant to be
/// tight notes — a recurring fact about the user, a rule from
/// feedback, a reference to an external system. Pre-fix `system_prompt_section`
/// inlined the full body of every entry without limit; a runaway 100K
/// note would burn 100K tokens of system prompt every turn. Cap is
/// generous enough for legitimate paragraphs of detail, mean enough
/// to catch accidental dumps.
pub const MEMORY_ENTRY_MAX_LINES: usize = 80;
pub const MEMORY_ENTRY_MAX_BYTES: usize = 8_000;

/// M6.26 BUG #5: total budget for inlined memory bodies in the system
/// prompt. Beyond this, entries become one-line on-demand pointers
/// (`call MemoryRead(name: "...") to fetch`). Caps are filled greedily
/// in alphabetical order — small entries early in the alphabet have
/// an advantage. Pre-M6.26 every entry's body was always inlined,
/// burning unbounded tokens per turn at scale (50 entries × 8 KB =
/// 400 KB of system prompt). 16 KB is generous enough for ~5-10
/// always-on identity entries while keeping the prompt under control.
pub const MEMORY_TOTAL_INLINE_BYTES: usize = 16_000;

/// Generic truncate-with-notice for any text inlined into a system
/// prompt section. Truncates first by lines (natural newline boundary,
/// keeps markdown reasonable), then by bytes if still over cap (cuts
/// at the last newline under the cap). Appends a one-line HTML-comment
/// notice describing what was kept and what dropped, so the model sees
/// the truncation explicitly. M6.18 BUG M5/M6/M7 helper — `truncate_index`
/// is now a thin wrapper that calls this with `MEMORY.md`'s caps.
pub fn truncate_for_prompt(raw: &str, max_lines: usize, max_bytes: usize, label: &str) -> String {
    let trimmed = raw.trim_end_matches('\n');
    let total_lines = trimmed.split('\n').count();
    let total_bytes = trimmed.len();
    let mut line_truncated = false;
    let mut byte_truncated = false;

    let after_lines: String = if total_lines > max_lines {
        line_truncated = true;
        trimmed
            .split('\n')
            .take(max_lines)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        trimmed.to_string()
    };

    let after_bytes: String = if after_lines.len() > max_bytes {
        byte_truncated = true;
        let cap = max_bytes.min(after_lines.len());
        // Walk back to a UTF-8 char boundary so we can slice safely.
        let mut end = cap;
        while end > 0 && !after_lines.is_char_boundary(end) {
            end -= 1;
        }
        let slice = &after_lines[..end];
        match slice.rfind('\n') {
            Some(cut) => slice[..cut].to_string(),
            None => slice.to_string(),
        }
    } else if total_bytes > max_bytes {
        byte_truncated = true;
        after_lines
    } else {
        after_lines
    };

    if !line_truncated && !byte_truncated {
        return after_bytes;
    }

    let mut out = after_bytes;
    out.push_str(&format!("\n\n<!-- {label} truncated: "));
    match (line_truncated, byte_truncated) {
        (true, true) => out.push_str(&format!(
            "{total_lines} lines / {total_bytes} bytes → kept first {max_lines} lines under {max_bytes} byte cap.",
        )),
        (true, false) => out.push_str(&format!(
            "{total_lines} lines → kept first {max_lines}.",
        )),
        (false, true) => out.push_str(&format!(
            "{total_bytes} bytes > {max_bytes} cap → kept earlier content.",
        )),
        _ => unreachable!(),
    }
    out.push_str(" -->\n");
    out
}

/// Apply `MEMORY.md`'s 200-line / 25 KB caps and, when either triggered,
/// append a single notice line so the model (and the user, if they /memory)
/// see that older entries dropped off.
pub fn truncate_index(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('\n');
    let total_lines = trimmed.split('\n').count();
    let total_bytes = trimmed.len();
    let mut line_truncated = false;
    let mut byte_truncated = false;

    // Line pass first — natural newline boundary keeps markdown
    // reasonable (no mid-bullet cuts).
    let after_lines: String = if total_lines > MEMORY_INDEX_MAX_LINES {
        line_truncated = true;
        trimmed
            .split('\n')
            .take(MEMORY_INDEX_MAX_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        trimmed.to_string()
    };

    // Byte pass: if still too fat (long-line index), cut at the last
    // newline under the cap. The original byte count drives the "was
    // truncated" flag so long lines still trip the warning even when
    // the line cap happens to pass.
    let after_bytes: String = if after_lines.len() > MEMORY_INDEX_MAX_BYTES {
        byte_truncated = true;
        let slice = &after_lines[..MEMORY_INDEX_MAX_BYTES];
        match slice.rfind('\n') {
            Some(cut) => slice[..cut].to_string(),
            None => slice.to_string(),
        }
    } else if total_bytes > MEMORY_INDEX_MAX_BYTES {
        byte_truncated = true;
        after_lines
    } else {
        after_lines
    };

    if !line_truncated && !byte_truncated {
        return after_bytes;
    }

    let mut out = after_bytes;
    out.push_str("\n\n<!-- MEMORY.md truncated: ");
    match (line_truncated, byte_truncated) {
        (true, true) => out.push_str(&format!(
            "{total_lines} lines / {total_bytes} bytes → kept first {} lines under {} byte cap.",
            MEMORY_INDEX_MAX_LINES, MEMORY_INDEX_MAX_BYTES,
        )),
        (true, false) => out.push_str(&format!(
            "{total_lines} lines → kept first {}. Move older entries into topic-named `<name>.md` files so the index stays an index.",
            MEMORY_INDEX_MAX_LINES,
        )),
        (false, true) => out.push_str(&format!(
            "{total_bytes} bytes > {} cap → kept earlier content.",
            MEMORY_INDEX_MAX_BYTES,
        )),
        _ => unreachable!(),
    }
    out.push_str(" -->\n");
    out
}

fn parse_entry_file(path: &Path) -> Option<MemoryEntry> {
    let contents = std::fs::read_to_string(path).ok()?;
    let name = path.file_stem()?.to_string_lossy().into_owned();
    let (front, body) = parse_frontmatter(&contents);
    let description = front.get("description").cloned().unwrap_or_default();
    let memory_type = front.get("type").cloned();
    Some(MemoryEntry {
        name,
        description,
        memory_type,
        body,
    })
}

/// Parse YAML-ish frontmatter between `---` fences at the start of the file.
/// Anything else goes in `body`. Intentionally permissive — missing fences,
/// trailing whitespace, and non-`key: value` lines inside the block are all OK.
pub fn parse_frontmatter(s: &str) -> (HashMap<String, String>, String) {
    let mut map = HashMap::new();

    // Must open with `---` on the first line.
    let mut lines = s.lines();
    let Some(first) = lines.next() else {
        return (map, String::new());
    };
    if first.trim() != "---" {
        return (map, s.to_string());
    }

    let mut fm_lines: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        fm_lines.push(line);
    }
    if !closed {
        return (map, s.to_string());
    }

    let mut i = 0;
    while i < fm_lines.len() {
        let line = fm_lines[i];
        let Some((k, v)) = line.split_once(':') else {
            i += 1;
            continue;
        };
        let key = k.trim().to_string();
        let vtrim = v.trim();

        // YAML block scalar — `key: >` (folded) or `key: |` (literal),
        // with optional chomping/indent indicators (`>-`, `|+`, `>2`).
        // The value is the indented lines that follow. The old
        // line-based parser dropped them, so a `description: >` block
        // surfaced as literally ">" (and hid the skill on /skills).
        let is_block = {
            let mut c = vtrim.chars();
            matches!(c.next(), Some('>') | Some('|'))
                && c.all(|ch| ch == '-' || ch == '+' || ch.is_ascii_digit())
        };
        if is_block {
            let folded = vtrim.starts_with('>');
            // Frontmatter keys sit at column 0, so any blank or
            // leading-whitespace line belongs to the block; the first
            // column-0 line ends it.
            let mut parts: Vec<String> = Vec::new();
            let mut j = i + 1;
            while j < fm_lines.len() {
                let cont = fm_lines[j];
                if cont.trim().is_empty() {
                    parts.push(String::new());
                } else if cont.starts_with([' ', '\t']) {
                    parts.push(cont.trim().to_string());
                } else {
                    break;
                }
                j += 1;
            }
            let value = if folded {
                // Fold: blank line → newline, adjacent lines → one space.
                let mut out = String::new();
                for p in &parts {
                    if p.is_empty() {
                        out.push('\n');
                    } else {
                        if !out.is_empty() && !out.ends_with('\n') {
                            out.push(' ');
                        }
                        out.push_str(p);
                    }
                }
                out.trim().to_string()
            } else {
                parts.join("\n").trim().to_string()
            };
            map.insert(key, value);
            i = j;
            continue;
        }

        // M6.26: strip surrounding quotes so values written by
        // `write_frontmatter_map` round-trip correctly. Matches
        // kms::parse_frontmatter behavior.
        let val = vtrim.trim_matches('"').trim_matches('\'').to_string();
        map.insert(key, val);
        i += 1;
    }

    // Remaining iterator is the body.
    let body: String = lines
        .collect::<Vec<_>>()
        .join("\n")
        .trim_start()
        .to_string();
    (map, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn parse_frontmatter_extracts_fields_and_body() {
        let s = "---\nname: foo\ndescription: a thing\ntype: user\n---\nbody text\nmore body";
        let (front, body) = parse_frontmatter(s);
        assert_eq!(front.get("name").map(String::as_str), Some("foo"));
        assert_eq!(
            front.get("description").map(String::as_str),
            Some("a thing")
        );
        assert_eq!(front.get("type").map(String::as_str), Some("user"));
        assert_eq!(body, "body text\nmore body");
    }

    #[test]
    fn parse_frontmatter_folds_block_scalar_description() {
        // Regression: a `description: >` folded block used to parse as
        // literally ">", hiding the skill on /skills. It must fold into
        // one space-joined string, and a later key must still parse.
        let s = "---\nname: social-featured-image\ndescription: >\n  Generate a social\n  featured image from a prompt.\nmodel: gemini\n---\nbody";
        let (front, body) = parse_frontmatter(s);
        assert_eq!(
            front.get("description").map(String::as_str),
            Some("Generate a social featured image from a prompt."),
        );
        assert_eq!(
            front.get("name").map(String::as_str),
            Some("social-featured-image")
        );
        assert_eq!(front.get("model").map(String::as_str), Some("gemini"));
        assert_eq!(body, "body");
    }

    #[test]
    fn parse_frontmatter_literal_block_scalar_keeps_newlines() {
        let s = "---\ndescription: |\n  line one\n  line two\nname: x\n---\nbody";
        let (front, _body) = parse_frontmatter(s);
        assert_eq!(
            front.get("description").map(String::as_str),
            Some("line one\nline two"),
        );
        assert_eq!(front.get("name").map(String::as_str), Some("x"));
    }

    #[test]
    fn parse_frontmatter_missing_fences_returns_body_as_is() {
        let s = "no frontmatter here";
        let (front, body) = parse_frontmatter(s);
        assert!(front.is_empty());
        assert_eq!(body, s);
    }

    #[test]
    fn truncate_index_short_input_untouched() {
        let s = "# memory\n\n- one line\n- two lines\n";
        assert_eq!(truncate_index(s), s.trim_end_matches('\n'));
    }

    #[test]
    fn truncate_index_caps_at_200_lines() {
        let lines: Vec<String> = (0..500).map(|i| format!("- entry {i}")).collect();
        let s = lines.join("\n");
        let out = truncate_index(&s);
        // First 200 entries + notice block.
        assert!(out.starts_with("- entry 0"));
        assert!(out.contains("- entry 199"));
        assert!(!out.contains("- entry 200"));
        assert!(out.contains("MEMORY.md truncated"));
        assert!(out.contains("500 lines"));
    }

    #[test]
    fn truncate_index_caps_at_25kb_for_long_lines() {
        // One line, 30 KB of text → under the line cap, over the byte cap.
        let s: String = "x".repeat(30_000);
        let out = truncate_index(&s);
        assert!(out.len() < 30_000);
        assert!(out.contains("MEMORY.md truncated"));
        assert!(out.contains("bytes"));
    }

    #[test]
    fn parse_frontmatter_unclosed_fence_is_body() {
        let s = "---\nname: foo\nno closing fence";
        let (front, body) = parse_frontmatter(s);
        assert!(front.is_empty());
        assert_eq!(body, s);
    }

    #[test]
    fn list_returns_empty_when_missing() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("nonexistent"));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_skips_memory_md_index_and_non_md_files() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        write(&store.root.join("MEMORY.md"), "# index");
        write(&store.root.join("scratch.txt"), "not markdown");
        write(
            &store.root.join("user.md"),
            "---\ndescription: who I am\ntype: user\n---\nbody",
        );
        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "user");
        assert_eq!(entries[0].description, "who I am");
        assert_eq!(entries[0].memory_type.as_deref(), Some("user"));
    }

    #[test]
    fn get_reads_single_entry_by_name() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        write(
            &store.root.join("proj.md"),
            "---\ndescription: current sprint\ntype: project\n---\nsprint body",
        );
        let entry = store.get("proj").unwrap();
        assert_eq!(entry.name, "proj");
        assert_eq!(entry.description, "current sprint");
        assert_eq!(entry.memory_type.as_deref(), Some("project"));
        assert_eq!(entry.body, "sprint body");
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        assert!(store.get("nope").is_none());
    }

    #[test]
    fn list_sorts_by_name() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        write(&store.root.join("b.md"), "---\ndescription: second\n---\n");
        write(&store.root.join("a.md"), "---\ndescription: first\n---\n");
        let names: Vec<String> = store.list().unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn system_prompt_section_omits_when_empty_and_no_index() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        std::fs::create_dir_all(&store.root).unwrap();
        assert!(store.system_prompt_section().is_none());
    }

    #[test]
    fn system_prompt_section_renders_full_bodies() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        write(&store.root.join("MEMORY.md"), "- [foo](foo.md) — hook line");
        write(
            &store.root.join("foo.md"),
            "---\ndescription: foo entry\ntype: user\n---\nActual body content goes here.",
        );
        write(
            &store.root.join("bar.md"),
            "---\n---\njust a body, no frontmatter",
        );

        let section = store.system_prompt_section().unwrap();
        // Index is rendered
        assert!(section.contains("## Index"));
        assert!(section.contains("hook line"));
        // Each entry becomes its own ## section
        assert!(section.contains("## foo"));
        assert!(section.contains("(user)")); // type annotation
        assert!(section.contains("_foo entry_")); // description
        assert!(section.contains("Actual body content goes here."));
        // Body-only entry (no description)
        assert!(section.contains("## bar"));
        assert!(section.contains("just a body"));
    }

    /// M6.18 BUG M5: per-entry body cap. Pre-fix a runaway 100K
    /// memory entry burned 100K tokens of system prompt every turn;
    /// now the body is truncated and a notice tells the model what
    /// dropped.
    #[test]
    fn system_prompt_section_caps_oversized_entry_body() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        // 200 lines × 100 chars = ~20 KB → exceeds both caps.
        let huge_body = (0..200)
            .map(|i| format!("line {i}: {}", "x".repeat(100)))
            .collect::<Vec<_>>()
            .join("\n");
        let entry = format!("---\ndescription: huge\ntype: project\n---\n{huge_body}\n");
        write(&store.root.join("huge.md"), &entry);

        let section = store.system_prompt_section().unwrap();
        assert!(section.contains("## huge"));
        assert!(
            section.contains("memory entry `huge` truncated"),
            "expected truncation notice; got: {}",
            &section[section.len().saturating_sub(400)..]
        );
        // Section is bounded — well under the original 20 KB.
        assert!(
            section.len() < 12_000,
            "expected truncated section; got len={}",
            section.len()
        );
    }

    /// M6.18 BUG M5/M6/M7: shared `truncate_for_prompt` helper. Pin
    /// the basic shape — line cap fires + UTF-8 char-boundary safe.
    #[test]
    fn truncate_for_prompt_handles_line_cap_and_unicode() {
        let raw = (0..50)
            .map(|i| format!("ทดสอบ {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        // Force line truncation at 10.
        let out = truncate_for_prompt(&raw, 10, 100_000, "test");
        assert!(out.contains("ทดสอบ 0"));
        assert!(out.contains("ทดสอบ 9"));
        assert!(!out.contains("ทดสอบ 10"));
        assert!(out.contains("test truncated"));
    }

    // ─── M6.26: write/append/delete + index maintenance ───────────────────

    #[test]
    fn writable_entry_path_rejects_traversal_and_reserved() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        assert!(writable_entry_path(&store, "../escape").is_err());
        assert!(writable_entry_path(&store, "foo/bar").is_err());
        assert!(writable_entry_path(&store, "").is_err());
        assert!(writable_entry_path(&store, "MEMORY").is_err()); // reserved
        assert!(writable_entry_path(&store, "memory").is_err()); // case-insensitive
        assert!(writable_entry_path(&store, "ok-entry").is_ok());
    }

    #[test]
    fn write_entry_creates_with_stamps_and_index_bullet() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        let path = write_entry(
            &store,
            "user_role",
            "---\ndescription: senior backend engineer\ntype: user\n---\nLikes Rust.",
        )
        .unwrap();
        assert!(path.exists());
        let raw = std::fs::read_to_string(&path).unwrap();
        let (fm, body) = parse_frontmatter(&raw);
        assert!(fm.contains_key("created"));
        assert!(fm.contains_key("updated"));
        assert_eq!(
            fm.get("description").map(String::as_str),
            Some("senior backend engineer")
        );
        assert!(body.contains("Likes Rust."));

        let index = std::fs::read_to_string(store.root.join("MEMORY.md")).unwrap();
        assert!(index.contains("[user_role](user_role.md)"));
        assert!(index.contains("senior backend engineer"));
    }

    #[test]
    fn write_entry_replace_dedupes_index_and_preserves_created() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        write_entry(&store, "topic", "---\ndescription: v1\n---\nbody1").unwrap();
        let raw1 = std::fs::read_to_string(store.root.join("topic.md")).unwrap();
        let (fm1, _) = parse_frontmatter(&raw1);
        let created = fm1.get("created").cloned();

        write_entry(&store, "topic", "---\ndescription: v2\n---\nbody2").unwrap();
        let raw2 = std::fs::read_to_string(store.root.join("topic.md")).unwrap();
        let (fm2, body2) = parse_frontmatter(&raw2);
        // `created` should be preserved (not bumped on replace).
        assert_eq!(fm2.get("created").cloned(), created);
        assert!(body2.contains("body2"));

        let index = std::fs::read_to_string(store.root.join("MEMORY.md")).unwrap();
        assert_eq!(index.matches("(topic.md)").count(), 1, "{index}");
        assert!(index.contains("v2"));
        assert!(!index.contains("v1"));
    }

    #[test]
    fn append_to_entry_creates_or_extends() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        // Create with bare body.
        append_to_entry(&store, "rolling", "first\n").unwrap();
        // Add frontmatter via write.
        write_entry(&store, "rolling", "---\ndescription: log\n---\nseed line\n").unwrap();
        // Append again — frontmatter preserved, updated bumped.
        append_to_entry(&store, "rolling", "second\n").unwrap();
        let raw = std::fs::read_to_string(store.root.join("rolling.md")).unwrap();
        let (fm, body) = parse_frontmatter(&raw);
        assert_eq!(fm.get("description").map(String::as_str), Some("log"));
        assert!(fm.contains_key("updated"));
        assert!(body.contains("seed line"));
        assert!(body.contains("second"));
    }

    #[test]
    fn delete_entry_removes_file_and_index_bullet() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        write_entry(&store, "old", "---\ndescription: x\n---\nbody").unwrap();
        write_entry(&store, "keep", "---\ndescription: y\n---\nbody").unwrap();

        let index_before = std::fs::read_to_string(store.root.join("MEMORY.md")).unwrap();
        assert!(index_before.contains("(old.md)"));
        assert!(index_before.contains("(keep.md)"));

        delete_entry(&store, "old").unwrap();
        assert!(!store.root.join("old.md").exists());
        assert!(store.root.join("keep.md").exists());

        let index_after = std::fs::read_to_string(store.root.join("MEMORY.md")).unwrap();
        assert!(!index_after.contains("(old.md)"));
        assert!(index_after.contains("(keep.md)"));
    }

    #[test]
    fn delete_missing_entry_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        // No entry exists; deleting should still succeed.
        delete_entry(&store, "nope").unwrap();
    }

    // ─── M6.26 BUG #4 + #5: categorized index + budget cap ────────────────

    #[test]
    fn system_prompt_section_categorizes_when_frontmatter_has_category() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        std::fs::create_dir_all(&store.root).unwrap();
        write(
            &store.root.join("user.md"),
            "---\ncategory: identity\ndescription: who I am\n---\nbody",
        );
        write(
            &store.root.join("policy.md"),
            "---\ncategory: feedback\ndescription: rules\n---\nbody",
        );
        write(
            &store.root.join("legacy.md"),
            "---\ndescription: no category\n---\nbody",
        );
        let section = store.system_prompt_section().unwrap();
        assert!(section.contains("**identity**"));
        assert!(section.contains("**feedback**"));
        assert!(section.contains("**uncategorized**"));
        assert!(section.contains("- user — who I am"));
        assert!(section.contains("- policy — rules"));
    }

    #[test]
    fn system_prompt_section_defers_bodies_when_over_budget() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        std::fs::create_dir_all(&store.root).unwrap();
        // Three entries × 10 KB each → total 30 KB exceeds the 16 KB
        // inline budget. First entry should fit; later ones should
        // become deferred pointers.
        let big_body = "x".repeat(10_000);
        for n in &["aaa", "bbb", "ccc"] {
            write(
                &store.root.join(format!("{n}.md")),
                &format!("---\ndescription: {n}\n---\n{big_body}"),
            );
        }
        let section = store.system_prompt_section().unwrap();
        // First entry inlines fully (under budget).
        assert!(section.contains("## aaa"));
        // At least one later entry should be deferred.
        let deferred = section.matches("body deferred").count();
        assert!(
            deferred >= 1,
            "expected at least one deferred body, got 0 in section length {}",
            section.len()
        );
        // Tool affordances always present.
        assert!(section.contains("MemoryRead"));
    }

    #[test]
    fn system_prompt_section_advertises_tools() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        std::fs::create_dir_all(&store.root).unwrap();
        write(
            &store.root.join("just-one.md"),
            "---\ndescription: x\n---\nbody",
        );
        let section = store.system_prompt_section().unwrap();
        assert!(section.contains("## Tools"));
        assert!(section.contains("MemoryRead"));
        assert!(section.contains("MemoryWrite"));
        assert!(section.contains("MemoryAppend"));
    }

    #[test]
    fn write_frontmatter_map_quotes_values_with_special_chars() {
        let mut fm = HashMap::new();
        fm.insert("name".into(), "user_role".into());
        fm.insert("description".into(), "has: colon".into());
        let out = write_frontmatter_map(&fm, "body\n");
        // Re-parse to verify round-trip. parse_frontmatter strips the
        // trailing newline from body (pre-existing behavior of joining
        // lines without re-emitting them) — that's OK.
        let (parsed, body) = parse_frontmatter(&out);
        assert_eq!(parsed.get("name").map(String::as_str), Some("user_role"));
        assert_eq!(
            parsed.get("description").map(String::as_str),
            Some("has: colon")
        );
        assert!(body.starts_with("body"));
    }
}
