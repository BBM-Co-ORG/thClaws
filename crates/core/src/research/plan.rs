//! Research v2 step 3: turn the entity/claim tables into a note plan —
//! which atomic notes to create or update, and one map-of-content.

use super::digest::{sanitize_slug, Claim, Digest};
use super::graph::KnownNote;
use super::llm_calls::{oneshot, oneshot_planning};
use crate::cancel::CancelToken;
use crate::error::Result;
use crate::providers::Provider;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

pub const HARD_MAX_NOTES: u32 = 50;

/// Entities listed in the planning prompt, best-evidenced first. A run
/// that reads 40 sources finds a few hundred entities, most of them
/// mentioned once; listing all of them buries the twenty that deserve a
/// note and costs the reply budget the plan itself needs.
const PLAN_ENTITY_ROWS: usize = 150;
/// Claims listed in the planning prompt. Claims attach to notes by
/// entity tag, so this list is context for the topic page's outline,
/// not the assignment itself — it is sampled across sources rather
/// than truncated, so no source is invisible to the planner.
const PLAN_CLAIM_ROWS: usize = 240;

/// Take up to `cap` claims, round-robin across sources, so a cap never
/// silently hides everything a late round found.
fn sample_claims(claims: &[Claim], cap: usize) -> Vec<&Claim> {
    if claims.len() <= cap {
        return claims.iter().collect();
    }
    let mut by_source: BTreeMap<u32, Vec<&Claim>> = BTreeMap::new();
    for c in claims {
        by_source.entry(c.source).or_default().push(c);
    }
    let mut out: Vec<&Claim> = Vec::with_capacity(cap);
    let mut round = 0usize;
    while out.len() < cap {
        let mut added = false;
        for list in by_source.values() {
            if let Some(c) = list.get(round) {
                out.push(c);
                added = true;
                if out.len() == cap {
                    break;
                }
            }
        }
        if !added {
            break;
        }
        round += 1;
    }
    out.sort_by(|a, b| (a.source, a.id.as_str()).cmp(&(b.source, b.id.as_str())));
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Entity,
    Concept,
    Claim,
    Moc,
}

impl NoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Concept => "concept",
            Self::Claim => "claim",
            Self::Moc => "moc",
        }
    }
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "entity" => Self::Entity,
            "claim" => Self::Claim,
            "moc" => Self::Moc,
            _ => Self::Concept,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Create,
    Update,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotePlan {
    pub slug: String,
    pub kind: NoteKind,
    pub title: String,
    pub action: Action,
    pub claim_ids: Vec<String>,
    pub related: Vec<String>,
    /// Why the topic page links here — one clause, shown in the MOC's
    /// map and given to the child writer so it goes deeper, not wider.
    pub role: String,
    /// Topic page only: the `##` sections it should describe, in order.
    pub outline: Vec<String>,
}

/// Aggregated view of a run's digests, the only thing the planner and
/// the MOC writer ever see (no source bodies).
#[derive(Debug, Default)]
pub struct Tables {
    /// slug → (display name, kind, claim count, distinct sources)
    pub entities: BTreeMap<String, (String, String, u32, u32)>,
    pub claims: Vec<Claim>,
}

pub fn build_tables(digests: &[Digest]) -> Tables {
    let mut t = Tables::default();
    let mut sources_per_entity: HashMap<String, HashSet<u32>> = HashMap::new();
    for d in digests {
        for e in &d.entities {
            let entry = t
                .entities
                .entry(e.slug.clone())
                .or_insert_with(|| (e.name.clone(), e.kind.clone(), 0, 0));
            if entry.1.is_empty() {
                entry.1 = e.kind.clone();
            }
        }
        for c in &d.claims {
            for slug in &c.entities {
                let entry = t
                    .entities
                    .entry(slug.clone())
                    .or_insert_with(|| (slug.replace('-', " "), String::new(), 0, 0));
                entry.2 += 1;
                sources_per_entity
                    .entry(slug.clone())
                    .or_default()
                    .insert(c.source);
            }
            t.claims.push(c.clone());
        }
    }
    for (slug, set) in sources_per_entity {
        if let Some(e) = t.entities.get_mut(&slug) {
            e.3 = set.len() as u32;
        }
    }
    t
}

/// Bounds on the existing-notes block of the plan prompt.
const KNOWN_NOTE_ROWS: usize = 150;
const KNOWN_NOTE_BYTES: usize = 24 * 1024;

