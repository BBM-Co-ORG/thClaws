//! Research v2 steps 4–6: write each planned note from its own claims,
//! rewrite `[c:ID]` markers into `[N]` source citations, stamp `type:
//! note` frontmatter, and persist notes / sources / the run log.

use super::digest::Claim;
use super::graph::KnownNote;
use super::llm_calls::ResearchSource;
use super::plan::{Action, NoteKind, NotePlan};
use crate::cancel::CancelToken;
use crate::error::Result;
use crate::kms::KmsRef;
use crate::providers::Provider;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
    /// `/research refresh`: keep the page, add references. See
    /// [`claims_for_refresh`].
    pub refresh: bool,
}

/// Most claims a refresh hands the writer. A refresh adds references to a
/// page; it does not turn a 2 KB note into a 19 KB report, which is what
/// handing it all 143 claims of a run did.
pub const REFRESH_MAX_CLAIMS: usize = 30;
/// Rarity-weighted share of a claim's terms that must occur in one
/// paragraph of the page for the claim to "bear on" it.
const REFRESH_MIN_RELEVANCE: f32 = 0.30;

/// Owner's rule (2026-09-20): "keep the page, add references." Of everything
/// a refresh run learned, keep the claims that bear on what the page already
/// says — scored against its best-matching paragraph, terms weighted by
/// rarity across the run's claims, so everyday words count for little — best
/// first, and no more than [`REFRESH_MAX_CLAIMS`].
pub fn claims_for_refresh<'a>(existing: &str, claims: Vec<&'a Claim>) -> Vec<&'a Claim> {
    use super::graph::relevance_terms;
    let paras: Vec<HashSet<String>> = existing
        .split("\n\n")
        .map(relevance_terms)
        .filter(|t| !t.is_empty())
        .collect();
    let claim_terms: Vec<HashSet<String>> = claims
        .iter()
        .map(|c| relevance_terms(&format!("{} {}", c.text, c.quote)))
        .collect();
    let mut df: HashMap<&str, usize> = HashMap::new();
    for t in claim_terms.iter().flatten() {
        *df.entry(t.as_str()).or_default() += 1;
    }
    let n = claims.len().max(1) as f32;
    let weight = |t: &str| (1.0 + n / df.get(t).copied().unwrap_or(1) as f32).ln();
    let mut scored: Vec<(f32, usize)> = claim_terms
        .iter()
        .enumerate()
        .map(|(i, terms)| {
            let total: f32 = terms.iter().map(|t| weight(t)).sum();
            let best = paras
                .iter()
                .map(|p| terms.intersection(p).map(|t| weight(t)).sum::<f32>())
                .fold(0.0f32, f32::max);
            (if total > 0.0 { best / total } else { 0.0 }, i)
        })
        .filter(|(score, _)| *score >= REFRESH_MIN_RELEVANCE)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .take(REFRESH_MAX_CLAIMS)
        .map(|(_, i)| claims[i])
        .collect()
}

/// How much of the topic page a child writer sees.
pub const PARENT_OVERVIEW_CHARS: usize = 1_800;

pub fn build_note_prompt(inp: &NoteInput<'_>) -> String {
    build_note_prompt_split(inp).joined()
}

