//! Research v2 step 0: what the knowledge base already knows, and how
//! much a round added to it.

use super::digest::{normalize_for_match, Digest};
use crate::kms::KmsRef;
use std::collections::HashSet;

/// Per-note budget for the summary the planner sees. Paired with the
/// 150-note cap on that list in `plan.rs`, the section stays bounded as
/// the knowledge base grows.
pub const SUMMARY_MAX_CHARS: usize = 400;

#[derive(Debug, Clone, PartialEq)]
pub struct KnownNote {
    pub slug: String,
    pub title: String,
    pub summary: String,
    /// `kind:` frontmatter (`entity` / `concept` / `claim` / `moc`), empty when absent.
    pub kind: String,
    /// `updated:` frontmatter (`YYYY-MM-DD`) when present.
    pub updated: Option<String>,
    /// Whether `related:` names anything — i.e. whether this page is a
    /// hub other pages hang off. `kind: moc` is not the same question:
    /// a one-page ingest is written through the topic-page branch and
    /// carries that label with an empty `related:`, and reading the
    /// label instead of the list is what hid every such page from
    /// `related-refresh`.
    pub has_children: bool,
}

/// What a text is "about", cheaply: lower-cased words of three letters or
/// more for scripts that put spaces between words, and character pairs for
/// the ones that do not — a Thai sentence is one unbroken run, and as a
/// single "word" it matches nothing.
pub(crate) fn relevance_terms(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for token in text.split(|c: char| !c.is_alphanumeric() && !crate::kms::is_spaceless_script(c)) {
        if token.is_empty() {
            continue;
        }
        if token.is_ascii() {
            if token.len() >= 3 {
                out.insert(token.to_ascii_lowercase());
            }
            continue;
        }
        let chars: Vec<char> = token.chars().flat_map(|c| c.to_lowercase()).collect();
        if chars.len() == 1 {
            out.insert(chars[0].to_string());
        }
        for pair in chars.windows(2) {
            out.insert(pair.iter().collect());
        }
    }
    out
}

/// dev-plan/64 P4.6: the notes a run is most likely to touch, first.
///
/// The planner is shown the first 150 existing notes and each digest the
/// first 80 slugs, and both lists were alphabetical — so in a vault past
/// that size a run about `zoning` was told about `abundance-…` through
/// `m…` and nothing else, planned a new note for one that existed, and the
/// vault grew a duplicate. `about` is whatever describes the run: the
/// query, and once there are digests, the entities they found. A note
/// whose slug IS one of `exact` (an entity of this run) always leads.
pub fn rank_known(known: &[KnownNote], about: &str, exact: &HashSet<&str>) -> Vec<KnownNote> {
    let want = relevance_terms(about);
    let mut scored: Vec<(usize, &KnownNote)> = known
        .iter()
        .map(|k| {
            let have = relevance_terms(&format!("{} {} {}", k.slug, k.title, k.summary));
            let overlap = want.intersection(&have).count();
            let bonus = if exact.contains(k.slug.as_str()) {
                1_000_000
            } else {
                0
            };
            (overlap + bonus, k)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.slug.cmp(&b.1.slug)));
    scored.into_iter().map(|(_, k)| k.clone()).collect()
}

/// Every page in the KMS as `(slug, title, first prose line)`. Reads
/// files, never the LLM. Skips `_summary` and other underscore pages.
pub fn load_known(kref: &KmsRef) -> Vec<KnownNote> {
    let dir = kref.root.join("pages");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.starts_with('_') || stem.starts_with('.') {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (fm, body) = crate::kms::parse_frontmatter(&raw);
        // A placeholder is not knowledge. Listed here, its "this page is
        // being written by /research" line told the planner the subject
        // was already covered, so it planned an `update` of nothing.
        if fm.get("status").map(|s| s.trim()) == Some("researching") {
            continue;
        }
        let title = fm
            .get("title")
            .map(|t| t.trim_matches('"').to_string())
            .unwrap_or_else(|| stem.to_string());
        out.push(KnownNote {
            slug: stem.to_string(),
            title,
            summary: first_prose_line(&body),
            kind: fm.get("kind").cloned().unwrap_or_default(),
            updated: fm.get("updated").cloned().filter(|u| !u.is_empty()),
            has_children: fm
                .get("related")
                .map(|r| {
                    r.trim()
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .trim()
                })
                .is_some_and(|r| !r.is_empty()),
        });
    }
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

/// First line that is not a heading, the injected `Description:` line,
/// a rule, or blank. Clamped to [`SUMMARY_MAX_CHARS`].
///
/// This feeds one line per existing note into the planner's prompt, and
/// it is the only thing the planner has to judge whether a note already
/// covers what it is about to create. A summary cut too short is how a
/// near-duplicate gets planned, and how a note about one aspect of a
/// thing ends up taking the thing's own name. Now that a note opens with
/// a deliberate description rather than a bare definition, there is more
/// worth passing on — bounded, because this is multiplied by every note
/// in the knowledge base.
pub fn first_prose_line(body: &str) -> String {
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty()
            || t.starts_with('#')
            || t.starts_with("Description:")
            || t.starts_with("---")
            || t.starts_with("```")
        {
            continue;
        }
        let mut s: String = t.chars().take(SUMMARY_MAX_CHARS).collect();
        if t.chars().count() > SUMMARY_MAX_CHARS {
            s.push('…');
        }
        return s;
    }
    String::new()
}

