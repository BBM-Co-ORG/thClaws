//! `/kms verify` — does the vault still stand on its evidence?
//!
//! `/kms lint` asks whether the *structure* holds: links resolve, the
//! index matches the filesystem, frontmatter is present. This asks the
//! other question: are the claims still backed by what the pages cite?
//!
//! Two layers, because they cost very different things.
//!
//! **Deterministic (default, no LLM).** Everything that can be decided
//! by reading files: a `[N]` that resolves to nothing, a `sources:`
//! list that disagrees with the body, a cited source whose archive is
//! gone, a paragraph full of numbers and no citation at all, a note
//! nobody has refreshed in months. It also re-runs the digest-time
//! **quote check** against the archived source: every research claim
//! was accepted because a verbatim span of it appeared in the fetched
//! body, so re-testing those spans against `sources/` catches an
//! archive that has been edited or replaced since.
//!
//! **Entailment (`--llm`, opt-in).** The one question files cannot
//! answer: does a sentence follow from the claims it cites? The v1
//! research verifier asked this by re-sending every source body —
//! ~300 k characters per page, one page at a time, which took longer
//! than the search that produced the pages. It does not need them: a
//! claim has already been checked against its source, so the audit only
//! needs the note and the claim *texts*, which is a ~15 KB prompt per
//! page, run eight at a time with thinking off.
//!
//! The auditor's own output is quote-checked in turn — a flagged
//! sentence that is not in the note verbatim is dropped, the same
//! defence the digest step uses against a model that invents its input.

use crate::error::Result;
use crate::kms::KmsRef;
use crate::providers::Provider;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

/// Notes older than this (by `updated:`) are reported as unrefreshed.
pub const DEFAULT_STALE_DAYS: i64 = 90;
/// Uncited-paragraph findings reported per page before summarising.
const MAX_UNCITED_PER_PAGE: usize = 3;
/// Concurrent entailment calls. Matches the research writer's wave.
const LLM_CONCURRENCY: usize = 8;

#[derive(Debug, Clone)]
pub struct VerifyOptions {
    pub stale_days: i64,
    /// Verify one page instead of the whole KMS.
    pub page: Option<String>,
    pub llm: bool,
    /// Repair the one class of damage whose fix is unambiguous:
    /// wikilinks a linker wrote inside a URL.
    pub fix: bool,
    /// Look in the archived source for what supports each flagged sentence.
    pub ground: bool,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            stale_days: DEFAULT_STALE_DAYS,
            page: None,
            llm: false,
            fix: false,
            ground: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Page stem, or `sources/<file>` for an evidence-level finding.
    pub subject: String,
    /// Stable slug for grouping in the report.
    pub kind: &'static str,
    pub detail: String,
    /// `unsupported` only: the flagged text exactly as the auditor quoted
    /// it, unclipped — what `--llm --fix` looks for in the page.
    pub quote: Option<String>,
    /// `unsupported` only: every audit pass flagged it. `--fix` removes
    /// nothing else.
    pub confirmed: bool,
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub pages_checked: u32,
    pub sources_checked: u32,
    pub claims_checked: u32,
    /// Pages the entailment pass actually read (0 without `--llm`).
    pub llm_pages: u32,
    /// Pages repaired by `--fix`.
    pub pages_repaired: u32,
    pub findings: Vec<Finding>,
}

impl VerifyReport {
    pub fn is_clean(&self) -> bool {
        // Evidence that was found and recorded is good news, not a defect.
        self.findings.iter().all(|f| f.kind == "grounded")
    }
    fn of_kind(&self, kind: &str) -> Vec<&Finding> {
        self.findings.iter().filter(|f| f.kind == kind).collect()
    }
}

fn citation_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\[(\d{1,4})\]").expect("static regex"))
}

/// A number worth citing: two or more digits, a percentage, or a
/// currency amount. One stray digit (`GPT-4`, `step 3`) is not a claim.
fn hard_fact_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\d{2,}|\d+\s?%|[$€£¥฿]\s*\d").expect("static regex"))
}

pub fn cited_indices(body: &str) -> BTreeSet<u32> {
    citation_re()
        .captures_iter(body)
        .filter_map(|c| c[1].parse::<u32>().ok())
        .collect()
}

fn frontmatter_sources(fm: &BTreeMap<String, String>) -> BTreeSet<u32> {
    fm.get("sources")
        .map(|raw| {
            crate::kms::sources_entries(raw)
                .into_iter()
                .filter_map(|t| t.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn days_since(date: &str) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(date.trim().trim_matches('"'), "%Y-%m-%d").ok()?;
    Some((chrono::Local::now().date_naive() - d).num_days())
}

/// The prose a reader is meant to check: no frontmatter, no generated
/// `## Sources`, no `## Map` and no `## Open questions` — those are a
/// list of links and a list of questions, neither of them assertions.
pub fn checkable_body(raw: &str) -> String {
    let (_, body) = crate::kms::parse_frontmatter(raw);
    let mut body = crate::research::kms_writer::strip_sources_section(&body);
    for heading in ["\n## Map", "\n## Open questions"] {
        if let Some(i) = body.find(heading) {
            body = match body[i + 1..].find("\n## ") {
                Some(j) => format!("{}{}", &body[..i], &body[i + 1 + j..]),
                None => body[..i].to_string(),
            };
        }
    }
    body
}

/// Drop link syntax before deciding whether a paragraph asserts a
/// number. A year inside a link label (`[[china-ai-agent-regulation-2026|…]]`)
/// is part of a name, not a claim the paragraph is making.
fn without_links(p: &str) -> String {
    static WIKI: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static MD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let wiki = WIKI.get_or_init(|| regex::Regex::new(r"\[\[[^\]\n]*\]\]").expect("static regex"));
    let md = MD.get_or_init(|| {
        regex::Regex::new(
            r"\[[^\]\n]*\]\([^)\n]*\)|<?(?:kms://[^\n]*|[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s)>\]]+)>?",
        )
        .expect("static regex")
    });
    md.replace_all(&wiki.replace_all(p, " "), " ").into_owned()
}

/// Paragraphs that assert a number and cite nothing. Paragraph-level on
/// purpose: notes default to Thai, which has no sentence terminator to
/// split on, and the writer is told to cite every factual sentence — a
/// whole paragraph with no marker at all is the unambiguous signal.
fn uncited_paragraphs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_code = false;
    for para in body.split("\n\n") {
        let p = para.trim();
        if p.is_empty() {
            continue;
        }
        if p.contains("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let first = p.lines().next().unwrap_or("").trim_start();
        if first.starts_with('#')
            || first.starts_with('|')
            || first.starts_with('>')
            || first.starts_with("---")
            || first.starts_with("See also:")
            || first.starts_with("ดูเพิ่มเติม:")
        {
            continue;
        }
        if citation_re().is_match(p) {
            continue;
        }
        let prose = without_links(p);
        if !hard_fact_re().is_match(&prose) {
            continue;
        }
        // A question is not an assertion, whatever language it is in.
        if prose.trim_end().ends_with('?') {
            continue;
        }
        let mut snippet: String = p.chars().take(120).collect();
        if p.chars().count() > 120 {
            snippet.push('…');
        }
        out.push(snippet.replace('\n', " "));
    }
    out
}

fn link_in_url_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"((?:kms://[^\n\[]*?|[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s)>\]]*?))\[\[([^\]\n|]+)(?:\|([^\]\n]*))?\]\]")
            .expect("static regex")
    })
}

/// `https://[[tracxn]].com/x` → `https://tracxn.com/x`. A linker that
/// matched a word inside an address left the original text intact
/// inside the brackets, so unwrapping restores the URL exactly: the
/// display half when the link has one, the target otherwise.
pub fn unwrap_links_in_urls(body: &str) -> (String, usize) {
    let mut n = 0usize;
    let mut out = body.to_string();
    // Repeatedly: one URL can carry more than one wrapped word.
    loop {
        let next = link_in_url_re()
            .replace_all(&out, |c: &regex::Captures| {
                let text = c
                    .get(3)
                    .map(|m| m.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| c.get(2).map(|m| m.as_str()).unwrap_or(""));
                format!("{}{}", &c[1], text)
            })
            .into_owned();
        if next == out {
            break;
        }
        n += 1;
        out = next;
    }
    (out, n)
}

/// Deterministic pass. Never calls a model; safe to run on any KMS,
/// including one with no research metadata at all (the citation and
/// quote checks simply find nothing to check).
pub fn verify(kref: &KmsRef, opts: &VerifyOptions) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();
    let registry = crate::research::registry::SourceRegistry::load(kref);
    let meta = registry.meta();
    let known_indices: BTreeSet<u32> = meta.iter().map(|(i, _, _)| *i).collect();
    let url_by_index: BTreeMap<u32, String> =
        meta.iter().map(|(i, _, url)| (*i, url.clone())).collect();
    let archived: BTreeSet<String> = crate::kms::list_sources(kref)
        .iter()
        .map(|s| s.stem.clone())
        .collect();

    let mut pages: Vec<(String, String)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(kref.pages_dir()) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.starts_with('.') || stem == "_summary" {
                continue;
            }
            if let Some(only) = &opts.page {
                if stem != only.trim().trim_end_matches(".md") {
                    continue;
                }
            }
            if let Ok(raw) = std::fs::read_to_string(&path) {
                pages.push((stem.to_string(), raw));
            }
        }
    }
    pages.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(only) = &opts.page {
        if pages.is_empty() {
            return Err(crate::error::Error::Tool(format!(
                "no page '{only}' in KMS '{}'",
                kref.name
            )));
        }
    }

    for (stem, raw) in &pages {
        report.pages_checked += 1;
        let (fm, _) = crate::kms::parse_frontmatter(raw);
        let body = checkable_body(raw);
        let cited = cited_indices(&body);
        let declared = frontmatter_sources(&fm);

        for n in &cited {
            if !known_indices.is_empty() && !known_indices.contains(n) {
                report.findings.push(Finding {
                    subject: stem.clone(),
                    kind: "unresolved_citation",
                    detail: format!("[{n}] is not a source in this KMS's registry"),
                    quote: None,
                    confirmed: false,
                });
                continue;
            }
            if let Some(url) = url_by_index.get(n) {
                let file = crate::research::kms_writer::url_to_filename(url);
                if !archived.contains(&file) {
                    report.findings.push(Finding {
                        subject: stem.clone(),
                        kind: "missing_archive",
                        detail: format!("[{n}] cites {url}, but sources/{file}.md is not there"),
                        quote: None,
                        confirmed: false,
                    });
                }
            }
        }
        if !declared.is_empty() || !cited.is_empty() {
            let body_only: Vec<String> =
                cited.difference(&declared).map(|n| n.to_string()).collect();
            let fm_only: Vec<String> = declared.difference(&cited).map(|n| n.to_string()).collect();
            if !body_only.is_empty() {
                report.findings.push(Finding {
                    subject: stem.clone(),
                    kind: "frontmatter_drift",
                    detail: format!(
                        "body cites [{}] but `sources:` does not list them",
                        body_only.join("], [")
                    ),
                    quote: None,
                    confirmed: false,
                });
            }
            if !fm_only.is_empty() {
                report.findings.push(Finding {
                    subject: stem.clone(),
                    kind: "frontmatter_drift",
                    detail: format!(
                        "`sources:` lists {} that the body never cites",
                        fm_only.join(", ")
                    ),
                    quote: None,
                    confirmed: false,
                });
            }
        }

        // Uncited assertions only make sense where citing is the rule.
        let is_research_note = fm.get("type").map(|t| t.trim()) == Some("note");
        if is_research_note {
            let uncited = uncited_paragraphs(&body);
            for p in uncited.iter().take(MAX_UNCITED_PER_PAGE) {
                report.findings.push(Finding {
                    subject: stem.clone(),
                    kind: "uncited_assertion",
                    detail: format!("no citation in: {p}"),
                    quote: None,
                    confirmed: false,
                });
            }
            if uncited.len() > MAX_UNCITED_PER_PAGE {
                report.findings.push(Finding {
                    subject: stem.clone(),
                    kind: "uncited_assertion",
                    detail: format!(
                        "… and {} more uncited paragraph(s) carrying numbers",
                        uncited.len() - MAX_UNCITED_PER_PAGE
                    ),
                    quote: None,
                    confirmed: false,
                });
            }
        }

        // A wikilink inside a URL: the address resolves nowhere, and
        // no reader can tell what it was meant to be.
        if link_in_url_re().is_match(raw) {
            let (repaired, _) = unwrap_links_in_urls(raw);
            let broken = link_in_url_re().find_iter(raw).count();
            if opts.fix {
                let path = kref.pages_dir().join(format!("{stem}.md"));
                // Byte-level rewrite on purpose: `write_page` would
                // re-stamp `updated:`, and a repair inside a URL is not
                // a change to what the note says.
                match std::fs::write(&path, repaired.as_bytes()) {
                    Ok(()) => {
                        report.pages_repaired += 1;
                        report.findings.push(Finding {
                            subject: stem.clone(),
                            kind: "repaired",
                            detail: format!("unwrapped {broken} wikilink(s) written inside a URL"),
                            quote: None,
                            confirmed: false,
                        });
                    }
                    Err(e) => report.findings.push(Finding {
                        subject: stem.clone(),
                        kind: "corrupted_link",
                        detail: format!("{broken} wikilink(s) inside a URL; repair failed: {e}"),
                        quote: None,
                        confirmed: false,
                    }),
                }
            } else {
                report.findings.push(Finding {
                    subject: stem.clone(),
                    kind: "corrupted_link",
                    detail: format!(
                        "{broken} wikilink(s) written inside a URL — the address resolves nowhere. `--fix` unwraps them."
                    ),
                    quote: None,
                    confirmed: false,
                });
            }
        }

        if let Some(days) = fm.get("updated").and_then(|u| days_since(u)) {
            if days > opts.stale_days {
                report.findings.push(Finding {
                    subject: stem.clone(),
                    kind: "stale",
                    detail: format!("not refreshed for {days} days"),
                    quote: None,
                    confirmed: false,
                });
            }
        }
    }

    // ── evidence: do the archived sources still say it? ─────────────
    let digests = kref.root.join(".research").join("digests");
    if let Ok(rd) = std::fs::read_dir(&digests) {
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(d) = serde_json::from_str::<crate::research::digest::Digest>(&raw) else {
                continue;
            };
            if d.claims.is_empty() {
                continue;
            }
            let file = crate::research::kms_writer::url_to_filename(&d.url);
            let src_path = kref.root.join("sources").join(format!("{file}.md"));
            let Ok(archive) = std::fs::read_to_string(&src_path) else {
                continue; // never cited, so never archived — not a defect
            };
            report.sources_checked += 1;
            let norm = crate::research::digest::normalize_for_match(&archive);
            let mut drifted = Vec::new();
            for c in &d.claims {
                report.claims_checked += 1;
                if !crate::research::digest::quote_check(&norm, &c.quote) {
                    drifted.push(c.text.clone());
                }
            }
            if !drifted.is_empty() {
                let mut detail = format!(
                    "{} of {} extracted claim(s) no longer appear in the archive",
                    drifted.len(),
                    d.claims.len()
                );
                for t in drifted.iter().take(2) {
                    detail.push_str(&format!("\n      · {}", clamp(t, 100)));
                }
                report.findings.push(Finding {
                    subject: format!("sources/{file}.md"),
                    kind: "quote_drift",
                    detail,
                    quote: None,
                    confirmed: false,
                });
            }
        }
    }

    Ok(report)
}

fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut o: String = s.chars().take(max).collect();
    o.push('…');
    o
}

// ── Entailment pass ──────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct RawUnsupported {
    #[serde(default)]
    quote: String,
    #[serde(default)]
    why: String,
    /// `"unsupported"` for a finding. A model asked to list unsupported
    /// sentences also lists ones it goes on to clear — two items of a real
    /// run ended "…จึง supported" — and with `--fix` those were about to be
    /// deleted. Asking for the verdict makes the model commit to one, and
    /// anything but `unsupported` is not a finding.
    #[serde(default)]
    verdict: String,
}

/// What one audit call came to.
enum AuditReply {
    Items(Vec<RawUnsupported>),
    /// No JSON, but the reply says in words that nothing is wrong.
    Clean,
    Unreadable(String),
}

/// One audit of one page. About one reply in six came back as prose — "I'll
/// audit each sentence of the note…" — instead of the JSON asked for, so it
/// is asked once more, bluntly. A reply that says in words that it found
/// nothing is a clean page, not a failed audit.
async fn audit_page(
    provider: &dyn Provider,
    model: &str,
    prompt: String,
    timeout: Duration,
    cancel: &crate::cancel::CancelToken,
) -> AuditReply {
    let parse = |raw: &str| -> Option<Vec<RawUnsupported>> {
        let json = crate::research::digest::extract_json(raw, '[', ']');
        serde_json::from_str::<Vec<RawUnsupported>>(json).ok()
    };
    let says_clean = |raw: &str| {
        let t = raw.to_lowercase();
        [
            "no unsupported",
            "nothing unsupported",
            "all sentences are supported",
            "ไม่พบประโยค",
        ]
        .iter()
        .any(|m| t.contains(m))
    };
    let mut last = String::new();
    for attempt in 0..2 {
        let ask = if attempt == 0 {
            prompt.clone()
        } else {
            format!(
                "{prompt}\n\nYour previous reply was not JSON. Reply with the JSON array ONLY — \
                 no preamble, no analysis, no markdown. `[]` if nothing is unsupported."
            )
        };
        match crate::research::llm_calls::oneshot_judgement(provider, model, ask, timeout, cancel)
            .await
        {
            Ok(raw) => {
                if let Some(items) = parse(&raw) {
                    return AuditReply::Items(items);
                }
                if says_clean(&raw) {
                    return AuditReply::Clean;
                }
                last = format!(
                    "the auditor's reply was not the JSON asked for, twice: {}",
                    clamp(raw.trim(), 120)
                );
            }
            Err(e) => return AuditReply::Unreadable(format!("entailment check did not run: {e}")),
        }
    }
    AuditReply::Unreadable(last)
}

/// Do two quotes name the same sentence? Two passes rarely copy the same
/// span: one takes the whole sentence, the other its second clause.
fn same_sentence(a: &str, b: &str) -> bool {
    let (a, b) = (
        crate::research::digest::normalize_for_match(a),
        crate::research::digest::normalize_for_match(b),
    );
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a.contains(&b) || b.contains(&a) {
        return true;
    }
    let pairs = |t: &str| -> std::collections::HashSet<(char, char)> {
        let c: Vec<char> = t.chars().filter(|c| !c.is_whitespace()).collect();
        c.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let (pa, pb) = (pairs(&a), pairs(&b));
    let shared = pa.intersection(&pb).count();
    shared * 10 >= pa.len().min(pb.len()) * 7
}

/// An item is a finding only if the auditor said so and did not take it
/// back in the same breath.
fn is_finding(it: &RawUnsupported) -> bool {
    let verdict = it.verdict.trim().to_lowercase();
    if !verdict.is_empty() && verdict != "unsupported" {
        return false;
    }
    // Older replies, and models that ignore the field: read the reason. It
    // clears the sentence when it says "supported" without negating it.
    let why = it.why.to_lowercase();
    let clears = [
        "จึง supported",
        "ซึ่ง supported",
        "is supported",
        "are supported",
        "fully supported",
    ];
    !clears.iter().any(|c| why.contains(c))
}

fn build_entail_prompt(slug: &str, body: &str, claims: &[(u32, String)]) -> String {
    let mut s = format!(
        "You are auditing ONE knowledge-base note against the evidence it cites.\n\n\
         === NOTE `{slug}` ===\n{body}\n\n\
         === CLAIMS AVAILABLE (already verified against the sources this note cites) ===\n"
    );
    for (idx, text) in claims {
        s.push_str(&format!("- [{idx}] {text}\n"));
    }
    s.push_str(
        "\nReport ONLY sentences of the note whose meaning no listed claim supports.\n\n\
         Be generous about synthesis. A faithful paraphrase, a merge of two claims, a \
         reordering, a summary, or a general statement the claims plainly imply is \
         SUPPORTED — do not report it. Report a sentence only when a load-bearing \
         detail has no backing: a number, a date, a name, a ranking, a superlative, or \
         a causal link that no claim carries.\n\n\
         `quote` must be copied from the note character for character. If you cannot \
         copy it exactly, leave the sentence out.\n\n\
         Output STRICT JSON, no fence, no commentary. An empty array means the note \
         checks out:\n\
         [{\"quote\": \"…\", \"verdict\": \"unsupported\", \"why\": \"…\"}]\n\n\
         Every item you output will be treated as UNSUPPORTED and may be deleted from \
         the note. If, while writing `why`, you find a claim that does support the \
         sentence, do not output the item at all.",
    );
    s
}

/// Ask the model which sentences are not carried by the claims behind
/// their citations. Findings whose `quote` is not in the note verbatim
/// are dropped — the auditor gets the same treatment as the extractor.
#[allow(clippy::too_many_arguments)]
pub async fn verify_entailment(
    kref: &KmsRef,
    report: &mut VerifyReport,
    provider: Arc<dyn Provider>,
    model: &str,
    timeout: Duration,
    cancel: &crate::cancel::CancelToken,
    opts: &VerifyOptions,
) -> Result<()> {
    // Claims per source index, from the digest cache.
    // Text AND quote. The text is the digest's paraphrase and drops detail
    // — a real note was flagged for "51%", a figure the source states and
    // the claim's text had left out while its verbatim quote still had it.
    let mut claims_by_source: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(kref.root.join(".research").join("digests")) {
        for entry in rd.flatten() {
            let Ok(raw) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(d) = serde_json::from_str::<crate::research::digest::Digest>(&raw) else {
                // An unreadable digest is claims the audit never sees. With
                // every digest unreadable it used to return early and the
                // report read "clean" over a vault nothing had looked at.
                report.findings.push(Finding {
                    subject: format!(
                        ".research/digests/{}",
                        entry.file_name().to_string_lossy()
                    ),
                    kind: "audit_failed",
                    detail: "this digest could not be read, so its claims were not available to the audit".into(),
                    quote: None,
                    confirmed: false,
                });
                continue;
            };
            for c in d.claims {
                let line = if c.quote.trim().is_empty() {
                    c.text
                } else {
                    format!("{} — the source's words: \"{}\"", c.text, c.quote.trim())
                };
                claims_by_source.entry(c.source).or_default().push(line);
            }
        }
    }
    if claims_by_source.is_empty() {
        return Ok(());
    }

    let mut jobs: Vec<(String, String, Vec<(u32, String)>)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(kref.pages_dir()) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.starts_with('.') || stem == "_summary" {
                continue;
            }
            if let Some(only) = &opts.page {
                if stem != only.trim().trim_end_matches(".md") {
                    continue;
                }
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let body = checkable_body(&raw);
            let cited = cited_indices(&body);
            if cited.is_empty() {
                continue;
            }
            let claims: Vec<(u32, String)> = cited
                .iter()
                .flat_map(|n| {
                    claims_by_source
                        .get(n)
                        .into_iter()
                        .flatten()
                        .map(move |t| (*n, t.clone()))
                })
                .collect();
            if claims.is_empty() {
                continue;
            }
            jobs.push((stem.to_string(), body, claims));
        }
    }
    jobs.sort_by(|a, b| a.0.cmp(&b.0));
    report.llm_pages = jobs.len() as u32;

    // `--fix` deletes, so it asks twice. Three runs over one vault returned
    // 62, 41 and 21 findings, and one page went 0 → 0 → 10: a small model
    // with thinking off is a noisy judge, and a single pass would delete
    // whatever it happened to say that time. A sentence is removed only
    // when two independent passes both flag it; the report shows everything
    // either pass found, and says which.
    // Grounding has its own, deterministic, second opinion — the source.
    let passes = if opts.fix && !opts.ground { 2 } else { 1 };
    let sem = Arc::new(tokio::sync::Semaphore::new(LLM_CONCURRENCY));
    let futs = jobs.iter().flat_map(|(slug, body, claims)| {
        let prompt = build_entail_prompt(slug, body, claims);
        (0..passes).map({
            let provider = provider.clone();
            let sem = sem.clone();
            let cancel = cancel.clone();
            move |_| {
                let provider = provider.clone();
                let sem = sem.clone();
                let cancel = cancel.clone();
                let prompt = prompt.clone();
                let model = model.to_string();
                async move {
                    let _p = sem.acquire().await;
                    audit_page(provider.as_ref(), &model, prompt, timeout, &cancel).await
                }
            }
        })
    });
    let replies = futures::future::join_all(futs).await;

    for (job, page_replies) in jobs.iter().zip(replies.chunks(passes)) {
        let (slug, body, _) = job;
        let norm_body = crate::research::digest::normalize_for_match(body);
        let norm_exempt = crate::research::digest::normalize_for_match(&exempt_text(body));
        // Per pass: the quotes that survive every check.
        let mut per_pass: Vec<Vec<(String, String)>> = Vec::new();
        for reply in page_replies {
            let items = match reply {
                AuditReply::Items(items) => items,
                AuditReply::Clean => {
                    per_pass.push(Vec::new());
                    continue;
                }
                AuditReply::Unreadable(why) => {
                    report.findings.push(Finding {
                        subject: slug.clone(),
                        kind: "audit_failed",
                        detail: why.clone(),
                        quote: None,
                        confirmed: false,
                    });
                    continue;
                }
            };
            let mut kept = Vec::new();
            for it in items {
                let quote = it.quote.trim();
                if quote.is_empty() || !is_finding(it) {
                    continue;
                }
                // The auditor must quote the note, not invent it.
                if !crate::research::digest::quote_check(&norm_body, quote) {
                    continue;
                }
                // What a note is ALLOWED to say without a claim is not a
                // finding: its opening description of the subject, and its
                // "See also" line. A first run flagged both on real pages.
                if crate::research::digest::quote_check(&norm_exempt, quote) {
                    continue;
                }
                kept.push((quote.to_string(), it.why.trim().to_string()));
            }
            per_pass.push(kept);
        }
        // Every pass had to be read for anything to count as confirmed.
        let all_read = per_pass.len() == passes;
        let mut seen: Vec<String> = Vec::new();
        for (p, found) in per_pass.iter().enumerate() {
            for (quote, why) in found {
                if seen.iter().any(|q| same_sentence(q, quote)) {
                    continue;
                }
                seen.push(quote.clone());
                let confirmed = all_read
                    && per_pass.iter().enumerate().all(|(o, other)| {
                        o == p || other.iter().any(|(q, _)| same_sentence(q, quote))
                    });
                let tag = match (passes, confirmed) {
                    (1, _) => "",
                    (_, true) => " [both passes]",
                    (_, false) => " [one pass only — left alone]",
                };
                report.findings.push(Finding {
                    subject: slug.clone(),
                    kind: "unsupported",
                    detail: format!("{}{tag}\n      · {}", clamp(quote, 160), clamp(why, 120)),
                    quote: Some(quote.clone()),
                    confirmed,
                });
            }
        }
    }
    if opts.ground {
        ground_findings(kref, report, provider.clone(), model, timeout, cancel).await;
    }
    Ok(())
}

