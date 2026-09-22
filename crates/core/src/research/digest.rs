//! Research v2 step 1: read each source exactly once.
//!
//! A source body (≤ `DIGEST_BODY_CHARS`) becomes a [`Digest`] — the
//! entities it talks about and the claims it makes, each claim anchored
//! to a verbatim quote. The quote is the verification: a claim whose
//! quote is not a substring of the fetched body is dropped before it
//! can be cited (`quote_check`). Digests are cached per URL under
//! `<kms>/.research/digests/` so a re-run on a neighbouring query never
//! pays for the same page twice.

use super::llm_calls::ResearchSource;
use super::pipeline::{ResearchTools, SearchHit};
use crate::cancel::CancelToken;
use crate::error::Result;
use crate::kms::KmsRef;
use crate::providers::Provider;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Body characters handed to the digest prompt. Title + lead + headings
/// carry most of a page's claims; the tail is navigation and comments.
pub const DIGEST_BODY_CHARS: usize = 10_000;
/// Claims kept per source after parsing (the prompt asks for ≤ this).
pub const MAX_CLAIMS_PER_SOURCE: usize = 14;
/// Verbatim quote length the prompt asks for; longer quotes are kept if
/// they still check out, but the parser drops anything past 400 chars
/// (it is no longer a quote, it is the paragraph).
pub const MAX_QUOTE_CHARS: usize = 400;
/// Concurrent LLM digests in one wave — enough to hide latency without
/// tripping provider rate limits.
pub const DIGEST_CONCURRENCY: usize = 12;
/// One slow site must not stall a round: past this a fetch falls back
/// to the search snippet, and a search yields no hits.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(25);
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    /// Publication date of the source (`YYYY-MM-DD`) when the page
    /// states one; drives "prefer newer" at plan/write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    /// `s<source>c<n>` — stable within a run, used as the `[c:ID]`
    /// marker the note writer emits.
    pub id: String,
    pub text: String,
    pub quote: String,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    /// Citation index of the source this claim came from.
    pub source: u32,
}