pub fn build_plan_prompt(
    query: &str,
    tables: &Tables,
    known: &[KnownNote],
    topic_slug: &str,
    max_notes: u32,
    language: &str,
    anchor: Option<&str>,
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Research query: {query}\n{}\n\n\
         You are planning a knowledge-base entry for this query. Work TOP-DOWN:\n\
         1. The TOPIC PAGE (kind `moc`) must DESCRIBE the subject as a report — decide its outline: \
            4–8 `##` sections that together answer the query (e.g. landscape, the main players, \
            how they compare, timeline, money/policy, trends, what is disputed).\n\
         2. Decide what that page must LINK to for depth. Each link target becomes its own note — \
            ONE NOTE = ONE IDEA (an organisation, a person, a product line, a concept, a single \
            pivotal claim). Notes link to each other with [[slug]].\n\
         3. Say which ENTITIES (slugs from the table below) each note covers. Claims are attached \
            automatically from their entity tags — you do not list claim ids. The topic page \
            receives every claim.\n\n",
        super::today_context()
    ));
    if !known.is_empty() {
        s.push_str("=== Notes that ALREADY EXIST (slug — title — summary). Prefer `update` over a new near-duplicate ===\n");
        // Most relevant first, then cut — by rows and by bytes, since a
        // Thai summary is three bytes a character and 150 of them were
        // ~100 KB of every plan prompt.
        let entity_slugs: std::collections::HashSet<&str> =
            tables.entities.keys().map(String::as_str).collect();
        let about = format!(
            "{query} {}",
            tables
                .entities
                .values()
                .map(|e| e.0.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let ranked = super::graph::rank_known(known, &about, &entity_slugs);
        let (mut shown, mut bytes) = (0usize, 0usize);
        for k in &ranked {
            let row = format!("- {} — {} — {}\n", k.slug, k.title, k.summary);
            if shown == KNOWN_NOTE_ROWS || bytes + row.len() > KNOWN_NOTE_BYTES {
                break;
            }
            bytes += row.len();
            shown += 1;
            s.push_str(&row);
        }
        if shown < ranked.len() {
            s.push_str(&format!(
                "… {} more existing notes, less related to this run, are not listed — a slug you invent may already exist.\n",
                ranked.len() - shown
            ));
        }
        s.push('\n');
    }
    let mut ents: Vec<_> = tables.entities.iter().collect();
    ents.sort_by(|a, b| (b.1 .3, b.1 .2).cmp(&(a.1 .3, a.1 .2)).then(a.0.cmp(b.0)));
    let ent_total = ents.len();
    ents.truncate(PLAN_ENTITY_ROWS);
    s.push_str(&format!(
        "=== Entities found in this run, best-evidenced first (slug — name — kind — claims — sources){} ===\n",
        if ent_total > ents.len() {
            format!(" — {} more with less evidence are omitted", ent_total - ents.len())
        } else {
            String::new()
        }
    ));
    for (slug, (name, kind, claims, sources)) in ents {
        s.push_str(&format!(
            "- {slug} — {name} — {kind} — {claims} — {sources}\n"
        ));
    }
    let sampled = sample_claims(&tables.claims, PLAN_CLAIM_ROWS);
    s.push_str(&format!(
        "\n=== Claims (id — text — entities — source — published){} ===\n",
        if tables.claims.len() > sampled.len() {
            format!(
                " — {} of {}, sampled across sources",
                sampled.len(),
                tables.claims.len()
            )
        } else {
            String::new()
        }
    ));
    for c in sampled {
        s.push_str(&format!(
            "- {} — {} — [{}] — src {} — {}\n",
            c.id,
            clamp(&c.text, 160),
            c.entities.join(", "),
            c.source,
            c.published.as_deref().unwrap_or("?")
        ));
    }
    if let Some(a) = anchor {
        let title = known
            .iter()
            .find(|k| k.slug == a)
            .map(|k| k.title.as_str())
            .unwrap_or(a);
        s.push_str(&format!(
            "=== REFRESH MODE ===\nThis run refreshes ONE existing note: `{a}` ({title}). Plan exactly that note, \
             action `update`, and give it every claim that concerns it (newest facts first). Do NOT create a MOC and \
             do NOT plan any other note — a refresh adds references and citations to the page it was asked about; \
             it does not grow the knowledge base.\n\n"
        ));
    }
    s.push_str(&format!(
        "\nRules:\n\
         - The topic page: slug `{topic_slug}`, kind `moc`, title = the query's topic, `outline` = its section headings \
           (title language). It is a synthesis, not an index, and gets every claim automatically. \
           Its `related` lists every other note in the plan.\n\
         - A thing gets its own note when claims about it come from ≥ 2 sources, or it has ≥ 3 claims and the topic \
           page needs to link it. Below that, fold it into its parent (a model into its maker, a person into their company).\n\
         - When the query is about organisations or people, one note per organisation/person comes first; a product or \
           model gets a separate note only when it has ≥ 4 claims of its own.\n\
         - A slug names what the note is ABOUT, at the scope the note actually covers, and agrees with its title: if the \
           title needed a qualifier, so does the slug. The plain name of a thing is reserved for a note about that thing. \
           A note covering one aspect of it scopes the slug to that aspect — `claude-code-subagent-nesting`, never \
           `claude-code`, for a note about how deeply Claude Code nests subagents. Slugs are permanent, so a note that \
           takes the plain name leaves the thing itself nowhere to live later.\n\
         - Do NOT drop a well-sourced entity to stay under the cap of {max_notes} notes INCLUDING the topic page; \
           merge thin ones instead.\n\
         - `role` = one clause (title language) saying why the topic page links to this note.\n\
         - `update` an existing slug when the run adds claims about it; `create` otherwise. Never rename existing slugs.\n\
         - `entities` = the entity slugs (from the table) a note covers: its own thing plus anything folded into it \
           (a company note lists the company AND its models/founders that did not get their own note). \
           `claim_ids` is optional — only for claims the entity tags would miss.\n\
         - `related` = other slugs (from this plan or existing) the note should link to; at most 6 per note.\n\
         - Keep titles short (≤ 8 words) and output compact JSON (no pretty-printing) — the reply must fit in one response.\n\
         - Title language: {lang_rule}\n\n\
         Output STRICT JSON array, topic page FIRST, no fence, no commentary:\n\
         [\n  \
           {{\"slug\": \"{topic_slug}\", \"kind\": \"moc\", \"title\": \"…\", \"action\": \"create|update\", \"outline\": [\"…\", …], \"related\": [\"slug\", …]}},\n  \
           {{\"slug\": \"…\", \"kind\": \"entity|concept|claim\", \"title\": \"…\", \"action\": \"create|update\", \"role\": \"…\", \"entities\": [\"slug\", …], \"related\": [\"slug\", …]}},\n  \
           …\n\
         ]",
        lang_rule = super::language_rule(language)
    ));
    s
}

#[derive(Deserialize)]
struct RawNote {
    slug: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    claim_ids: Vec<String>,
    #[serde(default)]
    related: Vec<String>,
    #[serde(default)]
    role: String,
    #[serde(default)]
    outline: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
}

/// Parse + enforce the plan rules: valid claim ids only, `update` only
/// for known slugs, dedup, cap, exactly one MOC (synthesised if the
/// LLM forgot it, carrying every unassigned claim).
pub fn parse_plan(
    raw: &str,
    tables: &Tables,
    known: &[KnownNote],
    topic_slug: &str,
    topic_title: &str,
    max_notes: u32,
    anchor: Option<&str>,
) -> Vec<NotePlan> {
    parse_plan_report(
        raw,
        tables,
        known,
        topic_slug,
        topic_title,
        max_notes,
        anchor,
    )
    .0
}

/// Split a (possibly broken) JSON array into its top-level `{…}` objects
/// by brace matching, string-aware, and parse each on its own. Trailing
/// commas inside an object are repaired; anything else is skipped and
/// counted. A truncated last object is simply never closed → skipped.
fn lenient_objects(s: &str) -> (Vec<RawNote>, usize) {
    let trailing_comma = regex::Regex::new(r",\s*([}\]])").expect("static regex");
    let mut out = Vec::new();
    let mut skipped = 0usize;
    let bytes = s.as_bytes();
    let (mut depth, mut in_str, mut esc, mut start) = (0i32, false, false, None::<usize>);
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(st) = start.take() {
                        let obj = &s[st..=i];
                        let parsed = serde_json::from_str::<RawNote>(obj).or_else(|_| {
                            serde_json::from_str::<RawNote>(&trailing_comma.replace_all(obj, "$1"))
                        });
                        match parsed {
                            Ok(n) => out.push(n),
                            Err(_) => skipped += 1,
                        }
                    }
                }
                if depth < 0 {
                    depth = 0;
                }
            }
            _ => {}
        }
    }
    if start.is_some() {
        skipped += 1;
    }
    (out, skipped)
}