// ── Grounding ────────────────────────────────────────────────────────
//
// The audit asks a model whether a sentence follows from the claims, and
// three runs over one vault showed how little that answer can carry: 62,
// 41, 21 findings, and of 18 sentences two passes agreed on one. Grounding
// asks a question a file can answer: is there a passage in the archived
// source that says this? The model only proposes the passage. Whether the
// words are really in the source, and really about the sentence, is
// checked here — the same defence the digest step uses.

/// Passages of a source most like `sentence`, best first.
///
/// Weighted by rarity: a term's worth is how few passages carry it. A plain
/// count of shared terms ranked the true passage for "cap ห้าชั้น … v2.1.217
/// … v2.1.219" FOURTH of 138 on a real source — behind three that merely
/// shared more everyday Thai — and only three were shown. With rarity
/// weighting it is first, because almost nothing else says `v2.1.217`.
fn nearest_passages<'a>(paras: &'a [String], sentence: &str, k: usize) -> Vec<&'a str> {
    let terms: Vec<std::collections::HashSet<String>> = paras
        .iter()
        .map(|p| crate::research::graph::relevance_terms(p))
        .collect();
    let mut df: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in terms.iter().flatten() {
        *df.entry(t.as_str()).or_default() += 1;
    }
    let n = paras.len() as f32;
    let want = crate::research::graph::relevance_terms(sentence);
    let mut scored: Vec<(f32, &str)> = paras
        .iter()
        .zip(&terms)
        .map(|(p, have)| {
            let score: f32 = want
                .intersection(have)
                .map(|t| (1.0 + n / df.get(t.as_str()).copied().unwrap_or(1) as f32).ln())
                .sum();
            (score, p.as_str())
        })
        .filter(|(score, _)| *score > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(k).map(|(_, p)| p).collect()
}

/// dev-plan/64 P5.1: what one `[n]` in a page stands on.
///
/// A citation marker names a *source*, not a sentence in it — the note
/// writer emits `[n]` for the registry index, and a source can carry
/// seventy claims. So a reader who wanted to check one sentence had to
/// open the archive and search it by hand, which is exactly the work a
/// citation is supposed to save.
///
/// `claim` and `quote` are therefore the closest thing this source
/// says to the sentence the marker sits in, ranked the same way
/// grounding ranks passages, and empty when nothing is close enough.
/// The source, the title and the URL are exact.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CitationEvidence {
    /// The `n` in `[n]`.
    pub index: u32,
    /// Stem of the archived source, which is what the viewer opens.
    pub source: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// The digest's paraphrase of the closest claim.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub claim: String,
    /// That claim's verbatim words from the source.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub quote: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// A linked citation marker: `[12](../sources/x.md)`, optionally with
/// a markdown link title. Deliberately narrower than [`citation_re`] —
/// the GUI decorates the anchors `marked` produces, and a bare `[12]`
/// with no link produces none, so counting those would slide every
/// marker after it onto the wrong evidence.
fn linked_citation_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"\[(\d{1,4})\]\(([^)\s]+)(?:\s+"[^"]*")?\)"#).expect("static regex")
    })
}

/// How much of the prose before a marker is taken as "what this
/// cites", when nothing closer bounds it. Thai does not end sentences
/// with punctuation, so a character window is the only boundary that
/// works in both scripts.
const CITE_WINDOW_CHARS: usize = 220;

/// The text a marker at `at` is citing: what has been said since the
/// previous marker, or since the paragraph began, whichever is nearer,
/// capped at [`CITE_WINDOW_CHARS`].
///
/// The cap alone is not enough. With only a character window, a marker
/// that opens a paragraph swallows the end of the one before it and
/// matches *its* claim — so in a page of short paragraphs every marker
/// answered with its predecessor's evidence.
fn cited_window(body: &str, at: usize, prev_end: usize) -> &str {
    let para = body[..at].rfind("\n\n").map(|i| i + 2).unwrap_or(0);
    let start = para.max(prev_end);
    let slice = &body[start..at];
    match slice.char_indices().rev().nth(CITE_WINDOW_CHARS - 1) {
        Some((i, _)) => &slice[i..],
        None => slice,
    }
}

/// Every linked citation marker in `page`, in the order they appear.
pub fn citation_evidence(kref: &KmsRef, page: &str) -> Vec<CitationEvidence> {
    let stem = page.trim().trim_end_matches(".md");
    let Ok(raw) = std::fs::read_to_string(kref.pages_dir().join(format!("{stem}.md"))) else {
        return Vec::new();
    };
    let (_, body) = crate::kms::parse_frontmatter(&raw);
    let mut prev_end = 0usize;
    let markers: Vec<(u32, String, usize, usize)> = linked_citation_re()
        .captures_iter(&body)
        .filter_map(|c| {
            let n = c[1].parse::<u32>().ok()?;
            let m = c.get(0)?;
            let out = (n, c[2].to_string(), m.start(), prev_end);
            prev_end = m.end();
            Some(out)
        })
        .collect();
    if markers.is_empty() {
        return Vec::new();
    }

    // Claims per source index, from the digest cache. Unreadable
    // digests are skipped rather than reported: this is a read for a
    // tooltip, and `/kms verify` is where a broken digest is an event.
    let mut claims_by_source: BTreeMap<u32, Vec<crate::research::digest::Claim>> = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(kref.root.join(".research").join("digests")) {
        for entry in rd.flatten() {
            let Ok(raw) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(d) = serde_json::from_str::<crate::research::digest::Digest>(&raw) else {
                continue;
            };
            for c in d.claims {
                claims_by_source.entry(c.source).or_default().push(c);
            }
        }
    }
    let catalogue = crate::kms_sources::load(kref);

    markers
        .into_iter()
        .map(|(index, href, at, prev_end)| {
            let file = href
                .rsplit('/')
                .next()
                .unwrap_or(&href)
                .split(['#', '?'])
                .next()
                .unwrap_or(&href);
            let file = percent_decode(file);
            let source = std::path::Path::new(&file)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.clone());
            let record = catalogue.entries.get(&file);
            let before = cited_window(&body, at, prev_end);
            let best = claims_by_source
                .get(&index)
                .and_then(|claims| nearest_claim(claims, before));
            CitationEvidence {
                index,
                source,
                title: record.map(|r| r.title.clone()).unwrap_or_default(),
                url: record
                    .filter(|r| r.origin_ref.starts_with("http"))
                    .map(|r| r.origin_ref.clone())
                    .unwrap_or_default(),
                claim: best.map(|c| c.text.clone()).unwrap_or_default(),
                quote: best.map(|c| c.quote.trim().to_string()).unwrap_or_default(),
                confidence: best.map(|c| c.confidence),
            }
        })
        .collect()
}

/// `%E0%B8%A2…` — the note writer percent-encodes a Thai filename in a
/// markdown href, and the catalogue is keyed by the name on disk.
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// How much of what a sentence says the claim has to account for
/// before it is offered as that sentence's evidence.
///
/// A raw overlap score is not enough: pick the highest and a sentence
/// the source says nothing about still gets an answer, because Thai
/// bigrams like "ใน" and "เร" are everywhere. Weighing the match
/// against everything the sentence *asked for* — unmatched terms
/// included, at the weight of a term the source never uses — is what
/// lets "nothing here is close" be an answer.
///
/// Chosen from `survey_citation_evidence` over the owner's vault (39
/// pages, 435 linked citations), scoring each sentence against the
/// source it cites and, as a control, against one it does not:
///
/// ```text
/// floor   cited source   a source it does not cite
///  0.30        94%                  38%
///  0.40        87%                  12%
///  0.50        77%                   5%
///  0.70        62%                   0%
/// ```
///
/// 0.50 is the knee. Going down to 0.40 buys ten more points of
/// coverage and triples what gets through — and for a feature whose
/// whole job is being checkable, a quote that turns out not to be the
/// evidence is worse than no quote at all.
const CITE_MIN_OVERLAP: f32 = 0.50;

/// The claim closest to `sentence`, by IDF-weighted term overlap as a
/// share of what the sentence asked for. `None` when nothing in the
/// source comes close enough to be worth showing as evidence.
fn nearest_claim<'a>(
    claims: &'a [crate::research::digest::Claim],
    sentence: &str,
) -> Option<&'a crate::research::digest::Claim> {
    scored_nearest_claim(claims, sentence).map(|(_, c)| c)
}