/// Deterministic stop signal: the share of a round's entities + claims
/// that the run had not seen before.
#[derive(Debug, Default)]
pub struct Novelty {
    entities: HashSet<String>,
    claims: HashSet<String>,
}

impl Novelty {
    /// Fold a round's digests in; returns `(new_items, total_items_in_round, ratio)`.
    pub fn absorb(&mut self, round: &[Digest]) -> (u32, u32, f32) {
        let mut new = 0u32;
        let mut total = 0u32;
        let mut ent_new = 0u32;
        let mut ent_total = 0u32;
        for d in round {
            for e in &d.entities {
                total += 1;
                ent_total += 1;
                if self.entities.insert(e.slug.clone()) {
                    new += 1;
                    ent_new += 1;
                }
            }
            for c in &d.claims {
                total += 1;
                let key: String = normalize_for_match(&c.text).chars().take(120).collect();
                if self.claims.insert(key) {
                    new += 1;
                }
            }
        }
        // Stop signal uses entities only: they are LLM-normalised slugs
        // that repeat across sources, while claim text is reworded by
        // every source and would keep novelty near 100% forever.
        let ratio = if ent_total == 0 {
            if total == 0 {
                0.0
            } else {
                new as f32 / total as f32
            }
        } else {
            ent_new as f32 / ent_total as f32
        };
        (new, total, ratio)
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }
    pub fn claim_count(&self) -> usize {
        self.claims.len()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn known_notes_are_ranked_by_what_the_run_is_about() {
        let note = |slug: &str, title: &str| KnownNote {
            slug: slug.into(),
            title: title.into(),
            summary: String::new(),
            kind: String::new(),
            updated: None,
            has_children: false,
        };
        let mut known: Vec<KnownNote> = (0..200)
            .map(|i| note(&format!("aaa-filler-{i:03}"), "Unrelated filler"))
            .collect();
        known.push(note("zoning-reform", "Zoning reform and housing supply"));
        known.push(note("hidden-poverty-households", "ครัวเรือนยากจนแฝง"));
        known.push(note("zz-entity", "Named by the run"));

        let none = HashSet::new();
        let en = rank_known(&known, "how does zoning limit housing", &none);
        assert_eq!(en[0].slug, "zoning-reform");
        // Thai has no spaces: the query is one run, the title another.
        let th = rank_known(&known, "ครัวเรือนยากจนแฝงคืออะไร วัดยังไง", &none);
        assert_eq!(th[0].slug, "hidden-poverty-households");
        // An entity of the run outranks any amount of word overlap.
        let exact: HashSet<&str> = ["zz-entity"].into_iter().collect();
        assert_eq!(
            rank_known(&known, "zoning housing", &exact)[0].slug,
            "zz-entity"
        );
        // Nothing is dropped, and ties keep a stable order.
        assert_eq!(en.len(), known.len());
        assert_eq!(en[1].slug, "aaa-filler-000");
        assert_eq!(en[2].slug, "aaa-filler-001");
    }

    use super::*;
    use crate::research::digest::{Claim, Entity};

    fn d(ents: &[&str], claims: &[&str]) -> Digest {
        Digest {
            url: "u".into(),
            title: "t".into(),
            fetched: "d".into(),
            source: 1,
            entities: ents
                .iter()
                .map(|e| Entity {
                    name: e.to_string(),
                    slug: e.to_string(),
                    kind: String::new(),
                })
                .collect(),
            claims: claims
                .iter()
                .enumerate()
                .map(|(i, c)| Claim {
                    id: format!("s1c{i}"),
                    text: c.to_string(),
                    quote: c.to_string(),
                    entities: vec![],
                    confidence: 0.8,
                    source: 1,
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
    fn novelty_drops_as_rounds_repeat() {
        let mut n = Novelty::default();
        let (new, total, r) = n.absorb(&[d(&["a", "b"], &["x", "y"])]);
        assert_eq!((new, total), (4, 4));
        assert!((r - 1.0).abs() < 1e-6);
        let (new, total, r) = n.absorb(&[d(&["a", "c"], &["x", "Y "])]);
        assert_eq!((new, total), (1, 4));
        assert!((r - 0.5).abs() < 1e-6, "entity-based: 1 new of 2 entities");
        assert_eq!(n.entity_count(), 3);
        assert_eq!(n.claim_count(), 2);
    }

    #[test]
    fn first_prose_line_skips_header_boilerplate() {
        let body = "# Title\nDescription: x\n---\n\n## Heading\nThe real abstract. More.\n";
        assert_eq!(first_prose_line(body), "The real abstract. More.");
    }

    #[test]
    fn load_known_reads_pages() {
        let _h = crate::research::test_helpers::scoped_home();
        let kref = crate::kms::create("known-rt", crate::kms::KmsScope::Project).unwrap();
        crate::kms::write_page(
            &kref,
            "overtime-pay",
            "---\ntitle: \"Overtime pay\"\n---\n\nOvertime is 1.5x.\n",
        )
        .unwrap();
        crate::kms::write_page(&kref, "_summary", "---\ntitle: s\n---\n\nx\n").unwrap();
        let k = load_known(&kref);
        assert_eq!(k.len(), 1);
        assert_eq!(k[0].slug, "overtime-pay");
        assert_eq!(k[0].title, "Overtime pay");
        assert_eq!(k[0].summary, "Overtime is 1.5x.");
    }
}
