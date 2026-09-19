//! Research v2 steps 4–6: write each planned note from its own claims,
//! rewrite `[c:ID]` markers into `[N]` source citations, stamp `type:
//! note` frontmatter, and persist notes / sources / the run log.

use super::digest::Claim;
use super::graph::KnownNote;
use super::llm_calls::{oneshot, ResearchSource};
use super::plan::{Action, NoteKind, NotePlan};
use crate::cancel::CancelToken;
use crate::error::Result;
use crate::kms::KmsRef;
use crate::providers::Provider;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::Duration;

pub const NOTE_CONCURRENCY: usize = 8;

/// Above this many claims, the per-claim verbatim `quote:` line is left
/// out of the writing prompt. The quote is the digest-time verification
/// (a claim without one never reaches a note); by writing time it is
/// duplicated evidence. Keeping it on a 400-claim topic page doubled
/// that prompt to ~100 KB, which is what pushed one worker model into
/// truncating its reply.
pub const QUOTE_BUDGET_CLAIMS: usize = 60;
/// Claim text handed to the writer. Digest asks for ≤ 20 words; this
/// only guards against a model that ignored that.
const CLAIM_TEXT_CHARS: usize = 300;

pub struct NoteInput<'a> {
    pub query: &'a str,
    pub note: &'a NotePlan,
    pub claims: Vec<&'a Claim>,
    pub sources: &'a [ResearchSource],
    /// slug → title for `related` + the rest of the plan, so the writer
    /// links with real names.
    pub link_targets: &'a BTreeMap<String, String>,
    pub existing_body: Option<String>,
    pub append: bool,
    pub language: &'a str,
    /// Child notes only: the topic page's opening (already written), so
    /// the child extends the story instead of restating it.
    pub parent_overview: Option<String>,
}

/// How much of the topic page a child writer sees.
pub const PARENT_OVERVIEW_CHARS: usize = 1_800;

pub fn build_note_prompt(inp: &NoteInput<'_>) -> String {
    let mut s = String::new();
    let mode = match (inp.note.action, inp.append) {
        (Action::Create, _) => "CREATE a new note",
        (Action::Update, false) => "UPDATE an existing note by merging the new claims into it (rewrite the whole body; keep everything still true, drop nothing that the new claims don't contradict)",
        (Action::Update, true) => "APPEND to an existing note: output ONLY the new section that adds what these claims contribute — do not repeat what the note already says",
    };
    s.push_str(&format!(
        "Research query: {}\n{}\n\n\
         You are writing ONE zettelkasten note. One note = one idea. {mode}.\n\n\
         === This note ===\nSlug: {}\nKind: {}\nTitle: {}\n\n",
        inp.query,
        super::today_context(),
        inp.note.slug,
        inp.note.kind.as_str(),
        inp.note.title
    ));
    if let Some(body) = &inp.existing_body {
        s.push_str("=== Existing note body ===\n");
        s.push_str(body.trim());
        s.push_str("\n\n");
    }
    if inp.note.kind == NoteKind::Moc && !inp.note.outline.is_empty() {
        s.push_str("=== Outline this topic page must follow (one `##` per line, in order) ===\n");
        for h in &inp.note.outline {
            s.push_str(&format!("## {h}\n"));
        }
        s.push('\n');
    }
    if inp.note.kind != NoteKind::Moc {
        if !inp.note.role.is_empty() {
            s.push_str(&format!(
                "=== Why the topic page links here ===\n{}\n\n",
                inp.note.role
            ));
        }
        if let Some(p) = &inp.parent_overview {
            s.push_str("=== The topic page's opening (do NOT repeat it — go deeper on this one idea) ===\n");
            s.push_str(p.trim());
            s.push_str("\n\n");
        }
    }
    let with_quotes = inp.claims.len() <= QUOTE_BUDGET_CLAIMS;
    s.push_str("=== Claims to use (cite as [c:ID] right after the sentence) ===\n");
    for c in &inp.claims {
        let src = inp.sources.iter().find(|s| s.index == c.source);
        s.push_str(&format!(
            "- [c:{}] {}\n",
            c.id,
            clamp_chars(&c.text, CLAIM_TEXT_CHARS)
        ));
        if with_quotes {
            s.push_str(&format!("    quote: \"{}\"\n", c.quote));
        }
        s.push_str(&format!(
            "    source: {} ({}) published {}\n",
            src.map(|s| s.title.as_str()).unwrap_or(""),
            src.map(|s| s.url.as_str()).unwrap_or(""),
            c.published.as_deref().unwrap_or("unknown date")
        ));
    }
    s.push_str("\n=== Notes you may link to. The link TARGET must be the slug exactly as listed; put any display text after `|`, e.g. [[overtime-pay|ค่าล่วงเวลา]] ===\n");
    for (slug, title) in inp.link_targets {
        if slug != &inp.note.slug {
            s.push_str(&format!("- [[{slug}]] — {title}\n"));
        }
    }
    let shape = match inp.note.kind {
        // A one-page run (`--max-notes 1`, viewer "Create page (summary)")
        // has nothing to map; write it as a self-contained note.
        NoteKind::Moc if inp.note.related.is_empty() => "This is the only page for the query — a self-contained note, not an index. Structure: first line = a one-sentence definition/abstract (this becomes the index summary); then 3–6 `##` sections that answer the query from the claims (what it is, key facts and numbers, how it compares, timeline if claims carry dates, current status as of the newest date); 400–1200 words. No `## Map` section.",
        NoteKind::Moc => "This is the TOPIC PAGE — a report that DESCRIBES the subject, not an index. Structure: 1) an opening of 3–6 sentences that answers the query directly; 2) the outline sections above as `##` headings (if no outline was given, choose 4–8 yourself), each 1–3 paragraphs that SYNTHESISE across claims — compare, rank, explain cause and consequence, name who is ahead and why; when ≥ 3 entities share comparable attributes (models, prices, funding, dates, benchmarks) put them in a markdown table; add a `## Timeline` section when ≥ 5 claims carry dates; the first time the narrative covers a linked note, link it inline as [[slug|name]]; 3) `## Map` — every linked note as a bullet with the clause saying why it matters; 4) `## Open questions` (≤ 3 bullets). 700–1800 words (or the Thai equivalent). Use every claim you were given that fits.",
        NoteKind::Claim => "Structure: the claim in one sentence, then why it holds and what it depends on, 80–250 words.",
        _ => "Structure: first line = a one-sentence definition/abstract (this becomes the index summary); then 2–5 `##` sections that go DEEPER than the topic page (history, what it does, numbers, how it compares, current status as of the newest date); 250–800 words. Link to related notes where the reference genuinely helps.",
    };
    s.push_str(&format!(
        "\nRules:\n\
         - {shape}\n\
         - Only state what a claim supports; every factual sentence ends with its [c:ID] marker(s). No claim → no sentence.\n\
         - Link, don't bold: the first mention of anything in the link list is written as [[slug|name]], never as **name**.\n\
         - Currency: when claims disagree, the newer `published` date wins and the older fact is described as superseded. Anything that changes over time (latest version, price, headcount, leadership, \"newest\") must say `as of <published date>`; never present a dated fact as timeless.\n\
         - Do NOT start with a `#` heading and do NOT write a Sources section — both are generated.\n\
         - {}\n\
         - Output ONLY the markdown body.",
        super::language_rule(inp.language)
    ));
    s
}

fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut o: String = s.chars().take(max).collect();
    o.push('…');
    o
}

/// `[c:s3c2]` → `[3]`, deduplicated per sentence. Unknown ids are
/// removed rather than left as noise. Returns the rewritten body and
/// the set of cited source indices.
pub fn rewrite_claim_markers(body: &str, claims: &HashMap<String, u32>) -> (String, BTreeSet<u32>) {
    // Accept `[c:s1c2]` and the merged form models like to emit,
    // `[c:s1c2, c:s3c1]` / `[c:s1c2 s3c1]`.
    let re = regex::Regex::new(r"\[c:([^\]]+)\]").expect("static regex");
    let mut cited = BTreeSet::new();
    let out = re.replace_all(body, |caps: &regex::Captures| {
        let mut idxs: Vec<u32> = caps[1]
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(|t| t.trim().trim_start_matches("c:"))
            .filter(|t| !t.is_empty())
            .filter_map(|id| claims.get(id).copied())
            .collect();
        idxs.sort_unstable();
        idxs.dedup();
        for i in &idxs {
            cited.insert(*i);
        }
        idxs.iter()
            .map(|i| format!("[{i}]"))
            .collect::<Vec<_>>()
            .join("")
    });
    (collapse_repeated_citations(&out), cited)
}

/// `[3][3]` / `[3] [3]` (adjacent claims from one source) → `[3]`.
/// The regex crate has no backreferences, so this is a manual scan.
fn collapse_repeated_citations(s: &str) -> String {
    let cite = regex::Regex::new(r"\[(\d+)\]").expect("static regex");
    let mut out = String::with_capacity(s.len());
    let mut last_end = 0;
    let mut prev: Option<(String, usize)> = None; // (index, end offset of previous cite)
    for m in cite.find_iter(s) {
        let idx = &s[m.start() + 1..m.end() - 1];
        let gap = &s[last_end..m.start()];
        if let Some((pidx, pend)) = &prev {
            if pidx == idx && *pend == last_end && gap.trim().is_empty() {
                last_end = m.end();
                prev = Some((idx.to_string(), m.end()));
                continue;
            }
        }
        out.push_str(gap);
        out.push_str(m.as_str());
        last_end = m.end();
        prev = Some((idx.to_string(), m.end()));
    }
    out.push_str(&s[last_end..]);
    out
}