/// [`nearest_claim`] with the share it won by — the surveys pick the
/// floor from this, and it is the only way to tell a match that is
/// barely over the line from one that is unambiguous.
fn scored_nearest_claim<'a>(
    claims: &'a [crate::research::digest::Claim],
    sentence: &str,
) -> Option<(f32, &'a crate::research::digest::Claim)> {
    let terms: Vec<std::collections::HashSet<String>> = claims
        .iter()
        .map(|c| crate::research::graph::relevance_terms(&format!("{} {}", c.text, c.quote)))
        .collect();
    let mut df: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in terms.iter().flatten() {
        *df.entry(t.as_str()).or_default() += 1;
    }
    let n = claims.len() as f32;
    // A term no claim uses keeps df 1, so it carries the weight of the
    // rarest thing here — a sentence full of words the source never
    // says is exactly the case this has to reject.
    let idf = |t: &str| (1.0 + n / df.get(t).copied().unwrap_or(1) as f32).ln();
    let want = crate::research::graph::relevance_terms(sentence);
    let asked: f32 = want.iter().map(|t| idf(t.as_str())).sum();
    if asked <= 0.0 {
        return None;
    }
    claims
        .iter()
        .zip(&terms)
        .map(|(c, have)| {
            let got: f32 = want.intersection(have).map(|t| idf(t.as_str())).sum();
            (got / asked, c)
        })
        .filter(|(share, _)| *share >= CITE_MIN_OVERLAP)
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
}

/// A source as passages worth showing: blank-line paragraphs, with the
/// short ones (headings, one-liners) folded into what follows.
fn passages(source: &str) -> Vec<String> {
    let (_, body) = crate::kms::parse_frontmatter(source);
    let mut out: Vec<String> = Vec::new();
    let mut carry = String::new();
    for p in body.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        if !carry.is_empty() {
            carry.push('\n');
        }
        carry.push_str(p);
        if carry.chars().count() >= 200 {
            out.push(std::mem::take(&mut carry));
        }
    }
    if !carry.is_empty() {
        out.push(carry);
    }
    out
}

/// Is `quote` about `sentence`? A real quote that has nothing to do with the
/// sentence is not evidence for it. Measured against the SHORTER of the two:
/// measured against the quote alone, a long quote that contains everything
/// the sentence says was refused for also saying more.
fn quote_is_about(sentence: &str, quote: &str) -> bool {
    let q = crate::research::graph::relevance_terms(quote);
    let s = crate::research::graph::relevance_terms(sentence);
    let shorter = q.len().min(s.len());
    shorter > 0 && q.intersection(&s).count() * 100 >= shorter * GROUND_MIN_OVERLAP_PCT
}

/// Share of the shorter text's terms that the quote and the sentence share.
const GROUND_MIN_OVERLAP_PCT: usize = 30;
/// Passages of the source shown to the model for each sentence.
const GROUND_PASSAGES: usize = 4;
/// Shortest quote accepted as evidence, in characters.
const GROUND_MIN_QUOTE_CHARS: usize = 12;

#[derive(serde::Deserialize)]
struct RawGrounding {
    #[serde(default)]
    n: usize,
    #[serde(default)]
    quote: String,
}

fn build_ground_prompt(sentences: &[(usize, &str, Vec<&str>)]) -> String {
    let mut s = String::from(
        "For each numbered SENTENCE below, find the words in its PASSAGES that support it.\n\n\
         Rules:\n\
         - `quote` must be copied from a passage CHARACTER FOR CHARACTER — a clause or a \
           sentence, the shortest span that carries the support. Do not paraphrase, translate \
           or merge passages.\n\
         - Support means the passage states what the sentence states. A passage on the same \
           topic that does not say it is NOT support.\n\
         - If no passage supports a sentence, leave that sentence out of the output.\n\n",
    );
    for (n, sentence, passages) in sentences {
        s.push_str(&format!(
            "=== SENTENCE {n} ===\n{sentence}\n--- PASSAGES ---\n"
        ));
        for p in passages {
            s.push_str(p);
            s.push_str("\n\n");
        }
    }
    s.push_str(
        "Output STRICT JSON, no fence, no commentary — an array, possibly empty:\n\
         [{\"n\": 1, \"quote\": \"…\"}]",
    );
    s
}

/// The archived text a page's citations point at: registry index → URL →
/// archive file.
fn cited_source_text(kref: &KmsRef, body: &str) -> Vec<(u32, String, String)> {
    let reg = crate::research::registry::SourceRegistry::load(kref);
    let cited = cited_indices(body);
    reg.meta()
        .into_iter()
        .filter(|(i, _, _)| cited.contains(i))
        .filter_map(|(i, _, url)| {
            let file = crate::research::kms_writer::url_to_filename(&url);
            let path = crate::kms::source_path(kref, &file).ok()?;
            Some((i, url, std::fs::read_to_string(path).ok()?))
        })
        .collect()
}

/// For every `unsupported` finding, look for its evidence in the source. A
/// sentence whose evidence is found becomes a `grounded` finding and a new
/// claim in the digest cache, so the next audit sees it as supported. One
/// whose evidence is not found stays `unsupported`, now `confirmed` — the
/// model flagged it AND the source has nothing for it — which is what
/// `--fix` acts on.
async fn ground_findings(
    kref: &KmsRef,
    report: &mut VerifyReport,
    provider: Arc<dyn Provider>,
    model: &str,
    timeout: Duration,
    cancel: &crate::cancel::CancelToken,
) {
    let mut by_page: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, f) in report.findings.iter().enumerate() {
        if f.kind == "unsupported" && f.quote.is_some() {
            by_page.entry(f.subject.clone()).or_default().push(i);
        }
    }
    let mut new_claims: BTreeMap<u32, Vec<(String, String, String)>> = BTreeMap::new();
    for (slug, idxs) in by_page {
        let Ok(raw) = std::fs::read_to_string(kref.pages_dir().join(format!("{slug}.md"))) else {
            continue;
        };
        let sources = cited_source_text(kref, &checkable_body(&raw));
        if sources.is_empty() {
            continue;
        }
        let paras: Vec<(u32, &str, Vec<String>)> = sources
            .iter()
            .map(|(i, url, text)| (*i, url.as_str(), passages(text)))
            .collect();
        let all: Vec<String> = paras.iter().flat_map(|(_, _, p)| p.clone()).collect();
        let sentences: Vec<(usize, &str, Vec<&str>)> = idxs
            .iter()
            .enumerate()
            .filter_map(|(n, fi)| {
                let q = report.findings[*fi].quote.as_deref()?;
                let near = nearest_passages(&all, q, GROUND_PASSAGES);
                (!near.is_empty()).then_some((n + 1, q, near))
            })
            .collect();
        // Owned, so the loop below may change the findings the sentences borrow.
        let asked_for: BTreeSet<usize> = sentences.iter().map(|(m, _, _)| *m).collect();
        let mut found: BTreeMap<usize, String> = BTreeMap::new();
        if !sentences.is_empty() {
            let prompt = build_ground_prompt(&sentences);
            if let Ok(reply) = crate::research::llm_calls::oneshot_judgement(
                provider.as_ref(),
                model,
                prompt,
                timeout,
                cancel,
            )
            .await
            {
                let json = crate::research::digest::extract_json(&reply, '[', ']');
                for g in serde_json::from_str::<Vec<RawGrounding>>(json).unwrap_or_default() {
                    found.insert(g.n, g.quote.trim().to_string());
                }
            }
        }
        for (n, fi) in idxs.iter().enumerate() {
            let sentence = report.findings[*fi].quote.clone().unwrap_or_default();
            // The model proposes; the source decides. And when it decides
            // against, the report says at which step — a first real run left
            // fifteen sentences "unsupported" with no way to tell a sentence
            // the source does not back from one the lookup failed to find.
            let asked = asked_for.contains(&(n + 1));
            let mut why_not = "no passage of the source resembles it";
            let evidence = match found.get(&(n + 1)) {
                None => {
                    if asked {
                        why_not = "the model found no supporting words in the nearest passages";
                    }
                    None
                }
                Some(quote) if quote.chars().count() < GROUND_MIN_QUOTE_CHARS => {
                    why_not = "the words offered as support were too few to mean anything";
                    None
                }
                Some(quote) if !quote_is_about(&sentence, quote) => {
                    why_not = "the words offered as support are about something else";
                    None
                }
                Some(quote) => {
                    let hit = sources.iter().find_map(|(idx, url, text)| {
                        let norm = crate::research::digest::normalize_for_match(text);
                        crate::research::digest::quote_check(&norm, quote)
                            .then(|| (*idx, url.clone(), quote.clone()))
                    });
                    if hit.is_none() {
                        why_not = "the words offered as support are not in the source";
                    }
                    hit
                }
            };
            let f = &mut report.findings[*fi];
            match evidence {
                Some((idx, url, quote)) => {
                    f.kind = "grounded";
                    f.confirmed = false;
                    f.detail = format!(
                        "{}\n      · the source says: \"{}\"",
                        clamp(&sentence, 160),
                        clamp(&quote, 160)
                    );
                    new_claims
                        .entry(idx)
                        .or_default()
                        .push((url, sentence, quote));
                }
                None => {
                    f.confirmed = true;
                    f.detail =
                        f.detail
                            .replacen('\n', &format!(" [no support found: {why_not}]\n"), 1);
                }
            }
        }
    }
    record_grounded_claims(kref, new_claims);
}

/// Keep what grounding found, as claims in the digest cache — where the
/// audit reads its claims from, and where `/kms verify` re-checks every
/// quote against the archive. One file per source index, merged across
/// runs; a sentence already recorded is not recorded twice.
fn record_grounded_claims(kref: &KmsRef, found: BTreeMap<u32, Vec<(String, String, String)>>) {
    let dir = kref.root.join(".research").join("digests");
    if found.is_empty() || std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    for (idx, items) in found {
        let path = dir.join(format!("grounded-{idx}.json"));
        let mut digest = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<crate::research::digest::Digest>(&raw).ok())
            .unwrap_or_else(|| crate::research::digest::Digest {
                url: items[0].0.clone(),
                title: "evidence found by /kms verify --ground".into(),
                fetched: crate::usage::today_str(),
                source: idx,
                entities: Vec::new(),
                claims: Vec::new(),
                links_to_known: Vec::new(),
                dropped_claims: 0,
                model: String::new(),
                published: None,
            });
        for (_, sentence, quote) in items {
            if digest.claims.iter().any(|c| c.text == sentence) {
                continue;
            }
            let n = digest.claims.len() + 1;
            digest.claims.push(crate::research::digest::Claim {
                published: None,
                id: format!("g{idx}c{n}"),
                text: sentence,
                quote,
                entities: Vec::new(),
                confidence: 0.8,
                source: idx,
            });
        }
        if let Ok(json) = serde_json::to_string_pretty(&digest) {
            let _ = crate::kms::write_file(&path, json);
        }
    }
}

/// The parts of a note the entailment audit does not hold to a claim: the
/// opening description (the first prose paragraph — allowed to come from
/// ordinary knowledge since 2026-09-19) and link-list lines. Same rules as
/// `research::write::uncited_share`.
fn exempt_text(body: &str) -> String {
    let links = regex::Regex::new(r"\[\[[^\]]*\]\]").expect("static regex");
    let mut out = String::new();
    let mut seen_prose = false;
    for para in body.split("\n\n") {
        let p = para.trim();
        if p.is_empty() || p.starts_with('#') || p.starts_with("---") {
            continue;
        }
        let n = p.chars().filter(|c| !c.is_whitespace()).count();
        let rest = links
            .replace_all(p, "")
            .chars()
            .filter(|c| !c.is_whitespace())
            .count();
        if rest * 2 < n {
            out.push_str(p);
            out.push('\n');
            continue;
        }
        if !seen_prose && !p.starts_with('|') && !p.starts_with('>') {
            seen_prose = true;
            out.push_str(p);
            out.push('\n');
        }
    }
    out
}