/// The note prompt, shared part first (see [`super::SplitPrompt`]).
///
/// Shared by every child of one run: the query, the topic page's opening,
/// the full link list and the rules. The link list names this note's own
/// slug too — leaving it out made each child's list unique — and
/// `drop_self_links` already removes a link a note makes to itself.
/// A topic page has no parent overview, so its shared part differs from
/// its children's; it is one call and shares with nothing.
pub fn build_note_prompt_split(inp: &NoteInput<'_>) -> super::SplitPrompt {
    let mut shared = format!(
        "Research query: {}\n{}\n\n\
         You are writing ONE zettelkasten note. One note = one idea. Which note, in \
         which mode and from which claims comes in the message.\n\n",
        inp.query,
        super::today_context(),
    );
    if inp.note.kind != NoteKind::Moc {
        if let Some(p) = &inp.parent_overview {
            shared.push_str("=== The topic page's opening (do NOT repeat it — go deeper on this one idea) ===\n");
            shared.push_str(p.trim());
            shared.push_str("\n\n");
        }
    }
    // The form, not an example. This line used to end "e.g.
    // [[overtime-pay|ค่าล่วงเวลา]]", and a note about agent architectures came
    // back saying "…the same pattern as ค่าล่วงเวลา": the model linked to the
    // example, the link had no target so it was unwrapped to plain text, and
    // Thai labour law arrived in a page it had nothing to do with.
    shared.push_str("=== Notes you may link to. The link TARGET must be the slug exactly as listed; put any display text after `|`, in the form [[slug-from-this-list|words to show]]. Link ONLY to slugs in this list. Never link a note to itself. ===\n");
    for (slug, title) in inp.link_targets {
        shared.push_str(&format!("- [[{slug}]] — {title}\n"));
    }
    shared.push_str(&format!(
        "\nRules:\n\
         - Follow the structure the message gives for this kind of note.\n\
         - Only state what a claim supports; every factual sentence ends with its [c:ID] marker(s). No claim → no sentence.\n\
         - After the opening description, EVERY paragraph and bullet must carry at least one [c:ID]. A paragraph without one is deleted automatically before the note is saved, so explanation, background and transitions you add from your own knowledge are wasted effort — do not write them. Never pad to reach a length.\n\
         - The opening description is the one exception, because a source rarely bothers to define its own subject and a note that only relays claims reads as facts about a thing the reader was never told the nature of. Describe the subject there from ordinary knowledge, and keep it uncontestable: no figures, dates, versions, prices, rankings, or named people and organisations. Anything of that kind belongs below, with its marker.\n\
         - Link, don't bold: the first mention of anything in the link list is written as [[slug|name]], never as **name**. The opening description is exempt — it must read without following anything, so a term that first comes up there is linked at its next mention in the body instead.\n\
         - Currency: when claims disagree, the newer `published` date wins and the older fact is described as superseded. Anything that changes over time (latest version, price, headcount, leadership, \"newest\") must say `as of <published date>`; never present a dated fact as timeless.\n\
         - Do NOT start with a `#` heading and do NOT write a Sources section — both are generated.\n\
         - {}\n\
         - Output ONLY the markdown body.",
        super::language_rule(inp.language)
    ));

    let mut s = String::new();
    let mode = match (inp.note.action, inp.append) {
        _ if inp.refresh && inp.existing_body.is_some() => "REFRESH an existing note: KEEP its text, its headings and its order, and add references. For each claim below, find the existing sentence it supports and put its [c:ID] marker after that sentence. Leave every citation marker already in the note exactly as it is written — `[1](../sources/…)` and the like are the note's own sources and must all still be there. Change a sentence ONLY where a claim contradicts it, and then say what changed and as of when. A genuinely new fact may be added as ONE short sentence in the section it belongs to; do not add sections, do not reorder, do not rewrite prose that no claim touches. The note should come back recognisably the same, a little longer, and much better sourced",
        (Action::Create, _) => "CREATE a new note",
        (Action::Update, false) => "UPDATE an existing note by merging the new claims into it (rewrite the whole body; keep everything still true, drop nothing that the new claims don't contradict)",
        (Action::Update, true) => "APPEND to an existing note: output ONLY the new section that adds what these claims contribute — do not repeat what the note already says",
    };
    s.push_str(&format!(
        "{mode}.\n\n=== This note ===\nSlug: {}\nKind: {}\nTitle: {}\n\n",
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
    if inp.note.kind != NoteKind::Moc && !inp.note.role.is_empty() {
        s.push_str(&format!(
            "=== Why the topic page links here ===\n{}\n\n",
            inp.note.role
        ));
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
    let shape = match inp.note.kind {
        // A one-page run (`--max-notes 1`, viewer "Create page (summary)")
        // has nothing to map; write it as a self-contained note.
        NoteKind::Moc if inp.note.related.is_empty() => "This is the only page for the query — a self-contained note, not an index. Structure: first line = a one-sentence definition/abstract (this becomes the index summary); then 3–6 `##` sections that answer the query from the claims (what it is, key facts and numbers, how it compares, timeline if claims carry dates, current status as of the newest date); No minimum length and at most 1200 words: write what the claims support and stop. No `## Map` section.",
        NoteKind::Moc => "This is the TOPIC PAGE — a report that DESCRIBES the subject, not an index. Structure: 1) an opening of 3–6 sentences that answers the query directly; 2) the outline sections above as `##` headings (if no outline was given, choose 4–8 yourself), each 1–3 paragraphs that SYNTHESISE across claims — compare, rank, explain cause and consequence, name who is ahead and why; when ≥ 3 entities share comparable attributes (models, prices, funding, dates, benchmarks) put them in a markdown table; add a `## Timeline` section when ≥ 5 claims carry dates; the first time the narrative covers a linked note, link it inline as [[slug|name]]; 3) `## Map` — every linked note as a bullet with the clause saying why it matters; 4) `## Open questions` (≤ 3 bullets). No minimum length and at most 1800 words (or the Thai equivalent): length follows the claims. Use every claim you were given that fits.",
        NoteKind::Claim => "Structure: 1) an opening paragraph of 2–4 sentences describing what the subject of this claim IS, written for a reader who followed a link here and knows nothing about it, and containing no wikilinks; 2) the claim itself in one sentence; 3) why it holds and what it depends on. No minimum length, at most 350 words — with one claim this is a few sentences, and that is a finished note.",
        _ => "Structure: 1) an opening paragraph of 2–4 sentences that DESCRIBES the subject in its own right — what it is, what it is for, who uses it or argues about it — written for a reader who followed a link here and knows nothing about it, and containing no wikilinks; its first sentence doubles as the index summary, so it has to stand alone; 2) then 2–5 `##` sections that go DEEPER than the topic page (history, what it does, numbers, how it compares, current status as of the newest date), linking to related notes where the reference genuinely helps. No minimum length, at most 800 words: the note is as long as its claims make it. With one or two claims that is a short note, and a short note is correct.",
    };
    // The rules now arrive before the claims instead of after them. What a
    // model reads last it weighs most, so the three that a note most often
    // broke are said once more where the rules used to be.
    let shape = if inp.refresh && inp.existing_body.is_some() {
        "The existing note's own structure. Output the WHOLE note — every existing section and sentence, with the new markers in place — not a diff and not just the additions."
    } else {
        shape
    };
    s.push_str(&format!(
        "\n=== Structure for this note ===\n{shape}\n\n\
         Write it now. Every factual sentence ends with its [c:ID], and any paragraph \
         after the opening with no [c:ID] will be deleted; the opening paragraph has no \
         wikilinks and no figures; stop when the claims run out; output only the \
         markdown body. {}",
        super::language_rule(inp.language)
    ));
    super::SplitPrompt { shared, item: s }
}

/// Owner's rule (2026-09-20): a note is as long as its citations. Remove
/// every prose paragraph that cites nothing, and any heading left with
/// nothing under it. Returns the body and how many paragraphs went.
///
/// Asking for it in the prompt is not enough — the prompt already said "no
/// claim → no sentence" when a real vault measured 28 % uncited prose, with
/// single notes at 75–100 %. What survives is what [`uncited_share`] does
/// not count against a note: the opening description, tables, quotes, link
/// lists, and the Map / Open questions / See also / Sources / Timeline
/// sections. Run AFTER `rewrite_claim_markers`, so a paragraph that cited
/// only claim ids that do not exist is uncited too.
///
/// For a NEW note only. An update merges a page that may hold paragraphs a
/// person wrote, and those carry no markers.
pub fn prune_uncited(body: &str) -> (String, usize) {
    const SKIP_SECTIONS: &[&str] = &["map", "open questions", "see also", "sources", "timeline"];
    let cited = regex::Regex::new(r"\[\d+\]").expect("static regex");
    let links = regex::Regex::new(r"\[\[[^\]]*\]\]").expect("static regex");
    let mut kept: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    let mut skipping = false;
    let mut seen_prose = false;
    for para in body.split("\n\n") {
        let p = para.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(h) = p.strip_prefix('#') {
            let name = h.trim_start_matches('#').trim().to_lowercase();
            skipping = SKIP_SECTIONS.iter().any(|s| name.starts_with(s));
            kept.push(p.to_string());
            continue;
        }
        let structural =
            skipping || p.starts_with('|') || p.starts_with('>') || p.starts_with("---");
        let n = p.chars().filter(|c| !c.is_whitespace()).count();
        let rest = links
            .replace_all(p, "")
            .chars()
            .filter(|c| !c.is_whitespace())
            .count();
        let navigation = rest * 2 < n;
        if structural || navigation {
            kept.push(p.to_string());
            continue;
        }
        if !seen_prose {
            seen_prose = true;
            kept.push(p.to_string());
            continue;
        }
        // A bullet list is judged item by item: one cited item must not
        // carry five uncited ones through.
        if p.lines()
            .all(|l| l.trim_start().starts_with(['-', '*']) || l.trim().is_empty())
        {
            let items: Vec<&str> = p.lines().filter(|l| cited.is_match(l)).collect();
            let all = p.lines().filter(|l| !l.trim().is_empty()).count();
            dropped += all - items.len();
            if !items.is_empty() {
                kept.push(items.join("\n"));
            }
            continue;
        }
        if cited.is_match(p) {
            kept.push(p.to_string());
        } else {
            dropped += 1;
        }
    }
    // A heading whose section was emptied says nothing. A "See also" line
    // under it is the note's, not the section's, so it does not count as
    // content — the heading goes and the line stays.
    let is_nav = |p: &str| {
        let n = p.chars().filter(|c| !c.is_whitespace()).count();
        let rest = links
            .replace_all(p, "")
            .chars()
            .filter(|c| !c.is_whitespace())
            .count();
        !p.starts_with('#') && rest * 2 < n
    };
    let level = |h: &str| h.bytes().take_while(|b| *b == b'#').count();
    let mut out: Vec<&str> = Vec::new();
    for (i, p) in kept.iter().enumerate() {
        // The page title (`# …`) is never a section.
        let generated = SKIP_SECTIONS.iter().any(|s| {
            p.trim_start_matches('#')
                .trim()
                .to_lowercase()
                .starts_with(s)
        });
        if p.starts_with("##") && !generated {
            // Empty: nothing under it, or only the note's closing "See
            // also" line. A link list in the MIDDLE of a note is content —
            // the topic page's Map is nothing but links.
            let rest = &kept[i + 1..];
            let empty = match rest.first() {
                None => true,
                Some(n) if n.starts_with('#') => level(n) <= level(p),
                Some(_) => rest.iter().all(|q| is_nav(q)),
            };
            if empty {
                continue;
            }
        }
        out.push(p.as_str());
    }
    (out.join("\n\n"), dropped)
}

/// Above this [`uncited_share`] a note is named in the run log's warnings.
/// A third: a note with one framing paragraph per cited one is normal; a
/// note that is mostly the model talking is not.
pub const UNCITED_WARN: f32 = 0.35;

/// dev-plan/64 P4.2: how much of a note's prose cites nothing.
///
/// The note prompt says "no claim → no sentence", and the only check that
/// it was obeyed was reading the note. This one is arithmetic: the share of
/// prose characters sitting in paragraphs with no `[N]` marker at all.
/// Paragraphs, not sentences — Thai has no sentence terminator to split on,
/// and a paragraph with one citation is at least anchored somewhere.
///
/// Not counted, because they are not supposed to cite: the opening
/// description (allowed to be unsourced since 2026-09-19), headings, tables,
/// quotes, and the generated `Map` / `Open questions` / `See also` /
/// `Sources` sections. Returns `None` when there is too little prose to
/// say anything.
pub fn uncited_share(body: &str) -> Option<f32> {
    const SKIP_SECTIONS: &[&str] = &["map", "open questions", "see also", "sources", "timeline"];
    let cited = regex::Regex::new(r"\[\d+\]").expect("static regex");
    let links = regex::Regex::new(r"\[\[[^\]]*\]\]").expect("static regex");
    let (mut total, mut bare) = (0usize, 0usize);
    let mut skipping = false;
    let mut seen_prose = false;
    for para in body.split("\n\n") {
        let p = para.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(h) = p.strip_prefix('#') {
            let name = h.trim_start_matches('#').trim().to_lowercase();
            skipping = SKIP_SECTIONS.iter().any(|s| name.starts_with(s));
            continue;
        }
        if skipping || p.starts_with('|') || p.starts_with('>') || p.starts_with("---") {
            continue;
        }
        if !seen_prose {
            // The opening description.
            seen_prose = true;
            continue;
        }
        let n = p.chars().filter(|c| !c.is_whitespace()).count();
        // A "See also: [[a]] · [[b]]" line is navigation, in any language:
        // what is left of it without its links is next to nothing.
        let without_links = links.replace_all(p, "");
        let rest = without_links.chars().filter(|c| !c.is_whitespace()).count();
        if rest * 2 < n {
            continue;
        }
        total += n;
        if !cited.is_match(p) {
            bare += n;
        }
    }
    (total >= 200).then(|| bare as f32 / total as f32)
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

/// A note does not link to itself. The writer is shown its own slug and
/// sometimes wraps a whole clause in it (`[[jevons-paradox|…แต่กลับเพิ่ม]]`
/// inside `jevons-paradox`), which put self-edges in the graph and the
/// backlink strip. The link goes; the words stay.
pub fn drop_self_links(body: &str, self_slug: &str) -> String {
    let re = regex::Regex::new(r"\[\[([^\]|]+)(?:\|([^\]]*))?\]\]").expect("static regex");
    re.replace_all(body, |caps: &regex::Captures| {
        if caps[1].trim() != self_slug {
            return caps[0].to_string();
        }
        caps.get(2)
            .map(|m| m.as_str().trim())
            .filter(|d| !d.is_empty())
            .unwrap_or(caps[1].trim())
            .to_string()
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
        // Characters, not bytes: `len() >= 3` let a single Thai character
        // through, since one is three bytes.
        .filter(|(slug, title)| slug.as_str() != self_slug && title.trim().chars().count() >= 3)
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
use crate::kms::is_spaceless_script;

/// True when `neighbour` continues the word that `edge` (the first or
/// last character of a title) belongs to — so the title is not standing
/// on its own there and must not be linked.
///
/// ASCII used to be the only case checked, which for Thai meant *every*
/// position counted as a word boundary: the title `งาน` was linked inside
/// `พนักงาน`. Without a segmenter there is no telling where a Thai word
/// ends, so in a spaceless script a title links only where something
/// other than that script — a space, punctuation, Latin text, the line
/// edge — sets it apart. That links less than it could; it never splices
/// a link into the middle of a word.
fn glued(neighbour: char, edge: Option<char>) -> bool {
    if neighbour.is_ascii_alphanumeric() {
        return true;
    }
    is_spaceless_script(neighbour) && edge.is_some_and(is_spaceless_script)
}

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
            .map(|c| glued(c, title.chars().next()))
            .unwrap_or(false);
        let ok_right = !after
            .chars()
            .next()
            .map(|c| glued(c, title.chars().next_back()))
            .unwrap_or(false);
        let open_wiki = before.matches("[[").count() > before.matches("]]").count();
        let open_md_text = before.matches('[').count() > before.matches(']').count();
        let open_md_url = before
            .rfind("](")
            .map(|i| !before[i..].contains(')'))
            .unwrap_or(false);
        let open_md = open_md_text || open_md_url;
        let open_code = before.matches('`').count() % 2 == 1;
        // Inside a bare URL of ANY scheme: no whitespace since the last
        // `://`, so the address is still running. This used to look for
        // `http`, and the local-ingest path then introduced `kms://` —
        // whose address runs to the end of its line, because a KMS name
        // may contain spaces. `kms::auto_link` had the same narrow guard
        // and wrote a wikilink into the source URL of 25 pages.
        let in_url = before.contains("kms://")
            || before
                .rfind("://")
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
    /// [`uncited_share`] of the body as written; `None` for an append or a
    /// note too short to judge.
    pub uncited: Option<f32>,
}

/// Frontmatter keys a research write owns. Everything else on an
/// existing page belongs to whoever put it there — a category, tags,
/// aliases, a `verified:` stamp a human added — and survives the
/// update. `status` is excluded on purpose: its research/ingest
/// lifecycle values mean "not written yet", which stops being true the
/// moment this function runs.
const MANAGED_FRONTMATTER: &[&str] = &[
    "title",
    "topic",
    "type",
    "kind",
    "related",
    "sources",
    "claims",
    "confidence",
    "uncited",
    "updated",
];

/// `verified:` is ours only when this write checked something: every claim
/// that survives into the body was matched against its source's text on
/// the way in, which is what the stamp says. A note written with no claims
/// verified nothing, and keeps whatever stamp it already had.
fn verified_line(carried: &[(String, String)], claim_count: usize, today: &str) -> String {
    if claim_count > 0 {
        return format!("verified: {today}\n");
    }
    carried
        .iter()
        .find(|(k, _)| k == "verified")
        .map(|(_, v)| format!("verified: {v}\n"))
        .unwrap_or_default()
}

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
    pub sources_meta: &'a [(u32, String, String, String)],
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
        // The page's own text is carried here in code and never handed
        // back to the model, so an update cannot lose it. What the plain
        // append got wrong was the tail: the chunk brought a `## Sources`
        // of its own and landed after the page's, leaving two lists with
        // the original stranded mid-page. Both lists come off, the new
        // section goes in, and one list is rebuilt over the merged body —
        // so the old citations stay listed alongside the new.
        let page = kref.root.join("pages").join(format!("{}.md", note.slug));
        if let Ok(raw) = std::fs::read_to_string(&page) {
            let (mut fm, old_body) = crate::kms::parse_frontmatter(&raw);
            let kept = super::kms_writer::strip_sources_section(&old_body);
            let added = super::kms_writer::strip_sources_section(&body);
            let mut merged = format!(
                "{}\n\n## Update {today}\n\n{}\n",
                kept.trim_end(),
                added.trim()
            );
            // `parse_citation_indices` returns a `HashSet`; the order
            // matters here because it is written straight into
            // `sources:`.
            let all: BTreeSet<u32> = super::kms_writer::parse_citation_indices(&merged)
                .into_iter()
                .collect();
            if all.is_empty() {
                // Nothing in the merged page cites anything, and
                // `ensure_sources_section` is a no-op then — rebuilding
                // would drop the old list without writing a new one.
                merged = format!(
                    "{}\n\n## Update {today}\n\n{}\n",
                    old_body.trim_end(),
                    added.trim()
                );
            } else {
                merged = super::kms_writer::ensure_sources_section(&merged, sources_meta);
                fm.insert(
                    "sources".into(),
                    format!(
                        "[{}]",
                        all.iter()
                            .map(|i| i.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            }
            fm.insert("updated".into(), today.to_string());
            let composed = crate::kms::write_frontmatter(&fm, &merged);
            let path = crate::kms::write_page(kref, &note.slug, &composed)?;
            return Ok(WrittenNote {
                slug: note.slug.clone(),
                path,
                action: note.action,
                // This run's own citations, not the merged set: the
                // caller uses it to decide which fetched sources to
                // persist, and the older indices are already on disk.
                cited: cited.clone(),
                uncited: None,
            });
        }
        // Planned as an update but nothing on disk — fall through and
        // write it as a normal page rather than lose the body.
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
    let carried = carried_frontmatter(kref, &note.slug);
    let verified = verified_line(&carried, claim_count, today);
    let carried: String = carried
        .into_iter()
        .filter(|(k, _)| k != "verified")
        .map(|(k, v)| {
            let flow = v.starts_with('[') && v.ends_with(']');
            if flow || !(v.contains(':') || v.contains('#') || v.contains('"')) {
                format!("{k}: {v}\n")
            } else {
                format!("{k}: \"{}\"\n", v.replace('"', "'"))
            }
        })
        .collect();
    // Measured on the finished body, after `[c:ID]` became `[N]`.
    let uncited_now = uncited_share(&body);
    let uncited = uncited_now
        .map(|u| format!("uncited: {u:.2}\n"))
        .unwrap_or_default();
    // dev-plan/64 P4.3: `topic:` is a one-line description of what the
    // page covers, and the planner already wrote one — `role`, the
    // clause the topic page's map uses to say why it links here. It was
    // used for that and thrown away, so `topic:` was absent on every
    // page of a research vault and the index summary — which prefers
    // `topic:` — fell back to the opening sentence every single time.
    let topic = note.role.trim().replace(['\n', '\r'], " ");
    let topic = if topic.is_empty() {
        String::new()
    } else {
        format!("topic: \"{}\"\n", topic.replace('"', "'"))
    };
    let composed = format!(
        "---\n\
         title: \"{}\"\n\
         {topic}\
         type: note\n\
         kind: {}\n\
         related: [{related}]\n\
         sources: [{sources}]\n\
         claims: {claim_count}\n\
         confidence: {confidence:.2}\n\
         {uncited}updated: {today}\n\
         {verified}{carried}\
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
        uncited: uncited_now,
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
    let prompt = build_note_prompt_split(inp);
    let raw = super::llm_calls::oneshot_split(
        provider,
        model,
        prompt,
        timeout,
        cancel,
        super::llm_calls::CallKind::Writing,
    )
    .await?;
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
    /// What kind of run this was — `research`, `refresh`, `ingest` or
    /// `selection` (dev-plan/64 P5.4). A run log is the only record of
    /// what a run cost, and the GUI estimates the price of a click from
    /// runs of the same kind: a refresh and a full research run are not
    /// the same money, and telling them apart by filename prefix was a
    /// guess waiting to rot.
    pub mode: &'a str,
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
        "---\ntype: research-run\nquery: \"{}\"\nmode: {}\ndate: {}\nelapsed_secs: {}\nworker_model: {}\nsources_digested: {}\nsources_cached: {}\nclaims: {}\nclaims_dropped_by_quote_check: {}\n{}---\n\n# Research run — {}\n\n**Query:** {}\n\n## Rounds\n\n| round | sources | new items | total items | novelty | queries |\n|---|---|---|---|---|---|\n",
        log.query.replace('"', "'"),
        log.mode,
        log.today,
        log.elapsed_secs,
        log.worker_model,
        log.sources_digested,
        log.sources_cached,
        log.claims_total,
        log.claims_dropped,
        // dev-plan/64 P2.7: what the run cost, where the rest of what it
        // did is already recorded.
        super::llm_calls::usage_so_far().frontmatter(),
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
    crate::kms::write_file(&path, s)
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
    /// dev-plan/64 P2.3. The schema every KMS is created with said research
    /// stamps `verified:`; v2 never did, so all 47 pages of a real vault
    /// carried a "treat with caution" banner on every read.
    #[test]
    fn a_write_that_checked_claims_stamps_verified() {
        let old = vec![("verified".to_string(), "2026-01-01".to_string())];
        assert_eq!(
            verified_line(&old, 3, "2026-09-19"),
            "verified: 2026-09-19\n"
        );
        assert_eq!(
            verified_line(&old, 0, "2026-09-19"),
            "verified: 2026-01-01\n"
        );
        assert_eq!(verified_line(&[], 0, "2026-09-19"), "");
    }

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
            sources_meta: &[(1, "S".into(), "https://s".into(), String::new())],
        })
        .unwrap();
        let on_disk = std::fs::read_to_string(kref.pages_dir().join("deepseek.md")).unwrap();
        assert!(on_disk.contains("category: vendors"), "{on_disk}");
        assert!(on_disk.contains("tags: china, llm"), "{on_disk}");
        // Not carried: this write checked its one claim against the source,
        // which is a newer and truer stamp than the one it found.
        assert!(on_disk.contains("verified: 2026-09-08"), "{on_disk}");
        assert_eq!(on_disk.matches("verified:").count(), 1, "{on_disk}");
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
                mode: "research",
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
                refresh: false,
            })
        };
        let few = mk(QUOTE_BUDGET_CLAIMS);
        assert!(build(&few).contains("quote: \"verbatim 0\""));
        let many = mk(QUOTE_BUDGET_CLAIMS + 1);
        let p = build(&many);
        assert!(!p.contains("quote:"), "quotes dropped past the budget");
        assert!(p.contains("[c:s1c0] claim 0"), "claims still listed");
    }

    /// Not a test: what `prune_uncited` would have left of each note in a
    /// real vault. `KMS_BENCH_VAULT=<kms folder> cargo test --lib
    /// simulate_prune -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn simulate_prune_on_a_real_vault() {
        let Ok(root) = std::env::var("KMS_BENCH_VAULT") else {
            return;
        };
        let mut rows: Vec<(usize, usize, usize, String)> = Vec::new();
        for e in std::fs::read_dir(std::path::Path::new(&root).join("pages"))
            .unwrap()
            .flatten()
        {
            let raw = std::fs::read_to_string(e.path()).unwrap_or_default();
            let (fm, body) = crate::kms::parse_frontmatter(&raw);
            if fm.get("type").map(String::as_str) != Some("note") {
                continue;
            }
            let body = crate::research::kms_writer::strip_sources_section(&body);
            let (after, dropped) = prune_uncited(&body);
            rows.push((
                body.chars().count(),
                after.chars().count(),
                dropped,
                e.file_name().to_string_lossy().into_owned(),
            ));
        }
        rows.sort_by(|a, b| (b.0 - b.1).cmp(&(a.0 - a.1)));
        for (before, after, dropped, f) in rows.iter().take(10) {
            eprintln!("{before:>6} → {after:>6} chars  (-{dropped} para)  {f}");
        }
        let (b, a): (usize, usize) = rows.iter().fold((0, 0), |x, r| (x.0 + r.0, x.1 + r.1));
        let untouched = rows.iter().filter(|r| r.2 == 0).count();
        let tiny = rows.iter().filter(|r| r.1 < 400).count();
        eprintln!(
            "{} notes: {b} → {a} chars ({:.0}% kept); {untouched} untouched; {tiny} end under 400 chars",
            rows.len(),
            a as f32 * 100.0 / b.max(1) as f32
        );
    }

    /// Not a test: a survey. `KMS_BENCH_VAULT=<kms folder> cargo test --lib
    /// survey_uncited -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn survey_uncited_share_on_a_real_vault() {
        let Ok(root) = std::env::var("KMS_BENCH_VAULT") else {
            return;
        };
        let mut rows: Vec<(f32, String)> = Vec::new();
        for e in std::fs::read_dir(std::path::Path::new(&root).join("pages"))
            .unwrap()
            .flatten()
        {
            let raw = std::fs::read_to_string(e.path()).unwrap_or_default();
            let (fm, body) = crate::kms::parse_frontmatter(&raw);
            if fm.get("type").map(String::as_str) != Some("note") {
                continue;
            }
            let body = crate::research::kms_writer::strip_sources_section(&body);
            if let Some(u) = uncited_share(&body) {
                rows.push((u, e.file_name().to_string_lossy().into_owned()));
            }
        }
        rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        for (u, f) in rows.iter().take(8) {
            eprintln!("{:>4.0}%  {f}", u * 100.0);
        }
        let over = rows.iter().filter(|r| r.0 > UNCITED_WARN).count();
        let mean = rows.iter().map(|r| r.0).sum::<f32>() / rows.len().max(1) as f32;
        eprintln!(
            "{} research notes, mean {:.0}%, {over} over the {:.0}% line",
            rows.len(),
            mean * 100.0,
            UNCITED_WARN * 100.0
        );
    }

    /// Owner's rule: a note is as long as its citations. What the model adds
    /// from its own knowledge does not reach the page.
    #[test]
    fn a_new_note_keeps_only_what_it_can_cite() {
        let body = "เปิดเรื่อง อธิบายว่าสิ่งนี้คืออะไร ไม่ต้องอ้าง\n\n\
                    ## กลไก\n\nย่อหน้านี้มีหลักฐาน [1]\n\nย่อหน้านี้โมเดลแต่งเอง ยาวแค่ไหนก็ไม่รอด\n\n\
                    ## ส่วนที่แต่งทั้งส่วน\n\nไม่มีอ้างอิงเลย\n\nยังไม่มีอีก\n\n\
                    ## รายการ\n\n- ข้อที่อ้าง [2]\n- ข้อที่แต่ง\n- อีกข้อที่อ้าง [1][3]\n\n\
                    | a | b |\n|---|---|\n| ตาราง | อยู่ |\n\n\
                    ดูเพิ่มเติม: [[x|เอ็กซ์]] · [[y|วาย]]\n\n\
                    ## Map\n\n- [[x]] — ไม่ต้องอ้าง\n";
        let (out, dropped) = prune_uncited(body);
        assert_eq!(dropped, 4, "{out}");
        assert!(out.starts_with("เปิดเรื่อง"), "the opening stays: {out}");
        assert!(out.contains("ย่อหน้านี้มีหลักฐาน [1]"));
        assert!(
            !out.contains("โมเดลแต่งเอง") && !out.contains("ข้อที่แต่ง"),
            "{out}"
        );
        assert!(
            !out.contains("ส่วนที่แต่งทั้งส่วน"),
            "an emptied section loses its heading: {out}"
        );
        assert!(out.contains("- ข้อที่อ้าง [2]\n- อีกข้อที่อ้าง [1][3]"), "{out}");
        assert!(
            out.contains("| ตาราง | อยู่ |") && out.contains("ดูเพิ่มเติม") && out.contains("## Map")
        );
        assert_eq!(uncited_share(&out).unwrap_or(0.0), 0.0);

        // Nothing to drop: byte-for-byte the same paragraphs.
        let clean = "opening\n\n## H\n\ncited [1]\n";
        assert_eq!(
            prune_uncited(clean),
            ("opening\n\n## H\n\ncited [1]".to_string(), 0)
        );
        // A one-claim note is a short note, not an empty one.
        let one =
            "opening only\n\nthe claim [1]\n\nand three\n\nparagraphs of\n\nthe model talking\n";
        assert_eq!(prune_uncited(one).0, "opening only\n\nthe claim [1]");
    }

    /// A prompt's example is content the model may copy. None of ours may be
    /// a real-looking slug or a real phrase.
    #[test]
    fn the_note_prompt_carries_no_example_a_model_could_copy_into_a_note() {
        let targets = BTreeMap::new();
        let plan = note_plan("t", "T", Action::Create);
        let p = build_note_prompt_split(&NoteInput {
            query: "q",
            note: &plan,
            claims: vec![],
            sources: &[],
            link_targets: &targets,
            existing_body: None,
            append: false,
            language: "th",
            parent_overview: None,
            refresh: false,
        });
        let all = format!("{}{}", p.shared, p.item);
        assert!(
            !all.contains("overtime") && !all.contains("ค่าล่วงเวลา"),
            "{all}"
        );
        assert!(all.contains("[[slug-from-this-list|words to show]]"));
    }

    /// Owner's rule: "keep the page, add references." A real refresh was
    /// handed all 143 claims of its run and turned a 2 KB note into a 19 KB
    /// report that no longer cited the owner's document at all.
    #[test]
    fn a_refresh_is_offered_only_the_claims_that_bear_on_the_page() {
        let page = "Baumol's cost disease อธิบายว่าบริการที่ productivity ไม่เพิ่มจะแพงขึ้นสัมพัทธ์ [1](../sources/doc.md)\n\n\
                    ## ผลต่อสัดส่วน\n\nภาคที่ productivity โตเร็วที่สุดจะมีสัดส่วนใน GDP หดลง ไม่ใช่โตขึ้น [1](../sources/doc.md)";
        let claim = |id: &str, text: &str| Claim {
            id: id.into(),
            text: text.into(),
            quote: text.into(),
            entities: vec![],
            confidence: 0.9,
            source: 2,
            published: None,
        };
        let mut all = vec![
            claim(
                "s2c1",
                "Baumol พบว่าบริการที่ productivity ไม่เพิ่มมีราคาสัมพัทธ์แพงขึ้นเรื่อย ๆ",
            ),
            claim(
                "s2c2",
                "สัดส่วนใน GDP ของภาคที่ productivity โตเร็วหดลง ตามข้อมูล OECD",
            ),
            claim("s2c3", "Nvidia มูลค่าตลาดลดลง 17% ในวันเดียวหลัง DeepSeek เปิดตัว"),
        ];
        for i in 0..60 {
            all.push(claim(
                &format!("s3c{i}"),
                &format!("ข่าวเรื่องชิปและศูนย์ข้อมูลฉบับที่ {i} ไม่เกี่ยวกับหน้านี้เลย"),
            ));
        }
        let kept = claims_for_refresh(page, all.iter().collect());
        let ids: Vec<&str> = kept.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"s2c1") && ids.contains(&"s2c2"), "{ids:?}");
        assert!(
            !ids.contains(&"s2c3"),
            "Nvidia has nothing to do with this page: {ids:?}"
        );
        assert!(kept.len() <= REFRESH_MAX_CLAIMS);
        assert!(
            kept.len() < 10,
            "the unrelated sixty are not offered: {}",
            kept.len()
        );

        // The prompt asks for the page back, not a new one.
        let plan = note_plan("baumols-cost-disease", "Baumol", Action::Update);
        let targets = BTreeMap::new();
        let p = build_note_prompt_split(&NoteInput {
            query: "q",
            note: &plan,
            claims: kept,
            sources: &[],
            link_targets: &targets,
            existing_body: Some(page.into()),
            append: false,
            language: "th",
            parent_overview: None,
            refresh: true,
        });
        assert!(
            p.item.contains("KEEP its text") && p.item.contains("must all still be there"),
            "{}",
            p.item
        );
        assert!(p.item.contains("Output the WHOLE note"));
        assert!(!p.item.contains("250–800") && !p.item.contains("rewrite the whole body"));
    }

    #[test]
    fn uncited_share_counts_prose_that_cites_nothing() {
        let cited = "ย่อหน้าที่มีอ้างอิง ".repeat(12);
        let bare = "ย่อหน้าที่ไม่มีอ้างอิงเลย ".repeat(12);
        let body = format!(
            "เปิดเรื่องโดยไม่ต้องอ้าง ยาวแค่ไหนก็ไม่นับ {bare}\n\n## ส่วนที่หนึ่ง\n\n{cited}[3]\n\n{bare}\n\n| a | b |\n|---|---|\n| ไม่นับ | ตาราง |\n\n## Map\n\n- [[x]] ไม่นับ {bare}\n\n## Sources\n\n1. ไม่นับ\n"
        );
        let share = uncited_share(&body).unwrap();
        let expect = bare.trim().chars().filter(|c| !c.is_whitespace()).count() as f32
            / (bare.trim().chars().filter(|c| !c.is_whitespace()).count()
                + format!("{cited}[3]")
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .count()) as f32;
        assert!((share - expect).abs() < 0.001, "{share} vs {expect}");
        assert!(share > 0.45 && share < 0.60, "{share}");

        let nav = "ดูเพิ่มเติม: [[ยุคที่ความฉลาดล้นเหลือ|ยุคที่ความฉลาดล้นเหลือ — ควรสร้างอะไร]] · [[ai-slop|AI slop และเศรษฐกิจความสนใจ]]";
        let with_nav = format!("opening\n\n{cited}[1]\n\n{cited}[2]\n\n{nav}\n");
        assert_eq!(
            uncited_share(&with_nav),
            Some(0.0),
            "a see-also line is not prose"
        );

        let all_cited = format!("opening\n\n{cited}[1]\n\n{cited}[2][4]\n");
        assert_eq!(uncited_share(&all_cited), Some(0.0));
        assert_eq!(
            uncited_share("opening\n\nshort [1]\n"),
            None,
            "too little to judge"
        );
    }

    /// dev-plan/64 P2.6. A prefix cache pays only when the prefix repeats
    /// byte for byte, so nothing about one child may reach the shared part
    /// — the slug left out of its own link list was enough to make all 25
    /// prompts of a run unique.
    /// dev-plan/64 P4.3: the planner writes a one-clause description of
    /// every note and it was thrown away after the topic page's map
    /// used it. `topic:` is the key the index summary prefers, so on a
    /// research vault that preference had never once fired.
    #[test]
    fn a_written_note_carries_the_planner_s_one_line_description() {
        let _h = crate::research::test_helpers::scoped_home();
        let k = crate::kms::create("nb", crate::kms::KmsScope::Project).unwrap();
        let mut plan = note_plan("baumol", "Baumol's cost disease", Action::Create);
        plan.role = "why service prices rise when \"manufacturing\" gets cheaper".into();
        persist_note(NoteToPersist {
            kref: &k,
            note: &plan,
            body: "A paragraph that says something else entirely.",
            cited: &BTreeSet::new(),
            claim_count: 0,
            confidence: 0.0,
            today: "2026-09-21",
            append: false,
            sources_meta: &[],
        })
        .unwrap();

        let raw = std::fs::read_to_string(k.pages_dir().join("baumol.md")).unwrap();
        let (fm, _) = crate::kms::parse_frontmatter(&raw);
        assert_eq!(
            fm.get("topic").map(String::as_str),
            // The quote is escaped, not dropped, and not left to break
            // the YAML it sits in.
            Some("why service prices rise when 'manufacturing' gets cheaper")
        );
        // And the index summary now says that instead of the opening line.
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(
            index.contains("why service prices rise"),
            "the index still falls back to the body:\n{index}"
        );

        // A note the planner gave no role keeps no empty key.
        let bare = note_plan("plain", "Plain", Action::Create);
        persist_note(NoteToPersist {
            kref: &k,
            note: &bare,
            body: "body",
            cited: &BTreeSet::new(),
            claim_count: 0,
            confidence: 0.0,
            today: "2026-09-21",
            append: false,
            sources_meta: &[],
        })
        .unwrap();
        let raw2 = std::fs::read_to_string(k.pages_dir().join("plain.md")).unwrap();
        assert!(!raw2.contains("topic:"), "empty topic written:\n{raw2}");
    }

    #[test]
    fn every_child_of_a_run_shares_one_opening() {
        let mut targets = BTreeMap::new();
        targets.insert("alpha".to_string(), "Alpha".to_string());
        targets.insert("beta".to_string(), "Beta".to_string());
        let claim = Claim {
            id: "s1c1".into(),
            text: "a fact".into(),
            quote: "a fact".into(),
            entities: vec![],
            confidence: 0.9,
            source: 1,
            published: None,
        };
        let mut a = note_plan("alpha", "Alpha", Action::Create);
        a.role = "why alpha".into();
        let mut b = note_plan("beta", "Beta", Action::Update);
        b.kind = NoteKind::Claim;
        let build = |plan: &NotePlan, existing: Option<String>| {
            build_note_prompt_split(&NoteInput {
                query: "q",
                note: plan,
                claims: vec![&claim],
                sources: &[],
                link_targets: &targets,
                existing_body: existing,
                append: false,
                language: "th",
                parent_overview: Some("the topic page opens like this".into()),
                refresh: false,
            })
        };
        let (pa, pb) = (build(&a, None), build(&b, Some("old body".into())));
        assert_eq!(pa.shared, pb.shared);
        assert!(pa.shared.contains("[[alpha]]") && pa.shared.contains("Rules:"));
        assert!(pa.shared.contains("the topic page opens like this"));
        assert!(!pa.shared.contains("why alpha") && !pa.shared.contains("a fact"));
        assert!(pa.item.contains("Slug: alpha") && pa.item.contains("why alpha"));
        assert!(pb.item.contains("old body") && pb.item.contains("UPDATE"));
        assert_ne!(pa.item, pb.item);
        // What is read last is weighed most: the item ends on the rules a
        // note most often broke.
        assert!(pa
            .item
            .trim_end()
            .ends_with(&crate::research::language_rule("th")));
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

    /// A Thai title must not be linked in the middle of a longer Thai
    /// word. Only ASCII neighbours used to count as "part of a word", so
    /// in Thai every position was a boundary and `งาน` ("work") was linked
    /// inside `พนักงาน` ("employee").
    #[test]
    fn autolink_never_splices_a_link_into_a_thai_word() {
        let mut targets = BTreeMap::new();
        targets.insert("work".to_string(), "งาน".to_string());
        let inside = autolink("พนักงานทุกคนได้รับค่าจ้าง", "other", &targets, &[], None, "th");
        assert!(!inside.contains("[[work"), "linked inside a word: {inside}");

        // Set apart by spaces or Latin text, it is a mention and links.
        let apart = autolink(
            "คำว่า งาน ในที่นี้หมายถึง labour",
            "other",
            &targets,
            &[],
            None,
            "th",
        );
        assert!(apart.contains("[[work|งาน]]"), "{apart}");
    }

    #[test]
    fn a_note_never_links_to_itself() {
        let body =
            "เจวอนส์พบว่า [[jevons-paradox|การบริโภครวมกลับเพิ่ม]] ดู [[baumol]] และ [[jevons-paradox]]";
        let out = drop_self_links(body, "jevons-paradox");
        assert_eq!(
            out,
            "เจวอนส์พบว่า การบริโภครวมกลับเพิ่ม ดู [[baumol]] และ jevons-paradox"
        );
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
    fn append_update_merges_sources_into_one_list() {
        let _h = crate::research::test_helpers::scoped_home();
        let kref = crate::kms::create("append-merge-rt", crate::kms::KmsScope::Project).unwrap();
        let note = NotePlan {
            slug: "collingridge".into(),
            kind: NoteKind::Concept,
            title: "Collingridge".into(),
            action: Action::Create,
            claim_ids: vec!["s1c1".into()],
            related: vec!["moral-luck".into()],
            role: String::new(),
            outline: Vec::new(),
        };
        let one = vec![(
            1u32,
            "Src one".to_string(),
            "https://s1".to_string(),
            String::new(),
        )];
        let c1: BTreeSet<u32> = [1u32].into_iter().collect();
        let w = persist_note(NoteToPersist {
            kref: &kref,
            note: &note,
            body: "Reversibility matters [1].",
            cited: &c1,
            claim_count: 1,
            confidence: 0.9,
            today: "2026-09-20",
            append: false,
            sources_meta: &one,
        })
        .unwrap();

        let two = vec![
            (
                1u32,
                "Src one".to_string(),
                "https://s1".to_string(),
                String::new(),
            ),
            (
                2u32,
                "Src two".to_string(),
                "https://s2".to_string(),
                String::new(),
            ),
        ];
        let c2: BTreeSet<u32> = [2u32].into_iter().collect();
        let upd = NotePlan {
            action: Action::Update,
            ..note.clone()
        };
        persist_note(NoteToPersist {
            kref: &kref,
            note: &upd,
            body: "A later account [2].",
            cited: &c2,
            claim_count: 1,
            confidence: 0.9,
            today: "2026-09-21",
            append: true,
            sources_meta: &two,
        })
        .unwrap();

        let raw = std::fs::read_to_string(&w.path).unwrap();
        assert!(
            raw.contains("Reversibility matters"),
            "the page's own text survives: {raw}"
        );
        assert!(raw.contains("## Update 2026-09-21"), "{raw}");
        assert_eq!(
            raw.matches("## Sources").count(),
            1,
            "one list, not one per run: {raw}"
        );
        assert!(
            raw.contains("sources: [1, 2]"),
            "the old source stays declared alongside the new: {raw}"
        );
        let (_, body) = crate::kms::parse_frontmatter(&raw);
        let idx = body.find("## Sources").unwrap();
        assert!(
            body[..idx].contains("## Update 2026-09-21"),
            "the new section goes above the list, not after it: {raw}"
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
        let meta = vec![(
            1u32,
            "Src".to_string(),
            "https://s1".to_string(),
            String::new(),
        )];
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