/// Writers sometimes link by title instead of slug (`[[นายจ้าง]]`).
/// Map known titles back to their slug, keep known slugs, and turn
/// anything else into plain text so no dangling links reach the graph.
pub fn fix_wikilinks(body: &str, targets: &BTreeMap<String, String>) -> String {
    let re = regex::Regex::new(r"\[\[([^\]|]+)(?:\|([^\]]*))?\]\]").expect("static regex");
    let by_title: HashMap<String, &str> = targets
        .iter()
        .map(|(slug, title)| (title.trim().to_lowercase(), slug.as_str()))
        .collect();
    re.replace_all(body, |caps: &regex::Captures| {
        let target = caps[1].trim();
        let display = caps
            .get(2)
            .map(|m| m.as_str().trim())
            .filter(|d| !d.is_empty());
        if targets.contains_key(target) {
            return caps[0].to_string();
        }
        if let Some(slug) = by_title.get(&target.to_lowercase()) {
            return format!("[[{slug}|{}]]", display.unwrap_or(target));
        }
        display.unwrap_or(target).to_string()
    })
    .into_owned()
}

/// Deterministic linking. Models reliably write `**DeepSeek**` where
/// the prompt asked for `[[deepseek|DeepSeek]]`, which leaves the
/// graph view a star around the topic page. So after the LLM pass:
/// 1. the FIRST plain mention of every link target's title (outside
///    existing links, headings, code and the auto-generated sections)
///    becomes `[[slug|title]]`;
/// 2. any `related` slug still unlinked (plus the topic page, for a
///    child) is appended as a `See also` line so every planned edge
///    exists on disk.
pub fn autolink(
    body: &str,
    self_slug: &str,
    targets: &BTreeMap<String, String>,
    related: &[String],
    parent: Option<&str>,
    language: &str,
) -> String {
    let mut out = String::with_capacity(body.len() + 256);
    // Links in the generated tail (`## Map`, `## Sources`) must not
    // count as "already linked": every Map entry would otherwise
    // suppress its own inline link in the narrative above.
    let tail_at = ["\n## Map", "\n## Sources"]
        .iter()
        .filter_map(|h| body.find(h))
        .min()
        .unwrap_or(body.len());
    let mut linked: BTreeSet<String> = extract_links(&body[..tail_at]);
    // Longest titles first so "DeepSeek V4" wins over "DeepSeek".
    let mut cands: Vec<(&str, &str)> = targets
        .iter()
        .filter(|(slug, title)| slug.as_str() != self_slug && title.trim().len() >= 3)
        .map(|(s, t)| (s.as_str(), t.trim()))
        .collect();
    cands.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

    // A bold mention (`**DeepSeek**`) is the model marking a key entry —
    // link every one of them first, so a passing plain mention earlier
    // in the text does not steal the link.
    let mut body = body.to_string();
    for (slug, title) in &cands {
        let bold = format!("**{title}**");
        if body.contains(&bold) {
            body = body.replace(&bold, &format!("[[{slug}|{title}]]"));
            linked.insert(slug.to_string());
        }
    }
    let mut in_code = false;
    for line in body.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("```") {
            in_code = !in_code;
            out.push_str(line);
            continue;
        }
        if in_code || t.starts_with('#') || t.starts_with("---") {
            out.push_str(line);
            continue;
        }
        if t.starts_with('|') {
            out.push_str(line);
            continue;
        }
        let mut cur = line.to_string();
        for (slug, title) in &cands {
            if linked.contains(*slug) {
                continue;
            }
            if let Some(pos) = find_plain_mention(&cur, title) {
                let end = pos + title.len();
                cur = format!("{}[[{slug}|{title}]]{}", &cur[..pos], &cur[end..]);
                linked.insert(slug.to_string());
            }
        }
        out.push_str(&cur);
    }

    let mut missing: Vec<&str> = Vec::new();
    if let Some(p) = parent {
        if p != self_slug && !linked.contains(p) && targets.contains_key(p) {
            missing.push(p);
        }
    }
    for r in related {
        if r != self_slug
            && !linked.contains(r)
            && targets.contains_key(r)
            && !missing.contains(&r.as_str())
        {
            missing.push(r);
        }
    }
    if !missing.is_empty() {
        let label = match language.trim().to_ascii_lowercase().as_str() {
            "th" | "thai" => "ดูเพิ่มเติม",
            _ => "See also",
        };
        let links: Vec<String> = missing
            .iter()
            .map(|s| {
                format!(
                    "[[{s}|{}]]",
                    targets.get(*s).map(String::as_str).unwrap_or(s)
                )
            })
            .collect();
        let trimmed = out.trim_end().to_string();
        out = format!("{trimmed}\n\n{label}: {}\n", links.join(" · "));
    }
    out
}