/// Same as [`parse_plan`] plus human-readable warnings for the run log
/// (plan truncated / did not parse), so a one-page result explains itself.
pub fn parse_plan_report(
    raw: &str,
    tables: &Tables,
    known: &[KnownNote],
    topic_slug: &str,
    topic_title: &str,
    max_notes: u32,
    anchor: Option<&str>,
) -> (Vec<NotePlan>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let stripped = super::digest::extract_json(raw, '[', ']');
    let parsed: Vec<RawNote> = match serde_json::from_str(stripped) {
        Ok(v) => v,
        Err(e) => {
            // Object-by-object: a trailing comma, a comment, or a cut-off
            // tail only costs the object it sits in, not the whole plan.
            let (objs, skipped) = lenient_objects(&raw[raw.find('[').unwrap_or(0)..]);
            if objs.is_empty() {
                let w = format!(
                    "note plan did not parse ({e}); planning from the entity table instead. Head: {}",
                    raw.chars().take(160).collect::<String>().replace('\n', " ")
                );
                eprintln!("[research] {w}");
                warnings.push(w);
            } else {
                let w = format!(
                    "note plan JSON was malformed ({e}); recovered {} notes, skipped {skipped}",
                    objs.len()
                );
                eprintln!("[research] {w}");
                warnings.push(w);
            }
            objs
        }
    };
    let valid_ids: HashSet<&str> = tables.claims.iter().map(|c| c.id.as_str()).collect();
    let known_slugs: HashSet<&str> = known.iter().map(|k| k.slug.as_str()).collect();
    let cap = max_notes.clamp(1, HARD_MAX_NOTES) as usize;

    let mut seen = HashSet::new();
    let mut notes: Vec<NotePlan> = Vec::new();
    let mut moc: Option<NotePlan> = None;
    for r in parsed {
        let slug = sanitize_slug(&r.slug);
        if slug.is_empty() || crate::kms::is_reserved_page_stem(&slug) || !seen.insert(slug.clone())
        {
            continue;
        }
        let kind = NoteKind::parse(&r.kind);
        let mut claim_ids: Vec<String> = r
            .claim_ids
            .into_iter()
            .filter(|id| valid_ids.contains(id.as_str()))
            .collect();
        // Claims attach by entity tag; a note that names no entities
        // covers its own slug.
        let mut ents: Vec<String> = r
            .entities
            .iter()
            .map(|s| sanitize_slug(s))
            .filter(|s| !s.is_empty())
            .collect();
        if ents.is_empty() && kind != NoteKind::Moc {
            ents.push(slug.clone());
        }
        if kind != NoteKind::Moc {
            for c in &tables.claims {
                if c.entities.iter().any(|e| ents.contains(e)) && !claim_ids.contains(&c.id) {
                    claim_ids.push(c.id.clone());
                }
            }
        }
        let action = if r.action.trim().eq_ignore_ascii_case("update")
            && known_slugs.contains(slug.as_str())
        {
            Action::Update
        } else {
            Action::Create
        };
        let related: Vec<String> = r
            .related
            .iter()
            .map(|s| sanitize_slug(s))
            .filter(|s| !s.is_empty() && s != &slug)
            .collect();
        let title = if r.title.trim().is_empty() {
            slug.replace('-', " ")
        } else {
            r.title.trim().to_string()
        };
        let note = NotePlan {
            slug,
            kind,
            title,
            action,
            claim_ids,
            related,
            role: r.role.trim().to_string(),
            outline: r
                .outline
                .iter()
                .map(|h| h.trim().trim_start_matches('#').trim().to_string())
                .filter(|h| !h.is_empty())
                .take(10)
                .collect(),
        };
        if kind == NoteKind::Moc {
            if moc.is_none() {
                moc = Some(note);
            }
            continue;
        }
        if claim_ids_len(&note) == 0 {
            continue;
        }
        notes.push(note);
    }
    // A planned slug that is a new name for a note the KMS already has
    // (same title) must update it, not fork it.
    alias_to_known(&mut notes, known);
    if notes.len() > cap.saturating_sub(1) {
        // The cap decides which notes exist at all, so spend it on the
        // best-evidenced ones rather than on whichever the model listed
        // first. Evidence = distinct sources, then claim count.
        let source_of: HashMap<&str, u32> = tables
            .claims
            .iter()
            .map(|c| (c.id.as_str(), c.source))
            .collect();
        let score = |n: &NotePlan| -> (usize, usize) {
            let srcs: HashSet<u32> = n
                .claim_ids
                .iter()
                .filter_map(|id| source_of.get(id.as_str()).copied())
                .collect();
            (srcs.len(), n.claim_ids.len())
        };
        let mut scored: Vec<(usize, (usize, usize), NotePlan)> = notes
            .drain(..)
            .enumerate()
            .map(|(i, n)| (i, score(&n), n))
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.truncate(cap.saturating_sub(1));
        scored.sort_by_key(|(i, _, _)| *i);
        notes = scored.into_iter().map(|(_, _, n)| n).collect();
    }
    if notes.is_empty() && anchor.is_none() {
        notes = fallback_notes(tables, known, topic_slug, cap.saturating_sub(1));
        if !notes.is_empty() {
            let w = format!(
                "no child notes in the plan; created {} from the entity table (entities with ≥ 2 sources or ≥ 3 claims)",
                notes.len()
            );
            eprintln!("[research] {w}");
            warnings.push(w);
        }
    }

    let assigned: HashSet<&str> = notes
        .iter()
        .flat_map(|n| n.claim_ids.iter().map(String::as_str))
        .collect();
    let orphans: Vec<String> = tables
        .claims
        .iter()
        .filter(|c| !assigned.contains(c.id.as_str()))
        .map(|c| c.id.clone())
        .collect();
    if let Some(a) = anchor {
        // Refresh mode: the anchored note is the only note. Owner's rule
        // (2026-09-20): "refresh should only refresh — no new pages, just
        // references and citations." One refresh of `jevons-paradox` had
        // written ten pages, nine of them new (Nvidia, DeepSeek, Nadella),
        // and a vault about abundance began turning into AI-compute news.
        // Every other planned note is dropped here, whatever the model
        // planned; its claims fall to the anchor with the other orphans,
        // so nothing the run learned is lost — it is cited where asked.
        let a = sanitize_slug(a);
        let title = known
            .iter()
            .find(|k| k.slug == a)
            .map(|k| k.title.clone())
            .unwrap_or_else(|| topic_title.to_string());
        let kind = known
            .iter()
            .find(|k| k.slug == a)
            .map(|k| NoteKind::parse(&k.kind))
            .unwrap_or(NoteKind::Concept);
        let pos = notes.iter().position(|n| n.slug == a);
        let mut anchor_note = match pos {
            Some(i) => notes.remove(i),
            None => NotePlan {
                slug: a.clone(),
                kind,
                title,
                action: Action::Update,
                claim_ids: Vec::new(),
                related: Vec::new(),
                role: String::new(),
                outline: Vec::new(),
            },
        };
        anchor_note.action = Action::Update;
        if anchor_note.kind == NoteKind::Moc {
            anchor_note.kind = kind;
        }
        for id in orphans {
            if !anchor_note.claim_ids.contains(&id) {
                anchor_note.claim_ids.push(id);
            }
        }
        let dropped: Vec<String> = notes
            .iter()
            .filter(|n| n.slug != a)
            .map(|n| n.slug.clone())
            .collect();
        for n in notes.iter().filter(|n| n.slug != a) {
            for id in &n.claim_ids {
                if !anchor_note.claim_ids.contains(id) {
                    anchor_note.claim_ids.push(id.clone());
                }
            }
        }
        if !dropped.is_empty() {
            let w = format!(
                "refresh writes only `{a}`: {} other planned note(s) were not created, and their claims went to it — {}",
                dropped.len(),
                dropped.join(", ")
            );
            eprintln!("[research] {w}");
            warnings.push(w);
        }
        let mut notes = vec![anchor_note];
        // Links only to notes that exist: nothing new is being written.
        prune_related(&mut notes, &known_slugs);
        return (notes, warnings);
    }
    // `topic_slug` arrives as a page name the caller already resolved —
    // for an ingest run it is the alias of the stub at `pages/<alias>.md`
    // that this MOC is about to become. Re-slugging it here is how a Thai
    // topic page ended up named "", and it would still fold an alias's
    // '_' to '-' and miss the stub. Take it as given.
    let mut moc = moc.unwrap_or_else(|| NotePlan {
        slug: topic_slug.to_string(),
        kind: NoteKind::Moc,
        title: topic_title.to_string(),
        action: Action::Create,
        claim_ids: Vec::new(),
        related: Vec::new(),
        role: String::new(),
        outline: Vec::new(),
    });
    moc.slug = topic_slug.to_string();
    moc.kind = NoteKind::Moc;
    moc.claim_ids = tables.claims.iter().map(|c| c.id.clone()).collect();
    moc.action = if known_slugs.contains(moc.slug.as_str()) {
        Action::Update
    } else {
        Action::Create
    };
    for id in orphans {
        if !moc.claim_ids.contains(&id) {
            moc.claim_ids.push(id);
        }
    }
    for n in &notes {
        if !moc.related.contains(&n.slug) {
            moc.related.push(n.slug.clone());
        }
    }
    notes.retain(|n| n.slug != moc.slug);
    notes.push(moc);
    prune_related(&mut notes, &known_slugs);
    (notes, warnings)
}