fn default_confidence() -> f32 {
    0.7
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Digest {
    pub url: String,
    pub title: String,
    pub fetched: String,
    pub source: u32,
    #[serde(default)]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub links_to_known: Vec<String>,
    /// Claims the LLM produced whose quote was not found in the body.
    #[serde(default)]
    pub dropped_claims: u32,
    /// Worker model that produced this digest (diagnostics).
    #[serde(default)]
    pub model: String,
    /// Page publication / last-updated date if the page shows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
}

// ── Prompt ───────────────────────────────────────────────────────────

pub fn build_digest_prompt(
    query: &str,
    src: &ResearchSource,
    known_slugs: &[String],
    language: &str,
) -> String {
    build_digest_prompt_split(query, src, known_slugs, language).joined()
}

/// The digest prompt, shared part first (see [`super::SplitPrompt`]): the
/// instructions, the known slugs and the output contract are the same for
/// every source of a run, and only the source itself differs.
pub fn build_digest_prompt_split(
    query: &str,
    src: &ResearchSource,
    known_slugs: &[String],
    language: &str,
) -> super::SplitPrompt {
    let mut s = format!(
        "Research query: {query}\n{}\n\n\
         You are extracting structured knowledge from ONE web source so it can \
         be filed into a zettelkasten (one note per idea). The source comes in the \
         message. Read it and output what it actually says — nothing from your own \
         knowledge.\n\n",
        super::today_context(),
    );
    if !known_slugs.is_empty() {
        s.push_str("=== Notes that already exist in the knowledge base (slugs) ===\n");
        for k in known_slugs.iter().take(80) {
            s.push_str(&format!("- {k}\n"));
        }
        s.push('\n');
    }
    s.push_str(&format!(
        "Output STRICT JSON, no markdown fence, no commentary:\n\
         {{\n  \
           \"published\": \"YYYY-MM-DD or null — the page's publication or last-updated date if it states one\",\n  \
           \"entities\": [ {{\"name\": \"…\", \"slug\": \"lowercase-ascii-hyphens\", \"kind\": \"person|org|law|product|concept|event|place|other\"}} ],\n  \
           \"claims\": [ {{\"text\": \"one self-contained factual statement (language rule below)\",\n               \
                        \"quote\": \"VERBATIM excerpt from the source that supports it, the SHORTEST distinctive span that pins it down (≤ 60 chars — a clause, not a paragraph), copied exactly\",\n               \
                        \"entities\": [\"slug\", …],\n               \
                        \"confidence\": 0.0-1.0 }} ],\n  \
           \"links_to_known\": [\"existing-slug\", …]\n\
         }}\n\n\
         Rules:\n\
         - At most {} claims; take every load-bearing fact (definitions, numbers, dates, rules, attributions, comparisons, who-did-what) — a rich article should yield close to the cap, a thin one a few. Keep `text` under 20 words.\n\
         - Output compact JSON on one line per claim; no explanations.\n\
         - `quote` MUST be copied character-for-character from the source text in the message. Do not paraphrase, do not translate, do not merge two passages. If you cannot find a verbatim span, omit the claim.\n\
         - Entity slugs: the idea's own name, stable across sources (`labour-protection-act-2541`, `overtime-pay`, `andrej-karpathy`). Reuse an existing slug from the list above when it is the same thing.\n\
         - `links_to_known`: existing slugs this source substantively discusses.\n\
         - If the source is navigation, a listing, or off-topic, output {{\"entities\": [], \"claims\": [], \"links_to_known\": []}}.\n\
         - Claim `text` language: {}  (`quote` is always verbatim from the source, whatever its language.)",
        MAX_CLAIMS_PER_SOURCE,
        super::language_rule(language)
    ));
    let item = format!(
        "=== SOURCE [{}] ===\nTitle: {}\nURL: {}\n\n{}\n\n\
         === END OF SOURCE ===\nOutput the JSON now. Every `quote` is copied \
         character-for-character from the source above; no fence, no commentary.",
        src.index,
        src.title,
        src.url,
        head_chars(&src.body, DIGEST_BODY_CHARS)
    );
    super::SplitPrompt { shared: s, item }
}

// ── Parsing + verification ───────────────────────────────────────────

#[derive(Deserialize)]
struct RawDigest {
    #[serde(default)]
    published: Option<String>,
    #[serde(default)]
    entities: Vec<RawEntity>,
    #[serde(default)]
    claims: Vec<RawClaim>,
    #[serde(default)]
    links_to_known: Vec<String>,
}
#[derive(Deserialize)]
struct RawEntity {
    name: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    kind: String,
}
#[derive(Deserialize)]
struct RawClaim {
    text: String,
    #[serde(default)]
    quote: String,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    confidence: Option<f32>,
}

/// Parse the LLM's JSON and drop every claim whose quote is not in the
/// body. Soft-fails to an empty digest on malformed JSON so one bad
/// source never kills the run.
pub fn parse_digest(raw: &str, src: &ResearchSource, today: &str) -> Digest {
    let stripped = strip_json_fences(raw.trim());
    let parsed: RawDigest = match serde_json::from_str(stripped) {
        Ok(p) => p,
        Err(_) => RawDigest {
            published: None,
            entities: vec![],
            claims: vec![],
            links_to_known: vec![],
        },
    };
    let body_norm = normalize_for_match(&src.body);
    let published = parsed
        .published
        .as_deref()
        .map(str::trim)
        .filter(|d| {
            d.len() >= 4
                && d.chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
        })
        .map(|d| d.chars().take(10).collect::<String>());
    let mut claims = Vec::new();
    let mut dropped = 0u32;
    for (n, c) in parsed.claims.into_iter().enumerate() {
        let text = c.text.trim();
        let quote = c.quote.trim();
        if text.is_empty() || quote.is_empty() || quote.chars().count() > MAX_QUOTE_CHARS {
            dropped += 1;
            continue;
        }
        if !quote_check(&body_norm, quote) {
            dropped += 1;
            continue;
        }
        if claims.len() >= MAX_CLAIMS_PER_SOURCE {
            break;
        }
        claims.push(Claim {
            published: published.clone(),
            id: format!("s{}c{}", src.index, n + 1),
            text: text.to_string(),
            quote: quote.to_string(),
            entities: c
                .entities
                .iter()
                .map(|e| sanitize_slug(e))
                .filter(|e| !e.is_empty())
                .collect(),
            confidence: c.confidence.unwrap_or(0.7).clamp(0.0, 1.0),
            source: src.index,
        });
    }
    let mut entities: Vec<Entity> = parsed
        .entities
        .into_iter()
        .filter_map(|e| {
            let name = e.name.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let slug = if e.slug.trim().is_empty() {
                sanitize_slug(&name)
            } else {
                sanitize_slug(&e.slug)
            };
            if slug.is_empty() {
                return None;
            }
            Some(Entity {
                name,
                slug,
                kind: e.kind.trim().to_ascii_lowercase(),
            })
        })
        .collect();
    entities.sort_by(|a, b| a.slug.cmp(&b.slug));
    entities.dedup_by(|a, b| a.slug == b.slug);
    Digest {
        url: src.url.clone(),
        title: src.title.clone(),
        fetched: today.to_string(),
        source: src.index,
        entities,
        claims,
        links_to_known: parsed
            .links_to_known
            .iter()
            .map(|s| sanitize_slug(s))
            .filter(|s| !s.is_empty())
            .collect(),
        dropped_claims: dropped,
        model: String::new(),
        published,
    }
}

/// Whitespace-insensitive, case-insensitive containment. `body_norm`
/// is the pre-normalised body (normalise once per source, not per
/// claim). Thai has no word boundaries, so stripping whitespace is the
/// only normalisation that matters there; for Latin text it also
/// forgives the LLM re-wrapping a quote.
pub fn quote_check(body_norm: &str, quote: &str) -> bool {
    let q = normalize_for_match(quote);
    !q.is_empty() && body_norm.contains(&q)
}

/// The form two strings are compared in when deciding whether a quote
/// really appears in a source (and whether two claims are the same).
///
/// NFC plus "drop whitespace" looked sufficient and is not, for Thai:
/// - NFC does **not** order a Thai above-vowel and a tone mark — above
///   vowels have combining class 0 — so `เกิ่ย` typed vowel-first and
///   tone-first stay different strings. Keyboards, PDF extraction and OCR
///   produce both, and a claim whose quote disagreed with its source on
///   that was dropped as unverifiable, silently.
/// - U+200B and friends are not `White_Space`. Thai web pages and PDFs are
///   full of zero-width spaces as line-break hints; a model transcribing
///   what it sees leaves them out.
/// - A source that prints `๒๕๖๙` and a quote that says `2569`.
pub fn normalize_for_match(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let mut out = String::with_capacity(s.len());
    // A run of Thai combining marks waiting to be put in one order.
    let mut marks: Vec<char> = Vec::new();
    let flush = |marks: &mut Vec<char>, out: &mut String| {
        marks.sort_by_key(|c| thai_mark_rank(*c));
        out.extend(marks.drain(..));
    };
    for c in s.nfc() {
        if c.is_whitespace()
            || matches!(
                c,
                '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{00AD}' | '\u{FEFF}'
            )
        {
            continue;
        }
        if thai_mark_rank(c) > 0 {
            marks.push(c);
            continue;
        }
        flush(&mut marks, &mut out);
        match c {
            '๐'..='๙' => out.push(char::from(b'0' + (c as u32 - '๐' as u32) as u8)),
            _ => out.extend(c.to_lowercase()),
        }
    }
    flush(&mut marks, &mut out);
    out
}

/// 0 for anything that is not a Thai combining mark; otherwise the
/// position it takes in a run: vowel signs, then tone marks, then the
/// rest. The sort is stable, so marks of one rank keep their order.
pub(crate) fn thai_mark_rank(c: char) -> u8 {
    match c {
        '\u{0E31}' | '\u{0E34}'..='\u{0E3A}' | '\u{0E47}' => 1,
        '\u{0E48}'..='\u{0E4B}' => 2,
        '\u{0E4C}'..='\u{0E4E}' => 3,
        _ => 0,
    }
}

fn strip_json_fences(raw: &str) -> &str {
    extract_json(raw, '{', '}')
}

/// The outermost `open … close` span of an LLM reply: tolerates code
/// fences, a chatty preamble, and trailing commentary.
pub fn extract_json(raw: &str, open: char, close: char) -> &str {
    let t = raw.trim();
    let start = t.find(open);
    let end = t.rfind(close);
    match (start, end) {
        (Some(s), Some(e)) if e > s => &t[s..=e],
        _ => t,
    }
}

/// Kebab-case a name into something usable as a KMS page name and as a
/// wikilink target.
///
/// Non-ASCII letters are kept, not dropped. Keeping only ASCII turned any
/// name written entirely in Thai (or CJK, Arabic, …) into the empty
/// string, and empty is the one page name the KMS refuses — so ingesting
/// a Thai document as atomic notes died on `invalid page name ''`, and
/// every Thai entity was silently filtered out of the graph before that.
/// `kms::sanitize_alias` had the same bug and was fixed the same way; the
/// KMS has never required ASCII. Thai in particular cannot be filtered
/// character-class by character-class. Rust counts Thai vowel signs as
/// alphabetic but not the tone marks (U+0E48–0E4B) or thanthakhat
/// (U+0E4C), so a filter on `is_alphabetic()` / `is_alphanumeric()` does
/// not blank a Thai word — it quietly turns it into a different one:
/// `ก้าวหน้า` becomes `กาวหนา`, and `ก้าว` ("step") now equals `กาว`
/// ("glue").
pub fn sanitize_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = true;
    for c in raw.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || (!c.is_ascii() && !c.is_whitespace() && !c.is_control()) {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.chars().take(64).collect()
}

fn head_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

// ── Cache ────────────────────────────────────────────────────────────

fn cache_dir(kref: &KmsRef) -> PathBuf {
    kref.root.join(".research").join("digests")
}

/// Where a digest of `url` is cached.
///
/// The readable part is the archive name; the key is a hash of the
/// **whole** URL. The archive name alone was the key before, and it is
/// lossy on purpose — a local document's five `#part-N` windows all map
/// to its one archive, and a Thai URL path has nothing ASCII to keep — so
/// five windows raced onto one file (the survivor held 13 of a run's 67
/// claims) and every Thai Wikipedia article shared `th-wikipedia-org-wiki`.
pub fn cache_path(kref: &KmsRef, url: &str) -> PathBuf {
    cache_dir(kref).join(format!(
        "{}-{}.json",
        super::kms_writer::url_to_filename(url),
        super::kms_writer::short_hash(url)
    ))
}

/// The pre-hash location, read so caches written by older builds still hit.
fn legacy_cache_path(kref: &KmsRef, url: &str) -> PathBuf {
    cache_dir(kref).join(format!("{}.json", super::kms_writer::url_to_filename(url)))
}

/// A cached digest of exactly this URL.
///
/// The `d.url == url` check is not belt-and-braces: a legacy file is
/// shared by every URL that collapsed to its name, and serving it
/// unchecked attributed one article's claims to another with nothing to
/// show for it. A mismatch is a miss.
pub fn load_cached(kref: &KmsRef, url: &str) -> Option<Digest> {
    [cache_path(kref, url), legacy_cache_path(kref, url)]
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|raw| serde_json::from_str::<Digest>(&raw).ok())
        .find(|d| d.url == url)
}