/// `**[[slug|Name]]**` / `__[[slug]]__` / `[[**Name**]]` → `[[slug|Name]]`.
/// Models bold the thing they were told to link; a bold-wrapped
/// wikilink survives the viewer but breaks in the editor and in
/// plain-markdown consumers, so the file on disk carries the link
/// bare. Runs on the whole body, links or not.
pub fn unbold_links(body: &str) -> String {
    let outer = regex::Regex::new(r"(\*\*|__)(\[\[[^\]\n]+\]\])(\*\*|__)").expect("static regex");
    let inner = regex::Regex::new(r"\[\[(?:\*\*|__)([^\]|*_\n]+)(?:\*\*|__)(\|[^\]\n]*)?\]\]")
        .expect("static regex");
    let s = outer.replace_all(body, "$2");
    inner.replace_all(&s, "[[$1$2]]").into_owned()
}

fn extract_links(body: &str) -> BTreeSet<String> {
    let re = regex::Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]*)?\]\]").expect("static regex");
    re.captures_iter(body)
        .map(|c| c[1].trim().to_string())
        .collect()
}

/// Byte offset of the first occurrence of `title` in `line` that is not
/// inside `[[…]]`, `[…](…)` or backticks, and sits on word boundaries
/// when its neighbours are ASCII letters/digits.
fn find_plain_mention(line: &str, title: &str) -> Option<usize> {
    let mut from = 0usize;
    while let Some(rel) = line[from..].find(title) {
        let pos = from + rel;
        let end = pos + title.len();
        let before = &line[..pos];
        let after = &line[end..];
        let ok_left = !before
            .chars()
            .next_back()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false);
        let ok_right = !after
            .chars()
            .next()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false);
        let open_wiki = before.matches("[[").count() > before.matches("]]").count();
        let open_md_text = before.matches('[').count() > before.matches(']').count();
        let open_md_url = before
            .rfind("](")
            .map(|i| !before[i..].contains(')'))
            .unwrap_or(false);
        let open_md = open_md_text || open_md_url;
        let open_code = before.matches('`').count() % 2 == 1;
        // Inside a bare URL: the last `http` before us has had no
        // whitespace since, so the address is still running.
        let in_url = before
            .rfind("http")
            .map(|i| !before[i..].contains(char::is_whitespace))
            .unwrap_or(false);
        if ok_left && ok_right && !open_wiki && !open_md && !open_code && !in_url {
            return Some(pos);
        }
        let mut next = end;
        while next < line.len() && !line.is_char_boundary(next) {
            next += 1;
        }
        from = next.max(pos + 1);
        while from < line.len() && !line.is_char_boundary(from) {
            from += 1;
        }
    }
    None
}

pub struct WrittenNote {
    pub slug: String,
    pub path: PathBuf,
    pub action: Action,
    pub cited: BTreeSet<u32>,
}

/// Frontmatter keys a research write owns. Everything else on an
/// existing page belongs to whoever put it there — a category, tags,
/// aliases, a `verified:` stamp a human added — and survives the
/// update. `status` is excluded on purpose: its research/ingest
/// lifecycle values mean "not written yet", which stops being true the
/// moment this function runs.
const MANAGED_FRONTMATTER: &[&str] = &[
    "title",
    "type",
    "kind",
    "related",
    "sources",
    "claims",
    "confidence",
    "updated",
];

fn carried_frontmatter(kref: &KmsRef, slug: &str) -> Vec<(String, String)> {
    let path = kref.root.join("pages").join(format!("{slug}.md"));
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let (fm, _) = crate::kms::parse_frontmatter(&raw);
    fm.into_iter()
        .filter(|(k, v)| {
            !(MANAGED_FRONTMATTER.contains(&k.as_str())
                || k == "status" && matches!(v.trim(), "derived" | "researching"))
        })
        .collect()
}

pub struct NoteToPersist<'a> {
    pub kref: &'a KmsRef,
    pub note: &'a NotePlan,
    pub body: &'a str,
    pub cited: &'a BTreeSet<u32>,
    pub claim_count: usize,
    pub confidence: f32,
    pub today: &'a str,
    pub append: bool,
    pub sources_meta: &'a [(u32, String, String)],
}