/// A model asked to plan `deep-seek` when the KMS already holds
/// `deepseek` (same title) creates a second page for one idea and
/// splits the graph. Rewrite such a slug — and every reference to it —
/// onto the existing note, and make the plan update it.
fn alias_to_known(notes: &mut Vec<NotePlan>, known: &[KnownNote]) {
    let by_title: HashMap<String, &KnownNote> = known
        .iter()
        .filter(|k| !k.title.trim().is_empty())
        .map(|k| (normalize_title(&k.title), k))
        .collect();
    let known_slugs: HashSet<&str> = known.iter().map(|k| k.slug.as_str()).collect();
    let mut alias: HashMap<String, String> = HashMap::new();
    for n in notes.iter_mut() {
        if n.kind == NoteKind::Moc || known_slugs.contains(n.slug.as_str()) {
            continue;
        }
        if let Some(k) = by_title.get(&normalize_title(&n.title)) {
            if k.slug != n.slug {
                alias.insert(n.slug.clone(), k.slug.clone());
                n.slug = k.slug.clone();
                n.action = Action::Update;
            }
        }
    }
    if alias.is_empty() {
        return;
    }
    for n in notes.iter_mut() {
        for r in n.related.iter_mut() {
            if let Some(target) = alias.get(r.as_str()) {
                *r = target.clone();
            }
        }
        n.related.retain(|r| r != &n.slug);
        n.related.dedup();
    }
    // Two planned notes can now share a slug; merge the later into the
    // earlier so one idea stays one note.
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut merged: Vec<NotePlan> = Vec::with_capacity(notes.len());
    for n in notes.drain(..) {
        match seen.get(&n.slug).copied() {
            Some(i) => {
                let first: &mut NotePlan = &mut merged[i];
                for id in n.claim_ids {
                    if !first.claim_ids.contains(&id) {
                        first.claim_ids.push(id);
                    }
                }
                for r in n.related {
                    if r != first.slug && !first.related.contains(&r) {
                        first.related.push(r);
                    }
                }
            }
            None => {
                seen.insert(n.slug.clone(), merged.len());
                merged.push(n);
            }
        }
    }
    *notes = merged;
}

/// Notes are merged on this key, so it must never make two different
/// titles equal — see [`crate::kms::fold_for_compare`] for how the old
/// `is_alphanumeric` filter did exactly that to Thai.
fn normalize_title(t: &str) -> String {
    crate::kms::fold_for_compare(t)
}