pub fn store_cached(kref: &KmsRef, d: &Digest) -> Result<()> {
    let dir = cache_dir(kref);
    std::fs::create_dir_all(&dir)
        .map_err(|e| crate::error::Error::Tool(format!("create {}: {e}", dir.display())))?;
    let path = cache_path(kref, &d.url);
    crate::kms::write_file(&path, serde_json::to_string_pretty(d).unwrap_or_default())
        .map_err(|e| crate::error::Error::Tool(format!("write {}: {e}", path.display())))
}

// ── Parallel I/O helpers ─────────────────────────────────────────────

/// Run every search concurrently; a failed query yields no hits rather
/// than failing the round.
pub async fn search_many(
    tools: &Arc<dyn ResearchTools>,
    queries: &[String],
    max_results: u32,
) -> Vec<Vec<SearchHit>> {
    let futs = queries.iter().map(|q| {
        let tools = tools.clone();
        let q = q.clone();
        async move {
            match tokio::time::timeout(SEARCH_TIMEOUT, tools.search(&q, max_results)).await {
                Ok(Ok(hits)) => hits,
                _ => Vec::new(),
            }
        }
    });
    futures::future::join_all(futs).await
}

/// Like `search_many`, but a query that asks for what is current (names
/// the current year, or says latest / newest / new release) goes through
/// the freshness-filtered `search_recent`.
pub async fn search_many_mixed(
    tools: &Arc<dyn ResearchTools>,
    queries: &[String],
    max_results: u32,
    year: &str,
) -> Vec<Vec<SearchHit>> {
    let futs = queries.iter().map(|q| {
        let tools = tools.clone();
        let q = q.clone();
        let recent = wants_recent(&q, year);
        async move {
            let fut = async {
                if recent {
                    tools.search_recent(&q, max_results).await
                } else {
                    tools.search(&q, max_results).await
                }
            };
            match tokio::time::timeout(SEARCH_TIMEOUT, fut).await {
                Ok(Ok(hits)) => hits,
                _ => Vec::new(),
            }
        }
    });
    futures::future::join_all(futs).await
}

