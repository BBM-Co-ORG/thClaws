//! Write a research result to KMS as a timestamped raw note + update
//! the topic-level `_summary.md` consolidated view.
//!
//! Two-step write so consolidation can never lose data:
//! 1. **Raw note** — `<YYYY-MM-DD>-<query-slug>.md` with the full
//!    synthesized markdown verbatim. Timestamped filename means
//!    repeated `/research` calls on similar queries don't collide.
//! 2. **Summary** — `_summary.md` updated to reference the raw note
//!    (in M6.39.1, just appends a bullet; M6.39.2+ may add an LLM
//!    consolidation pass that merges raw notes into a coherent
//!    summary).
//!
//! KMS resolution: if `kms_target` is provided and exists, use it;
//! otherwise create a project-scoped KMS with that name. If no target
//! given, caller supplies an LLM-derived slug via
//! [`super::llm_calls::derive_topic_slug`].

use crate::error::Result;
use crate::kms::{self, KmsRef, KmsScope};
use std::path::PathBuf;

/// Result of a successful KMS write.
#[derive(Debug, Clone)]
pub struct WriteOutcome {
    /// Filename (no path) of the timestamped raw note inside the KMS.
    pub raw_note: String,
    /// Absolute path on disk where it landed (useful for log lines).
    pub raw_note_path: PathBuf,
    /// Name of the KMS the note went to (handy when caller passed
    /// `None` and we auto-derived).
    pub kms_name: String,
}

/// Build the raw note filename from today + a query slug. Format:
/// `2026-05-09-obon-festival.md`. Timestamp prefix sorts naturally so
/// `ls` shows newest at the bottom.
pub fn make_raw_filename(today: &str, query_slug: &str) -> String {
    format!("{today}-{}", query_slug)
}

/// Resolve or create the target KMS. Always project-scoped — research
/// is workspace-specific by default. M6.39.2 may add `--user-kms` to
/// flip to user scope.
fn resolve_or_create_kms(name: &str) -> Result<KmsRef> {
    if let Some(k) = kms::resolve(name) {
        return Ok(k);
    }
    kms::create(name, KmsScope::Project)
}

/// M6.39.6: public alias of `resolve_or_create_kms` so the pipeline
/// can ensure the KMS exists before kicking off parallel page-synth
/// futures (each one will later write into it).
pub fn resolve_or_create(name: &str) -> Result<KmsRef> {
    resolve_or_create_kms(name)
}

/// Write the synthesized result + update summary. Returns the relative
/// raw-note filename (caller surfaces as `JobView::result_page`).
pub fn write(
    kms_name: &str,
    query: &str,
    query_slug: &str,
    today: &str,
    synthesized: &str,
) -> Result<WriteOutcome> {
    let kref = resolve_or_create_kms(kms_name)?;
    let filename = make_raw_filename(today, query_slug);
    let body = build_raw_note_body(query, today, synthesized);
    let raw_path = kms::write_page(&kref, &filename, &body)?;
    update_summary(&kref, today, query, &filename)?;
    Ok(WriteOutcome {
        raw_note: format!("{filename}.md"),
        raw_note_path: raw_path,
        kms_name: kref.name.clone(),
    })
}

/// Compose the raw note body with frontmatter that the KMS schema-aware
/// lint (M6.37) can validate against. The `type: research` discriminator
/// lets users filter `KmsSearch` to research notes only.
fn build_raw_note_body(query: &str, today: &str, synthesized: &str) -> String {
    // M6.39.5: body leads with synthesized content directly. Pre-fix
    // the body opened with `# Research: <query>` H1 + `**Query:**` +
    // `**Date:**` lines that just restated frontmatter. The KMS
    // auto-index pulls each page's "summary" from
    // `first_meaningful_line(body)`, which strips Markdown markers
    // and returned just `Research: <query>` — useless for the LLM
    // trying to decide whether the page is relevant.
    //
    // With this layout, the synthesize prompt's required 1-2
    // sentence abstract becomes the first meaningful line, and the
    // index summary actually describes what's inside.
    let _ = query;
    format!(
        "---\n\
         title: \"Research: {}\"\n\
         type: research\n\
         query: \"{}\"\n\
         created: {today}\n\
         updated: {today}\n\
         ---\n\n\
         {}\n",
        escape_yaml_string(query),
        escape_yaml_string(query),
        synthesized.trim()
    )
}

/// Append a one-line bullet to `_summary.md` so users have an index of
/// research entries in this KMS without opening each file. M6.39.1
/// keeps it dumb (chronological bullet list); future revs can add an
/// LLM consolidation pass over the bullets.
fn update_summary(kref: &KmsRef, today: &str, query: &str, raw_filename: &str) -> Result<()> {
    let line = format!(
        "- {today} — [{}]({}.md) — {}\n",
        truncate_for_summary(query, 80),
        raw_filename,
        raw_filename
    );
    let summary_name = "_summary";
    // KMS pages live under `<root>/pages/<stem>.md` — checking
    // `root.join("_summary.md")` (an earlier version of this check)
    // always returned "missing" and triggered a destructive
    // `write_page` (create-or-replace) on the second call, wiping
    // the first call's bullet. The real existence check is in
    // pages_dir.
    let summary_path = kref.pages_dir().join(format!("{summary_name}.md"));
    if !summary_path.exists() {
        let body = format!(
            "---\n\
             title: \"Research summary\"\n\
             type: research-summary\n\
             updated: {today}\n\
             ---\n\n\
             # Research summary\n\n\
             Auto-maintained index of research notes in this knowledge base. \
             Each line links to the timestamped raw note generated by `/research`.\n\n\
             {line}"
        );
        kms::write_page(kref, summary_name, &body)?;
    } else {
        kms::append_to_page(kref, summary_name, &line)?;
    }
    Ok(())
}