/// Where `head` ends once a joining word left hanging at its end is gone.
fn trim_dangling_joiner(head: &str) -> usize {
    const JOINERS: &[&str] = &[
        "และ",
        "แต่",
        "ซึ่ง",
        "โดย",
        "เพราะ",
        "หรือ",
        "and",
        "but",
        "which",
        "while",
        "because",
        "or",
    ];
    let trimmed = head.trim_end_matches([' ', '\t']);
    for j in JOINERS {
        if let Some(rest) = trimmed.strip_suffix(j) {
            // A whole word: what precedes it is a space, a marker, or nothing.
            if rest.is_empty() || rest.ends_with([' ', ')', ']', '\n']) {
                return rest.trim_end_matches([' ', '\t']).len();
            }
        }
    }
    trimmed.len()
}

/// `--llm --fix`: take out what the auditor flagged.
///
/// Only text found in the page EXACTLY is removed — the auditor's quote was
/// accepted on a tolerant match, and a tolerant delete could take the wrong
/// span. A citation marker left dangling by the cut goes with it, and then
/// `prune_uncited` drops any paragraph the cut left with no citation at all.
/// Research-written notes only (`type: note`): a page a person wrote has no
/// claims to be held to. Every page goes through `write_page`, so the
/// version before the cut is in the trash. Returns (pages changed,
/// sentences removed, sentences not found exactly, findings left on a topic
/// page).
fn remove_unsupported(kref: &KmsRef, report: &VerifyReport) -> (u32, u32, u32, u32) {
    let marker = regex::Regex::new(r"^[ \t]*(\[\d+\](\([^)]*\))?[ \t]*)+").expect("static regex");
    let mut by_page: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for f in report
        .of_kind("unsupported")
        .into_iter()
        .filter(|f| f.confirmed)
    {
        if let Some(q) = &f.quote {
            by_page.entry(f.subject.as_str()).or_default().push(q);
        }
    }
    let (mut pages, mut removed, mut missed, mut skipped_topic) = (0u32, 0u32, 0u32, 0u32);
    for (slug, quotes) in by_page {
        let path = kref.pages_dir().join(format!("{slug}.md"));
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (fm, body) = crate::kms::parse_frontmatter(&raw);
        if fm.get("type").map(String::as_str) != Some("note") {
            continue;
        }
        // The topic page is asked to synthesise — compare, rank, explain
        // cause and consequence across claims — and that is exactly what an
        // entailment audit reports. Its findings are worth reading; deleting
        // them would take the argument out of the report and leave a list.
        if fm.get("kind").map(|k| k.trim()) == Some("moc") {
            skipped_topic += quotes.len() as u32;
            continue;
        }
        let (mut text, sources) = match body.find("\n## Sources") {
            Some(i) => (body[..i].to_string(), body[i..].to_string()),
            None => (body.clone(), String::new()),
        };
        let mut cut = 0u32;
        for q in quotes {
            match text.find(q) {
                Some(i) => {
                    let after = &text[i + q.len()..];
                    let tail = marker.find(after).map(|m| m.end()).unwrap_or(0);
                    text.replace_range(i..i + q.len() + tail, "");
                    // An auditor quotes the clause, not the word that joined
                    // it on: a real page was left ending "… [1] และ".
                    let head_end = trim_dangling_joiner(&text[..i]);
                    text.replace_range(head_end..i, "");
                    cut += 1;
                }
                None => missed += 1,
            }
        }
        if cut == 0 {
            continue;
        }
        let (pruned, _) = crate::research::write::prune_uncited(&text);
        // The blank line between the frontmatter and the body is the page's.
        let lead = if body.starts_with('\n') { "\n" } else { "" };
        let rebuilt =
            crate::kms::write_frontmatter(&fm, &format!("{lead}{}\n{sources}", pruned.trim()));
        if crate::kms::write_page(kref, slug, &rebuilt).is_ok() {
            pages += 1;
            removed += cut;
        }
    }
    (pages, removed, missed, skipped_topic)
}