pub fn wants_recent(query: &str, year: &str) -> bool {
    let q = query.to_lowercase();
    (!year.is_empty() && q.contains(year))
        || [
            "latest",
            "newest",
            "new release",
            "released",
            "ล่าสุด",
            "ใหม่ล่าสุด",
        ]
        .iter()
        .any(|k| q.contains(k))
}

/// Fetch every URL concurrently (bounded); a failed fetch falls back to
/// the search snippet so the source still exists with *some* body.
pub async fn fetch_many(tools: &Arc<dyn ResearchTools>, hits: Vec<SearchHit>) -> Vec<SearchHit> {
    let sem = Arc::new(tokio::sync::Semaphore::new(DIGEST_CONCURRENCY));
    let futs = hits.into_iter().map(|hit| {
        let tools = tools.clone();
        let sem = sem.clone();
        async move {
            let _p = sem.acquire().await;
            let body = match tokio::time::timeout(FETCH_TIMEOUT, tools.fetch(&hit.url)).await {
                Ok(Ok(b)) if !b.trim().is_empty() => b,
                // Silent until now: a refusal fell back to the search
                // snippet with no trace, so a whole source could be
                // digested — and quote-checked — against one paragraph
                // while the run looked entirely healthy.
                Ok(Ok(_)) => {
                    eprintln!("[research] fetch returned an empty body: {}", hit.url);
                    hit.snippet.clone()
                }
                Ok(Err(e)) => {
                    eprintln!("[research] fetch failed ({e}): {}", hit.url);
                    hit.snippet.clone()
                }
                Err(_) => {
                    eprintln!(
                        "[research] fetch timed out ({}s): {}",
                        FETCH_TIMEOUT.as_secs(),
                        hit.url
                    );
                    hit.snippet.clone()
                }
            };
            SearchHit {
                title: hit.title,
                url: hit.url,
                snippet: body,
            }
        }
    });
    futures::future::join_all(futs).await
}

