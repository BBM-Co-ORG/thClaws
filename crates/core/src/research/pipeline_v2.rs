//! Research v2 orchestration (dev-plan/58). Digest each source once,
//! stop on novelty, plan atomic notes against the existing graph,
//! write them in parallel, MOC last.

use super::digest::{self, Digest};
use super::graph::{self, KnownNote, Novelty};
use super::llm_calls::{self, ResearchSource};
use super::pipeline::{ResearchTools, SearchHit};
use super::plan::{self, Action, NoteKind, NotePlan, Tables};
use super::registry::SourceRegistry;
use super::source_quality;
use super::write::{self, NoteInput, RunLog, WrittenNote};
use super::{kms_writer, manager, JobConfig};
use crate::cancel::CancelToken;
use crate::error::{Error, Result};
use crate::providers::Provider;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

const SEED_RESULTS: u32 = 10;
const SEED_FETCH: usize = 5;
/// Extra pages from the freshness-filtered seed search.
const SEED_FETCH_RECENT: usize = 3;
const HITS_PER_GAP_QUERY: u32 = 5;

pub struct PipelineRequest<'a> {
    pub job_id: &'a str,
    pub query: String,
    pub config: JobConfig,
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub cancel: CancelToken,
    pub tools: Arc<dyn ResearchTools>,
    pub digest: Option<(Arc<dyn Provider>, String)>,
}