fn truncate_for_summary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max - 1 {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

/// YAML string escaping — research queries can contain quotes,
/// backslashes, control chars. Keep it simple: escape `"` and `\` and
/// drop ASCII control chars (newlines etc).
fn escape_yaml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Bridge into the system-time → date-string conversion used by the
/// rest of KMS. Callers in tests pass an explicit date; production
/// uses [`crate::usage::today_str`].
/// Query → human title for the MOC note (first 80 chars, trimmed).
pub fn title_from_query(query: &str) -> String {
    let t = query.trim();
    let mut s: String = t.chars().take(80).collect();
    if t.chars().count() > 80 {
        s.push('…');
    }
    s
}

pub fn today_str() -> String {
    crate::usage::today_str()
}

/// Where a run keeps what it read and did not cite (dev-plan/64 D5).
pub const UNCITED_DIR: &str = ".uncited";
/// How long an uncited source is kept.
pub const UNCITED_KEEP_DAYS: u64 = 90;

/// Keep a source a run digested but no note cited.
///
/// It used to be thrown away, so the next run on the same subject fetched it
/// again, nobody could see what the model had read and chosen not to use,
/// and anything that looks for evidence — `/kms verify --ground`, a refresh —
/// could only search what had already been cited. A dot-folder, so the
/// source lister, the catalogue and the search index all pass over it: these
/// are not part of the knowledge base, they are its reading pile. Pruned
/// after [`UNCITED_KEEP_DAYS`]. Never overwrites a cited archive.
pub fn write_uncited_source(
    kms_name: &str,
    query: &str,
    today: &str,
    title: &str,
    url: &str,
    body: &str,
) -> Result<Option<std::path::PathBuf>> {
    let kref = crate::kms::resolve(kms_name).ok_or_else(|| {
        crate::error::Error::Tool(format!("KMS '{kms_name}' not found (write_uncited_source)"))
    })?;
    let filename = url_to_filename(url);
    let sources = kref.root.join("sources");
    if sources.join(format!("{filename}.md")).exists() || body.trim().is_empty() {
        return Ok(None);
    }
    let dir = sources.join(UNCITED_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|e| crate::error::Error::Tool(format!("create {}: {e}", dir.display())))?;
    let path = dir.join(format!("{filename}.md"));
    let text = format!(
        "---\ntype: research-source\ncited: false\ntitle: \"{}\"\nurl: \"{}\"\nfetched_for: \"{}\"\nfetched_at: {today}\n---\n\n# {}\n\n**Source:** [{}]({})\n\n{}\n",
        escape_yaml_string(title),
        escape_yaml_string(url),
        escape_yaml_string(query),
        title,
        url,
        url,
        body.trim()
    );
    crate::kms::write_file(&path, text)
        .map_err(|e| crate::error::Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(Some(path))
}

/// Drop uncited sources older than [`UNCITED_KEEP_DAYS`]. Returns how many.
pub fn prune_uncited_sources(kref: &crate::kms::KmsRef) -> usize {
    let dir = kref.root.join("sources").join(UNCITED_DIR);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let keep = std::time::Duration::from_secs(UNCITED_KEEP_DAYS * 24 * 3600);
    let mut gone = 0;
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > keep);
        if old && std::fs::remove_file(e.path()).is_ok() {
            gone += 1;
        }
    }
    gone
}

/// M6.39.5: persist a fetched source page into the KMS's `sources/`
/// directory for offline provenance. Called once per cited source
/// (those whose `[N]` index appears in the synthesized markdown) so
/// the KMS contains both the synthesized note AND the raw inputs the
/// LLM saw at synthesis time.
///
/// Filename is a deterministic slug of the URL — same URL across
/// different research runs maps to the same file (the latest fetch
/// wins). Frontmatter carries the original URL + citation index +
/// query so a user browsing `sources/` can trace which research run
/// produced each cached page.
///
/// A source that a run read and no note cited goes to `sources/.uncited/`
/// instead — see [`write_uncited_source`]. Citing it later promotes it:
/// this writes the real archive and removes the uncited copy.
pub fn write_source(
    kms_name: &str,
    query: &str,
    today: &str,
    index: u32,
    title: &str,
    url: &str,
    body: &str,
) -> Result<std::path::PathBuf> {
    let kref = crate::kms::resolve(kms_name).ok_or_else(|| {
        crate::error::Error::Tool(format!(
            "KMS '{kms_name}' not found — research-source write needs an existing KMS"
        ))
    })?;
    let dir = kref.root.join("sources");
    std::fs::create_dir_all(&dir).map_err(|e| {
        crate::error::Error::Tool(format!("create sources dir {}: {e}", dir.display()))
    })?;
    let filename = url_to_filename(url);
    let path = dir.join(format!("{filename}.md"));
    let body_str = format!(
        "---\n\
         type: research-source\n\
         title: \"{}\"\n\
         url: \"{}\"\n\
         fetched_for: \"{}\"\n\
         citation_index: {index}\n\
         fetched_at: {today}\n\
         ---\n\n\
         # {}\n\n\
         **Source:** [{}]({})\n\n\
         {}\n",
        escape_yaml_string(title),
        escape_yaml_string(url),
        escape_yaml_string(query),
        title,
        url,
        url,
        body.trim()
    );
    crate::kms::write_file(&path, &body_str)
        .map_err(|e| crate::error::Error::Tool(format!("write {}: {e}", path.display())))?;
    let _ = std::fs::remove_file(dir.join(UNCITED_DIR).join(format!("{filename}.md")));
    // dev-plan/64 P3.7: tell the catalogue. Only `/kms ingest` did, so a
    // vault built by `/research` had its sources on disk and not in the
    // catalogue — 63 of 64 in the one this was found on — and everything
    // that reads the catalogue (the index's source list, "uncited source"
    // hints, dedupe by hash) saw one file. Best effort: the archive is
    // written either way, and `/kms reindex` reconciles.
    let _ = crate::kms_sources::upsert(
        &kref,
        crate::kms_sources::SourceRecord {
            file: format!("{filename}.md"),
            title: title.to_string(),
            origin: crate::kms_sources::Origin::Research,
            origin_ref: url.to_string(),
            ingested: today.to_string(),
            bytes: body_str.len() as u64,
            sha256: crate::kms_sources::hash_file(&path),
            converted_from: None,
        },
    );
    Ok(path)
}

/// URL → filesystem-safe slug. Strip protocol, lowercase, replace
/// non-alphanumerics with `-`, collapse runs, trim, cap at 80 chars.
/// `https://en.wikipedia.org/wiki/Obon` → `en-wikipedia-org-wiki-obon`.
pub fn url_to_filename(url: &str) -> String {
    // A locally ingested source is already archived as
    // `sources/<alias>.md`; its research url is `kms://<kms>/sources/<alias>`.
    if let Some(rest) = url.strip_prefix("kms://") {
        if let Some((_, alias)) = rest.split_once("/sources/") {
            let alias = alias.split('#').next().unwrap_or(alias);
            if !alias.is_empty() {
                return alias.to_string();
            }
        }
    }
    let stripped = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let mut slug = String::with_capacity(stripped.len());
    let mut prev_dash = true;
    for c in stripped.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-').to_string();
    // The slug is only a name when nothing was thrown away to make it.
    // A Thai path has no ASCII to keep (three Thai Wikipedia articles all
    // came out as `th-wikipedia-org-wiki`), a percent-encoded one turns
    // into hex that the 80-char cut then truncates mid-title, and any two
    // long URLs sharing a prefix met at the cut. Each of those put two
    // documents in one archive file, with every citation pointing at
    // whichever was written last. When the slug is lossy it carries a
    // hash of the whole URL; a plain short URL keeps its old name, so
    // existing archives and the links into them are untouched.
    let lossy = stripped.chars().any(|c| !c.is_ascii() || c == '%') || trimmed.len() > 80;
    if !lossy {
        return if trimmed.is_empty() {
            "source".into()
        } else {
            trimmed
        };
    }
    let head: String = trimmed.chars().take(71).collect();
    let head = head.trim_matches('-');
    let head = if head.is_empty() { "source" } else { head };
    format!("{head}-{}", short_hash(url))
}