pub struct DigestBatch<'a> {
    pub provider: Arc<dyn Provider>,
    pub model: &'a str,
    pub query: &'a str,
    pub sources: &'a [ResearchSource],
    pub known_slugs: &'a [String],
    pub kref: &'a KmsRef,
    pub today: &'a str,
    pub timeout: Duration,
    pub cancel: &'a CancelToken,
    pub language: &'a str,
}

/// Digest every source concurrently (bounded), consulting the cache
/// first. Returns digests in the same order as `sources`. A cache hit
/// is re-stamped with the source's current citation index so `[c:ID]`
/// markers stay consistent within this run.
pub async fn digest_many(context: DigestBatch<'_>) -> Vec<Digest> {
    let DigestBatch {
        provider,
        model,
        query,
        sources,
        known_slugs,
        kref,
        today,
        timeout,
        cancel,
        language,
    } = context;
    let sem = Arc::new(tokio::sync::Semaphore::new(DIGEST_CONCURRENCY));
    let known: Arc<Vec<String>> = Arc::new(known_slugs.to_vec());
    let futs = sources.iter().map(|src| {
        let provider = provider.clone();
        let sem = sem.clone();
        let known = known.clone();
        let model = model.to_string();
        let query = query.to_string();
        let src = src.clone();
        let kref = kref.clone();
        let today = today.to_string();
        let cancel = cancel.clone();
        let language = language.to_string();
        async move {
            if let Some(mut d) = load_cached(&kref, &src.url) {
                rebase_claim_ids(&mut d, src.index);
                return d;
            }
            let waited = std::time::Instant::now();
            let _p = sem.acquire().await;
            let prompt = build_digest_prompt_split(&query, &src, &known, &language);
            let prompt_chars = prompt.shared.chars().count() + prompt.item.chars().count();
            let t = std::time::Instant::now();
            let d = match super::llm_calls::oneshot_split(
                provider.as_ref(),
                &model,
                prompt,
                timeout,
                &cancel,
                super::llm_calls::CallKind::Mechanical,
            )
            .await
            {
                Ok(raw) => {
                    eprintln!(
                        "[research] digest [{}] {}: {} chars in → {} chars out, {:.1}s (queued {:.1}s, model {})",
                        src.index,
                        src.url.chars().take(60).collect::<String>(),
                        prompt_chars,
                        raw.chars().count(),
                        t.elapsed().as_secs_f32(),
                        waited.elapsed().as_secs_f32() - t.elapsed().as_secs_f32(),
                        model
                    );
                    parse_digest(&raw, &src, &today)
                }
                Err(e) => {
                    eprintln!("[research] digest failed for {}: {e}", src.url);
                    parse_digest("{}", &src, &today)
                }
            };
            let mut d = d;
            d.model = model.clone();
            // Cache even an empty digest: a navigation page stays empty
            // next run and should not be paid for twice.
            let _ = store_cached(&kref, &d);
            d
        }
    });
    futures::future::join_all(futs).await
}