pub async fn run_with_tools(context: PipelineRequest<'_>) -> Result<String> {
    let PipelineRequest {
        job_id,
        query,
        config,
        provider,
        model,
        cancel,
        tools,
        digest,
    } = context;
    let mgr = manager();
    let (dprov, dmodel): (Arc<dyn Provider>, String) = match digest {
        Some((p, m)) => (p, m),
        None => (provider.clone(), model.clone()),
    };
    let started = std::time::Instant::now();
    let today = kms_writer::today_str();
    // Search rounds are gated by the time budget; once we hold claims the
    // plan/write phases only honour cancellation — throwing away a run that
    // already read 30 sources because the slow worker crossed a timer is
    // worse than a late result (the failure users actually saw).
    let alive = |phase: &str| -> Result<()> {
        if cancel.is_cancelled() {
            return Err(Error::Tool("research cancelled".into()));
        }
        if mgr.is_over_budget(job_id) {
            return Err(Error::Tool("research time budget exhausted".into()));
        }
        mgr.update_phase(job_id, phase);
        Ok(())
    };
    let phase = |phase: &str| -> Result<()> {
        if cancel.is_cancelled() {
            return Err(Error::Tool("research cancelled".into()));
        }
        mgr.update_phase(job_id, phase);
        Ok(())
    };

    // ── 0. KMS + known graph ────────────────────────────────────────
    alive("resolving knowledge base")?;
    // The query's own slug names the MOC note and the run log; the KMS
    // is either `--kms` or that same slug. Keeping them separate lets a
    // second query into the same KMS get its own MOC instead of
    // overwriting the first one's.
    let topic_slug = match (
        &config.topic_slug,
        &config.local_source,
        &config.refresh_slug,
    ) {
        (Some(fixed), _, _) => super::digest::sanitize_slug(fixed),
        // The ingest stub `pages/<alias>.md` becomes the topic page.
        (None, Some(alias), _) => super::digest::sanitize_slug(alias),
        (None, None, Some(slug)) => format!("refresh-{slug}"),
        (None, None, None) => llm_calls::derive_topic_slug(
            dprov.as_ref(),
            &dmodel,
            &query,
            config.llm_timeout,
            &cancel,
        )
        .await
        .map(|s| super::digest::sanitize_slug(&s))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "research".into()),
    };
    let kms_name = config
        .kms_target
        .clone()
        .unwrap_or_else(|| topic_slug.clone());
    let kref = kms_writer::resolve_or_create(&kms_name)?;
    // Zettelkasten default: the KMS a run writes into becomes the
    // attached (and therefore default) target of the next run.
    match crate::config::ProjectConfig::attach_kms(&kms_name) {
        Ok(true) => eprintln!("[research] attached KMS '{kms_name}' to this project"),
        Ok(false) => {}
        Err(e) => eprintln!("[research] could not attach KMS '{kms_name}': {e}"),
    }
    let mut registry = SourceRegistry::load(&kref);
    let known: Vec<KnownNote> = graph::load_known(&kref);
    if let Some(slug) = &config.refresh_slug {
        if !known.iter().any(|k| &k.slug == slug) {
            return Err(Error::Tool(format!(
                "no note '{slug}' in KMS '{kms_name}' to refresh"
            )));
        }
    }
    let known_slugs: Vec<String> = known.iter().map(|k| k.slug.clone()).collect();

    // ── 1. Seed round ───────────────────────────────────────────────
    alive("round 1: seed search")?;
    let mut sources: Vec<ResearchSource> = Vec::new();
    let mut digests: Vec<Digest> = Vec::new();
    let mut novelty = Novelty::default();
    let mut rounds: Vec<(u32, u32, u32, u32, f32, Vec<String>)> = Vec::new();
    let year: String = today.chars().take(4).collect();
    let recency_query = format!("{query} latest {year}");
    let mut queries_run: Vec<String> = vec![query.clone(), recency_query.clone()];
    let mut cached_hits = 0u32;

    if let Some(alias) = config.local_source.clone() {
        // ── Local document instead of the web ───────────────────────
        alive(&format!("reading {alias}"))?;
        let path = kref.root.join("sources").join(format!("{alias}.md"));
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;
        let (_, body) = crate::kms::parse_frontmatter(&raw);
        let title = local_source_title(&raw, &alias);
        let url = format!("kms://{kms_name}/sources/{alias}");
        let index = registry.index_for(&url, &title);
        let windows = split_windows(&body, digest::DIGEST_BODY_CHARS);
        let n_win = windows.len();
        alive(&format!("digesting {alias} ({n_win} part(s))"))?;
        // Each window is its own digest call; the fragment carries the
        // window's length so an edited document misses the cache.
        let win_sources: Vec<ResearchSource> = windows
            .into_iter()
            .enumerate()
            .map(|(i, w)| ResearchSource {
                index,
                title: if n_win > 1 {
                    format!("{title} (part {}/{n_win})", i + 1)
                } else {
                    title.clone()
                },
                url: format!("{url}#part-{}-{}", i + 1, w.chars().count()),
                body: w,
            })
            .collect();
        let t = std::time::Instant::now();
        let parts = digest::digest_many(digest::DigestBatch {
            provider: dprov.clone(),
            model: &dmodel,
            query: &query,
            sources: &win_sources,
            known_slugs: &known_slugs,
            kref: &kref,
            today: &today,
            timeout: config.llm_timeout,
            cancel: &cancel,
            language: &config.language,
        })
        .await;
        let merged = merge_digests(parts, index, &url, &title, &today);
        let (new, total, ratio) = novelty.absorb(std::slice::from_ref(&merged));
        eprintln!(
            "[research] ingest {alias}: {n_win} part(s), {} claims ({} dropped), {} entities, {:.1}s",
            merged.claims.len(),
            merged.dropped_claims,
            merged.entities.len(),
            t.elapsed().as_secs_f32()
        );
        sources.push(ResearchSource {
            index,
            title: title.clone(),
            url,
            body,
        });
        digests.push(merged);
        rounds.push((1, 1, new, total, ratio, vec![format!("ingest {alias}")]));
        mgr.record_iteration(job_id, 1, 1, Some(ratio));
    } else {
        // Two seed searches: the query as given, and a freshness-filtered
        // "latest" variant so a run in {year} sees this year's releases even
        // when the evergreen ranking is dominated by older pages.
        let (seed_plain, seed_recent) = tokio::join!(
            tokio::time::timeout(digest::SEARCH_TIMEOUT, tools.search(&query, SEED_RESULTS)),
            tokio::time::timeout(
                digest::SEARCH_TIMEOUT,
                tools.search_recent(&recency_query, SEED_RESULTS)
            ),
        );
        let seed_plain = seed_plain.map_err(|_| Error::Tool("seed search timed out".into()))??;
        let seed_recent: Vec<SearchHit> = match seed_recent {
            Ok(Ok(h)) => h,
            _ => Vec::new(),
        };
        // Pool both searches, then let the ranker decide who is read:
        // the subject's own site and reference works first, at most two
        // pages per domain. Engine order breaks ties inside a tier.
        let words = source_quality::subject_words(&query, &[]);
        let mut pool: Vec<SearchHit> = seed_plain;
        for h in seed_recent {
            if !pool.iter().any(|s| same_source(&s.url, &h.url)) {
                pool.push(h);
            }
        }
        let seed: Vec<SearchHit> = source_quality::rank_candidates(pool, &words)
            .into_iter()
            .take(SEED_FETCH + SEED_FETCH_RECENT)
            .collect();
        alive(&format!("round 1: reading {} sources", seed.len()))?;
        let t = std::time::Instant::now();
        let fetched = digest::fetch_many(&tools, seed).await;
        eprintln!(
            "[research] round 1: fetched {} in {:.1}s",
            fetched.len(),
            t.elapsed().as_secs_f32()
        );
        let new_sources = accumulate(&mut sources, fetched, &mut registry);
        let round_digests = digest_round(
            &dprov,
            &dmodel,
            &query,
            &new_sources,
            &known_slugs,
            &kref,
            &today,
            &config,
            &cancel,
            &mut cached_hits,
        )
        .await;
        let (new, total, ratio) = novelty.absorb(&round_digests);
        eprintln!(
            "[research] round 1: {} sources, {} new / {} ({:.0}% novelty), {:.1}s",
            sources.len(),
            new,
            total,
            ratio * 100.0,
            t.elapsed().as_secs_f32()
        );
        digests.extend(round_digests);
        rounds.push((
            1,
            sources.len() as u32,
            new,
            total,
            ratio,
            vec![query.clone(), recency_query.clone()],
        ));
        mgr.record_iteration(job_id, 1, sources.len() as u32, Some(ratio));

        // ── 2. Gap rounds ───────────────────────────────────────────────
        let round1_secs = t.elapsed().as_secs_f32();
        if !new_sources.is_empty() && round1_secs / new_sources.len() as f32 > 20.0 {
            eprintln!(
            "[research] '{dmodel}' is taking {:.0}s per source (calls run serially on this provider); \
             a full run may need 15–25 min. `--worker-model <id>` runs the research steps on another model.",
            round1_secs / new_sources.len() as f32
        );
        }
        for round in 2..=config.max_iter {
            // Soft cap: a new round must fit in what is left. Stop searching
            // and write with what we have instead of failing at the end.
            let spent = started.elapsed();
            if spent > config.time_budget.mul_f32(0.6) {
                eprintln!(
                "[research] {:.0}% of the time budget spent after round {} — skipping further search rounds",
                spent.as_secs_f32() / config.time_budget.as_secs_f32() * 100.0,
                round - 1
            );
                break;
            }
            alive(&format!("round {round}/{}: finding gaps", config.max_iter))?;
            let tables = plan::build_tables(&digests);
            let gaps = plan::gap_queries(plan::GapQueryRequest {
                provider: dprov.as_ref(),
                model: &dmodel,
                query: &query,
                tables: &tables,
                prior: &queries_run,
                n: config.subtopics_per_iter,
                timeout: config.llm_timeout,
                cancel: &cancel,
            })
            .await
            .unwrap_or_default();
            if gaps.is_empty() {
                break;
            }
            alive(&format!(
                "round {round}/{}: searching {} queries",
                config.max_iter,
                gaps.len()
            ))?;
            let hit_lists =
                digest::search_many_mixed(&tools, &gaps, HITS_PER_GAP_QUERY, &year).await;
            queries_run.extend(gaps.iter().cloned());
            // Rank the round's whole pool before taking the budget:
            // picking the top-N of each query separately hands the round
            // to whoever ranks well on every query, which is how one SEO
            // domain used to supply half a run's evidence.
            let names: Vec<String> = tables
                .entities
                .values()
                .map(|(name, _, _, _)| name.clone())
                .collect();
            let words = source_quality::subject_words(&query, &names);
            let budget = (gaps.len() * config.fetch_top_n as usize).max(1);
            let mut pool: Vec<SearchHit> = Vec::new();
            for hits in hit_lists {
                for h in hits {
                    if !h.url.is_empty()
                        && !sources.iter().any(|s| same_source(&s.url, &h.url))
                        && !pool.iter().any(|c| same_source(&c.url, &h.url))
                    {
                        pool.push(h);
                    }
                }
            }
            let candidates: Vec<SearchHit> = source_quality::rank_candidates(pool, &words)
                .into_iter()
                .take(budget)
                .collect();
            if candidates.is_empty() {
                rounds.push((round, sources.len() as u32, 0, 0, 0.0, gaps));
                break;
            }
            alive(&format!(
                "round {round}/{}: reading {} sources",
                config.max_iter,
                candidates.len()
            ))?;
            let t = std::time::Instant::now();
            let fetched = digest::fetch_many(&tools, candidates).await;
            eprintln!(
                "[research] round {round}: fetched {} in {:.1}s",
                fetched.len(),
                t.elapsed().as_secs_f32()
            );
            let new_sources = accumulate(&mut sources, fetched, &mut registry);
            let round_digests = digest_round(
                &dprov,
                &dmodel,
                &query,
                &new_sources,
                &known_slugs,
                &kref,
                &today,
                &config,
                &cancel,
                &mut cached_hits,
            )
            .await;
            let (new, total, ratio) = novelty.absorb(&round_digests);
            eprintln!(
                "[research] round {round}: {} sources, {} new / {} ({:.0}% novelty), {:.1}s",
                sources.len(),
                new,
                total,
                ratio * 100.0,
                t.elapsed().as_secs_f32()
            );
            digests.extend(round_digests);
            rounds.push((round, sources.len() as u32, new, total, ratio, gaps));
            mgr.record_iteration(job_id, round, sources.len() as u32, Some(ratio));
            if round >= config.min_iter && ratio < config.novelty_threshold {
                break;
            }
        }
    }

    // ── 3. Plan ─────────────────────────────────────────────────────
    phase("planning notes")?;
    let t_plan = std::time::Instant::now();
    let tables: Tables = plan::build_tables(&digests);
    let claims_total = tables.claims.len() as u32;
    let claims_dropped: u32 = digests.iter().map(|d| d.dropped_claims).sum();
    if claims_total == 0 {
        return Err(Error::Tool(
            "research found no verifiable claims — nothing to write".into(),
        ));
    }
    let topic_title = kms_writer::title_from_query(&query);
    let plan::PlanOutcome {
        notes: plan_notes,
        warnings: mut plan_warnings,
        raw: plan_raw,
    } = plan::plan_notes(plan::PlanningRequest {
        provider: dprov.as_ref(),
        model: &dmodel,
        query: &query,
        tables: &tables,
        known: &known,
        topic_slug: &topic_slug,
        topic_title: &topic_title,
        max_notes: config.max_notes,
        timeout: config.llm_timeout,
        cancel: &cancel,
        language: &config.language,
        anchor: config.refresh_slug.as_deref(),
    })
    .await?;

    {
        let dir = kref.root.join(".research");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("last-plan.json"), &plan_raw);
    }
    eprintln!(
        "[research] planned {} notes from {} claims in {:.1}s (model {})",
        plan_notes.len(),
        claims_total,
        t_plan.elapsed().as_secs_f32(),
        dmodel
    );

    let mut all_cited: BTreeSet<u32> = BTreeSet::new();
    if config.dry_run {
        let path = write::write_run_log(
            &kref,
            &RunLog {
                query: &query,
                topic_slug: &topic_slug,
                today: &today,
                rounds: &rounds,
                sources_digested: sources.len() as u32 - cached_hits,
                sources_cached: cached_hits,
                claims_total,
                claims_dropped,
                notes: &[],
                dry_run_plan: Some(&plan_notes),
                elapsed_secs: started.elapsed().as_secs(),
                worker_model: &dmodel,
                warnings: &plan_warnings,
                sources: &sources,
                cited: &all_cited,
            },
        )?;
        return Ok(format!(
            "{kms_name}/runs/{}",
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("run.md")
        ));
    }

    // ── 4. Write notes (parallel), MOC last ─────────────────────────
    let claim_by_id: HashMap<String, &digest::Claim> =
        tables.claims.iter().map(|c| (c.id.clone(), c)).collect();
    let claim_source: HashMap<String, u32> = tables
        .claims
        .iter()
        .map(|c| (c.id.clone(), c.source))
        .collect();
    let targets = write::link_targets(&plan_notes, &known);
    // Citation metadata from the registry, not just this run's sources:
    // an updated note keeps `[N]` markers from earlier runs and its
    // Sources block must still resolve them.
    registry.save(&kref)?;
    let sources_meta: Vec<(u32, String, String)> = registry.meta();
    let (moc_plans, atomic): (Vec<&NotePlan>, Vec<&NotePlan>) =
        plan_notes.iter().partition(|n| n.kind == NoteKind::Moc);

    // Topic page FIRST: it describes the subject and decides what the
    // children are for; each child then gets its opening as context.
    let mut written: Vec<WrittenNote> = Vec::new();
    let mut parent_overview: Option<String> = None;
    for n in moc_plans {
        phase("writing topic page")?;
        let claims: Vec<&digest::Claim> = n
            .claim_ids
            .iter()
            .filter_map(|id| claim_by_id.get(id).copied())
            .collect();
        let existing = if n.action == Action::Update {
            write::read_existing_body(&kref, &n.slug)
        } else {
            None
        };
        let inp = NoteInput {
            query: &query,
            note: n,
            claims,
            sources: &sources,
            link_targets: &targets,
            existing_body: existing,
            append: false,
            language: &config.language,
            parent_overview: None,
        };
        let body = match write::write_note_body(
            dprov.as_ref(),
            &dmodel,
            &inp,
            config.llm_timeout,
            &cancel,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                // The children are independent of this call and the run
                // has already paid for every source. Record it and keep
                // writing; the result path falls back to a child note.
                let w = format!("topic page `{}` was not written: {e}", n.slug);
                eprintln!("[research] {w}");
                plan_warnings.push(w);
                continue;
            }
        };
        let (rewritten, cited) = write::rewrite_claim_markers(&body, &claim_source);
        let rewritten = write::fix_wikilinks(&rewritten, &targets);
        let rewritten = write::autolink(
            &rewritten,
            &n.slug,
            &targets,
            &n.related,
            None,
            &config.language,
        );
        let rewritten = write::unbold_links(&rewritten);
        parent_overview = Some(opening_of(&rewritten, write::PARENT_OVERVIEW_CHARS));
        let conf = mean_confidence(n, &claim_by_id);
        let w = write::persist_note(write::NoteToPersist {
            kref: &kref,
            note: n,
            body: &rewritten,
            cited: &cited,
            claim_count: n.claim_ids.len(),
            confidence: conf,
            today: &today,
            append: false,
            sources_meta: &sources_meta,
        })?;
        all_cited.extend(cited.iter().copied());
        written.push(w);
    }

    phase(&format!("writing {} linked notes", atomic.len()))?;
    let sem = Arc::new(tokio::sync::Semaphore::new(write::NOTE_CONCURRENCY));
    let mut futs = Vec::new();
    for n in &atomic {
        let claims: Vec<&digest::Claim> = n
            .claim_ids
            .iter()
            .filter_map(|id| claim_by_id.get(id).copied())
            .collect();
        let existing = if n.action == Action::Update {
            write::read_existing_body(&kref, &n.slug)
        } else {
            None
        };
        let inp = NoteInput {
            query: &query,
            note: n,
            claims,
            sources: &sources,
            link_targets: &targets,
            existing_body: existing,
            append: config.append,
            language: &config.language,
            parent_overview: parent_overview.clone(),
        };
        let prompt = write::build_note_prompt(&inp);
        let dprov = dprov.clone();
        let dmodel = dmodel.clone();
        let sem = sem.clone();
        let cancel = cancel.clone();
        let timeout = config.llm_timeout;
        futs.push(async move {
            let _p = sem.acquire().await;
            llm_calls::oneshot_pub(dprov.as_ref(), &dmodel, prompt, timeout, &cancel).await
        });
    }
    let t = std::time::Instant::now();
    let bodies = futures::future::join_all(futs).await;
    eprintln!(
        "[research] wrote {} note bodies in {:.1}s",
        bodies.len(),
        t.elapsed().as_secs_f32()
    );

    for (n, body) in atomic.iter().zip(bodies) {
        let body = match body {
            Ok(b) => b,
            Err(e) => {
                let w = format!("note `{}` was not written: {e}", n.slug);
                eprintln!("[research] {w}");
                plan_warnings.push(w);
                continue;
            }
        };
        let (rewritten, cited) = write::rewrite_claim_markers(&strip_heading(&body), &claim_source);
        let rewritten = write::fix_wikilinks(&rewritten, &targets);
        let parent = if config.refresh_slug.is_some() {
            None
        } else {
            Some(topic_slug.as_str())
        };
        let rewritten = write::autolink(
            &rewritten,
            &n.slug,
            &targets,
            &n.related,
            parent,
            &config.language,
        );
        let rewritten = write::unbold_links(&rewritten);
        let conf = mean_confidence(n, &claim_by_id);
        let w = write::persist_note(write::NoteToPersist {
            kref: &kref,
            note: n,
            body: &rewritten,
            cited: &cited,
            claim_count: n.claim_ids.len(),
            confidence: conf,
            today: &today,
            append: config.append,
            sources_meta: &sources_meta,
        })?;
        all_cited.extend(cited.iter().copied());
        written.push(w);
    }

    if written.is_empty() {
        return Err(Error::Tool(format!(
            "research wrote no notes — every note body failed ({})",
            plan_warnings.join("; ")
        )));
    }

    // ── 5. Sources + run log ────────────────────────────────────────
    phase("writing sources")?;
    let mut sources_written = 0u32;
    for s in &sources {
        // Locally ingested sources are already archived under their alias.
        if s.url.starts_with("kms://") {
            continue;
        }
        if all_cited.contains(&s.index) {
            match kms_writer::write_source(
                &kms_name, &query, &today, s.index, &s.title, &s.url, &s.body,
            ) {
                Ok(_) => sources_written += 1,
                Err(e) => eprintln!("[research] source [{}] not written: {e}", s.index),
            }
        }
    }
    eprintln!("[research] wrote {sources_written} cited sources into '{kms_name}'");
    let _ = write::write_run_log(
        &kref,
        &RunLog {
            query: &query,
            topic_slug: &topic_slug,
            today: &today,
            rounds: &rounds,
            sources_digested: sources.len() as u32 - cached_hits,
            sources_cached: cached_hits,
            claims_total,
            claims_dropped,
            notes: &written,
            dry_run_plan: None,
            elapsed_secs: started.elapsed().as_secs(),
            worker_model: &dmodel,
            warnings: &plan_warnings,
            sources: &sources,
            cited: &all_cited,
        },
    );
    let result_slug = match &config.refresh_slug {
        Some(slug) => slug.clone(),
        None => written
            .iter()
            .find(|w| w.slug == topic_slug)
            .or(written.first())
            .map(|w| w.slug.clone())
            .unwrap_or_else(|| topic_slug.clone()),
    };
    Ok(format!("{kms_name}/{result_slug}.md"))
}