/// Compose frontmatter + body and write through the KMS (create /
/// merge) or append a dated section (`--append`).
pub fn persist_note(context: NoteToPersist<'_>) -> Result<WrittenNote> {
    let NoteToPersist {
        kref,
        note,
        body,
        cited,
        claim_count,
        confidence,
        today,
        append,
        sources_meta,
    } = context;
    let mut body = super::kms_writer::strip_sources_section(body.trim());
    body = super::kms_writer::ensure_sources_section(&body, sources_meta);
    body = super::kms_writer::linkify_citations(&body, sources_meta);

    if append && note.action == Action::Update {
        let chunk = format!("\n\n## Update {today}\n\n{}\n", body.trim());
        let path = crate::kms::append_to_page(kref, &note.slug, &chunk)?;
        return Ok(WrittenNote {
            slug: note.slug.clone(),
            path,
            action: note.action,
            cited: cited.clone(),
        });
    }
    let related = note
        .related
        .iter()
        .map(|r| format!("\"{r}\""))
        .collect::<Vec<_>>()
        .join(", ");
    // What the finished body cites, not just what this run rewrote: an
    // update merges the previous note, whose `[N]` markers are already
    // numerals and so never pass through `rewrite_claim_markers`.
    // `sources:` used to list only the new ones — on a real vault a
    // note citing 21 sources declared 8, and `/kms lint` reads the
    // frontmatter.
    let mut all: BTreeSet<u32> = cited.clone();
    all.extend(super::kms_writer::parse_citation_indices(&body));
    let sources = all
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let carried: String = carried_frontmatter(kref, &note.slug)
        .into_iter()
        .map(|(k, v)| {
            let flow = v.starts_with('[') && v.ends_with(']');
            if flow || !(v.contains(':') || v.contains('#') || v.contains('"')) {
                format!("{k}: {v}\n")
            } else {
                format!("{k}: \"{}\"\n", v.replace('"', "'"))
            }
        })
        .collect();
    let composed = format!(
        "---\n\
         title: \"{}\"\n\
         type: note\n\
         kind: {}\n\
         related: [{related}]\n\
         sources: [{sources}]\n\
         claims: {claim_count}\n\
         confidence: {confidence:.2}\n\
         updated: {today}\n\
         {carried}\
         ---\n\n{}\n",
        note.title.replace('"', "'"),
        note.kind.as_str(),
        body.trim()
    );
    let path = crate::kms::write_page(kref, &note.slug, &composed)?;
    Ok(WrittenNote {
        slug: note.slug.clone(),
        path,
        action: note.action,
        cited: cited.clone(),
    })
}

pub fn read_existing_body(kref: &KmsRef, slug: &str) -> Option<String> {
    let path = kref.root.join("pages").join(format!("{slug}.md"));
    let raw = std::fs::read_to_string(path).ok()?;
    let (_, body) = crate::kms::parse_frontmatter(&raw);
    Some(super::kms_writer::strip_sources_section(&body))
}

/// One LLM call per note; MOC last because it links the others.
pub async fn write_note_body(
    provider: &dyn Provider,
    model: &str,
    inp: &NoteInput<'_>,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<String> {
    let prompt = build_note_prompt(inp);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    Ok(strip_leading_heading(raw.trim()))
}

fn strip_leading_heading(s: &str) -> String {
    let mut lines = s.lines().peekable();
    while let Some(l) = lines.peek() {
        if l.trim().starts_with('#') || l.trim().is_empty() {
            lines.next();
        } else {
            break;
        }
    }
    lines.collect::<Vec<_>>().join("\n")
}

pub type RoundSummary = (u32, u32, u32, u32, f32, Vec<String>);

/// `runs/<date>-<slug>.md`: machine-readable record of what the run
/// did. What `/research show` opens.
pub struct RunLog<'a> {
    pub query: &'a str,
    pub topic_slug: &'a str,
    pub today: &'a str,
    pub rounds: &'a [RoundSummary],
    pub sources_digested: u32,
    pub sources_cached: u32,
    pub claims_total: u32,
    pub claims_dropped: u32,
    pub notes: &'a [WrittenNote],
    pub dry_run_plan: Option<&'a [NotePlan]>,
    pub elapsed_secs: u64,
    pub worker_model: &'a str,
    pub warnings: &'a [String],
    /// Every source the run read, and which of them a note ended up
    /// citing — rendered as the evidence table so a reader can judge
    /// what the answer stands on without opening each note.
    pub sources: &'a [ResearchSource],
    pub cited: &'a BTreeSet<u32>,
}