fn rebase_claim_ids(d: &mut Digest, index: u32) {
    d.source = index;
    for (n, c) in d.claims.iter_mut().enumerate() {
        c.id = format!("s{index}c{}", n + 1);
        c.source = index;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_digest_of_a_run_shares_one_opening() {
        let src = |i: u32, body: &str| ResearchSource {
            index: i,
            title: format!("T{i}"),
            url: format!("https://s{i}"),
            body: body.into(),
        };
        let known = vec!["alpha".to_string()];
        let a = build_digest_prompt_split("q", &src(1, "first body"), &known, "th");
        let b = build_digest_prompt_split("q", &src(2, "second body"), &known, "th");
        assert_eq!(a.shared, b.shared);
        assert!(a.shared.contains("Output STRICT JSON") && a.shared.contains("- alpha"));
        assert!(!a.shared.contains("first body"));
        assert!(a.item.contains("first body") && a.item.contains("URL: https://s1"));
    }

    use super::*;

    fn src(body: &str) -> ResearchSource {
        ResearchSource {
            index: 3,
            title: "T".into(),
            url: "https://ex.ample/p".into(),
            body: body.into(),
        }
    }

    #[test]
    fn claims_without_a_verbatim_quote_are_dropped() {
        let s = src("ค่าล่วงเวลาไม่น้อยกว่า หนึ่งเท่าครึ่ง ของอัตราค่าจ้างต่อชั่วโมง. Overtime is paid at 1.5x.");
        let raw = r#"{"entities":[{"name":"Overtime pay","slug":"Overtime Pay","kind":"concept"}],
          "claims":[
            {"text":"OT ≥ 1.5× hourly wage","quote":"ค่าล่วงเวลาไม่น้อยกว่าหนึ่งเท่าครึ่งของอัตราค่าจ้างต่อชั่วโมง","entities":["overtime-pay"],"confidence":0.9},
            {"text":"Paraphrased and wrong","quote":"overtime is paid at 2x","entities":[]},
            {"text":"Re-wrapped latin","quote":"Overtime is\n paid at 1.5x.","entities":[]}
          ],"links_to_known":["Minimum Wage 2568"]}"#;
        let d = parse_digest(raw, &s, "2026-09-06");
        assert_eq!(d.claims.len(), 2, "{d:?}");
        assert_eq!(d.claims[0].id, "s3c1");
        assert_eq!(d.claims[1].id, "s3c3");
        assert_eq!(d.dropped_claims, 1);
        assert_eq!(d.entities[0].slug, "overtime-pay");
        assert_eq!(d.links_to_known, vec!["minimum-wage-2568"]);
    }

    #[test]
    fn malformed_json_yields_empty_digest_not_error() {
        let d = parse_digest("not json at all", &src("body"), "2026-09-06");
        assert!(d.claims.is_empty() && d.entities.is_empty());
    }

    #[test]
    fn fenced_json_is_accepted() {
        let d = parse_digest(
            "```json\n{\"claims\":[{\"text\":\"a\",\"quote\":\"body\"}]}\n```",
            &src("the body text"),
            "2026-09-06",
        );
        assert_eq!(d.claims.len(), 1);
    }

    #[test]
    fn published_date_is_parsed_and_stamped_on_claims() {
        let d = parse_digest(
            r#"{"published":"2026-08-28T10:00:00Z","claims":[{"text":"a","quote":"body"}]}"#,
            &src("the body"),
            "2026-09-07",
        );
        assert_eq!(d.published.as_deref(), Some("2026-08-28"));
        assert_eq!(d.claims[0].published.as_deref(), Some("2026-08-28"));
        let d = parse_digest(
            r#"{"published":"unknown","claims":[]}"#,
            &src("x"),
            "2026-09-07",
        );
        assert_eq!(d.published, None);
    }

    #[test]
    fn recency_detection() {
        assert!(wants_recent("DeepSeek newest model release 2026", "2026"));
        assert!(wants_recent("Qwen latest version", "2026"));
        assert!(!wants_recent("Alibaba AI Labs founding 2017", "2026"));
    }

    /// A quote that is in the source must verify, however the two copies
    /// happen to be encoded. Each of these used to drop a valid claim,
    /// silently — the only trace was an integer in the run log.
    #[test]
    fn a_thai_quote_verifies_across_encodings() {
        // Same syllable, above-vowel and tone mark typed in either order.
        // NFC leaves these different: the vowel is combining class 0.
        let vowel_first = "เก\u{0E34}\u{0E48}ย";
        let tone_first = "เก\u{0E48}\u{0E34}ย";
        assert_ne!(vowel_first, tone_first);
        assert_eq!(
            normalize_for_match(vowel_first),
            normalize_for_match(tone_first)
        );

        // Zero-width spaces as line-break hints, as Thai web text has them.
        let body = normalize_for_match("มาตรฐาน\u{200B}การ\u{200B}ครองชีพ ของครัวเรือน");
        assert!(quote_check(&body, "มาตรฐานการครองชีพ"));

        // Thai digits in the source, Arabic in the quote.
        let body = normalize_for_match("พ.ศ. ๒๕๖๙ ค่าจ้างขั้นต่ำ ๔๐๐ บาท");
        assert!(quote_check(&body, "2569"));
        assert!(quote_check(&body, "ค่าจ้างขั้นต่ำ 400 บาท"));

        // And it must not start matching things that are not there.
        assert!(!quote_check(&body, "ค่าจ้างขั้นต่ำ 500 บาท"));
        // Tone marks are kept: step ≠ glue.
        assert_ne!(normalize_for_match("ก้าว"), normalize_for_match("กาว"));
    }

    #[test]
    fn slug_sanitizer() {
        assert_eq!(sanitize_slug("  Andrej Karpathy! "), "andrej-karpathy");
        assert_eq!(sanitize_slug("---"), "");

        // A Thai name keeps its letters. This used to return "2541" —
        // the ASCII digits and nothing else — which is why a Thai
        // document produced no entities and no topic page.
        assert_eq!(
            sanitize_slug("พ.ร.บ. คุ้มครองแรงงาน 2541"),
            "พ-ร-บ-คุ้มครองแรงงาน-2541"
        );
        assert_eq!(sanitize_slug("ยุคที่ความฉลาดล้นเหลือ"), "ยุคที่ความฉลาดล้นเหลือ");
        assert_eq!(sanitize_slug("日本語"), "日本語");

        // Tone marks are not `is_alphabetic()` (vowel signs are), so a
        // filter written on it respells the word instead of dropping it:
        // "ก้าวหน้า" would come back as "กาวหนา". Both must survive whole.
        assert_eq!(sanitize_slug("ก้าวหน้า"), "ก้าวหน้า");
        assert_eq!(sanitize_slug("ยุค"), "ยุค");
        assert_ne!(sanitize_slug("ก้าว"), sanitize_slug("กาว"), "step ≠ glue");
    }

    #[test]
    fn cache_round_trip() {
        let _h = super::super::test_helpers::scoped_home();
        let kref = crate::kms::create("cache-rt", crate::kms::KmsScope::Project).unwrap();
        let d = parse_digest(
            r#"{"claims":[{"text":"a","quote":"hello"}]}"#,
            &src("say hello"),
            "2026-09-06",
        );
        store_cached(&kref, &d).unwrap();
        let back = load_cached(&kref, "https://ex.ample/p").unwrap();
        assert_eq!(back, d);
        assert!(load_cached(&kref, "https://ex.ample/other").is_none());
    }

    /// Each window of a local document has its own cache entry. They
    /// used to share one: a 5-window ingest left a single file holding
    /// window 3's 13 claims out of the run's 67, and a re-ingest replayed
    /// that one window five times while reporting nothing was cached.
    #[test]
    fn every_window_of_a_document_is_cached_separately() {
        let _h = super::super::test_helpers::scoped_home();
        let kref = crate::kms::create("cache-win", crate::kms::KmsScope::Project).unwrap();
        let window = |i: u32| {
            let mut s = src("say hello");
            s.url = format!("kms://cache-win/sources/ยุคที่ความฉลาดล้นเหลือ#part-{i}-9814");
            s.index = 1;
            parse_digest(
                &format!(r#"{{"claims":[{{"text":"window {i}","quote":"hello"}}]}}"#),
                &s,
                "2026-09-19",
            )
        };
        for i in 1..=5 {
            store_cached(&kref, &window(i)).unwrap();
        }
        for i in 1..=5 {
            let url = format!("kms://cache-win/sources/ยุคที่ความฉลาดล้นเหลือ#part-{i}-9814");
            let d = load_cached(&kref, &url).expect("each window hits its own entry");
            assert_eq!(d.claims[0].text, format!("window {i}"));
        }
    }

    /// A cache file written before the key carried a hash is shared by
    /// every URL that collapsed to its name. It may only be served to the
    /// URL it was made from.
    #[test]
    fn a_legacy_cache_file_is_only_served_to_its_own_url() {
        let _h = super::super::test_helpers::scoped_home();
        let kref = crate::kms::create("cache-old", crate::kms::KmsScope::Project).unwrap();
        // Exactly what is on disk in a vault ingested by an older build:
        // one `<alias>.json`, holding whichever window won the race.
        let part = |i: u32| format!("kms://cache-old/sources/my-doc#part-{i}-9814");
        let mut s = src("say hello");
        s.url = part(3);
        let survivor = parse_digest(
            r#"{"claims":[{"text":"window 3","quote":"hello"}]}"#,
            &s,
            "2026-09-19",
        );
        std::fs::create_dir_all(cache_dir(&kref)).unwrap();
        let legacy = legacy_cache_path(&kref, &part(3));
        assert_eq!(
            legacy,
            legacy_cache_path(&kref, &part(1)),
            "the shared name"
        );
        std::fs::write(&legacy, serde_json::to_string(&survivor).unwrap()).unwrap();

        assert!(
            load_cached(&kref, &part(3)).is_some(),
            "its own URL still hits"
        );
        assert!(
            load_cached(&kref, &part(1)).is_none(),
            "window 1 must not be served window 3's claims"
        );
    }
}