/// Two URLs naming the same document (tracking parameters, fragment,
/// `www.`, scheme and trailing slash are not identity).
fn same_source(a: &str, b: &str) -> bool {
    a == b || source_quality::canonical_url(a) == source_quality::canonical_url(b)
}

/// Append hits as sources (dedupe on the canonical URL); returns the
/// newly added sources so the round only digests what it fetched.
fn accumulate(
    sources: &mut Vec<ResearchSource>,
    hits: Vec<SearchHit>,
    registry: &mut SourceRegistry,
) -> Vec<ResearchSource> {
    let mut added = Vec::new();
    for h in hits {
        if h.url.is_empty() || sources.iter().any(|s| same_source(&s.url, &h.url)) {
            continue;
        }
        let src = ResearchSource {
            index: registry.index_for(&h.url, &h.title),
            title: h.title,
            url: h.url,
            body: h.snippet,
        };
        sources.push(src.clone());
        added.push(src);
    }
    added
}

#[allow(clippy::too_many_arguments)]
async fn digest_round(
    provider: &Arc<dyn Provider>,
    model: &str,
    query: &str,
    new_sources: &[ResearchSource],
    known_slugs: &[String],
    kref: &crate::kms::KmsRef,
    today: &str,
    config: &JobConfig,
    cancel: &CancelToken,
    cached_hits: &mut u32,
) -> Vec<Digest> {
    for s in new_sources {
        if digest::load_cached(kref, &s.url).is_some() {
            *cached_hits += 1;
        }
    }
    digest::digest_many(digest::DigestBatch {
        provider: provider.clone(),
        model,
        query,
        sources: new_sources,
        known_slugs,
        kref,
        today,
        timeout: config.llm_timeout,
        cancel,
        language: &config.language,
    })
    .await
}

