//! Research v2 step 1: read each source exactly once.
//!
//! A source body (≤ `DIGEST_BODY_CHARS`) becomes a [`Digest`] — the
//! entities it talks about and the claims it makes, each claim anchored
//! to a verbatim quote. The quote is the verification: a claim whose
//! quote is not a substring of the fetched body is dropped before it
//! can be cited (`quote_check`). Digests are cached per URL under
//! `<kms>/.research/digests/` so a re-run on a neighbouring query never
//! pays for the same page twice.

use super::llm_calls::{oneshot, ResearchSource};
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
    let mut s = String::new();
    s.push_str(&format!(
        "Research query: {query}\n{}\n\n\
         You are extracting structured knowledge from ONE web source so it can \
         be filed into a zettelkasten (one note per idea). Read the source and \
         output what it actually says — nothing from your own knowledge.\n\n\
         === SOURCE [{}] ===\nTitle: {}\nURL: {}\n\n{}\n\n",
        super::today_context(),
        src.index,
        src.title,
        src.url,
        head_chars(&src.body, DIGEST_BODY_CHARS)
    ));
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
         - `quote` MUST be copied character-for-character from the source text above. Do not paraphrase, do not translate, do not merge two passages. If you cannot find a verbatim span, omit the claim.\n\
         - Entity slugs: the idea's own name, stable across sources (`labour-protection-act-2541`, `overtime-pay`, `andrej-karpathy`). Reuse an existing slug from the list above when it is the same thing.\n\
         - `links_to_known`: existing slugs this source substantively discusses.\n\
         - If the source is navigation, a listing, or off-topic, output {{\"entities\": [], \"claims\": [], \"links_to_known\": []}}.\n\
         - Claim `text` language: {}  (`quote` is always verbatim from the source, whatever its language.)",
        MAX_CLAIMS_PER_SOURCE,
        super::language_rule(language)
    ));
    s
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

pub fn normalize_for_match(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
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

pub fn sanitize_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = true;
    for c in raw.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
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

pub fn cache_path(kref: &KmsRef, url: &str) -> PathBuf {
    cache_dir(kref).join(format!("{}.json", super::kms_writer::url_to_filename(url)))
}

pub fn load_cached(kref: &KmsRef, url: &str) -> Option<Digest> {
    let raw = std::fs::read_to_string(cache_path(kref, url)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn store_cached(kref: &KmsRef, d: &Digest) -> Result<()> {
    let dir = cache_dir(kref);
    std::fs::create_dir_all(&dir)
        .map_err(|e| crate::error::Error::Tool(format!("create {}: {e}", dir.display())))?;
    let path = cache_path(kref, &d.url);
    std::fs::write(&path, serde_json::to_string_pretty(d).unwrap_or_default())
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
                Ok(_) => hit.snippet.clone(),
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
            let prompt = build_digest_prompt(&query, &src, &known, &language);
            let prompt_chars = prompt.chars().count();
            let t = std::time::Instant::now();
            let d = match oneshot(provider.as_ref(), &model, prompt, timeout, &cancel).await {
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

    #[test]
    fn slug_sanitizer() {
        assert_eq!(sanitize_slug("  Andrej Karpathy! "), "andrej-karpathy");
        assert_eq!(sanitize_slug("พ.ร.บ. คุ้มครองแรงงาน 2541"), "2541");
        assert_eq!(sanitize_slug("---"), "");
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
}