/// Drop `related` slugs that neither this plan nor the KMS will have —
/// a dangling entry in frontmatter is a broken link in the browser.
fn prune_related(notes: &mut [NotePlan], known: &HashSet<&str>) {
    let planned: HashSet<String> = notes.iter().map(|n| n.slug.clone()).collect();
    for n in notes.iter_mut() {
        n.related
            .retain(|r| planned.contains(r) || known.contains(r.as_str()));
    }
}

/// Deterministic plan when the model's plan is unusable: one note per
/// well-sourced entity, claims attached by entity tag, MOC synthesised
/// by the caller. Guarantees a run never collapses to a single page.
fn fallback_notes(
    tables: &Tables,
    known: &[KnownNote],
    topic_slug: &str,
    cap: usize,
) -> Vec<NotePlan> {
    let known_slugs: HashSet<&str> = known.iter().map(|k| k.slug.as_str()).collect();
    let topic = sanitize_slug(topic_slug);
    let mut ents: Vec<_> = tables
        .entities
        .iter()
        .filter(|(slug, (_, _, claims, sources))| {
            **slug != topic && (*sources >= 2 || *claims >= 3)
        })
        .collect();
    ents.sort_by(|a, b| (b.1 .3, b.1 .2).cmp(&(a.1 .3, a.1 .2)).then(a.0.cmp(b.0)));
    ents.truncate(cap);
    ents.into_iter()
        .map(|(slug, (name, kind, _, _))| {
            let kind = match kind.as_str() {
                "org" | "person" | "product" | "place" | "law" | "event" => NoteKind::Entity,
                _ => NoteKind::Concept,
            };
            let claim_ids: Vec<String> = tables
                .claims
                .iter()
                .filter(|c| c.entities.iter().any(|e| e == slug))
                .map(|c| c.id.clone())
                .collect();
            NotePlan {
                slug: slug.clone(),
                kind,
                title: if name.trim().is_empty() {
                    slug.replace('-', " ")
                } else {
                    name.clone()
                },
                action: if known_slugs.contains(slug.as_str()) {
                    Action::Update
                } else {
                    Action::Create
                },
                claim_ids,
                related: Vec::new(),
                role: String::new(),
                outline: Vec::new(),
            }
        })
        .collect()
}

fn claim_ids_len(n: &NotePlan) -> usize {
    n.claim_ids.len()
}

fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut o: String = s.chars().take(max).collect();
        o.push('…');
        o
    }
}

pub struct PlanningRequest<'a> {
    pub provider: &'a dyn Provider,
    pub model: &'a str,
    pub query: &'a str,
    pub tables: &'a Tables,
    pub known: &'a [KnownNote],
    pub topic_slug: &'a str,
    pub topic_title: &'a str,
    pub max_notes: u32,
    pub timeout: Duration,
    pub cancel: &'a CancelToken,
    pub language: &'a str,
    pub anchor: Option<&'a str>,
}

pub async fn plan_notes(context: PlanningRequest<'_>) -> Result<PlanOutcome> {
    let PlanningRequest {
        provider,
        model,
        query,
        tables,
        known,
        topic_slug,
        topic_title,
        max_notes,
        timeout,
        cancel,
        language,
        anchor,
    } = context;
    let prompt = build_plan_prompt(
        query, tables, known, topic_slug, max_notes, language, anchor,
    );
    let raw = oneshot_planning(provider, model, prompt, timeout, cancel).await?;
    let (mut notes, warnings) = parse_plan_report(
        &raw,
        tables,
        known,
        topic_slug,
        topic_title,
        max_notes,
        anchor,
    );
    // dev-plan/64 P4.3: a child note's `role` says why the topic page
    // links to it, and becomes its `topic:`. The topic page has no
    // parent to be linked from, so the question it answers is the
    // nearest true thing — and it is the page in a run most worth
    // describing in the index. Not its title, which `topic:` must not
    // restate. Set here because this is where the query is; the prompt
    // builder skips a MOC's role, so nothing else changes.
    for n in &mut notes {
        if n.kind == NoteKind::Moc && n.role.trim().is_empty() {
            n.role = query.trim().to_string();
        }
    }
    Ok(PlanOutcome {
        notes,
        warnings,
        raw,
    })
}

pub struct PlanOutcome {
    pub notes: Vec<NotePlan>,
    pub warnings: Vec<String>,
    /// The model's reply verbatim — saved as `.research/last-plan.json`
    /// so a bad plan can be inspected.
    pub raw: String,
}