/// The whole report, uncapped, where the next run can be compared with it.
///
/// dev-plan/64 P5.6: the frontmatter carries what the run cost and how
/// much it found, so the Runs panel can show a ledger without opening
/// each file. An audit is the most expensive thing the KMS does on a
/// person's behalf, and until now it was the only run that left no
/// record of its price.
fn save_report(kref: &KmsRef, text: &str, findings: usize) -> Option<std::path::PathBuf> {
    let dir = kref.root.join("runs");
    std::fs::create_dir_all(&dir).ok()?;
    let today = crate::usage::today_str();
    let mut path = dir.join(format!("{today}-verify.md"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{today}-verify-{n}.md"));
        n += 1;
    }
    let body = format!(
        "---\ntype: verify-run\ndate: {today}\nfindings: {findings}\n{}---\n\n```\n{text}\n```\n",
        crate::research::llm_calls::usage_so_far().frontmatter()
    );
    crate::kms::write_file(&path, body).ok()?;
    Some(path)
}

/// Resolve, verify, optionally audit, and format — the whole command
/// in one call so the CLI and GUI handlers stay identical.
pub async fn run(
    kms_name: &str,
    opts: &VerifyOptions,
    provider: Option<Arc<dyn Provider>>,
    model: &str,
    timeout: Duration,
    cancel: &crate::cancel::CancelToken,
) -> Result<String> {
    // The ledger is task-local: opening it here is what lets
    // `save_report` write what the audit cost.
    crate::research::llm_calls::track_usage(run_inner(
        kms_name, opts, provider, model, timeout, cancel,
    ))
    .await
}

async fn run_inner(
    kms_name: &str,
    opts: &VerifyOptions,
    provider: Option<Arc<dyn Provider>>,
    model: &str,
    timeout: Duration,
    cancel: &crate::cancel::CancelToken,
) -> Result<String> {
    let kref = crate::kms::resolve(kms_name)
        .ok_or_else(|| crate::error::Error::Tool(format!("no KMS named '{kms_name}'")))?;
    let mut report = verify(&kref, opts)?;
    let audited = provider.is_some();
    if let Some(p) = provider {
        verify_entailment(&kref, &mut report, p, model, timeout, cancel, opts).await?;
    }
    let mut out = format_report(kms_name, &report, opts, 40);
    if audited && opts.fix {
        let (pages, removed, missed, topic) = remove_unsupported(&kref, &report);
        out.push_str(&format!(
            "\n--fix removed {removed} sentence(s) from {pages} page(s) — those {}. The versions before are in the trash (`/kms trash {}`).",
            if opts.ground {
                "the auditor flagged AND the source has nothing to support"
            } else {
                "both audit passes flagged"
            },
            crate::repl::quote_slash_arg(&kref.name)
        ));
        if topic > 0 {
            out.push_str(&format!(
                " {topic} finding(s) on the topic page were left: synthesis across claims is that page's job — read them, don't delete them."
            ));
        }
        if missed > 0 {
            out.push_str(&format!(
                " {missed} flagged sentence(s) were not found in their page exactly as quoted and were left."
            ));
        }
        out.push('\n');
    }
    if !report.findings.is_empty() {
        let full = format_report(kms_name, &report, opts, usize::MAX);
        if let Some(path) = save_report(&kref, &full, report.findings.len()) {
            out.push_str(&format!("\nFull report: {}\n", path.display()));
        }
    }
    Ok(out)
}

// ── Report ───────────────────────────────────────────────────────────

const SECTIONS: &[(&str, &str)] = &[
    ("repaired", "repaired"),
    ("corrupted_link", "links written inside a URL"),
    ("unresolved_citation", "citations that resolve to nothing"),
    ("missing_archive", "cited sources with no archived copy"),
    ("quote_drift", "claims the archive no longer supports"),
    ("unsupported", "sentences no cited claim supports (LLM)"),
    (
        "grounded",
        "sentences the claims missed and the source does support — evidence recorded",
    ),
    ("audit_failed", "pages the entailment pass could not read"),
    ("frontmatter_drift", "`sources:` disagreeing with the body"),
    ("uncited_assertion", "numbers asserted without a citation"),
    ("stale", "notes nobody has refreshed"),
];

pub fn format_report(
    name: &str,
    report: &VerifyReport,
    opts: &VerifyOptions,
    cap: usize,
) -> String {
    let scope = match &opts.page {
        Some(p) => format!("page `{p}`"),
        None => format!("{} page(s)", report.pages_checked),
    };
    let mut head = format!(
        "KMS '{name}' verify — {scope}, {} claim(s) re-checked against {} archived source(s)",
        report.claims_checked, report.sources_checked
    );
    if report.llm_pages > 0 {
        head.push_str(&format!(
            ", {} page(s) audited by the model",
            report.llm_pages
        ));
    }
    if report.pages_repaired > 0 {
        head.push_str(&format!(", {} page(s) repaired", report.pages_repaired));
    }
    if report.findings.is_empty() {
        return format!(
            "{head}\n\nclean — every citation resolves and every quote still checks out."
        );
    }
    let mut out = format!("{head}\n\n{} finding(s)\n", report.findings.len());
    for (kind, title) in SECTIONS {
        let items = report.of_kind(kind);
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{title} ({}):\n", items.len()));
        for f in items.iter().take(cap) {
            out.push_str(&format!("  - {}: {}\n", f.subject, f.detail));
        }
        if items.len() > cap {
            out.push_str(&format!(
                "  … and {} more — all of them are in the full report.\n",
                items.len() - cap
            ));
        }
    }
    if report.llm_pages == 0 {
        out.push_str(
            "\nThis was the file-only pass. `--llm` adds the one check files cannot make: \
             whether each sentence follows from the claims it cites.\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    const NOTE: &str = "---\ntitle: Moral luck\ntype: note\nclaims: 1\nsources: [1]\n---\n\n# Moral luck\n---\n\nmoral luck คือปัญหาทางปรัชญาจริยศาสตร์ที่ถามว่าการตัดสินควรขึ้นกับผลลัพธ์หรือไม่\n\n## ที่มา\n\nNagel และ Williams เสนอแนวคิดนี้ในปี 1976 [1](../sources/doc.md) เป็นแนวคิดที่ใช้กันในหมู่แพทย์และนักลงทุน [1](../sources/doc.md)\n\n## นัย\n\nผู้สร้างจึงต้องรับผิดชอบต่อสิ่งที่ตนยังมองไม่เห็น [1](../sources/doc.md)\n\nดูเพิ่มเติม: [[itn-framework|ITN Framework]] · [[goodharts-law|Goodhart\'s Law]]\n\n## Sources\n\n1. [doc](../sources/doc.md)\n";

    /// What a note may say without a claim is not a finding: a first real
    /// run flagged opening descriptions and "See also" lines.
    #[test]
    fn the_opening_and_the_link_line_are_not_held_to_a_claim() {
        let body = checkable_body(NOTE);
        let exempt = exempt_text(&body);
        assert!(exempt.contains("moral luck คือปัญหาทางปรัชญา"), "{exempt}");
        assert!(exempt.contains("ดูเพิ่มเติม"), "{exempt}");
        assert!(
            !exempt.contains("Nagel") && !exempt.contains("ผู้สร้างจึง"),
            "{exempt}"
        );
    }

    /// A real run listed two sentences as findings and ended each reason
    /// "…so it is supported". `--fix` would have deleted them.
    #[test]
    fn a_sentence_the_auditor_clears_is_not_a_finding() {
        let item = |verdict: &str, why: &str| RawUnsupported {
            quote: "q".into(),
            why: why.into(),
            verdict: verdict.into(),
        };
        assert!(is_finding(&item(
            "unsupported",
            "no claim carries this causal link"
        )));
        assert!(is_finding(&item(
            "",
            "No listed claim supports the assertion"
        )));
        assert!(is_finding(&item(
            "",
            "the claim is not supported by any source"
        )));
        assert!(!is_finding(&item(
            "supported",
            "a claim says this directly"
        )));
        assert!(!is_finding(&item("", "claim มีข้อความนี้ตรง ๆ จึง supported")));
        assert!(!is_finding(&item("", "Waymo … ซึ่ง supported")));
        assert!(!is_finding(&item(
            "unsupported",
            "on reflection this is supported by claim 4, so it is supported"
        )));
    }

    /// A marker's evidence is the claim nearest *that* sentence, not the
    /// first claim the source happens to make. Three markers, one source,
    /// three different answers — and a Thai filename percent-encoded in
    /// the href still finds its catalogue record.
    #[test]
    fn each_citation_gets_the_claim_nearest_its_own_sentence() {
        let _h = crate::research::test_helpers::scoped_home();
        let k = crate::kms::create("cites", crate::kms::KmsScope::Project).unwrap();
        let href = "../sources/%E0%B8%A2%E0%B8%B8%E0%B8%84.md";
        std::fs::write(
            k.pages_dir().join("moral-luck.md"),
            format!(
                "---\ntitle: Moral luck\nsources: [1]\n---\n\n# Moral luck\n\n\
                 Nagel และ Williams เสนอ moral luck ในปี 1976 [1]({href})\n\n\
                 การใช้ถ่านหินของอังกฤษเพิ่มขึ้นสามเท่าภายในปี 1900 [1]({href})\n\n\
                 ตารางเวลารถไฟฟ้าสายสีม่วงเปิดให้บริการถึงเที่ยงคืน [1]({href})\n"
            ),
        )
        .unwrap();
        let dig = k.root.join(".research").join("digests");
        std::fs::create_dir_all(&dig).unwrap();
        std::fs::write(
            dig.join("d.json"),
            r#"{"url":"https://e.com","title":"ยุค","fetched":"2026-09-20","source":1,"entities":[],"claims":[
              {"id":"s1c1","text":"Nagel และ Williams เสนอ moral luck ในปี 1976","quote":"Nagel and Williams introduced moral luck in 1976","entities":[],"confidence":0.9,"source":1},
              {"id":"s1c2","text":"การใช้ถ่านหินของอังกฤษเพิ่มขึ้นสามเท่าภายในปี 1900","quote":"British coal use tripled by 1900","entities":[],"confidence":0.8,"source":1}
            ]}"#,
        )
        .unwrap();

        let out = citation_evidence(&k, "moral-luck");
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(out.iter().all(|c| c.index == 1 && c.source == "ยุค"));
        assert!(out[0].claim.contains("1976"), "{:?}", out[0]);
        assert_eq!(out[0].confidence, Some(0.9));
        assert!(out[1].claim.contains("ถ่านหิน"), "{:?}", out[1]);
        assert_eq!(out[1].quote, "British coal use tripled by 1900");
        // The third sentence shares only stray bigrams with either
        // claim. "Nothing here is close" has to be an available answer,
        // or every marker gets evidence and none of it means anything.
        assert!(out[2].claim.is_empty(), "{:?}", out[2]);
        assert!(out[2].quote.is_empty() && out[2].confidence.is_none());
        // It is still a citation: the source it names is exact.
        assert_eq!(out[2].source, "ยุค");
    }

    /// How often a real page's citations get evidence, and what the
    /// near-misses look like — the floor in [`CITE_MIN_OVERLAP`] was
    /// picked from this, not by eye. Run against a **copy**:
    ///
    /// ```sh
    /// KMS_BENCH_VAULT=/tmp/vault-copy cargo test --features gui,kms_search_index \
    ///   -- --ignored --nocapture survey_citation_evidence
    /// ```
    #[test]
    #[ignore = "survey: needs KMS_BENCH_VAULT pointing at a copy of a real vault"]
    fn survey_citation_evidence() {
        let Ok(root) = std::env::var("KMS_BENCH_VAULT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let kref = KmsRef {
            name: root
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            root: root.clone(),
            scope: crate::kms::KmsScope::Project,
        };
        // Same load as `citation_evidence`, kept here so the survey can
        // score a marker against a source it does NOT cite.
        let mut by_source: BTreeMap<u32, Vec<crate::research::digest::Claim>> = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(kref.root.join(".research").join("digests")) {
            for e in rd.flatten() {
                let Ok(raw) = std::fs::read_to_string(e.path()) else {
                    continue;
                };
                let Ok(d) = serde_json::from_str::<crate::research::digest::Digest>(&raw) else {
                    continue;
                };
                for c in d.claims {
                    by_source.entry(c.source).or_default().push(c);
                }
            }
        }
        let indices: Vec<u32> = by_source.keys().copied().collect();

        let mut pages = 0usize;
        let mut markers = 0usize;
        // The winning share for the cited source, and for a source the
        // marker does not cite — the floor is chosen off the gap.
        let mut real: Vec<f32> = Vec::new();
        let mut decoy: Vec<f32> = Vec::new();
        let rd = std::fs::read_dir(kref.pages_dir()).expect("pages/");
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            pages += 1;
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (_, body) = crate::kms::parse_frontmatter(&raw);
            let mut prev_end = 0usize;
            for c in linked_citation_re().captures_iter(&body) {
                let Some(m) = c.get(0) else { continue };
                let Ok(n) = c[1].parse::<u32>() else { continue };
                let window = cited_window(&body, m.start(), prev_end);
                prev_end = m.end();
                markers += 1;
                if let Some((share, _)) = by_source
                    .get(&n)
                    .and_then(|cl| scored_nearest_claim(cl, window))
                {
                    real.push(share);
                }
                // The control: the same sentence against a source this
                // marker does not cite. If that passes as often, the
                // floor is measuring Thai bigram frequency, not evidence.
                if let Some(other) = indices.iter().find(|i| **i != n) {
                    if let Some((share, _)) = by_source
                        .get(other)
                        .and_then(|cl| scored_nearest_claim(cl, window))
                    {
                        decoy.push(share);
                    }
                }
            }
        }
        // Can the viewer actually find the quote in the archive it is
        // about to open? The digest verified it against a *normalised*
        // archive, so verbatim is not a given — and a quote that is not
        // found verbatim means the click lands at the top of the file.
        let (mut quoted, mut verbatim, mut normalised) = (0usize, 0usize, 0usize);
        let mut archives: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        let rd = std::fs::read_dir(kref.pages_dir()).expect("pages/");
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            for c in citation_evidence(&kref, &stem) {
                if c.quote.is_empty() {
                    continue;
                }
                quoted += 1;
                let entry = archives.entry(c.source.clone()).or_insert_with(|| {
                    let raw = crate::kms::source_path(&kref, &c.source)
                        .ok()
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .unwrap_or_default();
                    let norm = crate::research::digest::normalize_for_match(&raw);
                    (raw, norm)
                });
                if entry.0.contains(&c.quote) {
                    verbatim += 1;
                }
                if crate::research::digest::quote_check(&entry.1, &c.quote) {
                    normalised += 1;
                }
            }
        }
        println!(
            "\nquotes offered {quoted}\n  found verbatim in the archive: {verbatim} ({:.0}%)\n  found after normalising:       {normalised} ({:.0}%)",
            100.0 * verbatim as f32 / quoted.max(1) as f32,
            100.0 * normalised as f32 / quoted.max(1) as f32,
        );

        let pct = |v: &[f32], floor: f32| {
            100.0 * v.iter().filter(|s| **s >= floor).count() as f32 / markers as f32
        };
        println!("\npages {pages}, linked citations {markers}");
        println!("  floor   cited source   a source it does not cite");
        let mut floor = CITE_MIN_OVERLAP;
        while floor < 0.95 {
            println!(
                "   {floor:.2}        {:>5.0}%                      {:>5.0}%",
                pct(&real, floor),
                pct(&decoy, floor)
            );
            floor += 0.1;
        }
    }

    /// A bare `[1]` renders no anchor, so counting it would slide every
    /// later marker onto the wrong evidence.
    #[test]
    fn an_unlinked_marker_is_not_counted() {
        let _h = crate::research::test_helpers::scoped_home();
        let k = crate::kms::create("bare", crate::kms::KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("p.md"),
            "---\ntitle: P\n---\n\n# P\n\nหนึ่ง [1]\n\nสอง [2](../sources/doc.md)\n",
        )
        .unwrap();
        let out = citation_evidence(&k, "p");
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].index, 2);
        assert_eq!(out[0].source, "doc");
    }

    /// `--llm --fix` removes exactly what was flagged, the marker it left
    /// dangling, and any section that leaves empty — and nothing else. The
    /// page before the cut is recoverable.
    #[test]
    fn fix_removes_what_the_auditor_flagged_and_nothing_else() {
        let _h = crate::research::test_helpers::scoped_home();
        let k = crate::kms::create("verify-fix", crate::kms::KmsScope::Project).unwrap();
        std::fs::write(k.pages_dir().join("moral-luck.md"), NOTE).unwrap();
        std::fs::write(
            k.pages_dir().join("by-hand.md"),
            "---\ntitle: Mine\n---\n\nเขียนเอง ไม่มี claim\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("topic.md"),
            "---\ntitle: Topic\ntype: note\nkind: moc\n---\n\nเปิด\n\nดังนั้นสองข้อนี้รวมกันอธิบายภาพใหญ่ [1]\n",
        )
        .unwrap();
        let flag = |subject: &str, quote: &str| Finding {
            subject: subject.into(),
            kind: "unsupported",
            detail: String::new(),
            quote: Some(quote.into()),
            confirmed: true,
        };
        let report = VerifyReport {
            findings: vec![
                flag("moral-luck", "เป็นแนวคิดที่ใช้กันในหมู่แพทย์และนักลงทุน"),
                flag("moral-luck", "ผู้สร้างจึงต้องรับผิดชอบต่อสิ่งที่ตนยังมองไม่เห็น"),
                flag("moral-luck", "ประโยคที่ไม่มีอยู่ในหน้าแบบนี้เป๊ะ"),
                flag("by-hand", "เขียนเอง ไม่มี claim"),
                flag("topic", "ดังนั้นสองข้อนี้รวมกันอธิบายภาพใหญ่"),
            ],
            ..Default::default()
        };
        assert_eq!(remove_unsupported(&k, &report), (1, 2, 1, 1));

        let after = std::fs::read_to_string(k.pages_dir().join("moral-luck.md")).unwrap();
        assert!(
            after.contains("Nagel และ Williams เสนอแนวคิดนี้ในปี 1976 [1](../sources/doc.md)"),
            "{after}"
        );
        assert!(!after.contains("แพทย์และนักลงทุน"), "{after}");
        assert!(!after.contains("## นัย"), "an emptied section goes: {after}");
        assert!(
            !after.contains("[1](../sources/doc.md) [1]"),
            "no dangling marker: {after}"
        );
        assert!(
            after.contains("moral luck คือปัญหา") && after.contains("ดูเพิ่มเติม"),
            "{after}"
        );
        assert!(after.contains("## Sources\n\n1. [doc]"), "{after}");
        let topic = std::fs::read_to_string(k.pages_dir().join("topic.md")).unwrap();
        assert!(
            topic.contains("ดังนั้นสองข้อนี้รวมกัน"),
            "a topic page's synthesis is reported, never cut"
        );
        let mine = std::fs::read_to_string(k.pages_dir().join("by-hand.md")).unwrap();
        assert!(
            mine.contains("เขียนเอง ไม่มี claim"),
            "a hand-written page is never cut"
        );
        assert_eq!(
            crate::kms_trash::list(&k).len(),
            1,
            "the version before is kept"
        );
    }

    use super::*;
    use crate::kms::{create, write_page, KmsScope};
    use crate::research::test_helpers::scoped_home;

    #[test]
    fn cited_indices_and_uncited_paragraphs() {
        assert_eq!(cited_indices("a [1] b [12] c [1]"), BTreeSet::from([1, 12]));
        let body = "## H2 with 2026\n\nAlibaba shipped 1.6 trillion parameters [3].\n\n\
                    Revenue reached 45 billion baht last year.\n\n\
                    A sentence with no numbers at all.\n\n| table | 2026 |\n";
        let u = uncited_paragraphs(body);
        assert_eq!(u.len(), 1, "{u:?}");
        assert!(u[0].starts_with("Revenue reached"), "{u:?}");
    }

    #[test]
    fn checkable_body_drops_generated_sections() {
        let raw = "---\ntitle: T\n---\n\nlead [1].\n\n## Map\n\n- [[a]] — x\n\n## Detail\n\nmore [1].\n\n## Sources\n\n1. [T](../sources/t.md) — https://t\n";
        let b = checkable_body(raw);
        assert!(b.contains("lead [1]") && b.contains("more [1]"), "{b}");
        assert!(!b.contains("## Map") && !b.contains("## Sources"), "{b}");
    }

    #[test]
    fn verify_flags_citations_frontmatter_and_staleness() {
        let _h = scoped_home();
        let k = create("verify-rt", KmsScope::Project).unwrap();
        write_page(
            &k,
            "note",
            "---\ntitle: \"Note\"\ntype: note\nsources: [1, 9]\nupdated: 2020-01-01\n---\n\n\
             Backed by evidence [1].\n\nThe market grew to 16 billion in 2026.\n",
        )
        .unwrap();
        let report = verify(&k, &VerifyOptions::default()).unwrap();
        let kinds: Vec<&str> = report.findings.iter().map(|f| f.kind).collect();
        assert!(
            kinds.contains(&"frontmatter_drift"),
            "{:?}",
            report.findings
        );
        assert!(
            kinds.contains(&"uncited_assertion"),
            "{:?}",
            report.findings
        );
        assert!(kinds.contains(&"stale"), "{:?}", report.findings);
        // No registry in this KMS, so a `[1]` cannot be called unresolved.
        assert!(
            !kinds.contains(&"unresolved_citation"),
            "{:?}",
            report.findings
        );
        assert_eq!(report.pages_checked, 1);
        assert!(!format_report("verify-rt", &report, &VerifyOptions::default(), 40).is_empty());
    }

    #[test]
    fn verify_rechecks_quotes_against_the_archived_source() {
        let _h = scoped_home();
        let k = create("drift-rt", KmsScope::Project).unwrap();
        write_page(
            &k,
            "n",
            "---\ntitle: \"N\"\ntype: note\nsources: [1]\n---\n\nA fact [1].\n",
        )
        .unwrap();
        let sources = k.root.join("sources");
        std::fs::create_dir_all(&sources).unwrap();
        std::fs::write(sources.join("ex-com-p.md"), "the archive says apples\n").unwrap();
        let digests = k.root.join(".research").join("digests");
        std::fs::create_dir_all(&digests).unwrap();
        std::fs::write(
            digests.join("ex-com-p.json"),
            r#"{"url":"https://ex.com/p","title":"P","fetched":"2026-01-01","source":1,
                "entities":[],"claims":[
                  {"id":"s1c1","text":"about apples","quote":"archive says apples","source":1},
                  {"id":"s1c2","text":"about oranges","quote":"archive says oranges","source":1}],
                "links_to_known":[],"dropped_claims":0,"model":"m"}"#,
        )
        .unwrap();
        let report = verify(&k, &VerifyOptions::default()).unwrap();
        assert_eq!(report.claims_checked, 2);
        assert_eq!(report.sources_checked, 1);
        let drift = report.of_kind("quote_drift");
        assert_eq!(drift.len(), 1, "{:?}", report.findings);
        assert!(drift[0].detail.starts_with("1 of 2"), "{}", drift[0].detail);
        assert!(drift[0].detail.contains("oranges"), "{}", drift[0].detail);
    }

    #[test]
    fn wikilinks_inside_a_url_are_detected_and_unwrapped() {
        let _h = scoped_home();
        let k = create("url-rt", KmsScope::Project).unwrap();
        let damaged = "---\ntitle: \"N\"\ntype: note\n---\n\n\
             prose with a real [[link|Link]].\n\n\
             1. [T](../sources/t.md) — https://[[tracxn]].com/d/[[baichuan|baichuan]]/x\n";
        write_page(&k, "n", damaged).unwrap();
        let report = verify(&k, &VerifyOptions::default()).unwrap();
        assert_eq!(
            report.of_kind("corrupted_link").len(),
            1,
            "{:?}",
            report.findings
        );

        let fixing = VerifyOptions {
            fix: true,
            ..Default::default()
        };
        let report = verify(&k, &fixing).unwrap();
        assert_eq!(report.pages_repaired, 1);
        let on_disk = std::fs::read_to_string(k.pages_dir().join("n.md")).unwrap();
        assert!(
            on_disk.contains("https://tracxn.com/d/baichuan/x"),
            "both wrapped words unwrapped: {on_disk}"
        );
        assert!(
            on_disk.contains("[[link|Link]]"),
            "a genuine wikilink is untouched: {on_disk}"
        );
        // Idempotent: nothing left to repair.
        assert_eq!(verify(&k, &fixing).unwrap().pages_repaired, 0);
    }

    /// The same damage in a `kms://` address — which is what a locally
    /// ingested document's Sources line carries, and which may contain
    /// spaces because a KMS name may. Verify used to look for `https?://`
    /// only, so it reported a clean vault with 25 of these in it.
    #[test]
    fn a_wikilink_inside_a_kms_url_is_detected_and_unwrapped() {
        let _h = scoped_home();
        let k = create("kms-url", KmsScope::Project).unwrap();
        let damaged = "---\ntitle: \"N\"\ntype: note\n---\n\n\
             prose with a real [[link|Link]].\n\n\
             1. [T](../sources/my-doc.md) — kms://Age of Abundance/sources/[[my-doc]]\n";
        write_page(&k, "n", damaged).unwrap();
        assert_eq!(
            verify(&k, &VerifyOptions::default())
                .unwrap()
                .of_kind("corrupted_link")
                .len(),
            1
        );
        let fixing = VerifyOptions {
            fix: true,
            ..Default::default()
        };
        assert_eq!(verify(&k, &fixing).unwrap().pages_repaired, 1);
        let on_disk = std::fs::read_to_string(k.pages_dir().join("n.md")).unwrap();
        assert!(
            on_disk.contains("— kms://Age of Abundance/sources/my-doc\n"),
            "{on_disk}"
        );
        assert!(on_disk.contains("[[link|Link]]"), "{on_disk}");
    }

    #[test]
    fn unwrap_links_in_urls_leaves_ordinary_prose_alone() {
        let (out, n) = unwrap_links_in_urls("see [[a|A]] and https://x.com/plain\n");
        assert_eq!(n, 0);
        assert_eq!(out, "see [[a|A]] and https://x.com/plain\n");
    }

    #[test]
    fn verify_one_page_and_unknown_page() {
        let _h = scoped_home();
        let k = create("one-rt", KmsScope::Project).unwrap();
        write_page(&k, "a", "---\ntitle: A\ntype: note\n---\n\nplain prose.\n").unwrap();
        write_page(&k, "b", "---\ntitle: B\ntype: note\n---\n\nplain prose.\n").unwrap();
        let opts = VerifyOptions {
            page: Some("a".into()),
            ..Default::default()
        };
        assert_eq!(verify(&k, &opts).unwrap().pages_checked, 1);
        let missing = VerifyOptions {
            page: Some("nope".into()),
            ..Default::default()
        };
        assert!(verify(&k, &missing).is_err());
    }

    /// Replies in order, one per call. Enough for one page: the calls for
    /// one page are what these tests are about.
    struct Scripted(
        std::sync::Mutex<std::collections::VecDeque<String>>,
        std::sync::atomic::AtomicU32,
    );

    impl Scripted {
        fn new(replies: &[&str]) -> Arc<Self> {
            Arc::new(Self(
                std::sync::Mutex::new(replies.iter().map(|r| r.to_string()).collect()),
                std::sync::atomic::AtomicU32::new(0),
            ))
        }
        fn calls(&self) -> u32 {
            self.1.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        async fn stream(
            &self,
            _req: crate::providers::StreamRequest,
        ) -> Result<crate::providers::EventStream> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "[]".into());
            let events: Vec<Result<crate::providers::ProviderEvent>> = vec![
                Ok(crate::providers::ProviderEvent::TextDelta(body)),
                Ok(crate::providers::ProviderEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    const AUDITED: &str = "---\ntitle: T\ntype: note\nsources: [1]\n---\n\nเปิดเรื่อง อธิบายหัวข้อ\n\n## ส่วน\n\nข้อเท็จจริงที่มีหลักฐาน [1] ประโยคที่โมเดลแต่งเองทั้งประโยคและยาวพอจะจับคู่ได้ [1] อีกประโยคที่รอบเดียวเท่านั้นที่ติดใจสงสัย [1]\n";

    fn audited_kms(name: &str) -> KmsRef {
        let k = create(name, KmsScope::Project).unwrap();
        std::fs::write(k.pages_dir().join("p.md"), AUDITED).unwrap();
        let digests = k.root.join(".research").join("digests");
        std::fs::create_dir_all(&digests).unwrap();
        std::fs::write(
            digests.join("d.json"),
            r#"{"url":"kms://x/sources/doc","title":"Doc","fetched":"2026-09-20","source":1,"claims":[{"id":"s1c1","text":"ข้อเท็จจริงที่มีหลักฐาน","quote":"ตัวเลข 51% อยู่ในคำพูดของแหล่ง","source":1}]}"#,
        )
        .unwrap();
        k
    }

    async fn audit(k: &KmsRef, p: Arc<Scripted>, fix: bool) -> VerifyReport {
        let mut report = VerifyReport::default();
        let opts = VerifyOptions {
            llm: true,
            fix,
            ..Default::default()
        };
        verify_entailment(
            k,
            &mut report,
            p,
            "mock",
            Duration::from_secs(5),
            &crate::cancel::CancelToken::new(),
            &opts,
        )
        .await
        .unwrap();
        report
    }

    const MADE_UP: &str = r#"[{"quote":"ประโยคที่โมเดลแต่งเองทั้งประโยคและยาวพอจะจับคู่ได้","verdict":"unsupported","why":"no claim"}]"#;

    /// About one reply in six was prose. It is asked again, once; a reply
    /// that says in words it found nothing is a clean page.
    #[tokio::test]
    async fn a_reply_that_is_not_json_is_asked_for_again() {
        let _h = scoped_home();
        let k = audited_kms("audit-retry");
        let p = Scripted::new(&[
            "I'll audit each sentence of the note against the claims.",
            MADE_UP,
        ]);
        let report = audit(&k, p.clone(), false).await;
        assert_eq!(p.calls(), 2, "asked twice");
        assert_eq!(report.of_kind("unsupported").len(), 1);
        assert!(report.of_kind("audit_failed").is_empty());

        let p = Scripted::new(&["No unsupported sentences found. Every sentence is covered."]);
        let report = audit(&k, p.clone(), false).await;
        assert_eq!(p.calls(), 1, "a clean page is not asked again");
        assert!(report.is_clean(), "{:?}", report.findings);

        let p = Scripted::new(&["Let me think.", "Still thinking, in prose."]);
        let report = audit(&k, p.clone(), false).await;
        assert_eq!(report.of_kind("audit_failed").len(), 1);
        assert!(report.of_kind("audit_failed")[0].detail.contains("twice"));
    }

    /// `--fix` deletes, so it asks twice and removes only what both passes
    /// flagged — even when they quote different spans of the sentence.
    #[tokio::test]
    async fn fix_acts_only_on_what_two_passes_agree_on() {
        let _h = scoped_home();
        let k = audited_kms("audit-twice");
        let second = r#"[{"quote":"โมเดลแต่งเองทั้งประโยคและยาวพอจะจับคู่ได้","verdict":"unsupported","why":"no claim"},
                          {"quote":"อีกประโยคที่รอบเดียวเท่านั้นที่ติดใจสงสัย","verdict":"unsupported","why":"hm"}]"#;
        let p = Scripted::new(&[MADE_UP, second]);
        let report = audit(&k, p.clone(), true).await;
        assert_eq!(p.calls(), 2, "two passes");
        let found = report.of_kind("unsupported");
        assert_eq!(found.len(), 2, "the report shows everything: {found:?}");
        assert_eq!(found.iter().filter(|f| f.confirmed).count(), 1);
        assert!(found.iter().any(|f| f.detail.contains("[both passes]")));
        assert!(found.iter().any(|f| f.detail.contains("[one pass only")));

        assert_eq!(remove_unsupported(&k, &report), (1, 1, 0, 0));
        let after = std::fs::read_to_string(k.pages_dir().join("p.md")).unwrap();
        assert!(!after.contains("โมเดลแต่งเอง"), "{after}");
        assert!(
            after.contains("รอบเดียวเท่านั้นที่ติดใจสงสัย"),
            "one pass is not enough: {after}"
        );
        assert!(after.contains("ข้อเท็จจริงที่มีหลักฐาน [1]"), "{after}");

        // One pass unreadable: nothing on that page is confirmed.
        std::fs::write(k.pages_dir().join("p.md"), AUDITED).unwrap();
        let p = Scripted::new(&[MADE_UP, "prose", "more prose"]);
        let report = audit(&k, p, true).await;
        assert!(report.of_kind("unsupported").iter().all(|f| !f.confirmed));
        assert_eq!(remove_unsupported(&k, &report).1, 0);
    }

    /// My first fixture for the tests above was an invalid digest: the audit
    /// made no calls and the report was "clean". A vault with a damaged
    /// digest cache would have read the same way.
    #[tokio::test]
    async fn an_unreadable_digest_is_reported_not_passed_over() {
        let _h = scoped_home();
        let k = audited_kms("audit-bad-digest");
        std::fs::write(
            k.root.join(".research/digests/d.json"),
            r#"{"url":"x","claims":[]}"#,
        )
        .unwrap();
        let p = Scripted::new(&[]);
        let report = audit(&k, p.clone(), false).await;
        assert_eq!(p.calls(), 0);
        assert!(!report.is_clean());
        assert!(report.of_kind("audit_failed")[0]
            .subject
            .ends_with("d.json"));
    }

    /// dev-plan/64, the owner's question: don't only delete what no claim
    /// supports — go back to the source and look. The model proposes a
    /// passage; whether those words are in the source, and about the
    /// sentence, is decided here. What is found is kept as a claim, so the
    /// next audit does not flag it again.
    #[tokio::test]
    async fn grounding_finds_evidence_in_the_source_or_confirms_there_is_none() {
        let _h = scoped_home();
        let k = create("ground", KmsScope::Project).unwrap();
        let src = "---\ntype: source\n---\n\n# เอกสารต้นทาง\n\nImperva Bad Bot Report 2025 พบว่า traffic อัตโนมัติแซงมนุษย์ คิดเป็น 51% ของ web traffic ทั้งหมด ซึ่งเป็นครั้งแรกในรอบสิบปี และรายงานยังแยกประเภทของ bot ไว้หลายกลุ่มเพื่อให้เห็นว่าส่วนใดเป็นอันตราย\n\nอีกย่อหน้าหนึ่งพูดเรื่องเศรษฐศาสตร์ของความสนใจ ซึ่งไม่เกี่ยวกับรายงานนี้เลย และยาวพอที่จะเป็น passage ของตัวเองได้โดยไม่ถูกรวมกับย่อหน้าอื่น ๆ ในเอกสารต้นทางฉบับนี้\n";
        std::fs::create_dir_all(k.sources_dir()).unwrap();
        std::fs::write(k.sources_dir().join("doc.md"), src).unwrap();
        let mut reg = crate::research::registry::SourceRegistry::load(&k);
        let idx = reg.index_for("kms://ground/sources/doc", "Doc");
        reg.save(&k).unwrap();
        let page = format!(
            "---\ntitle: Imperva\ntype: note\nsources: [{idx}]\n---\n\nเปิดเรื่อง\n\n## ตัวเลข\n\ntraffic อัตโนมัติคิดเป็น 51% ของ web traffic ทั้งหมด [{idx}] รายงานนี้ได้รับรางวัลจากสมาคมความปลอดภัยไซเบอร์โลก [{idx}]\n"
        );
        std::fs::write(k.pages_dir().join("imperva.md"), &page).unwrap();
        let digests = k.root.join(".research/digests");
        std::fs::create_dir_all(&digests).unwrap();
        std::fs::write(
            digests.join("d.json"),
            format!(r#"{{"url":"kms://ground/sources/doc","title":"Doc","fetched":"2026-09-20","source":{idx},"claims":[{{"id":"s1c1","text":"Imperva พบ traffic อัตโนมัติแซงมนุษย์","quote":"traffic อัตโนมัติแซงมนุษย์","source":{idx}}}]}}"#),
        )
        .unwrap();

        let audit_reply = r#"[{"quote":"traffic อัตโนมัติคิดเป็น 51% ของ web traffic ทั้งหมด","verdict":"unsupported","why":"no 51% in the claims"},
                              {"quote":"รายงานนี้ได้รับรางวัลจากสมาคมความปลอดภัยไซเบอร์โลก","verdict":"unsupported","why":"no award in the claims"}]"#;
        // Sentence 1: a real span. Sentence 2: the model offers real words
        // that have nothing to do with the sentence — refused.
        let ground_reply = r#"[{"n":1,"quote":"คิดเป็น 51% ของ web traffic ทั้งหมด"},
                               {"n":2,"quote":"อีกย่อหน้าหนึ่งพูดเรื่องเศรษฐศาสตร์ของความสนใจ"}]"#;
        let p = Scripted::new(&[audit_reply, ground_reply]);
        let mut report = VerifyReport::default();
        let opts = VerifyOptions {
            llm: true,
            ground: true,
            fix: true,
            ..Default::default()
        };
        verify_entailment(
            &k,
            &mut report,
            p.clone(),
            "mock",
            Duration::from_secs(5),
            &crate::cancel::CancelToken::new(),
            &opts,
        )
        .await
        .unwrap();
        assert_eq!(p.calls(), 2, "one audit pass, one grounding call");

        let grounded = report.of_kind("grounded");
        assert_eq!(grounded.len(), 1, "{:?}", report.findings);
        assert!(
            grounded[0].detail.contains("คิดเป็น 51%"),
            "{}",
            grounded[0].detail
        );
        assert!(!grounded[0].confirmed, "evidence found: never deleted");
        let left = report.of_kind("unsupported");
        assert_eq!(left.len(), 1);
        assert!(
            left[0].confirmed
                && left[0].detail.contains(
                    "no support found: the words offered as support are about something else"
                )
        );

        // The evidence is a claim now, with its quote, where the audit looks.
        let kept: crate::research::digest::Digest = serde_json::from_str(
            &std::fs::read_to_string(digests.join(format!("grounded-{idx}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.claims.len(), 1);
        assert_eq!(kept.claims[0].quote, "คิดเป็น 51% ของ web traffic ทั้งหมด");
        assert_eq!(kept.url, "kms://ground/sources/doc");

        // --fix takes the one with no evidence, and only that.
        assert_eq!(remove_unsupported(&k, &report), (1, 1, 0, 0));
        let after = std::fs::read_to_string(k.pages_dir().join("imperva.md")).unwrap();
        assert!(after.contains("51% ของ web traffic ทั้งหมด"), "{after}");
        assert!(!after.contains("ได้รับรางวัล"), "{after}");

        // A quote that is not in the source at all is refused too.
        std::fs::write(k.pages_dir().join("imperva.md"), &page).unwrap();
        std::fs::remove_file(digests.join(format!("grounded-{idx}.json"))).unwrap();
        let invented = r#"[{"n":1,"quote":"คิดเป็น 99% ของ web traffic ทั้งหมด"}]"#;
        let p = Scripted::new(&[audit_reply, invented]);
        let mut report = VerifyReport::default();
        verify_entailment(
            &k,
            &mut report,
            p,
            "mock",
            Duration::from_secs(5),
            &crate::cancel::CancelToken::new(),
            &opts,
        )
        .await
        .unwrap();
        assert!(
            report.of_kind("grounded").is_empty(),
            "{:?}",
            report.findings
        );
    }

    /// Found on a real source: the passage that states a version number
    /// ranked fourth by shared-term count and was never shown. Rarity puts
    /// it first. And a quote that says all the sentence says, and more, is
    /// about the sentence.
    #[test]
    fn retrieval_prefers_the_passage_with_the_rare_words() {
        let common = "ระบบ agent ทำงานร่วมกันหลายชั้น และมีการจำกัดความลึกของการเรียกซ้อนเพื่อควบคุมต้นทุนและความผิดพลาดที่ทบต้น ";
        let mut paras: Vec<String> = (0..12)
            .map(|i| format!("{common}{common} ย่อหน้าที่ {i}"))
            .collect();
        paras.push("Claude Code เปิดให้ subagent spawn subagent ใน v2.1.172 (cap ห้าชั้น) ปิด default ใน v2.1.217 แล้วเปิดกลับใน v2.1.219".into());
        let sentence = "โดยมี cap ห้าชั้น แล้วปิดความสามารถนี้ใน v2.1.217 ก่อนจะเปิดอีกครั้งใน v2.1.219 เพื่อควบคุมต้นทุนของระบบ agent หลายชั้น";
        let top = nearest_passages(&paras, sentence, 1);
        assert!(top[0].contains("v2.1.217"), "{}", top[0]);

        let long_quote = "Jones & Tonetti ให้สูตร growth accounting ในรูป harmonic mean โดยค่า σ ที่ calibrate คือ 0.2 ซึ่งหมายความว่างานที่ automate ยากจะกลายเป็นตัวจำกัดการเติบโตของทั้งระบบในระยะยาว";
        assert!(quote_is_about(
            "สูตรอยู่ในรูป harmonic mean โดยค่า σ คือ 0.2",
            long_quote
        ));
        assert!(!quote_is_about(
            "สูตรอยู่ในรูป harmonic mean โดยค่า σ คือ 0.2",
            "อีกย่อหน้าหนึ่งพูดเรื่องเศรษฐศาสตร์ของความสนใจ"
        ));
    }

    /// A real page was left ending "… [1] และ": the auditor quotes the
    /// clause, not the word that joined it on.
    #[test]
    fn a_cut_does_not_leave_its_joining_word_behind() {
        let head = "ระบบสื่อสารข้อมูล [1](../sources/x.md) และ";
        assert_eq!(
            &head[..trim_dangling_joiner(head)],
            "ระบบสื่อสารข้อมูล [1](../sources/x.md)"
        );
        let head = "prices carry information [1] and ";
        assert_eq!(
            &head[..trim_dangling_joiner(head)],
            "prices carry information [1]"
        );
        // Part of a word is not a joiner.
        let head = "เขาไปทะเลและ";
        assert_eq!(trim_dangling_joiner(head), head.len());
        let head = "a brand";
        assert_eq!(trim_dangling_joiner(head), head.len());
    }

    /// The auditor sees the source's own words, not only the digest's
    /// paraphrase — a figure the paraphrase dropped is still evidence.
    #[test]
    fn the_auditor_is_shown_each_claims_quote() {
        let prompt = build_entail_prompt(
            "p",
            "body",
            &[(1, "Imperva พบ traffic อัตโนมัติแซงมนุษย์ — the source's words: \"คิดเป็น 51% ของ web traffic\"".into())],
        );
        assert!(prompt.contains("51%"), "{prompt}");
        assert!(same_sentence(
            "ประโยค ยาว ๆ ที่ ถูก ยก มา ทั้ง ประโยค",
            "ที่ ถูก ยก มา ทั้ง ประโยค"
        ));
        assert!(!same_sentence(
            "ประโยคหนึ่งเรื่องเศรษฐกิจ",
            "อีกเรื่องหนึ่งเกี่ยวกับชีววิทยา"
        ));
    }

    #[test]
    fn the_auditors_own_output_is_quote_checked() {
        let body = "The market grew to 16 billion in 2026 [3].";
        let norm = crate::research::digest::normalize_for_match(body);
        assert!(crate::research::digest::quote_check(
            &norm,
            "grew to 16 billion"
        ));
        assert!(!crate::research::digest::quote_check(
            &norm,
            "grew to 60 billion"
        ));
    }
}