/// First eight hex digits of SHA-256 — enough to keep two documents out
/// of one file, short enough to stay readable in a filename.
pub fn short_hash(s: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(s.as_bytes())[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// M6.39.7: ensure each research page ends with a complete
/// `## Sources` section listing every `[N]` citation used in the
/// body, mapped to source title + URL + local cached file.
///
/// Pre-fix the LLM was inconsistent — sometimes wrote a Sources
/// section listing only the page's own subset, sometimes forgot it
/// entirely. The "Research notes" appendix (eval notes) uses [N]
/// references spanning ALL iterations, so per-page subsets were
/// never enough anyway.
///
/// Approach: scan the body for [N] citations, strip any existing
/// `## Sources` block the LLM wrote (it may be incomplete/wrong),
/// append a new section that lists exactly the cited indices in
/// numeric order. Idempotent — running twice yields the same
/// output.
///
/// Format uses numbered-list syntax (`N.`) intentionally — avoids
/// `[N]` at the start of an entry which would conflict with
/// [`linkify_citations`]'s `[N]` → `[N](path)` rewrite. Each entry:
///
///   `N. [Title](../sources/<slug>.md) — https://upstream-url`
///
/// Title is the clickable link to the local cached source body in
/// `<kms>/sources/`. The bare upstream URL follows so the user sees
/// the original. `sources` is the master accumulated source list
/// from the research run (index, title, url tuples) so callers can
/// share the same shape with [`linkify_citations`].
pub fn ensure_sources_section(
    body: &str,
    // (index, title, url, origin — the address to print when `url` is
    // not one, empty otherwise). An ingested source's url is
    // `kms://<base>/sources/<alias>`: right for resolving the in-page
    // link, useless to a reader who follows the citation to check it.
    sources: &[(u32, String, String, String)],
) -> String {
    let cited = parse_citation_indices(body);
    if cited.is_empty() {
        // Page genuinely has no citations — nothing to do, return
        // body unchanged. (Don't auto-append an empty Sources
        // section; that would just clutter the page.)
        return body.to_string();
    }
    let stripped = strip_sources_section(body);
    let mut indices: Vec<u32> = cited.into_iter().collect();
    indices.sort_unstable();

    let mut section = String::from("\n\n## Sources\n\n");
    for n in indices {
        if let Some((_, title, url, origin)) = sources.iter().find(|(i, _, _, _)| *i == n) {
            // The link resolves against the archive; the address printed
            // after it is the one a reader can open.
            let rel = format!("../sources/{}.md", url_to_filename(url));
            let shown = if origin.trim().is_empty() {
                url
            } else {
                origin
            };
            section.push_str(&format!("{n}. [{title}]({rel}) — {shown}\n"));
        } else {
            // Resolves to nothing — surface it explicitly rather
            // than silently dropping. Should be rare; would mean
            // the LLM hallucinated an index outside the source
            // list.
            section.push_str(&format!("{n}. (unknown source — index out of range)\n"));
        }
    }
    format!("{}{}", stripped.trim_end(), section)
}

/// M6.39.7: rewrite inline `[N]` citations in a research page body
/// to clickable markdown links pointing at the locally-cached
/// source file in `<kms>/sources/<url-slug>.md`.
///
/// Pages live at `<kms>/pages/<run>__<slug>.md`; sources live at
/// `<kms>/sources/<url-slug>.md`. Relative path from page to
/// source = `../sources/<url-slug>.md`. So `[1]` in the body
/// becomes `[1](../sources/en-wikipedia-org-wiki-obon.md)` — works
/// in Obsidian, GitHub web view, and the chat UI's KmsRead.
///
/// Skip cases:
/// - `[1](path)` already linked → next char after `]` is `(`, leave alone
/// - `[1, 3]` multi-cite → can't fail-safely split into two links, leave alone
/// - `[N]` where N doesn't resolve to any source → leave alone (the
///   `## Sources` section will surface the unresolved index)
/// - `[non-numeric]` → not a citation, leave alone
///
/// Idempotent — already-linkified citations are detected and skipped.
pub fn linkify_citations(body: &str, sources: &[(u32, String, String, String)]) -> String {
    let mut out = String::with_capacity(body.len() + 256);
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < body.len() {
        if bytes[i] == b'[' {
            if let Some((rewritten, consumed)) = try_rewrite_citation_at(body, i, sources) {
                out.push_str(&rewritten);
                i += consumed;
                continue;
            }
        }
        // Not a citation we can rewrite — emit one Unicode char and
        // advance to the next char boundary.
        let mut j = i + 1;
        while j < body.len() && !body.is_char_boundary(j) {
            j += 1;
        }
        out.push_str(&body[i..j]);
        i = j;
    }
    out
}

fn try_rewrite_citation_at(
    body: &str,
    pos: usize,
    sources: &[(u32, String, String, String)],
) -> Option<(String, usize)> {
    let bytes = body.as_bytes();
    debug_assert_eq!(bytes[pos], b'[');
    let after = body.get(pos + 1..)?;
    let end_rel = after.find(']')?;
    let inner = &after[..end_rel];
    if inner.is_empty() || inner.len() > 30 {
        return None;
    }
    if !inner
        .chars()
        .all(|c| c.is_ascii_digit() || c == ',' || c == ' ')
    {
        return None;
    }
    // Already linked? Next char after `]` is `(`?
    let after_close = pos + 1 + end_rel + 1;
    if bytes.get(after_close).copied() == Some(b'(') {
        return None;
    }
    // Single-index only — multi-cite `[1, 3]` doesn't have an
    // unambiguous single-link rewrite; leave it for the user to read
    // alongside the canonical `## Sources` section.
    let n: u32 = inner.trim().parse().ok()?;
    let (_, _, url, _) = sources.iter().find(|(i, _, _, _)| *i == n)?;
    let rel = format!("../sources/{}.md", url_to_filename(url));
    let rewritten = format!("[{n}]({rel})");
    let consumed = after_close - pos; // bytes from `[` through `]`
    Some((rewritten, consumed))
}

/// Strip the trailing `## Sources` section (case-insensitive) from
/// a markdown body if present. Preserves everything before the
/// header. Used by [`ensure_sources_section`] before regenerating.
pub fn strip_sources_section(body: &str) -> String {
    // Find a line that starts with `## Sources` (any case). Anything
    // after that (and the header line itself) goes. Walk lines so we
    // don't accidentally match `## Sources of inspiration` mid-body
    // — only an exact heading match.
    let mut keep = String::with_capacity(body.len());
    let mut found = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if !found && trimmed.eq_ignore_ascii_case("## sources") {
            found = true;
            break;
        }
        keep.push_str(line);
        keep.push('\n');
    }
    if found {
        // Trim trailing whitespace/newlines so the regenerated
        // section concatenates cleanly.
        keep.trim_end().to_string()
    } else {
        // No Sources header found — body unchanged.
        body.to_string()
    }
}

pub struct ResearchPage<'a> {
    pub kms_name: &'a str,
    pub page_slug: &'a str,
    pub page_title: &'a str,
    pub page_topic: &'a str,
    pub query: &'a str,
    pub today: &'a str,
    pub body: &'a str,
    pub verification_score: Option<f32>,
}

