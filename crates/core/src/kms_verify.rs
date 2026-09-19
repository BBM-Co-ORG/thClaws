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
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            stale_days: DEFAULT_STALE_DAYS,
            page: None,
            llm: false,
            fix: false,
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
        self.findings.is_empty()
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
            raw.trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter_map(|t| t.trim().trim_matches('"').parse::<u32>().ok())
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
        regex::Regex::new(r"\[[^\]\n]*\]\([^)\n]*\)|<?https?://[^\s)>\]]+>?").expect("static regex")
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
        regex::Regex::new(r"(https?://[^\s)>\]]*?)\[\[([^\]\n|]+)(?:\|([^\]\n]*))?\]\]")
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
                        });
                    }
                    Err(e) => report.findings.push(Finding {
                        subject: stem.clone(),
                        kind: "corrupted_link",
                        detail: format!("{broken} wikilink(s) inside a URL; repair failed: {e}"),
                    }),
                }
            } else {
                report.findings.push(Finding {
                    subject: stem.clone(),
                    kind: "corrupted_link",
                    detail: format!(
                        "{broken} wikilink(s) written inside a URL — the address resolves nowhere. `--fix` unwraps them."
                    ),
                });
            }
        }

        if let Some(days) = fm.get("updated").and_then(|u| days_since(u)) {
            if days > opts.stale_days {
                report.findings.push(Finding {
                    subject: stem.clone(),
                    kind: "stale",
                    detail: format!("not refreshed for {days} days"),
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
         [{\"quote\": \"…\", \"why\": \"…\"}]",
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
    let mut claims_by_source: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(kref.root.join(".research").join("digests")) {
        for entry in rd.flatten() {
            let Ok(raw) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(d) = serde_json::from_str::<crate::research::digest::Digest>(&raw) else {
                continue;
            };
            for c in d.claims {
                claims_by_source.entry(c.source).or_default().push(c.text);
            }
        }
    }
    if claims_by_source.is_empty() {
        return Ok(());
    }

    type VerificationJob = (String, String, Vec<(u32, String)>);
    let mut jobs: Vec<VerificationJob> = Vec::new();
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

    let sem = Arc::new(tokio::sync::Semaphore::new(LLM_CONCURRENCY));
    let futs = jobs.iter().map(|(slug, body, claims)| {
        let provider = provider.clone();
        let sem = sem.clone();
        let model = model.to_string();
        let cancel = cancel.clone();
        let prompt = build_entail_prompt(slug, body, claims);
        async move {
            let _p = sem.acquire().await;
            crate::research::llm_calls::oneshot_pub(
                provider.as_ref(),
                &model,
                prompt,
                timeout,
                &cancel,
            )
            .await
        }
    });
    let replies = futures::future::join_all(futs).await;

    for ((slug, body, _), reply) in jobs.iter().zip(replies) {
        let raw = match reply {
            Ok(r) => r,
            Err(e) => {
                report.findings.push(Finding {
                    subject: slug.clone(),
                    kind: "audit_failed",
                    detail: format!("entailment check did not run: {e}"),
                });
                continue;
            }
        };
        let json = crate::research::digest::extract_json(&raw, '[', ']');
        let Ok(items) = serde_json::from_str::<Vec<RawUnsupported>>(json) else {
            continue;
        };
        let norm_body = crate::research::digest::normalize_for_match(body);
        for it in items {
            let quote = it.quote.trim();
            if quote.is_empty() {
                continue;
            }
            // The auditor must quote the note, not invent it.
            if !crate::research::digest::quote_check(&norm_body, quote) {
                continue;
            }
            report.findings.push(Finding {
                subject: slug.clone(),
                kind: "unsupported",
                detail: format!(
                    "{}\n      · {}",
                    clamp(quote, 160),
                    clamp(it.why.trim(), 120)
                ),
            });
        }
    }
    Ok(())
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
    let kref = crate::kms::resolve(kms_name)
        .ok_or_else(|| crate::error::Error::Tool(format!("no KMS named '{kms_name}'")))?;
    let mut report = verify(&kref, opts)?;
    if let Some(p) = provider {
        verify_entailment(&kref, &mut report, p, model, timeout, cancel, opts).await?;
    }
    Ok(format_report(kms_name, &report, opts))
}

// ── Report ───────────────────────────────────────────────────────────

const SECTIONS: &[(&str, &str)] = &[
    ("repaired", "repaired"),
    ("corrupted_link", "links written inside a URL"),
    ("unresolved_citation", "citations that resolve to nothing"),
    ("missing_archive", "cited sources with no archived copy"),
    ("quote_drift", "claims the archive no longer supports"),
    ("unsupported", "sentences no cited claim supports (LLM)"),
    ("audit_failed", "pages the entailment pass could not read"),
    ("frontmatter_drift", "`sources:` disagreeing with the body"),
    ("uncited_assertion", "numbers asserted without a citation"),
    ("stale", "notes nobody has refreshed"),
];

pub fn format_report(name: &str, report: &VerifyReport, opts: &VerifyOptions) -> String {
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
    if report.is_clean() {
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
        for f in items.iter().take(40) {
            out.push_str(&format!("  - {}: {}\n", f.subject, f.detail));
        }
        if items.len() > 40 {
            out.push_str(&format!("  … and {} more.\n", items.len() - 40));
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
        assert!(!format_report("verify-rt", &report, &VerifyOptions::default()).is_empty());
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
