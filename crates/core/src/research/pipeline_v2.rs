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

/// The page name for a topic the caller pinned by slug. Never empty: an
/// empty page name is the one thing the KMS refuses outright, and every
/// downstream step treats this as a filename.
fn pinned_slug(raw: &str) -> String {
    let slug = super::digest::sanitize_slug(raw);
    if slug.is_empty() {
        "research".into()
    } else {
        slug
    }
}

/// The page name for an ingest run's topic page.
///
/// The ingest has already written the stub this run is about to adopt, at
/// `pages/<alias>.md`, where `alias` came from `kms::sanitize_alias`. So
/// the only correct answer is that same alias — kebab-casing it here
/// would fold its '_' to '-' and write a second page beside the stub
/// instead of filling it in. `sanitize_alias` is idempotent, and is
/// applied again only so a hand-passed alias cannot smuggle in a path
/// separator.
fn ingest_slug(alias: &str) -> String {
    let name = crate::kms::sanitize_alias(alias);
    if name.is_empty() {
        "research".into()
    } else {
        name
    }
}

/// How often the running cost is re-priced. Pricing re-parses the
/// baseline catalogue and reads two files; a run writing thirty notes
/// would otherwise do that thirty times for a figure nobody can read
/// changing that fast.
const COST_REFRESH_SECS: u64 = 10;

/// Where an archived source actually came from.
///
/// A source the run fetched carries its own address. One the user
/// ingested carries `kms://<base>/sources/<alias>` — the right thing to
/// resolve the in-page link against, and a path no reader can open. The
/// address is not lost: `ingest_url` stamps `origin:` on the archived
/// file. Returns "" for anything that is already an address, or when
/// there is no `origin:` to find.
fn archived_origin(kref: &crate::kms::KmsRef, url: &str) -> String {
    let Some(rest) = url.strip_prefix("kms://") else {
        return String::new();
    };
    let Some((_, alias)) = rest.split_once("/sources/") else {
        return String::new();
    };
    let alias = alias.split('#').next().unwrap_or(alias);
    let path = kref.root.join("sources").join(format!("{alias}.md"));
    let Ok(raw) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let (fm, _) = crate::kms::parse_frontmatter(&raw);
    fm.get("origin")
        .map(|o| o.trim().trim_matches('"').to_string())
        .filter(|o| o.contains("://"))
        .unwrap_or_default()
}

/// The language one note is written in.
///
/// The run's language comes from the document being ingested
/// (dev-plan/64 D6), which is right for a new note and wrong for one
/// that already exists: an English source must not re-language the
/// Thai page it is updating. `detect_language` answers "query" when it
/// cannot tell, and that is not an answer worth overriding the run's
/// choice with.
fn note_language(action: Action, existing: Option<&str>, run: &str) -> String {
    if action != Action::Update {
        return run.to_string();
    }
    match existing {
        Some(body) if !body.trim().is_empty() => match super::detect_language(body) {
            "query" => run.to_string(),
            lang => lang.to_string(),
        },
        _ => run.to_string(),
    }
}