/// Next-round search queries from the tables alone (no bodies).
pub fn build_gap_prompt(query: &str, tables: &Tables, prior_queries: &[String], n: u32) -> String {
    let mut s = format!(
        "Research query: {query}\n{}\n\nSearches already run:\n{}\n\nEntities known so far:\n",
        super::today_context(),
        prior_queries
            .iter()
            .map(|q| format!("- {q}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    for (slug, (name, kind, claims, _)) in tables.entities.iter().take(120) {
        s.push_str(&format!("- {slug} ({name}, {kind}) — {claims} claims\n"));
    }
    s.push_str("\nClaim headlines so far:\n");
    for c in tables.claims.iter().take(120) {
        s.push_str(&format!("- {}\n", clamp(&c.text, 120)));
    }
    s.push_str(&format!(
        "\nName up to {n} NEW search-engine queries (not questions) that would fill gaps: \
         entities mentioned but thin, sub-questions of the query with no claims yet, \
         opposing views, primary sources. Skip anything already well covered. \
         If nothing productive remains, output nothing.\n\
         At least one query must hunt for what is CURRENT: include the current year and a word like \
         latest / newest / release for the main entities (e.g. `<vendor> newest model release {year}`). \
         Do not put an older year in a query unless the topic is historical.\n\
         Prefer queries that reach PRIMARY sources — an organisation's own site, official docs, a \
         changelog, a filing, a paper — over queries that will return roundups and comparisons \
         (`site:`-style wording, `official`, `documentation`, `release notes`, `annual report`).\n\n\
         Output ONE query per line, no numbering, no commentary.",
        year = super::kms_writer::today_str().chars().take(4).collect::<String>()
    ));
    s
}

pub struct GapQueryRequest<'a> {
    pub provider: &'a dyn Provider,
    pub model: &'a str,
    pub query: &'a str,
    pub tables: &'a Tables,
    pub prior: &'a [String],
    pub n: u32,
    pub timeout: Duration,
    pub cancel: &'a CancelToken,
}

pub async fn gap_queries(context: GapQueryRequest<'_>) -> Result<Vec<String>> {
    let GapQueryRequest {
        provider,
        model,
        query,
        tables,
        prior,
        n,
        timeout,
        cancel,
    } = context;
    let raw = oneshot(
        provider,
        model,
        build_gap_prompt(query, tables, prior, n),
        timeout,
        cancel,
    )
    .await?;
    let mut out: Vec<String> = raw
        .lines()
        .map(|l| {
            l.trim()
                .trim_start_matches(|c: char| {
                    c.is_ascii_digit() || c == '.' || c == '-' || c == '*' || c == ')'
                })
                .trim()
                .to_string()
        })
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("none"))
        .filter(|l| !prior.iter().any(|p| p.eq_ignore_ascii_case(l)))
        .collect();
    out.dedup();
    out.truncate(n as usize);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::digest::{Claim, Entity};

    fn digest(idx: u32, ents: &[(&str, &str)], claims: &[(&str, &[&str])]) -> Digest {
        Digest {
            url: format!("https://s{idx}"),
            title: format!("S{idx}"),
            fetched: "d".into(),
            source: idx,
            entities: ents
                .iter()
                .map(|(s, n)| Entity {
                    name: n.to_string(),
                    slug: s.to_string(),
                    kind: "concept".into(),
                })
                .collect(),
            claims: claims
                .iter()
                .enumerate()
                .map(|(i, (t, e))| Claim {
                    id: format!("s{idx}c{}", i + 1),
                    text: t.to_string(),
                    quote: t.to_string(),
                    entities: e.iter().map(|s| s.to_string()).collect(),
                    confidence: 0.8,
                    source: idx,
                    published: None,
                })
                .collect(),
            links_to_known: vec![],
            dropped_claims: 0,
            model: String::new(),
            published: None,
        }
    }

    #[test]
    fn tables_aggregate_claims_and_sources_per_entity() {
        let t = build_tables(&[
            digest(
                1,
                &[("overtime-pay", "OT")],
                &[("a", &["overtime-pay"]), ("b", &["overtime-pay"])],
            ),
            digest(2, &[], &[("c", &["overtime-pay", "minimum-wage"])]),
        ]);
        let ot = &t.entities["overtime-pay"];
        assert_eq!((ot.2, ot.3), (3, 2));
        assert_eq!(t.entities["minimum-wage"].3, 1);
        assert_eq!(t.claims.len(), 3);
    }

    #[test]
    fn plan_enforces_rules_and_synthesises_moc() {
        let t = build_tables(&[
            digest(
                1,
                &[("overtime-pay", "OT")],
                &[("a", &["overtime-pay"]), ("b", &[])],
            ),
            digest(2, &[], &[("c", &["overtime-pay"])]),
        ]);
        let known = vec![KnownNote {
            slug: "overtime-pay".into(),
            title: "Overtime".into(),
            summary: "".into(),
            kind: "concept".into(),
            updated: None,
            has_children: false,
        }];
        let raw = r#"[
          {"slug":"Overtime Pay","kind":"concept","title":"Overtime pay","action":"update","claim_ids":["s1c1","s2c1","bogus"],"related":["thai-labour-law"]},
          {"slug":"thin","kind":"entity","title":"","action":"update","claim_ids":[],"related":[]},
          {"slug":"overtime-pay","kind":"concept","title":"dup","action":"create","claim_ids":["s1c1"],"related":[]}
        ]"#;
        let plan = parse_plan(
            raw,
            &t,
            &known,
            "thai-labour-law",
            "Thai labour law",
            12,
            None,
        );
        assert_eq!(plan.len(), 2, "{plan:?}");
        assert_eq!(plan[0].slug, "overtime-pay");
        assert_eq!(plan[0].action, Action::Update);
        assert_eq!(plan[0].claim_ids, vec!["s1c1", "s2c1"]);
        let moc = &plan[1];
        assert_eq!(moc.kind, NoteKind::Moc);
        assert_eq!(moc.slug, "thai-labour-law");
        assert_eq!(moc.action, Action::Create);
        assert_eq!(
            moc.claim_ids,
            vec!["s1c1", "s1c2", "s2c1"],
            "topic page gets every claim, orphans included"
        );
        assert_eq!(moc.related, vec!["overtime-pay"]);
    }

    /// A Thai topic page must survive planning under its own name.
    ///
    /// This is where ingesting a Thai document actually died: the run had
    /// already resolved a good topic slug, and `parse_plan` re-slugged it
    /// through an ASCII-only sanitiser that returned "" — then wrote the
    /// MOC to a page called nothing, and the KMS refused it. Testing the
    /// slug helper alone passed while the feature stayed broken, so this
    /// pins the stage that failed.
    /// Owner's rule: a refresh only refreshes. Whatever the model plans, one
    /// page is written — the one asked about — and everything the run
    /// learned is cited there. A real refresh of one page had written ten.
    #[test]
    fn a_refresh_writes_only_the_page_it_was_asked_about() {
        let t = build_tables(&[
            digest(
                1,
                &[("jevons-paradox", "Jevons")],
                &[("rebound over 50%", &["jevons-paradox"])],
            ),
            digest(
                2,
                &[("satya-nadella", "Nadella"), ("nvidia", "Nvidia")],
                &[
                    ("Nadella cited Jevons", &["satya-nadella"]),
                    ("Nvidia fell 17%", &["nvidia"]),
                ],
            ),
        ]);
        let known = vec![
            KnownNote {
                slug: "jevons-paradox".into(),
                title: "Jevons Paradox".into(),
                summary: String::new(),
                kind: "concept".into(),
                updated: None,
                has_children: false,
            },
            KnownNote {
                slug: "theory-of-constraints".into(),
                title: "ToC".into(),
                summary: String::new(),
                kind: "concept".into(),
                updated: None,
                has_children: false,
            },
        ];
        // The model ignores the instruction and plans a small encyclopedia.
        let raw = r#"[
          {"slug":"jevons-paradox","kind":"concept","title":"Jevons","action":"update","entities":["jevons-paradox"],"related":["satya-nadella","theory-of-constraints"]},
          {"slug":"satya-nadella","kind":"entity","title":"Nadella","action":"create","entities":["satya-nadella"]},
          {"slug":"nvidia-ai-compute-dominance","kind":"entity","title":"Nvidia","action":"create","entities":["nvidia"]}
        ]"#;
        let (plan, warnings) = parse_plan_report(
            raw,
            &t,
            &known,
            "refresh-jevons-paradox",
            "Jevons",
            30,
            Some("jevons-paradox"),
        );
        assert_eq!(plan.len(), 1, "{plan:?}");
        let n = &plan[0];
        assert_eq!(
            (n.slug.as_str(), n.action),
            ("jevons-paradox", Action::Update)
        );
        assert_eq!(
            n.claim_ids.len(),
            3,
            "every claim of the run is cited here: {:?}",
            n.claim_ids
        );
        assert_eq!(
            n.related,
            vec!["theory-of-constraints"],
            "links only to notes that exist"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("satya-nadella") && w.contains("not created")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_thai_topic_page_keeps_its_name_through_planning() {
        let t = build_tables(&[digest(
            1,
            &[("ยุคที่ความฉลาดล้นเหลือ", "ยุคที่ความฉลาดล้นเหลือ")],
            &[("a", &["ยุคที่ความฉลาดล้นเหลือ"])],
        )]);
        let topic = "ยุคที่ความฉลาดล้นเหลือ";

        // The model answers in Thai, including for the topic page itself.
        let raw = r#"[
          {"slug":"เศรษฐศาสตร์ของความอุดมสมบูรณ์","kind":"concept","title":"เศรษฐศาสตร์","action":"create","claim_ids":["s1c1"],"related":[]}
        ]"#;
        let plan = parse_plan(raw, &t, &[], topic, "ยุคที่ความฉลาดล้นเหลือ", 12, None);

        let moc = plan.iter().find(|n| n.kind == NoteKind::Moc).expect("moc");
        assert_eq!(moc.slug, topic, "the MOC must keep the name it was given");
        assert!(
            plan.iter().any(|n| n.slug == "เศรษฐศาสตร์ของความอุดมสมบูรณ์"),
            "a Thai note slug must not be dropped: {plan:?}"
        );

        // Every planned name has to be one the KMS will actually take.
        let kref = crate::kms::KmsRef {
            name: "t".into(),
            scope: crate::kms::KmsScope::Project,
            root: std::env::temp_dir().join("thclaws-thai-plan-test"),
        };
        for n in &plan {
            assert!(!n.slug.is_empty(), "empty slug in {n:?}");
            assert!(
                crate::kms::writable_page_path(&kref, &n.slug).is_ok(),
                "KMS refused {:?}",
                n.slug
            );
        }
    }

    #[test]
    fn a_new_slug_for_an_existing_title_updates_that_note() {
        let t = build_tables(&[digest(
            1,
            &[("deep-seek", "DeepSeek")],
            &[("a", &["deep-seek"]), ("b", &["deep-seek"])],
        )]);
        let known = vec![KnownNote {
            slug: "deepseek".into(),
            title: "DeepSeek".into(),
            summary: String::new(),
            kind: "entity".into(),
            updated: None,
            has_children: false,
        }];
        let raw = r#"[
          {"slug":"topic","kind":"moc","title":"Topic","action":"create","related":["deep-seek"]},
          {"slug":"deep-seek","kind":"entity","title":"DeepSeek ","action":"create","entities":["deep-seek"]}
        ]"#;
        let plan = parse_plan(raw, &t, &known, "topic", "Topic", 30, None);
        let slugs: Vec<&str> = plan.iter().map(|n| n.slug.as_str()).collect();
        assert!(slugs.contains(&"deepseek"), "{slugs:?}");
        assert!(
            !slugs.contains(&"deep-seek"),
            "no near-duplicate: {slugs:?}"
        );
        let ds = plan.iter().find(|n| n.slug == "deepseek").unwrap();
        assert_eq!(ds.action, Action::Update);
        let moc = plan.iter().find(|n| n.kind == NoteKind::Moc).unwrap();
        assert_eq!(moc.related, vec!["deepseek"], "references follow the alias");
    }

    #[test]
    fn the_note_cap_keeps_the_best_evidenced_notes() {
        // `thin` is listed first but stands on one source; `solid` has two.
        let t = build_tables(&[
            digest(1, &[], &[("a", &["thin"]), ("b", &["solid"])]),
            digest(2, &[], &[("c", &["solid"])]),
        ]);
        let raw = r#"[
          {"slug":"topic","kind":"moc","title":"T","action":"create","related":[]},
          {"slug":"thin","kind":"concept","title":"Thin","action":"create","entities":["thin"]},
          {"slug":"solid","kind":"concept","title":"Solid","action":"create","entities":["solid"]}
        ]"#;
        let plan = parse_plan(raw, &t, &[], "topic", "T", 2, None);
        let slugs: Vec<&str> = plan.iter().map(|n| n.slug.as_str()).collect();
        assert_eq!(
            slugs,
            vec!["solid", "topic"],
            "cap spends on evidence: {slugs:?}"
        );
    }

    #[test]
    fn the_plan_prompt_samples_claims_across_every_source() {
        let digests: Vec<Digest> = (1..=4)
            .map(|s| digest(s, &[], &[("x", &["e"]), ("y", &["e"]), ("z", &["e"])]))
            .collect();
        let t = build_tables(&digests);
        let sampled = sample_claims(&t.claims, 4);
        let sources: HashSet<u32> = sampled.iter().map(|c| c.source).collect();
        assert_eq!(sampled.len(), 4);
        assert_eq!(
            sources.len(),
            4,
            "one claim from each source, not four from one"
        );
        assert_eq!(sample_claims(&t.claims, 99).len(), t.claims.len());
    }

    #[test]
    fn malformed_plan_recovers_objects_and_empty_plan_falls_back_to_entities() {
        let t = build_tables(&[
            digest(
                1,
                &[("deepseek", "DeepSeek"), ("alibaba", "Alibaba")],
                &[
                    ("a", &["deepseek"]),
                    ("b", &["alibaba"]),
                    ("c", &["alibaba"]),
                ],
            ),
            digest(
                2,
                &[],
                &[
                    ("d", &["deepseek"]),
                    ("e", &["stepfun"]),
                    ("f", &["alibaba"]),
                ],
            ),
        ]);
        // Trailing comma in the first object, a comment line, then a good object.
        let raw = r#"```json
[
  {"slug": "chinese-ai", "kind": "moc", "title": "Chinese AI", "action": "create", "outline": ["Players"], "related": ["deepseek"],},
  // note
  {"slug": "deepseek", "kind": "entity", "title": "DeepSeek", "action": "create", "entities": ["deepseek"]}
]
```"#;
        let (plan, warnings) =
            parse_plan_report(raw, &t, &[], "chinese-ai", "Chinese AI", 30, None);
        assert!(warnings[0].contains("recovered 2"), "{warnings:?}");
        assert!(plan.iter().any(|n| n.slug == "deepseek"), "{plan:?}");
        let moc = plan.iter().find(|n| n.kind == NoteKind::Moc).unwrap();
        assert_eq!(
            moc.outline,
            vec!["Players"],
            "the repaired MOC object kept its outline"
        );

        // Nothing usable at all → entity table drives the plan.
        let (plan, warnings) = parse_plan_report(
            "sorry, no json",
            &t,
            &[],
            "chinese-ai",
            "Chinese AI",
            30,
            None,
        );
        let slugs: Vec<&str> = plan.iter().map(|n| n.slug.as_str()).collect();
        assert!(
            slugs.contains(&"deepseek") && slugs.contains(&"alibaba"),
            "{slugs:?}"
        );
        assert!(
            !slugs.contains(&"stepfun"),
            "one source, two claims → folded: {slugs:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("entity table")),
            "{warnings:?}"
        );
        assert_eq!(plan.last().unwrap().kind, NoteKind::Moc);
    }

    #[test]
    fn claims_attach_by_entity_and_truncated_plan_is_salvaged() {
        let t = build_tables(&[
            digest(
                1,
                &[("deepseek", "DeepSeek"), ("deepseek-v4", "DeepSeek V4")],
                &[
                    ("a", &["deepseek"]),
                    ("b", &["deepseek-v4"]),
                    ("c", &["qwen"]),
                ],
            ),
            digest(2, &[], &[("d", &["deepseek", "qwen"])]),
        ]);
        // No claim_ids anywhere; the second object is cut mid-stream.
        let raw = r#"[
          {"slug":"chinese-ai","kind":"moc","title":"Chinese AI","action":"create","outline":["Players"],"related":["deepseek","qwen"]},
          {"slug":"deepseek","kind":"entity","title":"DeepSeek","action":"create","role":"the lab","entities":["deepseek","deepseek-v4"],"related":["qwen"]},
          {"slug":"qwen","kind":"entity","title":"Qwen","action":"create","role":"open"#;
        let (plan, warnings) =
            parse_plan_report(raw, &t, &[], "chinese-ai", "Chinese AI", 30, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("recovered 2"), "{warnings:?}");
        let ds = plan
            .iter()
            .find(|n| n.slug == "deepseek")
            .expect("deepseek kept");
        assert_eq!(
            ds.claim_ids,
            vec!["s1c1", "s1c2", "s2c1"],
            "by entity tag, incl. folded v4"
        );
        assert!(
            ds.related.is_empty(),
            "qwen was cut off, so the related link is pruned"
        );
        let moc = plan.iter().find(|n| n.kind == NoteKind::Moc).unwrap();
        assert_eq!(moc.claim_ids.len(), 4);
        assert_eq!(moc.related, vec!["deepseek"]);
    }

    #[test]
    fn plan_caps_notes_and_keeps_moc() {
        let mut digests = Vec::new();
        for i in 1..=6u32 {
            digests.push(digest(i, &[], &[("x", &["e"])]));
        }
        let t = build_tables(&digests);
        let raw: String = (1..=6)
            .map(|i| format!(r#"{{"slug":"n{i}","kind":"concept","title":"N{i}","action":"create","claim_ids":["s{i}c1"],"related":[]}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let plan = parse_plan(&format!("[{raw}]"), &t, &[], "topic", "Topic", 3, None);
        assert_eq!(plan.len(), 3);
        assert_eq!(plan.last().unwrap().kind, NoteKind::Moc);
        assert!(
            plan.last().unwrap().claim_ids.len() >= 4,
            "dropped notes' claims fold into MOC"
        );
    }

    #[test]
    fn plan_with_preamble_and_fence_still_parses() {
        let t = build_tables(&[digest(1, &[], &[("a", &["e"]), ("b", &["e"])])]);
        let raw = "Here is the plan:\n```json\n[{\"slug\":\"e\",\"kind\":\"concept\",\"title\":\"E\",\"action\":\"create\",\"claim_ids\":[\"s1c1\",\"s1c2\"],\"related\":[]}]\n```\nDone.";
        let plan = parse_plan(raw, &t, &[], "topic", "Topic", 5, None);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].slug, "e");
    }

    #[test]
    fn refresh_anchor_absorbs_orphans_and_makes_no_moc() {
        let t = build_tables(&[digest(1, &[], &[("a", &["overtime-pay"]), ("b", &[])])]);
        let known = vec![KnownNote {
            slug: "overtime-pay".into(),
            title: "Overtime".into(),
            summary: "".into(),
            kind: "concept".into(),
            updated: Some("2026-01-01".into()),
            has_children: false,
        }];
        let raw = r#"[{"slug":"overtime-pay","kind":"concept","title":"Overtime pay","action":"update","claim_ids":["s1c1"],"related":[]},
                      {"slug":"topic-moc","kind":"moc","title":"x","action":"create","claim_ids":["s1c2"],"related":[]}]"#;
        let plan = parse_plan(
            raw,
            &t,
            &known,
            "overtime-pay",
            "Overtime",
            12,
            Some("overtime-pay"),
        );
        assert_eq!(plan.len(), 1, "{plan:?}");
        assert_eq!(plan[0].slug, "overtime-pay");
        assert_eq!(plan[0].action, Action::Update);
        assert_eq!(plan[0].kind, NoteKind::Concept);
        assert_eq!(
            plan[0].claim_ids,
            vec!["s1c1", "s1c2"],
            "orphan folds into the anchor"
        );
    }

    #[test]
    fn unparsable_plan_still_yields_a_moc_with_all_claims() {
        let t = build_tables(&[digest(1, &[], &[("a", &[]), ("b", &[])])]);
        let plan = parse_plan("garbage", &t, &[], "topic", "Topic", 5, None);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].claim_ids.len(), 2);
    }
}