pub fn write_run_log(kref: &KmsRef, log: &RunLog<'_>) -> Result<PathBuf> {
    let dir = kref.root.join("runs");
    std::fs::create_dir_all(&dir)
        .map_err(|e| crate::error::Error::Tool(format!("create {}: {e}", dir.display())))?;
    // Two runs on one topic in one day are two runs: the second used to
    // overwrite the first's log, taking the only record of what it read
    // with it.
    let mut path = dir.join(format!("{}-{}.md", log.today, log.topic_slug));
    for n in 2..100 {
        if !path.exists() {
            break;
        }
        path = dir.join(format!("{}-{}-{n}.md", log.today, log.topic_slug));
    }
    let mut s = format!(
        "---\ntype: research-run\nquery: \"{}\"\ndate: {}\nelapsed_secs: {}\nworker_model: {}\nsources_digested: {}\nsources_cached: {}\nclaims: {}\nclaims_dropped_by_quote_check: {}\n---\n\n# Research run — {}\n\n**Query:** {}\n\n## Rounds\n\n| round | sources | new items | total items | novelty | queries |\n|---|---|---|---|---|---|\n",
        log.query.replace('"', "'"),
        log.today,
        log.elapsed_secs,
        log.worker_model,
        log.sources_digested,
        log.sources_cached,
        log.claims_total,
        log.claims_dropped,
        log.today,
        log.query
    );
    for (r, srcs, new, total, ratio, queries) in log.rounds {
        s.push_str(&format!(
            "| {r} | {srcs} | {new} | {total} | {:.0}% | {} |\n",
            ratio * 100.0,
            queries.join(" · ")
        ));
    }
    if !log.sources.is_empty() {
        use super::source_quality::{subject_words, tier, Tier};
        let words = subject_words(log.query, &[]);
        let mut rows: Vec<(Tier, u32, u32)> = vec![
            (Tier::Primary, 0, 0),
            (Tier::Reference, 0, 0),
            (Tier::Other, 0, 0),
        ];
        for src in log.sources {
            let t = tier(&src.url, &words);
            if let Some(row) = rows.iter_mut().find(|r| r.0 == t) {
                row.1 += 1;
                if log.cited.contains(&src.index) {
                    row.2 += 1;
                }
            }
        }
        s.push_str("\n## Evidence\n\n| tier | read | cited |\n|---|---|---|\n");
        for (t, read, cited) in rows.iter().filter(|r| r.1 > 0) {
            s.push_str(&format!("| {} | {read} | {cited} |\n", t.as_str()));
        }
    }
    if let Some(plan) = log.dry_run_plan {
        s.push_str("\n## Plan (dry run — nothing written)\n\n");
        for n in plan {
            s.push_str(&format!(
                "- `{}` {} — {} ({:?}, {} claims, related: {})\n",
                n.slug,
                n.kind.as_str(),
                n.title,
                n.action,
                n.claim_ids.len(),
                n.related.join(", ")
            ));
        }
    } else {
        if !log.warnings.is_empty() {
            s.push_str("\n## Warnings\n\n");
            for w in log.warnings {
                s.push_str(&format!("- ⚠ {w}\n"));
            }
        }
        s.push_str("\n## Notes\n\n");
        for n in log.notes {
            s.push_str(&format!(
                "- [[{}]] — {} ({} sources)\n",
                n.slug,
                match n.action {
                    Action::Create => "created",
                    Action::Update => "updated",
                },
                n.cited.len()
            ));
        }
    }
    std::fs::write(&path, s)
        .map_err(|e| crate::error::Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(path)
}

pub fn link_targets(plan: &[NotePlan], known: &[KnownNote]) -> BTreeMap<String, String> {
    let mut m: BTreeMap<String, String> = known
        .iter()
        .map(|k| (k.slug.clone(), k.title.clone()))
        .collect();
    for n in plan {
        m.insert(n.slug.clone(), n.title.clone());
    }
    m
}

pub const _NOTE_CONCURRENCY_DOC: usize = NOTE_CONCURRENCY;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autolink_links_first_plain_mention_and_appends_see_also() {
        let mut targets = BTreeMap::new();
        targets.insert("deepseek".to_string(), "DeepSeek".to_string());
        targets.insert("deepseek-v4".to_string(), "DeepSeek V4".to_string());
        targets.insert("qwen".to_string(), "Qwen".to_string());
        targets.insert("topic".to_string(), "Chinese AI".to_string());
        let body = "## Players\n\n- **DeepSeek V4** ships [1](../sources/deepseek.md). DeepSeek again.\n\n| DeepSeek | x |\n";
        let out = autolink(
            body,
            "tencent",
            &targets,
            &["qwen".to_string(), "deepseek".to_string()],
            Some("topic"),
            "th",
        );
        assert!(
            out.contains("- [[deepseek-v4|DeepSeek V4]] ships"),
            "bold mention becomes a bare link: {out}"
        );
        assert!(out.contains(". [[deepseek|DeepSeek]] again"), "{out}");
        assert_eq!(
            out.matches("[[deepseek|").count(),
            1,
            "only the first mention: {out}"
        );
        assert!(out.contains("| DeepSeek | x |"), "tables untouched: {out}");
        assert!(
            out.contains("(../sources/deepseek.md)"),
            "citation paths untouched: {out}"
        );
        assert!(
            out.ends_with("ดูเพิ่มเติม: [[topic|Chinese AI]] · [[qwen|Qwen]]\n"),
            "{out}"
        );
    }

    fn note_plan(slug: &str, title: &str, action: Action) -> NotePlan {
        NotePlan {
            slug: slug.into(),
            kind: NoteKind::Concept,
            title: title.into(),
            action,
            claim_ids: vec!["s1c1".into()],
            related: vec![],
            role: String::new(),
            outline: vec![],
        }
    }

    #[test]
    fn a_research_update_keeps_what_the_reader_added() {
        let _h = crate::research::test_helpers::scoped_home();
        let kref = crate::kms::create("carry-rt", crate::kms::KmsScope::Project).unwrap();
        crate::kms::write_page(
            &kref,
            "deepseek",
            "---\ntitle: \"DeepSeek\"\ntype: note\nkind: entity\ncategory: vendors\ntags: china, llm\nverified: 2026-09-01\nstatus: researching\n---\n\nold body\n",
        )
        .unwrap();
        let cited = BTreeSet::from([1u32]);
        persist_note(NoteToPersist {
            kref: &kref,
            note: &note_plan("deepseek", "DeepSeek", Action::Update),
            body: "new body [1].",
            cited: &cited,
            claim_count: 1,
            confidence: 0.9,
            today: "2026-09-08",
            append: false,
            sources_meta: &[(1, "S".into(), "https://s".into())],
        })
        .unwrap();
        let on_disk = std::fs::read_to_string(kref.pages_dir().join("deepseek.md")).unwrap();
        assert!(on_disk.contains("category: vendors"), "{on_disk}");
        assert!(on_disk.contains("tags: china, llm"), "{on_disk}");
        assert!(on_disk.contains("verified: 2026-09-01"), "{on_disk}");
        assert!(
            on_disk.contains("created:"),
            "creation date survives: {on_disk}"
        );
        assert!(on_disk.contains("updated: 2026-09-08"), "{on_disk}");
        assert!(
            !on_disk.contains("status: researching"),
            "the stub marker is cleared once the note is written: {on_disk}"
        );
        assert!(on_disk.contains("new body"));
    }

    #[test]
    fn two_runs_on_one_day_keep_two_run_logs() {
        static EMPTY_CITED: std::sync::LazyLock<BTreeSet<u32>> =
            std::sync::LazyLock::new(BTreeSet::new);
        let _h = crate::research::test_helpers::scoped_home();
        let kref = crate::kms::create("runlog-rt", crate::kms::KmsScope::Project).unwrap();
        fn log<'a>(slug: &'a str) -> RunLog<'a> {
            RunLog {
                query: "q",
                topic_slug: slug,
                today: "2026-09-08",
                rounds: &[],
                sources_digested: 1,
                sources_cached: 0,
                claims_total: 1,
                claims_dropped: 0,
                notes: &[],
                dry_run_plan: None,
                elapsed_secs: 1,
                worker_model: "m",
                warnings: &[],
                sources: &[],
                cited: &EMPTY_CITED,
            }
        }
        let a = write_run_log(&kref, &log("topic")).unwrap();
        let b = write_run_log(&kref, &log("topic")).unwrap();
        assert_ne!(a, b);
        assert!(
            b.to_string_lossy().ends_with("2026-09-08-topic-2.md"),
            "{b:?}"
        );
        assert_eq!(kref.root.join("runs").read_dir().unwrap().count(), 2);
    }

    #[test]
    fn quotes_leave_the_prompt_once_a_note_carries_too_many_claims() {
        let plan = note_plan("t", "T", Action::Create);
        let mk = |n: usize| -> Vec<Claim> {
            (0..n)
                .map(|i| Claim {
                    id: format!("s1c{i}"),
                    text: format!("claim {i}"),
                    quote: format!("verbatim {i}"),
                    entities: vec![],
                    confidence: 0.9,
                    source: 1,
                    published: None,
                })
                .collect()
        };
        let targets = BTreeMap::new();
        let build = |claims: &[Claim]| {
            build_note_prompt(&NoteInput {
                query: "q",
                note: &plan,
                claims: claims.iter().collect(),
                sources: &[],
                link_targets: &targets,
                existing_body: None,
                append: false,
                language: "en",
                parent_overview: None,
            })
        };
        let few = mk(QUOTE_BUDGET_CLAIMS);
        assert!(build(&few).contains("quote: \"verbatim 0\""));
        let many = mk(QUOTE_BUDGET_CLAIMS + 1);
        let p = build(&many);
        assert!(!p.contains("quote:"), "quotes dropped past the budget");
        assert!(p.contains("[c:s1c0] claim 0"), "claims still listed");
    }

    #[test]
    fn unbold_strips_emphasis_around_and_inside_wikilinks() {
        let s = "- **[[moonshot-ai|Moonshot AI]]** builds Kimi; __[[qwen]]__ and [[**deepseek**|DeepSeek]] too. **bold text** stays.";
        assert_eq!(
            unbold_links(s),
            "- [[moonshot-ai|Moonshot AI]] builds Kimi; [[qwen]] and [[deepseek|DeepSeek]] too. **bold text** stays."
        );
    }

    #[test]
    fn autolink_ignores_map_links_and_links_every_bold_mention() {
        let mut targets = BTreeMap::new();
        targets.insert("deepseek".to_string(), "DeepSeek".to_string());
        targets.insert("baidu".to_string(), "Baidu".to_string());
        let body = "Labs like DeepSeek lead.\n\n- **DeepSeek** started as a quant fund.\n- **Baidu** moved first.\n\n| **Baidu** | x |\n\n## Map\n\n- [[deepseek|DeepSeek]] — ref\n- [[baidu|Baidu]] — first\n";
        let out = unbold_links(&autolink(body, "topic", &targets, &[], None, "en"));
        assert!(out.contains("- [[deepseek|DeepSeek]] started"), "{out}");
        assert!(out.contains("- [[baidu|Baidu]] moved"), "{out}");
        assert!(
            out.contains("| [[baidu|Baidu]] | x |"),
            "bold in tables links too: {out}"
        );
        assert!(
            out.contains("Labs like DeepSeek lead."),
            "plain mention stays once a bold one is linked: {out}"
        );
        assert!(!out.contains("See also"), "everything is linked: {out}");
    }

    #[test]
    fn autolink_leaves_existing_links_and_self_alone() {
        let mut targets = BTreeMap::new();
        targets.insert("qwen".to_string(), "Qwen".to_string());
        targets.insert("alibaba".to_string(), "Alibaba".to_string());
        let out = autolink(
            "[[qwen|Qwen]] is by Alibaba.",
            "alibaba",
            &targets,
            &[],
            None,
            "en",
        );
        assert_eq!(out, "[[qwen|Qwen]] is by Alibaba.");
    }

    #[test]
    fn markers_become_source_citations_and_dedupe() {
        let claims: HashMap<String, u32> = [
            ("s3c1".to_string(), 3u32),
            ("s3c2".to_string(), 3),
            ("s7c1".to_string(), 7),
        ]
        .into_iter()
        .collect();
        let (out, cited) = rewrite_claim_markers(
            "OT is 1.5x [c:s3c1][c:s3c2]. Min wage rose [c:s7c1] [c:nope]. Both [c:s3c1, c:s7c1].",
            &claims,
        );
        assert_eq!(out, "OT is 1.5x [3]. Min wage rose [7] . Both [3][7].");
        assert_eq!(cited.into_iter().collect::<Vec<_>>(), vec![3, 7]);
    }

    #[test]
    fn title_links_become_slug_links_and_unknown_links_become_text() {
        let targets: BTreeMap<String, String> = [
            ("employer".to_string(), "นายจ้าง".to_string()),
            ("overtime-pay".to_string(), "ค่าล่วงเวลา".to_string()),
        ]
        .into_iter()
        .collect();
        let out = fix_wikilinks(
            "[[นายจ้าง]] pays [[overtime-pay|OT]] per [[Some Law|the law]] and [[ghost]].",
            &targets,
        );
        assert_eq!(
            out,
            "[[employer|นายจ้าง]] pays [[overtime-pay|OT]] per the law and ghost."
        );
    }

    #[test]
    fn leading_heading_is_stripped() {
        assert_eq!(
            strip_leading_heading("# T\n\nBody line\n## H\nx"),
            "Body line\n## H\nx"
        );
    }

    #[test]
    fn persist_creates_then_appends() {
        let _h = crate::research::test_helpers::scoped_home();
        let kref = crate::kms::create("persist-rt", crate::kms::KmsScope::Project).unwrap();
        let note = NotePlan {
            slug: "overtime-pay".into(),
            kind: NoteKind::Concept,
            title: "Overtime pay".into(),
            action: Action::Create,
            claim_ids: vec!["s1c1".into()],
            related: vec!["minimum-wage".into()],
            role: String::new(),
            outline: Vec::new(),
        };
        let meta = vec![(1u32, "Src".to_string(), "https://s1".to_string())];
        let cited: BTreeSet<u32> = [1u32].into_iter().collect();
        let w = persist_note(NoteToPersist {
            kref: &kref,
            note: &note,
            body: "OT is 1.5x [1].",
            cited: &cited,
            claim_count: 1,
            confidence: 0.9,
            today: "2026-09-06",
            append: false,
            sources_meta: &meta,
        })
        .unwrap();
        let raw = std::fs::read_to_string(&w.path).unwrap();
        assert!(raw.contains("type: note"));
        assert!(raw.contains("kind: concept"));
        assert!(raw.contains("related: [\"minimum-wage\"]"));
        assert!(raw.contains("## Sources"));
        let upd = NotePlan {
            action: Action::Update,
            ..note.clone()
        };
        persist_note(NoteToPersist {
            kref: &kref,
            note: &upd,
            body: "New fact [1].",
            cited: &cited,
            claim_count: 1,
            confidence: 0.9,
            today: "2026-09-07",
            append: true,
            sources_meta: &meta,
        })
        .unwrap();
        let raw = std::fs::read_to_string(&w.path).unwrap();
        assert!(raw.contains("## Update 2026-09-07"));
        assert!(raw.contains("OT is 1.5x"), "append keeps the original body");
    }
}