pub async fn run_with_tools(
    job_id: &str,
    query: String,
    config: JobConfig,
    provider: Arc<dyn Provider>,
    model: String,
    cancel: CancelToken,
    tools: Arc<dyn ResearchTools>,
    digest: Option<(Arc<dyn Provider>, String)>,
) -> Result<String> {
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
    // dev-plan/64 P5.4: the running cost, refreshed as the run goes, so
    // a job in flight can say what it has spent instead of only telling
    // the owner afterwards. Throttled: pricing re-parses the baseline
    // catalogue and reads two files, and `phase` is called per note.
    let cost_at = std::sync::atomic::AtomicU64::new(0);
    let tick_cost = || {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let last = cost_at.load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(last) < COST_REFRESH_SECS {
            return;
        }
        cost_at.store(now, std::sync::atomic::Ordering::Relaxed);
        mgr.update_cost(job_id, llm_calls::usage_so_far().cost_usd());
    };
    let alive = |phase: &str| -> Result<()> {
        if cancel.is_cancelled() {
            return Err(Error::Tool("research cancelled".into()));
        }
        if mgr.is_over_budget(job_id) {
            return Err(Error::Tool("research time budget exhausted".into()));
        }
        mgr.update_phase(job_id, phase);
        tick_cost();
        Ok(())
    };
    let phase = |phase: &str| -> Result<()> {
        if cancel.is_cancelled() {
            return Err(Error::Tool("research cancelled".into()));
        }
        mgr.update_phase(job_id, phase);
        tick_cost();
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
        (Some(fixed), _, _) => pinned_slug(fixed),
        // The ingest stub `pages/<alias>.md` becomes the topic page.
        (None, Some(alias), _) => ingest_slug(alias),
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
    // dev-plan/64 P5.4: what kind of run this is, decided from the same
    // three fields that name it, and stamped on the run log so the GUI
    // can price a click from runs of the same kind.
    let mode = match (
        &config.topic_slug,
        &config.local_source,
        &config.refresh_slug,
    ) {
        (Some(_), _, _) => "selection",
        (None, Some(_), _) => "ingest",
        (None, None, Some(_)) => "refresh",
        (None, None, None) => "research",
    };
    let kms_name = config
        .kms_target
        .clone()
        .unwrap_or_else(|| topic_slug.clone());
    let kref = kms_writer::resolve_or_create(&kms_name)?;
    // dev-plan/64 P5.5: claim the page for as long as this run is
    // alive, so a run that dies without unwinding can be told from one
    // that is still working. Dropped on every exit path — including the
    // panic the caller catches — which releases the claim.
    let _claim = super::jobs::Guard::start(&kref, job_id, &query, &topic_slug, mode);
    // dev-plan/64 D6: `auto` follows the document being ingested, else the
    // query. Decided once, here, so every prompt of the run agrees.
    let mut config = config;
    let document_head = config.local_source.as_ref().and_then(|alias| {
        let raw =
            std::fs::read_to_string(kref.root.join("sources").join(format!("{alias}.md"))).ok()?;
        let (_, body) = crate::kms::parse_frontmatter(&raw);
        Some(body.chars().take(4_000).collect::<String>())
    });
    config.language = super::resolve_language(&config.language, &query, document_head.as_deref());
    // The base's real name from here on. `--kms age-of-abundance` finds
    // "Age of Abundance", and everything below — the attach, the
    // `kms://` source URLs, the archive writes — has to say the same name
    // the base is actually filed under.
    let kms_name = kref.name.clone();
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
    // Each digest is shown the first 80 of these. Ranked by the query —
    // and only the query, so the list is the same for every digest of the
    // run and their shared prompt prefix stays byte-identical.
    let known_slugs: Vec<String> =
        graph::rank_known(&known, &query, &std::collections::HashSet::new())
            .into_iter()
            .map(|k| k.slug)
            .collect();

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
        let parts = digest::digest_many(
            dprov.clone(),
            &dmodel,
            &query,
            &win_sources,
            &known_slugs,
            &kref,
            &today,
            config.llm_timeout,
            &cancel,
            &config.language,
        )
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
            let gaps = plan::gap_queries(
                dprov.as_ref(),
                &dmodel,
                &query,
                &tables,
                &queries_run,
                config.subtopics_per_iter,
                config.llm_timeout,
                &cancel,
            )
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
    } = plan::plan_notes(
        dprov.as_ref(),
        &dmodel,
        &query,
        &tables,
        &known,
        &topic_slug,
        &topic_title,
        config.max_notes,
        config.llm_timeout,
        &cancel,
        &config.language,
        config.refresh_slug.as_deref(),
    )
    .await?;

    {
        let dir = kref.root.join(".research");
        let _ = std::fs::create_dir_all(&dir);
        let _ = crate::kms::write_file(dir.join("last-plan.json"), &plan_raw);
    }
    eprintln!(
        "[research] planned {} notes from {} claims in {:.1}s (model {})",
        plan_notes.len(),
        claims_total,
        t_plan.elapsed().as_secs_f32(),
        dmodel
    );

    let mut all_cited: BTreeSet<u32> = BTreeSet::new();
    let mut archived: BTreeSet<u32> = BTreeSet::new();
    if config.dry_run {
        let path = write::write_run_log(
            &kref,
            &RunLog {
                query: &query,
                topic_slug: &topic_slug,
                mode,
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
    let sources_meta: Vec<(u32, String, String, String)> = registry
        .meta()
        .into_iter()
        .map(|(i, t, u)| {
            let origin = archived_origin(&kref, &u);
            (i, t, u, origin)
        })
        .collect();
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
        let note_lang = note_language(n.action, existing.as_deref(), &config.language);
        let inp = NoteInput {
            query: &query,
            note: n,
            claims,
            sources: &sources,
            link_targets: &targets,
            existing_body: existing,
            // Was hardcoded `false`, which left the one page an ingest
            // is most likely to collide with — the source's own — open
            // to a full rewrite even when the run was told never to
            // rewrite an existing note.
            append: config.append,
            language: &note_lang,
            parent_overview: None,
            refresh: false,
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
        let rewritten = prune_new_note(n, rewritten, &mut plan_warnings);
        let rewritten = write::fix_wikilinks(&rewritten, &targets);
        let rewritten = write::drop_self_links(&rewritten, &n.slug);
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
        archive_cited(&kms_name, &query, &today, &sources, &cited, &mut archived);
        match write::persist_note(
            &kref,
            n,
            &rewritten,
            &cited,
            n.claim_ids.len(),
            conf,
            &today,
            false,
            &sources_meta,
        ) {
            Ok(w) => {
                all_cited.extend(cited.iter().copied());
                written.push(w);
            }
            Err(e) => {
                let w = format!("topic page `{}` could not be saved: {e}", n.slug);
                eprintln!("[research] {w}");
                plan_warnings.push(w);
            }
        }
    }

    phase(&format!("writing {} linked notes", atomic.len()))?;
    // What each refreshed page said before, to check the refresh kept it.
    let mut refresh_before: HashMap<String, String> = HashMap::new();
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
        // A refresh keeps the page and adds references: only the claims
        // that bear on what the page already says reach the writer.
        let refreshing = config.refresh_slug.is_some() && existing.is_some();
        let claims = match (&existing, refreshing) {
            (Some(body), true) => {
                let all = claims.len();
                let kept = write::claims_for_refresh(body, claims);
                let w = format!(
                    "refresh of `{}`: {} of the run's {all} claims bear on what the page says and were offered as references",
                    n.slug,
                    kept.len()
                );
                eprintln!("[research] {w}");
                plan_warnings.push(w);
                kept
            }
            _ => claims,
        };
        if refreshing {
            refresh_before.insert(n.slug.clone(), existing.clone().unwrap_or_default());
        }
        let note_lang = note_language(n.action, existing.as_deref(), &config.language);
        let inp = NoteInput {
            query: &query,
            note: n,
            claims,
            sources: &sources,
            link_targets: &targets,
            existing_body: existing,
            append: config.append,
            language: &note_lang,
            parent_overview: parent_overview.clone(),
            refresh: refreshing,
        };
        let prompt = write::build_note_prompt_split(&inp);
        let dprov = dprov.clone();
        let dmodel = dmodel.clone();
        let sem = sem.clone();
        let cancel = cancel.clone();
        let timeout = config.llm_timeout;
        futs.push(async move {
            let _p = sem.acquire().await;
            llm_calls::oneshot_split(
                dprov.as_ref(),
                &dmodel,
                prompt,
                timeout,
                &cancel,
                llm_calls::CallKind::Writing,
            )
            .await
        });
    }
    let t = std::time::Instant::now();
    let bodies = futures::future::join_all(futs).await;
    eprintln!(
        "[research] wrote {} note bodies in {:.1}s",
        bodies.len(),
        t.elapsed().as_secs_f32()
    );

    // One index rebuild for the whole loop instead of one per note — each
    // is two full scans of `pages/`. The guard is thread-local, which is
    // sound here only because nothing below awaits.
    let index_batch = crate::kms::IndexBatch::new(&kref);
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
        let rewritten = prune_new_note(n, rewritten, &mut plan_warnings);
        // "Keep the page" is checked, not hoped for: a refresh that loses
        // the page's own citations, or comes back several times the size,
        // rewrote it. The previous version is in the trash either way.
        if let Some(before) = refresh_before.get(&n.slug) {
            let had = kms_writer::parse_citation_indices(before);
            let has = kms_writer::parse_citation_indices(&rewritten);
            let lost: Vec<String> = had.difference(&has).map(|i| format!("[{i}]")).collect();
            // Size is not the test: a page that gained 27 references is
            // longer, and a real run was warned about for exactly that with
            // every paragraph intact. What was there must still be there.
            let paragraphs: Vec<&str> = before
                .split("\n\n")
                .map(str::trim)
                .filter(|p| !p.starts_with('#') && p.chars().count() >= 80)
                .collect();
            let gone = paragraphs
                .iter()
                .filter(|p| {
                    let head: String = p.chars().take(60).collect();
                    !rewritten.contains(&head)
                })
                .count();
            if !lost.is_empty() || gone > 0 {
                let w = format!(
                    "refresh of `{}` did not keep the page: {gone} of its {} paragraph(s) no longer open the same way{}. The version before is in the trash (`/kms restore`).",
                    n.slug,
                    paragraphs.len(),
                    if lost.is_empty() {
                        String::new()
                    } else {
                        format!(", and it no longer cites {}", lost.join(" "))
                    }
                );
                eprintln!("[research] {w}");
                plan_warnings.push(w);
            }
        }
        let rewritten = write::fix_wikilinks(&rewritten, &targets);
        let rewritten = write::drop_self_links(&rewritten, &n.slug);
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
        // Sources first: a note must never be on disk citing an archive
        // that is not. They used to be written after every note, so a run
        // that died in this loop left citations that all 404.
        archive_cited(&kms_name, &query, &today, &sources, &cited, &mut archived);
        // One note that cannot be saved — a full disk, a reserved name —
        // is a warning, the same as one the model failed to write. `?`
        // here threw away every note after it, the sources and the run
        // log, once all the LLM calls had been paid for.
        match write::persist_note(
            &kref,
            n,
            &rewritten,
            &cited,
            n.claim_ids.len(),
            conf,
            &today,
            config.append,
            &sources_meta,
        ) {
            Ok(w) => {
                all_cited.extend(cited.iter().copied());
                written.push(w);
            }
            Err(e) => {
                let w = format!("note `{}` could not be saved: {e}", n.slug);
                eprintln!("[research] {w}");
                plan_warnings.push(w);
            }
        }
    }
    drop(index_batch);

    if written.is_empty() {
        return Err(Error::Tool(format!(
            "research wrote no notes — every note body failed ({})",
            plan_warnings.join("; ")
        )));
    }

    // dev-plan/64 P4.2: say which notes are mostly unanchored prose. The
    // page carries the number too (`uncited:`), but a number nobody is told
    // about is not a check.
    for w in &written {
        if let Some(u) = w.uncited.filter(|u| *u > write::UNCITED_WARN) {
            let msg = format!(
                "`{}`: {:.0}% of its prose sits in paragraphs with no citation — read it before relying on it",
                w.slug,
                u * 100.0
            );
            eprintln!("[research] {msg}");
            plan_warnings.push(msg);
        }
    }

    // ── 5. Sources + run log ────────────────────────────────────────
    phase("writing sources")?;
    // Anything cited that the per-note pass missed (it should be nothing).
    archive_cited(
        &kms_name,
        &query,
        &today,
        &sources,
        &all_cited,
        &mut archived,
    );
    eprintln!(
        "[research] wrote {} cited sources into '{kms_name}'",
        archived.len()
    );
    // dev-plan/64 D5: what the run read and did not cite is kept too, out
    // of the way, for ninety days.
    let mut uncited = 0usize;
    for s in &sources {
        if archived.contains(&s.index)
            || all_cited.contains(&s.index)
            || s.url.starts_with("kms://")
        {
            continue;
        }
        match kms_writer::write_uncited_source(&kms_name, &query, &today, &s.title, &s.url, &s.body)
        {
            Ok(Some(_)) => uncited += 1,
            Ok(None) => {}
            Err(e) => eprintln!("[research] uncited source not kept: {e}"),
        }
    }
    let pruned = kms_writer::prune_uncited_sources(&kref);
    if uncited > 0 || pruned > 0 {
        eprintln!(
            "[research] kept {uncited} read-but-uncited source(s) under sources/{}/ ({pruned} older than {} days removed)",
            kms_writer::UNCITED_DIR,
            kms_writer::UNCITED_KEEP_DAYS
        );
    }
    // The last note-driven rebuild ran before the sources were complete,
    // so `index.md` listed 1 of a vault's 33 sources. Once more, at the end.
    if let Err(e) = crate::kms::rebuild_index(&kref) {
        eprintln!("[research] index rebuild failed: {e}");
    }
    // dev-plan/64 P5.4: what the run produced, recorded once and only
    // now — a count kept while the run is in flight invites reading a
    // job that died as a job that produced something.
    mgr.record_pages_written(job_id, written.len() as u32);
    mgr.update_cost(job_id, llm_calls::usage_so_far().cost_usd());
    let _ = write::write_run_log(
        &kref,
        &RunLog {
            query: &query,
            topic_slug: &topic_slug,
            mode,
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

/// Archive every cited source that is not on disk yet. Locally ingested
/// sources are skipped — they were archived under their alias at ingest.
fn archive_cited(
    kms_name: &str,
    query: &str,
    today: &str,
    sources: &[ResearchSource],
    cited: &BTreeSet<u32>,
    archived: &mut BTreeSet<u32>,
) {
    for s in sources {
        if !cited.contains(&s.index) || archived.contains(&s.index) || s.url.starts_with("kms://") {
            continue;
        }
        match kms_writer::write_source(kms_name, query, today, s.index, &s.title, &s.url, &s.body) {
            Ok(_) => {
                archived.insert(s.index);
            }
            Err(e) => eprintln!("[research] source [{}] not written: {e}", s.index),
        }
    }
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
    digest::digest_many(
        provider.clone(),
        model,
        query,
        new_sources,
        known_slugs,
        kref,
        today,
        config.llm_timeout,
        cancel,
        &config.language,
    )
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
    if let Some(i) = out.rfind(|c: char| c == '.' || c == '\n') {
        out.truncate(i + 1);
    }
    out
}

/// A NEW note keeps only the paragraphs it can cite (`write::prune_uncited`).
/// An update is left alone: it merges a page that may hold what a person
/// wrote, and a person's paragraphs carry no markers.
fn prune_new_note(n: &NotePlan, body: String, warnings: &mut Vec<String>) -> String {
    if n.action != plan::Action::Create {
        return body;
    }
    let (pruned, dropped) = write::prune_uncited(&body);
    if dropped > 0 {
        let w = format!(
            "`{}`: removed {dropped} paragraph(s) the model wrote without a citation",
            n.slug
        );
        eprintln!("[research] {w}");
        warnings.push(w);
    }
    pruned
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

    /// An ingest run's topic page must land on the stub the ingest just
    /// wrote, and every topic name must be one the KMS will accept.
    /// Ingesting a Thai document failed with `invalid page name ''`
    /// *after* the ingest had written `pages/<Thai alias>.md`.
    #[test]
    fn an_update_keeps_the_page_its_own_language() {
        let thai = "Collingridge Dilemma คือกรอบคิดที่ชี้ว่าเทคโนโลยีใหม่มีช่วงเวลาที่เรารู้ผลกระทบช้า";
        // The run is English because the ingested document is; the page
        // being updated is Thai and stays Thai.
        assert_eq!(note_language(Action::Update, Some(thai), "en"), "th");
        // A new note follows the run, whatever happens to sit on disk.
        assert_eq!(note_language(Action::Create, Some(thai), "en"), "en");
        // Nothing to read, or nothing decisive in it: the run decides.
        assert_eq!(note_language(Action::Update, None, "en"), "en");
        assert_eq!(note_language(Action::Update, Some("   "), "en"), "en");
        assert_eq!(note_language(Action::Update, Some("123 456"), "en"), "en");
    }

    #[test]
    fn a_topic_page_lands_on_the_stub_the_ingest_wrote() {
        // `kms::ingest` names the stub with `sanitize_alias`, so that is
        // the only spelling that fills it in rather than writing beside it.
        for alias in ["ยุคที่ความฉลาดล้นเหลือ", "日本語", "my_report", "notes-2026"]
        {
            assert_eq!(
                ingest_slug(alias),
                crate::kms::sanitize_alias(alias),
                "topic page must match pages/{alias}.md"
            );
        }

        assert_eq!(
            pinned_slug("Quantum Error Correction"),
            "quantum-error-correction"
        );

        // Neither helper may return the empty string — it is the one page
        // name `writable_page_path` refuses outright, and both feed a
        // filename.
        let kref = crate::kms::KmsRef {
            name: "t".into(),
            scope: crate::kms::KmsScope::Project,
            root: std::env::temp_dir().join("thclaws-page-slug-test"),
        };
        for raw in [
            "ยุคที่ความฉลาดล้นเหลือ",
            "日本語",
            "  ",
            "///",
            "---",
            "..",
            "a/b",
        ] {
            for slug in [ingest_slug(raw), pinned_slug(raw)] {
                assert!(!slug.is_empty(), "empty slug for {raw:?}");
                assert!(
                    crate::kms::writable_page_path(&kref, &slug).is_ok(),
                    "KMS refused {slug:?} (from {raw:?})"
                );
            }
        }
    }

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
            let prompt = format!("{}\n{prompt}", req.system.clone().unwrap_or_default());
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
                    usage: Some(crate::providers::Usage {
                        input_tokens: 100,
                        output_tokens: 10,
                        cache_read_input_tokens: Some(40),
                        ..Default::default()
                    }),
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

    /// Answers a local-document ingest entirely in Thai — which is what
    /// the model does when the document is Thai.
    struct ThaiProvider;

    #[async_trait]
    impl Provider for ThaiProvider {
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
            let prompt = format!("{}\n{prompt}", req.system.clone().unwrap_or_default());
            let body = if prompt.contains("You are extracting structured knowledge") {
                r#"{"entities":[{"name":"ความอุดมสมบูรณ์","slug":"ความอุดมสมบูรณ์","kind":"concept"}],
                    "claims":[{"text":"ความฉลาดจะถูกลง","quote":"ความฉลาดจะถูกลง","entities":["ความอุดมสมบูรณ์"],"confidence":0.9}],
                    "links_to_known":[]}"#
                    .to_string()
            } else if prompt.contains("planning a knowledge-base entry") {
                r#"[{"slug":"ความอุดมสมบูรณ์","kind":"concept","title":"ความอุดมสมบูรณ์","action":"create","claim_ids":["s1c1"],"related":[]}]"#
                    .to_string()
            } else if prompt.contains("You are writing ONE zettelkasten note") {
                "ความฉลาดจะถูกลงเรื่อย ๆ [c:s1c1].".to_string()
            } else {
                String::new()
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

    /// Ingesting a Thai document as atomic notes, end to end.
    ///
    /// This is the run that failed with `tool error: invalid page name ''`
    /// — after the ingest had already written the source and the stub, so
    /// the KMS was left holding a document with no notes. It broke in two
    /// places, and a helper-level test caught only the first, so this
    /// drives the whole pipeline: the topic page must fill in the stub the
    /// ingest wrote, under its Thai name, and the Thai note beside it must
    /// exist rather than having been filtered out for sanitising to "".
    #[tokio::test]
    async fn a_thai_document_ingests_into_the_stub_the_ingest_wrote() {
        let _h = crate::research::test_helpers::scoped_home();
        let alias = "ยุคที่ความฉลาดล้นเหลือ";
        let kref = crate::kms::create("abundance", crate::kms::KmsScope::Project).unwrap();
        std::fs::create_dir_all(kref.root.join("sources")).unwrap();
        std::fs::create_dir_all(kref.root.join("pages")).unwrap();
        std::fs::write(
            kref.root.join("sources").join(format!("{alias}.md")),
            "---\ntitle: \"ยุคที่ความฉลาดล้นเหลือ\"\n---\n\nความฉลาดจะถูกลง\n",
        )
        .unwrap();
        // What `kms::ingest` leaves behind for the run to adopt.
        std::fs::write(
            kref.root.join("pages").join(format!("{alias}.md")),
            "---\ntitle: \"ยุคที่ความฉลาดล้นเหลือ\"\ntype: note\n---\n\nstub\n",
        )
        .unwrap();

        let cfg = JobConfig {
            kms_target: Some("abundance".into()),
            local_source: Some(alias.into()),
            legacy: false,
            min_iter: 1,
            max_iter: 1,
            ..JobConfig::default()
        };
        let (id, cancel) = manager().register(alias.into(), &cfg);
        let result = run_with_tools(
            &id,
            alias.into(),
            cfg,
            Arc::new(ThaiProvider),
            "mock".into(),
            cancel,
            Arc::new(Tools),
            None,
        )
        .await
        .expect("a Thai document must ingest");

        assert_eq!(result, format!("abundance/{alias}.md"));
        let moc = std::fs::read_to_string(kref.root.join("pages").join(format!("{alias}.md")))
            .expect("the topic page is the stub, filled in");
        assert!(moc.contains("kind: moc"), "stub not adopted as MOC: {moc}");
        assert!(
            kref.root.join("pages/ความอุดมสมบูรณ์.md").exists(),
            "a Thai note slug must survive planning"
        );
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
        let result = llm_calls::track_usage(run_with_tools(
            &id,
            "กฎหมายแรงงานไทย".into(),
            cfg,
            provider.clone(),
            "mock".into(),
            cancel,
            Arc::new(Tools),
            None,
        ))
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
        // dev-plan/64 P2.7: the run log says what the run cost. Every call
        // of the run is in it — digests and notes run concurrently, and the
        // ledger has to follow them.
        let log = std::fs::read_to_string(runs[0].as_ref().unwrap().path()).unwrap();
        let calls = provider.calls.lock().unwrap().len();
        assert!(
            log.contains(&format!("llm_calls: {calls}\n")),
            "{calls} calls:\n{log}"
        );
        assert!(
            log.contains(&format!("input_tokens: {}\n", calls * 100)),
            "{log}"
        );
        assert!(
            log.contains(&format!("cached_input_tokens: {}\n", calls * 40)),
            "{log}"
        );
        assert!(
            !log.contains("cost_usd"),
            "an unpriced model has no cost: {log}"
        );
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
        let result = run_with_tools(
            &id,
            "กฎหมายแรงงานไทย".into(),
            cfg,
            provider.clone(),
            "mock".into(),
            cancel,
            Arc::new(Tools),
            None,
        )
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
            run_with_tools(
                &id,
                "กฎหมายแรงงานไทย".into(),
                cfg,
                provider.clone(),
                "mock".into(),
                cancel,
                Arc::new(Tools),
                None,
            )
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
        run_with_tools(
            &id,
            "กฎหมายแรงงานไทย".into(),
            cfg,
            provider.clone(),
            "mock".into(),
            cancel,
            Arc::new(Tools),
            None,
        )
        .await
        .unwrap();
        let kref = crate::kms::resolve("labour-kb").unwrap();
        let pages_before = kref.root.join("pages").read_dir().unwrap().count();
        let before = std::fs::read_to_string(kref.root.join("pages/overtime-pay.md")).unwrap();

        let ids = crate::research::start_refresh(
            "labour-kb".into(),
            vec!["overtime-pay".into()],
            30,
            JobConfig::default(),
            provider.clone(),
            "mock".into(),
            None,
            Some(Arc::new(Tools)),
        )
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
        let err = crate::research::start_refresh(
            "labour-kb".into(),
            vec!["nope".into()],
            30,
            JobConfig::default(),
            provider,
            "mock".into(),
            None,
            None,
        )
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(err.contains("no note 'nope'"), "{err}");
    }
}