fn mean_confidence(n: &NotePlan, by_id: &HashMap<String, &digest::Claim>) -> f32 {
    let vals: Vec<f32> = n
        .claim_ids
        .iter()
        .filter_map(|id| by_id.get(id).map(|c| c.confidence))
        .collect();
    if vals.is_empty() {
        0.0
    } else {
        vals.iter().sum::<f32>() / vals.len() as f32
    }
}

/// Title of an archived source: frontmatter `title:`, else the first
/// `# ` heading, else the alias.
pub fn local_source_title(raw: &str, alias: &str) -> String {
    let (fm, body) = crate::kms::parse_frontmatter(raw);
    if let Some(t) = fm.get("title") {
        let t = t.trim().trim_matches('"').trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    body.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("# ").map(|h| h.trim().to_string()))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| alias.replace('-', " "))
}

/// Cut a long document into digest-sized windows at paragraph
/// boundaries so every part of it gets extracted, not just the head.
pub fn split_windows(body: &str, max_chars: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for para in body.split("\n\n") {
        let p = para.trim_end();
        if p.is_empty() {
            continue;
        }
        if !cur.is_empty() && cur.chars().count() + p.chars().count() + 2 > max_chars {
            out.push(std::mem::take(&mut cur));
        }
        if p.chars().count() > max_chars {
            // A single oversized block (a table, a code dump): hard-cut.
            let chars: Vec<char> = p.chars().collect();
            for chunk in chars.chunks(max_chars) {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(chunk.iter().collect());
            }
            continue;
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(p);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Fold per-window digests of one document into a single digest with
/// one citation index: entities unioned by slug, claims renumbered so
/// `s{index}c{k}` stays unique across windows.
pub fn merge_digests(
    parts: Vec<Digest>,
    index: u32,
    url: &str,
    title: &str,
    today: &str,
) -> Digest {
    let mut merged = Digest {
        url: url.to_string(),
        title: title.to_string(),
        fetched: today.to_string(),
        source: index,
        entities: Vec::new(),
        claims: Vec::new(),
        links_to_known: Vec::new(),
        dropped_claims: 0,
        model: String::new(),
        published: None,
    };
    let mut k = 0u32;
    for d in parts {
        if merged.model.is_empty() {
            merged.model = d.model;
        }
        if merged.published.is_none() {
            merged.published = d.published;
        }
        merged.dropped_claims += d.dropped_claims;
        for e in d.entities {
            if !merged.entities.iter().any(|x| x.slug == e.slug) {
                merged.entities.push(e);
            }
        }
        for l in d.links_to_known {
            if !merged.links_to_known.contains(&l) {
                merged.links_to_known.push(l);
            }
        }
        for mut c in d.claims {
            k += 1;
            c.id = format!("s{index}c{k}");
            c.source = index;
            merged.claims.push(c);
        }
    }
    merged
}

/// The topic page's opening — everything before its first `##`, capped.
fn opening_of(body: &str, max_chars: usize) -> String {
    let head = body.split("\n## ").next().unwrap_or(body).trim();
    if head.chars().count() <= max_chars {
        return head.to_string();
    }
    let mut out: String = head.chars().take(max_chars).collect();
    if let Some(i) = out.rfind(['.', '\n']) {
        out.truncate(i + 1);
    }
    out
}

fn strip_heading(s: &str) -> String {
    let mut lines = s.trim().lines().peekable();
    while let Some(l) = lines.peek() {
        if l.trim().starts_with('#') || l.trim().is_empty() {
            lines.next();
        } else {
            break;
        }
    }
    lines.collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{EventStream, ProviderEvent, StreamRequest};
    use async_trait::async_trait;
    use futures::stream;
    use std::sync::Mutex;

    /// Answers by what the prompt is for, so parallel digests can't
    /// desynchronise a scripted queue.
    struct KeyedProvider {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Provider for KeyedProvider {
        async fn stream(&self, req: StreamRequest) -> Result<EventStream> {
            let prompt = req
                .messages
                .iter()
                .flat_map(|m| m.content.iter())
                .filter_map(|b| match b {
                    crate::types::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let body = if prompt.contains("You are extracting structured knowledge") {
                self.calls.lock().unwrap().push("digest".into());
                if prompt.contains("URL: https://s1") {
                    r#"{"entities":[{"name":"Overtime pay","slug":"overtime-pay","kind":"concept"}],
                        "claims":[{"text":"OT is 1.5x","quote":"overtime is paid at 1.5x","entities":["overtime-pay"],"confidence":0.9},
                                  {"text":"hallucinated","quote":"not in the body","entities":[]}],
                        "links_to_known":[]}"#
                } else {
                    r#"{"entities":[{"name":"Minimum wage","slug":"minimum-wage","kind":"concept"}],
                        "claims":[{"text":"Min wage is 400","quote":"minimum wage is 400 baht","entities":["minimum-wage"],"confidence":0.8}],
                        "links_to_known":[]}"#
                }
                .to_string()
            } else if prompt.contains("Name up to") {
                self.calls.lock().unwrap().push("gap".into());
                // second gap round returns nothing → loop ends
                if self
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|c| *c == "gap")
                    .count()
                    == 1
                {
                    "minimum wage 2568".to_string()
                } else {
                    String::new()
                }
            } else if prompt.contains("REFRESH MODE") {
                self.calls.lock().unwrap().push("plan-refresh".into());
                r#"[{"slug":"overtime-pay","kind":"concept","title":"Overtime pay","action":"update","claim_ids":["s1c1"],"related":[]}]"#.to_string()
            } else if prompt.contains("planning a knowledge-base entry") {
                self.calls.lock().unwrap().push("plan".into());
                r#"[{"slug":"overtime-pay","kind":"concept","title":"Overtime pay","action":"create","claim_ids":["s1c1"],"related":["minimum-wage"]},
                    {"slug":"minimum-wage","kind":"concept","title":"Minimum wage","action":"create","claim_ids":["s2c1"],"related":["overtime-pay"]},
                    {"slug":"thai-labour-law","kind":"moc","title":"Thai labour law","action":"create","claim_ids":["s1c1","s2c1"],"related":["overtime-pay","minimum-wage"]}]"#.to_string()
            } else if prompt.contains("You are writing ONE zettelkasten note") {
                self.calls.lock().unwrap().push("note".into());
                if prompt.contains("Kind: moc") {
                    "Labour law answer [c:s1c1][c:s2c1].\n\n## Map\n- [[overtime-pay]] — pay rules\n- [[minimum-wage]] — floor\n".to_string()
                } else if prompt.contains("Slug: overtime-pay") {
                    "# Overtime\nOvertime is 1.5x the hourly wage [c:s1c1]. See [[minimum-wage]]."
                        .to_string()
                } else {
                    "The floor is 400 baht [c:s2c1].".to_string()
                }
            } else {
                self.calls.lock().unwrap().push("slug".into());
                "thai-labour-law".to_string()
            };
            let events: Vec<Result<ProviderEvent>> = vec![
                Ok(ProviderEvent::MessageStart {
                    model: "mock".into(),
                }),
                Ok(ProviderEvent::TextDelta(body)),
                Ok(ProviderEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    #[test]
    fn windows_split_on_paragraphs_and_merge_renumbers_claims() {
        let body = "para one\n\npara two is longer\n\npara three";
        let w = split_windows(body, 22);
        assert_eq!(w, vec!["para one", "para two is longer", "para three"]);
        assert_eq!(split_windows("", 100), vec![""]);

        let mk = |claims: &[&str]| Digest {
            url: "u".into(),
            title: "t".into(),
            fetched: "d".into(),
            source: 9,
            entities: vec![crate::research::digest::Entity {
                name: "X".into(),
                slug: "x".into(),
                kind: "org".into(),
            }],
            claims: claims
                .iter()
                .enumerate()
                .map(|(i, c)| crate::research::digest::Claim {
                    id: format!("s9c{}", i + 1),
                    text: c.to_string(),
                    quote: c.to_string(),
                    entities: vec!["x".into()],
                    confidence: 0.9,
                    source: 9,
                    published: None,
                })
                .collect(),
            links_to_known: vec![],
            dropped_claims: 1,
            model: "m".into(),
            published: None,
        };
        let m = merge_digests(
            vec![mk(&["a", "b"]), mk(&["c"])],
            3,
            "kms://k/sources/doc",
            "Doc",
            "today",
        );
        let ids: Vec<&str> = m.claims.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["s3c1", "s3c2", "s3c3"]);
        assert!(m.claims.iter().all(|c| c.source == 3));
        assert_eq!(m.entities.len(), 1, "entities unioned by slug");
        assert_eq!(m.dropped_claims, 2);
        assert_eq!(
            local_source_title("---\ntitle: \"Big Doc\"\n---\n\n# H\n", "big-doc"),
            "Big Doc"
        );
        assert_eq!(
            local_source_title("# Heading here\n\ntext", "x"),
            "Heading here"
        );
        assert_eq!(local_source_title("plain", "my-doc"), "my doc");
    }

    struct Tools;
    #[async_trait]
    impl ResearchTools for Tools {
        async fn search(&self, query: &str, _max: u32) -> Result<Vec<SearchHit>> {
            Ok(if query.contains("minimum wage") {
                vec![SearchHit {
                    title: "S2".into(),
                    url: "https://s2".into(),
                    snippet: "snip".into(),
                }]
            } else {
                vec![SearchHit {
                    title: "S1".into(),
                    url: "https://s1".into(),
                    snippet: "snip".into(),
                }]
            })
        }
        async fn fetch(&self, url: &str) -> Result<String> {
            Ok(if url.ends_with("s1") {
                "Overtime is paid at 1.5x of the hourly wage.".into()
            } else {
                "The minimum wage is 400 baht per day.".into()
            })
        }
    }

    #[tokio::test]
    async fn v2_writes_atomic_notes_and_moc_with_verified_citations() {
        let _h = crate::research::test_helpers::scoped_home();
        let provider = Arc::new(KeyedProvider {
            calls: Mutex::new(vec![]),
        });
        let (id, cancel) = manager().register("กฎหมายแรงงานไทย".into(), &JobConfig::default());
        let cfg = JobConfig {
            min_iter: 1,
            max_iter: 4,
            ..JobConfig::default()
        };
        let result = run_with_tools(PipelineRequest {
            job_id: &id,
            query: "กฎหมายแรงงานไทย".into(),
            config: cfg,
            provider: provider.clone(),
            model: "mock".into(),
            cancel,
            tools: Arc::new(Tools),
            digest: None,
        })
        .await
        .unwrap();
        assert_eq!(result, "thai-labour-law/thai-labour-law.md");

        let kref = crate::kms::resolve("thai-labour-law").unwrap();
        let ot = std::fs::read_to_string(kref.root.join("pages/overtime-pay.md")).unwrap();
        assert!(
            ot.contains("type: note") && ot.contains("kind: concept"),
            "{ot}"
        );
        assert!(
            ot.contains("wage [1]"),
            "marker rewritten to source index: {ot}"
        );
        assert!(!ot.contains("[c:"), "no raw markers left");
        assert!(!ot.contains("# Overtime\n"), "leading heading stripped");
        assert!(ot.contains("[[minimum-wage]]"));
        assert!(ot.contains("## Sources"));
        let moc = std::fs::read_to_string(kref.root.join("pages/thai-labour-law.md")).unwrap();
        assert!(moc.contains("kind: moc") && moc.contains("[[overtime-pay]]"));
        assert!(moc.contains("[1]") && moc.contains("[2]"));

        // hallucinated claim never reached a note; cache holds the digest
        let d = digest::load_cached(&kref, "https://s1").unwrap();
        assert_eq!(d.claims.len(), 1);
        assert_eq!(d.dropped_claims, 1);
        assert!(kref.root.join("sources").read_dir().unwrap().count() >= 2);
        let runs: Vec<_> = kref.root.join("runs").read_dir().unwrap().collect();
        assert_eq!(runs.len(), 1);
        let calls = provider.calls.lock().unwrap().clone();
        assert_eq!(calls.iter().filter(|c| *c == "digest").count(), 2);
        assert_eq!(calls.iter().filter(|c| *c == "plan").count(), 1);
        assert_eq!(calls.iter().filter(|c| *c == "note").count(), 3);
        assert!(!calls.contains(&"verify".to_string()));
        assert!(!kref.root.join("pages/_summary.md").exists());
    }

    #[tokio::test]
    async fn v2_dry_run_writes_only_the_plan() {
        let _h = crate::research::test_helpers::scoped_home();
        let provider = Arc::new(KeyedProvider {
            calls: Mutex::new(vec![]),
        });
        let (id, cancel) = manager().register("q".into(), &JobConfig::default());
        let cfg = JobConfig {
            min_iter: 1,
            max_iter: 1,
            dry_run: true,
            ..JobConfig::default()
        };
        let result = run_with_tools(PipelineRequest {
            job_id: &id,
            query: "กฎหมายแรงงานไทย".into(),
            config: cfg,
            provider: provider.clone(),
            model: "mock".into(),
            cancel,
            tools: Arc::new(Tools),
            digest: None,
        })
        .await
        .unwrap();
        assert!(result.starts_with("thai-labour-law/runs/"), "{result}");
        let kref = crate::kms::resolve("thai-labour-law").unwrap();
        assert!(!kref.root.join("pages/overtime-pay.md").exists());
        let log = std::fs::read_to_string(
            kref.root
                .join("runs")
                .read_dir()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(log.contains("dry run"));
        assert!(!provider.calls.lock().unwrap().contains(&"note".to_string()));
    }

    #[tokio::test]
    async fn v2_second_run_updates_instead_of_duplicating() {
        let _h = crate::research::test_helpers::scoped_home();
        let provider = Arc::new(KeyedProvider {
            calls: Mutex::new(vec![]),
        });
        for _ in 0..2 {
            let (id, cancel) = manager().register("q".into(), &JobConfig::default());
            // Explicit --kms: the KMS name differs from the query slug, which
            // is exactly the case where cited sources used to be written to
            // a non-existent KMS named after the query and silently lost.
            let cfg = JobConfig {
                min_iter: 1,
                max_iter: 1,
                append: true,
                kms_target: Some("labour-kb".into()),
                ..JobConfig::default()
            };
            run_with_tools(PipelineRequest {
                job_id: &id,
                query: "กฎหมายแรงงานไทย".into(),
                config: cfg,
                provider: provider.clone(),
                model: "mock".into(),
                cancel,
                tools: Arc::new(Tools),
                digest: None,
            })
            .await
            .unwrap();
        }
        let kref = crate::kms::resolve("labour-kb").unwrap();
        assert!(
            kref.root
                .join("sources")
                .read_dir()
                .map(|d| d.count())
                .unwrap_or(0)
                >= 1,
            "cited sources must land in the --kms target, not a query-named KMS"
        );
        assert!(crate::kms::resolve("thai-labour-law").is_none());
        let pages: Vec<String> = kref
            .root
            .join("pages")
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            pages
                .iter()
                .filter(|p| p.starts_with("overtime-pay"))
                .count(),
            1,
            "{pages:?}"
        );
        // second run's digests came from cache: only 1 digest call in total per source
        let calls = provider.calls.lock().unwrap().clone();
        assert_eq!(calls.iter().filter(|c| *c == "digest").count(), 1);
    }
    #[tokio::test]
    async fn refresh_updates_the_anchored_note_without_a_new_moc() {
        let _h = crate::research::test_helpers::scoped_home();
        let provider = Arc::new(KeyedProvider {
            calls: Mutex::new(vec![]),
        });
        let (id, cancel) = manager().register("q".into(), &JobConfig::default());
        let cfg = JobConfig {
            min_iter: 1,
            max_iter: 1,
            kms_target: Some("labour-kb".into()),
            ..JobConfig::default()
        };
        run_with_tools(PipelineRequest {
            job_id: &id,
            query: "กฎหมายแรงงานไทย".into(),
            config: cfg,
            provider: provider.clone(),
            model: "mock".into(),
            cancel,
            tools: Arc::new(Tools),
            digest: None,
        })
        .await
        .unwrap();
        let kref = crate::kms::resolve("labour-kb").unwrap();
        let pages_before = kref.root.join("pages").read_dir().unwrap().count();
        let before = std::fs::read_to_string(kref.root.join("pages/overtime-pay.md")).unwrap();

        let ids = crate::research::start_refresh(crate::research::RefreshRequest {
            kms: "labour-kb".into(),
            slugs: vec!["overtime-pay".into()],
            older_than_days: 30,
            base: JobConfig::default(),
            provider: provider.clone(),
            model: "mock".into(),
            digest_provider: None,
            tools: Some(Arc::new(Tools)),
        })
        .await
        .unwrap();
        assert_eq!(ids.len(), 1);
        for _ in 0..400 {
            let j = manager().get(&ids[0].0).unwrap();
            if j.status != crate::research::JobStatus::Running
                && j.status != crate::research::JobStatus::Pending
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let j = manager().get(&ids[0].0).unwrap();
        assert_eq!(j.status, crate::research::JobStatus::Done, "{:?}", j.error);
        assert_eq!(j.result_page.as_deref(), Some("labour-kb/overtime-pay.md"));
        let pages_after = kref.root.join("pages").read_dir().unwrap().count();
        assert_eq!(
            pages_after, pages_before,
            "refresh must not add a MOC or other pages"
        );
        let after = std::fs::read_to_string(kref.root.join("pages/overtime-pay.md")).unwrap();
        assert!(after.contains("type: note"));
        assert_ne!(after, before, "note body rewritten by the refresh");
        let calls = provider.calls.lock().unwrap().clone();
        assert!(calls.iter().any(|c| c == "plan-refresh"), "{calls:?}");
        let err = crate::research::start_refresh(crate::research::RefreshRequest {
            kms: "labour-kb".into(),
            slugs: vec!["nope".into()],
            older_than_days: 30,
            base: JobConfig::default(),
            provider,
            model: "mock".into(),
            digest_provider: None,
            tools: None,
        })
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(err.contains("no note 'nope'"), "{err}");
    }
}