/// M6.39.11: write one page from a multi-page research run.
/// Filename is just `<page-slug>.md` — frontmatter (date, run query,
/// topic) carries the metadata, so the long
/// `<run-prefix>__<slug>.md` form earlier revs used was redundant
/// noise. Re-running `/research` on the same topic + KMS now
/// updates pages in place rather than spamming dated copies — the
/// `_summary.md` per-run section still chronologically logs each
/// run, so history is preserved at the index level.
///
/// Frontmatter discriminator `type: research-page` (sibling of
/// `type: research` for the legacy single-page output).
pub fn write_research_page(context: ResearchPage<'_>) -> Result<std::path::PathBuf> {
    let ResearchPage {
        kms_name,
        page_slug,
        page_title,
        page_topic,
        query,
        today,
        body,
        verification_score,
    } = context;
    let kref = resolve_or_create_kms(kms_name)?;
    // `verified: <today>` is stamped on every research page so the
    // KmsRead staleness check has a baseline. `verification_score`
    // (when present) records the fraction of factual claims the
    // verify pass rated `Supported` — readers can sort or filter on
    // it. Skip the field when verification didn't run so we don't
    // misleadingly stamp a 0.0 score on pages that weren't audited.
    let verify_line = match verification_score {
        Some(score) => format!("verification_score: {:.2}\n", score),
        None => String::new(),
    };
    let composed = format!(
        "---\n\
         title: \"{}\"\n\
         type: research-page\n\
         page_slug: {}\n\
         query: \"{}\"\n\
         topic: \"{}\"\n\
         created: {today}\n\
         updated: {today}\n\
         verified: {today}\n\
         {verify_line}---\n\n\
         {}\n",
        escape_yaml_string(page_title),
        page_slug,
        escape_yaml_string(query),
        escape_yaml_string(page_topic),
        body.trim()
    );
    crate::kms::write_page(&kref, page_slug, &composed)
}

/// M6.39.6: append a "Run on `<date>`" section to `_summary.md`
/// listing every page produced by this research run. Replaces the
/// old single-bullet append from the legacy single-page write.
/// Each line links to the run's pages so the user can drill in
/// from the summary.
pub fn append_run_section(
    kms_name: &str,
    today: &str,
    query: &str,
    pages: &[(String, String, String)], // (slug, title, topic)
) -> Result<()> {
    let kref = crate::kms::resolve(kms_name).ok_or_else(|| {
        crate::error::Error::Tool(format!(
            "KMS '{kms_name}' not found — append_run_section needs an existing KMS"
        ))
    })?;
    let mut section = format!("\n## {today} — {}\n\n", truncate_for_summary(query, 80));
    for (slug, title, topic) in pages {
        // M6.39.11: bare slug — filenames no longer carry the run
        // prefix, so `[[karpathy]]` resolves directly.
        let topic_brief = if topic.trim().is_empty() {
            String::new()
        } else {
            format!(" — {}", truncate_for_summary(topic, 100))
        };
        section.push_str(&format!("- [[{slug}|{title}]]{topic_brief}\n"));
    }
    let summary_name = "_summary";
    let summary_path = kref.pages_dir().join(format!("{summary_name}.md"));
    if !summary_path.exists() {
        let body = format!(
            "---\n\
             title: \"Research summary\"\n\
             type: research-summary\n\
             updated: {today}\n\
             ---\n\n\
             # Research summary\n\n\
             Auto-maintained index of research notes in this knowledge base. \
             Each section corresponds to one `/research` run; pages within a \
             run are cross-linked.\n\
             {section}"
        );
        crate::kms::write_page(&kref, summary_name, &body)?;
    } else {
        crate::kms::append_to_page(&kref, summary_name, &section)?;
    }
    Ok(())
}

/// M6.39.5: parse citation indices `[N]` out of synthesized markdown.
/// Returns a set so duplicates collapse. Used by the pipeline to
/// decide which fetched sources to persist into `sources/`.
///
/// Matches `[1]`, `[12]`, `[1,3,7]`, `[1, 3]` — comma-separated
/// indices in a single bracket are common in academic-style synth
/// output. Non-numeric brackets (`[ref]`, `[link](url)`) are
/// ignored. False positives are cheap (one extra source file);
/// false negatives leak provenance, so the parser leans permissive.
pub(crate) fn parse_citation_indices(markdown: &str) -> std::collections::HashSet<u32> {
    let mut out = std::collections::HashSet::new();
    let bytes = markdown.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        // Find matching `]`. Bail if missing or content longer than 30
        // chars (excludes `[link text with stuff](url)` and other
        // non-citation brackets).
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end] != b']' {
            end += 1;
        }
        if end >= bytes.len() || end - start == 0 || end - start > 30 {
            i += 1;
            continue;
        }
        let inner = &markdown[start..end];
        // Inner must be only digits + commas + spaces — anything else
        // disqualifies (markdown link text, image alt, etc.).
        if !inner
            .chars()
            .all(|c| c.is_ascii_digit() || c == ',' || c == ' ')
        {
            i += 1;
            continue;
        }
        for tok in inner.split(',') {
            if let Ok(n) = tok.trim().parse::<u32>() {
                if n > 0 {
                    out.insert(n);
                }
            }
        }
        i = end + 1;
    }
    out
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use crate::research::test_helpers::scoped_home;

    #[test]
    fn make_raw_filename_format() {
        assert_eq!(
            make_raw_filename("2026-05-09", "obon-festival"),
            "2026-05-09-obon-festival"
        );
    }

    #[test]
    fn yaml_escape_handles_quotes_and_backslashes() {
        assert_eq!(escape_yaml_string(r#"hello "world""#), r#"hello \"world\""#);
        assert_eq!(escape_yaml_string(r"path\to\file"), r"path\\to\\file");
    }

    #[test]
    fn yaml_escape_strips_control_chars() {
        let s = "line one\nline two\tindented";
        let escaped = escape_yaml_string(s);
        assert!(!escaped.contains('\n'));
        assert!(!escaped.contains('\t'));
        assert!(escaped.contains("line one line two indented"));
    }

    #[test]
    fn truncate_for_summary_preserves_short_strings() {
        assert_eq!(truncate_for_summary("short", 50), "short");
    }

    #[test]
    fn truncate_for_summary_adds_ellipsis_when_long() {
        let s = "a".repeat(100);
        let t = truncate_for_summary(&s, 20);
        assert_eq!(t.chars().count(), 20);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn truncate_for_summary_handles_unicode() {
        let s = "ค้นหาข่าวล่าสุดเกี่ยวกับเทคโนโลยี";
        let t = truncate_for_summary(s, 10);
        assert_eq!(t.chars().count(), 10);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn build_raw_note_includes_required_frontmatter_keys() {
        let body = build_raw_note_body("test query", "2026-05-09", "Some synthesized content");
        assert!(body.starts_with("---\n"));
        assert!(body.contains("title: \"Research: test query\""));
        assert!(body.contains("type: research"));
        assert!(body.contains("query: \"test query\""));
        assert!(body.contains("created: 2026-05-09"));
        assert!(body.contains("updated: 2026-05-09"));
        assert!(body.contains("Some synthesized content"));
    }

    /// M6.39.5: pin the post-fix body shape so the synthesized
    /// abstract becomes the first non-frontmatter line. Pre-fix the
    /// body led with `# Research: <query>` H1 + `**Query:**` /
    /// `**Date:**` lines that just restated frontmatter — kms's
    /// `first_meaningful_line` indexer would pull `Research: <query>`
    /// as the page summary, useless for the LLM picking pages to read.
    #[test]
    fn build_raw_note_body_starts_with_synthesized_content_after_frontmatter() {
        let synthesized = "OBON is a Japanese summer festival honoring \
                           ancestors [1][3].\n\n## History\n\nMore content here.";
        let body = build_raw_note_body("what is OBON", "2026-05-09", synthesized);
        // Body MUST NOT contain the old preamble — those were the
        // exact things crowding out the abstract.
        assert!(
            !body.contains("# Research: what is OBON"),
            "body should not lead with H1 — frontmatter title covers it"
        );
        assert!(
            !body.contains("**Query:**"),
            "body should not restate Query — frontmatter has `query:`"
        );
        assert!(
            !body.contains("**Date:**"),
            "body should not restate Date — frontmatter has `created:`/`updated:`"
        );
        // The first non-frontmatter line must be the abstract prose.
        let after_fm = body.splitn(2, "---\n\n").nth(1).unwrap();
        let first_line = after_fm.lines().next().unwrap();
        assert!(
            first_line.starts_with("OBON is a Japanese"),
            "first non-frontmatter line should be the abstract, got: {first_line}"
        );
    }

    /// Integration test: write to a temp KMS and verify both files land
    /// with the right shape. Uses the same `scoped_home` helper pattern
    /// as `kms::tests`.
    #[test]
    fn write_creates_raw_and_summary_in_fresh_kms() {
        let _g = scoped_home();
        let outcome = write(
            "research-test-kms",
            "what is OBON",
            "obon-festival",
            "2026-05-09",
            "Obon is a Japanese festival [1].\n\n## Sources\n[1] https://example.com\n",
        )
        .unwrap();
        assert_eq!(outcome.raw_note, "2026-05-09-obon-festival.md");
        assert_eq!(outcome.kms_name, "research-test-kms");
        assert!(outcome.raw_note_path.exists());
        let raw = std::fs::read_to_string(&outcome.raw_note_path).unwrap();
        assert!(raw.contains("type: research"));
        assert!(raw.contains("https://example.com"));

        // Summary also exists with the expected bullet entry.
        let kref = kms::resolve("research-test-kms").unwrap();
        let summary_path = kref.pages_dir().join("_summary.md");
        assert!(summary_path.exists(), "summary should be created");
        let summary = std::fs::read_to_string(&summary_path).unwrap();
        assert!(summary.contains("type: research-summary"));
        assert!(summary.contains("2026-05-09 — [what is OBON]"));
        assert!(summary.contains("(2026-05-09-obon-festival.md)"));
    }

    /// End-to-end: write a research note whose body leads with an
    /// abstract, then verify the KMS auto-index picks up that abstract
    /// as the page summary (not "Research: ..." or any meta line).
    /// This is the exact flow that fixes the "LLM ignores KMS"
    /// observation from /system inspection — an informative summary
    /// signals page relevance to the model deciding which page to
    /// KmsRead.
    #[test]
    fn write_index_summary_uses_abstract_not_title() {
        let _g = scoped_home();
        let synthesized = "OBON is a Japanese festival honoring ancestors \
                           observed 13–16 August [1][3].\n\n## History\nmore.";
        write(
            "research-test-index",
            "what is OBON",
            "what-is-obon",
            "2026-05-09",
            synthesized,
        )
        .unwrap();
        let kref = kms::resolve("research-test-index").unwrap();
        let index = std::fs::read_to_string(&kref.index_path()).unwrap();
        // The auto-index should have a bullet for the page using the
        // abstract's first line as the summary, NOT "Research: ..."
        // or "Query: ...".
        assert!(
            index.contains("OBON is a Japanese festival"),
            "index should contain the abstract as page summary, got:\n{index}"
        );
        assert!(
            !index.contains("Research: what is OBON"),
            "index should NOT use the H1 title as summary (that's the pre-fix bug):\n{index}"
        );
    }

    // ── linkify_citations + ensure_sources_section (M6.39.7) ──────

    fn meta(idx: u32, title: &str, url: &str) -> (u32, String, String, String) {
        (idx, title.into(), url.into(), String::new())
    }

    #[test]
    fn linkify_citations_basic_single_index() {
        let body = "Citation here [1] and another [3].";
        let sources = vec![
            meta(1, "T1", "https://a.example"),
            meta(3, "T3", "https://b.example/path"),
        ];
        let out = linkify_citations(body, &sources);
        assert!(out.contains("[1](../sources/a-example.md)"));
        assert!(out.contains("[3](../sources/b-example-path.md)"));
    }

    #[test]
    fn linkify_citations_idempotent_on_already_linked() {
        let body = "Already linked [1](../sources/a-example.md) and bare [2].";
        let sources = vec![
            meta(1, "T1", "https://a.example"),
            meta(2, "T2", "https://b.example"),
        ];
        let out = linkify_citations(body, &sources);
        // Already-linked stays as-is (single occurrence, not double).
        let count = out.matches("[1](../sources/a-example.md)").count();
        assert_eq!(count, 1, "[1] link should appear once, got: {out}");
        assert!(out.contains("[2](../sources/b-example.md)"));
    }

    #[test]
    fn linkify_citations_skips_multi_cite() {
        // `[1, 3]` doesn't have an unambiguous single-link rewrite;
        // the canonical Sources section + per-citation [N] coverage
        // handles it.
        let body = "Multi cite [1, 3] here.";
        let sources = vec![
            meta(1, "T1", "https://a.example"),
            meta(3, "T3", "https://b.example"),
        ];
        let out = linkify_citations(body, &sources);
        assert!(
            out.contains("[1, 3]"),
            "multi-cite should pass through, got: {out}"
        );
        assert!(!out.contains("[1, 3](../sources/"));
    }

    #[test]
    fn linkify_citations_skips_unresolved_index() {
        // [99] not in source list → leave alone (Sources section
        // surfaces it as "unknown source").
        let body = "Unresolvable [99] cite.";
        let sources = vec![meta(1, "T1", "https://a.example")];
        let out = linkify_citations(body, &sources);
        assert!(out.contains("[99]"));
        assert!(!out.contains("[99]("));
    }

    #[test]
    fn linkify_citations_preserves_unicode() {
        let body = "ภาษาไทย [1] ทดสอบ.";
        let sources = vec![meta(1, "T", "https://a.example")];
        let out = linkify_citations(body, &sources);
        assert!(out.contains("ภาษาไทย"));
        assert!(out.contains("[1](../sources/a-example.md)"));
        assert!(out.contains("ทดสอบ"));
    }

    #[test]
    fn linkify_citations_handles_wikilinks_alongside() {
        // [[wiki-link]] shape (used by cross-linker) shouldn't get
        // linkified — its inner is not all-digits.
        let body = "Wiki [[karpathy|Karpathy]] and cite [1].";
        let sources = vec![meta(1, "T", "https://a.example")];
        let out = linkify_citations(body, &sources);
        assert!(out.contains("[[karpathy|Karpathy]]"));
        assert!(out.contains("[1](../sources/a-example.md)"));
    }

    #[test]
    fn ensure_sources_section_uses_numbered_list_format() {
        let body = "Body cites [1] and [3].";
        let sources = vec![
            meta(1, "Title One", "https://a.example"),
            meta(2, "Title Two", "https://b.example"),
            meta(3, "Title Three", "https://c.example"),
        ];
        let out = ensure_sources_section(body, &sources);
        assert!(out.contains("## Sources"));
        // Numbered list (`1. ...`), not `[1]` — avoids conflict with
        // linkify_citations which would otherwise double-link.
        assert!(out.contains("1. [Title One]"));
        assert!(out.contains("3. [Title Three]"));
        // [2] not cited → not in Sources section.
        assert!(!out.contains("Title Two"));
    }

    #[test]
    fn an_ingested_source_is_cited_by_its_origin_not_its_kms_path() {
        let body = "Cite [1] and [2].";
        let sources = vec![
            // Fetched by a run: the url IS the address.
            meta(1, "Wikipedia: Obon", "https://en.wikipedia.org/wiki/Obon"),
            // Ingested by the user: the url resolves the archive, the
            // origin is what a reader can open.
            (
                2u32,
                "Jevons paradox - Wikipedia".to_string(),
                "kms://Age of Abundance (v2)/sources/Jevons_paradox".to_string(),
                "https://en.wikipedia.org/wiki/Jevons_paradox".to_string(),
            ),
        ];
        let out = ensure_sources_section(body, &sources);
        // The link still resolves against the archived copy...
        assert!(
            out.contains("[Jevons paradox - Wikipedia](../sources/Jevons_paradox.md)"),
            "{out}"
        );
        // ...and the address printed after it is one a browser opens.
        assert!(
            out.contains("— https://en.wikipedia.org/wiki/Jevons_paradox"),
            "{out}"
        );
        assert!(
            !out.contains("— kms://"),
            "a kms:// path must never be printed as a citation's address: {out}"
        );
        // A source with no origin is unaffected.
        assert!(
            out.contains("— https://en.wikipedia.org/wiki/Obon"),
            "{out}"
        );
    }

    #[test]
    fn ensure_sources_section_includes_local_link_and_upstream_url() {
        let body = "Cite [1].";
        let sources = vec![meta(
            1,
            "Wikipedia: Obon",
            "https://en.wikipedia.org/wiki/Obon",
        )];
        let out = ensure_sources_section(body, &sources);
        // Local cached file link
        assert!(out.contains("[Wikipedia: Obon](../sources/en-wikipedia-org-wiki-obon.md)"));
        // Plus the upstream URL after the em-dash
        assert!(out.contains("— https://en.wikipedia.org/wiki/Obon"));
    }

    #[test]
    fn ensure_sources_section_strips_old_section_first() {
        // LLM wrote a partial Sources section; we replace it.
        let body = "Body [1] [2].\n\n## Sources\n\n[1] old\n";
        let sources = vec![
            meta(1, "T1", "https://a.example"),
            meta(2, "T2", "https://b.example"),
        ];
        let out = ensure_sources_section(body, &sources);
        // Old [1] old line gone
        assert!(!out.contains("[1] old"));
        // New section has both citations
        assert!(out.contains("1. [T1]"));
        assert!(out.contains("2. [T2]"));
    }

    #[test]
    fn ensure_sources_section_idempotent() {
        let body = "Cite [1].";
        let sources = vec![meta(1, "T1", "https://a.example")];
        let once = ensure_sources_section(body, &sources);
        let twice = ensure_sources_section(&once, &sources);
        assert_eq!(once, twice);
    }

    #[test]
    fn ensure_sources_section_handles_unknown_index() {
        let body = "Hallucinated [99] cite.";
        let sources = vec![meta(1, "T1", "https://a.example")];
        let out = ensure_sources_section(body, &sources);
        assert!(out.contains("99. (unknown source"));
    }

    #[test]
    fn ensure_sources_section_no_op_when_no_citations() {
        let body = "Body without any bracketed numbers.";
        let sources = vec![meta(1, "T1", "https://a.example")];
        let out = ensure_sources_section(body, &sources);
        assert_eq!(out, body, "no citations → body unchanged");
    }

    /// End-to-end: ensure_sources_section + linkify_citations
    /// composed in the order the pipeline calls them. The Sources
    /// section's numbered list must NOT get its leading `N.`
    /// touched by the linkifier (only `[N]` patterns rewrite).
    #[test]
    fn ensure_then_linkify_compose_correctly() {
        let body = "Cite [1] here.";
        let sources = vec![meta(1, "Title", "https://a.example")];
        let with_sources = ensure_sources_section(body, &sources);
        let final_body = linkify_citations(&with_sources, &sources);
        // Inline citation linkified
        assert!(final_body.contains("[1](../sources/a-example.md)"));
        // Sources section heading still there
        assert!(final_body.contains("## Sources"));
        // Numbered list entry intact (not turned into [1]( ... ))
        assert!(final_body.contains("1. [Title]"));
        // Local cached link in section
        assert!(final_body.contains("](../sources/a-example.md)"));
    }

    #[test]
    fn strip_sources_section_no_op_when_absent() {
        let body = "Body with no Sources heading.\n\nMore text.";
        assert_eq!(strip_sources_section(body), body);
    }

    #[test]
    fn strip_sources_section_drops_section_and_after() {
        let body = "Body here.\n\n## Sources\n\n[1] x\n[2] y\n";
        let out = strip_sources_section(body);
        assert_eq!(out, "Body here.");
    }

    // ── append_run_section + write_research_page integration ──────

    #[test]
    fn write_research_page_creates_bare_slug_filename() {
        // M6.39.11: filename is just `<slug>.md`, no run prefix.
        let _g = scoped_home();
        let _ = crate::kms::create("multi-page-test", crate::kms::KmsScope::Project).unwrap();
        let path = write_research_page(ResearchPage {
            kms_name: "multi-page-test",
            page_slug: "karpathy",
            page_title: "Andrej Karpathy",
            page_topic: "Karpathy's role as proponent",
            query: "what is OBON",
            today: "2026-05-09",
            body: "Andrej Karpathy is a researcher [1].\n\n## Background\n\nMore here.",
            verification_score: Some(0.95),
        })
        .unwrap();
        assert!(
            path.to_str().unwrap().ends_with("/karpathy.md"),
            "filename should be bare slug, got: {}",
            path.display()
        );
        // Pre-fix would have produced `<run-prefix>__karpathy.md`.
        assert!(!path.to_str().unwrap().contains("__"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("type: research-page"));
        assert!(body.contains("page_slug: karpathy"));
        assert!(body.contains("Andrej Karpathy is a researcher"));
    }

    #[test]
    fn append_run_section_creates_new_summary_with_bare_slug_links() {
        let _g = scoped_home();
        let _ = crate::kms::create("run-summary-test", crate::kms::KmsScope::Project).unwrap();
        let pages = vec![
            (
                "concept".to_string(),
                "Concept".to_string(),
                "Core idea".to_string(),
            ),
            (
                "karpathy".to_string(),
                "Karpathy".to_string(),
                "Person page".to_string(),
            ),
        ];
        append_run_section("run-summary-test", "2026-05-09", "what is llm-wiki", &pages).unwrap();
        let kref = crate::kms::resolve("run-summary-test").unwrap();
        let summary = std::fs::read_to_string(kref.pages_dir().join("_summary.md")).unwrap();
        assert!(summary.contains("type: research-summary"));
        // Section heading carries the date; page links use bare slugs.
        assert!(summary.contains("## 2026-05-09 — what is llm-wiki"));
        assert!(summary.contains("[[concept|Concept]]"));
        assert!(summary.contains("[[karpathy|Karpathy]]"));
        assert!(!summary.contains("__"));
    }

    #[test]
    fn append_run_section_appends_to_existing_summary() {
        let _g = scoped_home();
        let _ = crate::kms::create("run-append-test", crate::kms::KmsScope::Project).unwrap();
        append_run_section(
            "run-append-test",
            "2026-05-09",
            "first query",
            &[("a".into(), "A".into(), "first topic".into())],
        )
        .unwrap();
        append_run_section(
            "run-append-test",
            "2026-05-10",
            "second query",
            &[("b".into(), "B".into(), "second topic".into())],
        )
        .unwrap();
        let kref = crate::kms::resolve("run-append-test").unwrap();
        let summary = std::fs::read_to_string(kref.pages_dir().join("_summary.md")).unwrap();
        assert!(summary.contains("first query"));
        assert!(summary.contains("second query"));
        assert!(summary.contains("[[a|A]]"));
        assert!(summary.contains("[[b|B]]"));
    }

    // ── parse_citation_indices ─────────────────────────────────────

    #[test]
    fn parse_citation_indices_basic() {
        let md = "Result here [1] and another claim [2].";
        let got = parse_citation_indices(md);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&1));
        assert!(got.contains(&2));
    }

    #[test]
    fn parse_citation_indices_handles_comma_lists() {
        let md = "Multi-cite [1,3,5] and [2, 4].";
        let got = parse_citation_indices(md);
        assert_eq!(got.len(), 5);
        for n in [1, 2, 3, 4, 5] {
            assert!(got.contains(&n), "expected {n} in {:?}", got);
        }
    }

    #[test]
    fn parse_citation_indices_dedupes() {
        let md = "Same source twice [3] and again [3] elsewhere.";
        let got = parse_citation_indices(md);
        assert_eq!(got.len(), 1);
        assert!(got.contains(&3));
    }

    #[test]
    fn parse_citation_indices_ignores_markdown_links() {
        // Standard markdown link — `[text](url)` shouldn't trigger
        // even though brackets contain digits.
        let md = "See [the docs](https://example.com) for [3] more.";
        let got = parse_citation_indices(md);
        assert_eq!(got.len(), 1);
        assert!(got.contains(&3));
    }

    #[test]
    fn parse_citation_indices_ignores_non_numeric() {
        let md = "Plain text [abc] and [ref-foo] but [7] is real.";
        let got = parse_citation_indices(md);
        assert_eq!(got.len(), 1);
        assert!(got.contains(&7));
    }

    #[test]
    fn parse_citation_indices_empty_on_no_citations() {
        let md = "Just prose, no bracketed numbers anywhere.";
        assert!(parse_citation_indices(md).is_empty());
    }

    #[test]
    fn parse_citation_indices_skips_zero() {
        // [0] is meaningless as a citation index (citations are 1-based).
        let md = "Should ignore [0] and pick up [1].";
        let got = parse_citation_indices(md);
        assert_eq!(got.len(), 1);
        assert!(got.contains(&1));
    }

    #[test]
    fn parse_citation_indices_long_brackets_ignored() {
        // > 30 chars between brackets = probably not a citation.
        let md = "[this is a very very very very long bracket content with 1, 2, 3]";
        assert!(parse_citation_indices(md).is_empty());
    }

    // ── url_to_filename ────────────────────────────────────────────

    #[test]
    fn url_to_filename_maps_local_sources_to_their_alias() {
        assert_eq!(
            url_to_filename("kms://notes/sources/my-doc#part-2-1234"),
            "my-doc"
        );
    }

    #[test]
    fn url_to_filename_strips_protocol() {
        assert_eq!(
            url_to_filename("https://en.wikipedia.org/wiki/Obon"),
            "en-wikipedia-org-wiki-obon"
        );
        assert_eq!(url_to_filename("http://example.com/foo"), "example-com-foo");
    }

    #[test]
    fn url_to_filename_handles_query_string() {
        assert_eq!(
            url_to_filename("https://example.com/path?q=1&r=2"),
            "example-com-path-q-1-r-2"
        );
    }

    #[test]
    fn url_to_filename_caps_at_80_chars() {
        let long = format!("https://example.com/{}", "a".repeat(200));
        let slug = url_to_filename(&long);
        assert!(slug.len() <= 80);
    }

    /// Two documents must never share an archive file. Every Thai
    /// Wikipedia article used to map to `th-wikipedia-org-wiki`, so a
    /// Thai research run cited one overwritten file for all of them.
    #[test]
    fn different_urls_never_share_a_filename() {
        let urls = [
            "https://th.wikipedia.org/wiki/ประเทศไทย",
            "https://th.wikipedia.org/wiki/กรุงเทพมหานคร",
            "https://th.wikipedia.org/wiki/เศรษฐกิจไทย",
            // The same thing as a browser copies it.
            "https://th.wikipedia.org/wiki/%E0%B8%9B%E0%B8%A3%E0%B8%B0%E0%B9%80%E0%B8%97%E0%B8%A8",
            "https://th.wikipedia.org/wiki/%E0%B8%81%E0%B8%A3%E0%B8%B8%E0%B8%87%E0%B9%80%E0%B8%97%E0%B8%9E",
        ];
        // Two long URLs that agree on their first 80 characters.
        let stem = format!("https://example.gov/reports/{}", "a".repeat(90));
        let long = [format!("{stem}/2025"), format!("{stem}/2026")];

        let mut names: Vec<String> = urls.iter().map(|u| url_to_filename(u)).collect();
        names.extend(long.iter().map(|u| url_to_filename(u)));
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "collision: {names:?}");
        for n in &names {
            assert!(n.len() <= 80, "{n} is {} bytes", n.len());
            assert!(n.is_ascii(), "archive names stay portable: {n}");
        }
        // Stable: the same URL always names the same file.
        assert_eq!(url_to_filename(urls[0]), url_to_filename(urls[0]));
    }

    #[test]
    fn url_to_filename_falls_back_for_empty() {
        assert_eq!(url_to_filename(""), "source");
        assert_eq!(url_to_filename("---"), "source");
    }

    // ── write_source integration ───────────────────────────────────

    /// dev-plan/64 D5. What a run read and did not cite is kept, out of the
    /// way of everything that lists the knowledge base, and becomes a real
    /// source the moment something cites it.
    #[test]
    fn an_uncited_source_is_kept_aside_and_promoted_when_cited() {
        let _g = scoped_home();
        let k = crate::kms::create("uncited-kms", crate::kms::KmsScope::Project).unwrap();
        let url = "https://www.oecd.org/th/รายงาน-ผลิตภาพ";
        let kept = write_uncited_source(
            "uncited-kms",
            "q",
            "2026-09-20",
            "OECD",
            url,
            "เนื้อหาที่อ่านแล้วไม่ได้อ้าง",
        )
        .unwrap()
        .expect("kept");
        assert!(
            kept.to_string_lossy().contains("/sources/.uncited/"),
            "{}",
            kept.display()
        );
        assert!(std::fs::read_to_string(&kept)
            .unwrap()
            .contains("cited: false"));
        // Not a source of the knowledge base: not listed, not catalogued.
        assert!(crate::kms::list_sources(&k).is_empty());
        assert!(crate::kms_sources::load(&k).entries.is_empty());
        // An empty body is not worth keeping.
        assert!(write_uncited_source(
            "uncited-kms",
            "q",
            "2026-09-20",
            "T",
            "https://x.test/a",
            "  "
        )
        .unwrap()
        .is_none());

        let cited =
            write_source("uncited-kms", "q", "2026-09-21", 4, "OECD", url, "เนื้อหา").unwrap();
        assert!(cited.exists() && !kept.exists(), "citing it promotes it");
        assert_eq!(crate::kms::list_sources(&k).len(), 1);
        // And a cited archive is never shadowed by an uncited copy.
        assert!(
            write_uncited_source("uncited-kms", "q", "2026-09-22", "OECD", url, "อีกรอบ")
                .unwrap()
                .is_none()
        );
        assert_eq!(prune_uncited_sources(&k), 0, "nothing is old yet");
    }

    #[test]
    fn write_source_creates_file_in_sources_dir() {
        let _g = scoped_home();
        // Need an existing KMS to write into.
        let _ = crate::kms::create("test-kms", crate::kms::KmsScope::Project).unwrap();
        let path = write_source(
            "test-kms",
            "what is OBON",
            "2026-05-09",
            3,
            "Obon Festival - Wikipedia",
            "https://en.wikipedia.org/wiki/Obon",
            "Body content of the wikipedia page about Obon...",
        )
        .unwrap();
        assert!(path.exists());
        // File lives under <kms-root>/sources/, not pages/.
        assert!(path.to_str().unwrap().contains("/sources/"));
        assert!(!path.to_str().unwrap().contains("/pages/"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("---\n"));
        assert!(body.contains("type: research-source"));
        assert!(body.contains("citation_index: 3"));
        assert!(body.contains("https://en.wikipedia.org/wiki/Obon"));
        assert!(body.contains("Body content of the wikipedia"));

        // dev-plan/64 P3.7: the archive is in the catalogue, under its own
        // name, as research — and archiving it again is one record, not two.
        let kref = crate::kms::resolve("test-kms").unwrap();
        let cat = crate::kms_sources::load(&kref);
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let rec = cat.entries.get(&file).expect("catalogued");
        assert_eq!(rec.origin_ref, "https://en.wikipedia.org/wiki/Obon");
        assert_eq!(rec.origin.as_str(), "research");
        assert_eq!(rec.sha256, crate::kms_sources::hash_file(&path));
        write_source(
            "test-kms",
            "what is OBON",
            "2026-05-10",
            3,
            "Obon Festival - Wikipedia",
            "https://en.wikipedia.org/wiki/Obon",
            "Body content, refreshed.",
        )
        .unwrap();
        assert_eq!(crate::kms_sources::load(&kref).entries.len(), 1);
    }

    #[test]
    fn write_source_errors_on_unknown_kms() {
        let _g = scoped_home();
        let err = write_source(
            "no-such-kms",
            "q",
            "2026-05-09",
            1,
            "title",
            "https://x.example",
            "body",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("not found"));
    }

    #[test]
    fn second_write_appends_to_existing_summary() {
        let _g = scoped_home();
        write(
            "research-multi",
            "first query",
            "first-query",
            "2026-05-09",
            "first answer",
        )
        .unwrap();
        write(
            "research-multi",
            "second query",
            "second-query",
            "2026-05-10",
            "second answer",
        )
        .unwrap();
        let kref = kms::resolve("research-multi").unwrap();
        let summary = std::fs::read_to_string(kref.pages_dir().join("_summary.md")).unwrap();
        assert!(summary.contains("first query"));
        assert!(summary.contains("second query"));
    }
}
