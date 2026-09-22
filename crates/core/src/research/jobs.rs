//! dev-plan/64 P5.5: which research runs are actually alive.
//!
//! The viewer's "create page" writes a `status: researching`
//! placeholder before the run starts, so the link it just made
//! resolves at once. A run that fails *gracefully* takes its
//! placeholder with it ([`super::remove_abandoned_placeholder`]).
//! A run that dies — the app quits, the machine sleeps and the
//! provider connection never comes back, a `kill -9` — does not.
//!
//! What is left behind is worse than a stray file: it sits in the
//! index and the graph saying "this page is being written", and every
//! later planner reads it as a note that already covers the subject,
//! so the subject never gets researched again.
//!
//! Deleting every `researching` page at startup is not the fix. The
//! desktop window runs one `--serve` child per agent and they share
//! one vault, so "not a run *I* know about" includes every run the
//! agent in the next tab is in the middle of. A claim with a heartbeat
//! is what tells a live run from a dead one across processes.
//!
//! Stored at `<kms>/.research/jobs.json`.

use crate::kms::KmsRef;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// How often a running job refreshes its claim.
const HEARTBEAT_SECS: u64 = 30;
/// How old a heartbeat has to be before the run behind it is presumed
/// dead. Ten missed beats: long enough that a machine asleep for a
/// minute, or a digest phase that blocks the runtime, is not mistaken
/// for a corpse.
const STALE_SECS: u64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub query: String,
    /// The page this run is writing, which is the placeholder's slug.
    pub slug: String,
    /// `research` | `refresh` | `ingest` | `selection`.
    #[serde(default)]
    pub mode: String,
    /// Unix seconds. Compared, never displayed.
    pub started: u64,
    pub heartbeat: u64,
    /// Whose process. Diagnostic only — a pid can be reused, so it is
    /// never what decides whether a run is alive.
    #[serde(default)]
    pub pid: u32,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Jobs {
    #[serde(default)]
    pub jobs: BTreeMap<String, Claim>,
}

fn path(kref: &KmsRef) -> PathBuf {
    kref.root.join(".research").join("jobs.json")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn load(kref: &KmsRef) -> Jobs {
    std::fs::read_to_string(path(kref))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(kref: &KmsRef, jobs: &Jobs) {
    let p = path(kref);
    if let Some(d) = p.parent() {
        if std::fs::create_dir_all(d).is_err() {
            return;
        }
    }
    let _ = crate::kms::write_file(&p, serde_json::to_string_pretty(jobs).unwrap_or_default());
}

/// Read, change, write — under the KMS lock, because two agents in one
/// workspace write this file and a lost update is a run that looks
/// dead while it is running.
fn update(kref: &KmsRef, f: impl FnOnce(&mut Jobs)) {
    let _ = crate::kms::with_kms_lock(kref, || -> crate::error::Result<()> {
        let mut jobs = load(kref);
        f(&mut jobs);
        save(kref, &jobs);
        Ok(())
    });
}

pub fn claim(kref: &KmsRef, id: &str, query: &str, slug: &str, mode: &str) {
    let t = now();
    let c = Claim {
        query: query.to_string(),
        slug: slug.to_string(),
        mode: mode.to_string(),
        started: t,
        heartbeat: t,
        pid: std::process::id(),
    };
    update(kref, |j| {
        j.jobs.insert(id.to_string(), c);
    });
}

pub fn beat(kref: &KmsRef, id: &str) {
    let t = now();
    update(kref, |j| {
        if let Some(c) = j.jobs.get_mut(id) {
            c.heartbeat = t;
        }
    });
}

pub fn release(kref: &KmsRef, id: &str) {
    update(kref, |j| {
        j.jobs.remove(id);
    });
}

fn is_live(c: &Claim, at: u64) -> bool {
    at.saturating_sub(c.heartbeat) < STALE_SECS
}

/// Clear what dead runs left behind: prune their claims, and delete
/// the `status: researching` placeholders nothing is writing any more.
///
/// Only a page that *still* says `status: researching` is touched — a
/// run that got as far as writing the real page has replaced that, and
/// keeps its work. Returns the slugs cleared, for the caller to report.
pub fn reconcile(kref: &KmsRef) -> Vec<String> {
    let at = now();
    let jobs = load(kref);
    let live: std::collections::BTreeSet<&str> = jobs
        .jobs
        .values()
        .filter(|c| is_live(c, at))
        .map(|c| c.slug.as_str())
        .collect();

    let mut cleared = Vec::new();
    if let Ok(rd) = std::fs::read_dir(kref.pages_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if live.contains(stem) {
                continue;
            }
            // Frontmatter only: a reconcile runs on every browse, and
            // reading a whole vault's prose to look at one key is how a
            // cheap check becomes a reason not to run it.
            let (fm, _) = crate::kms::parse_frontmatter(&crate::kms::read_head(&p, 4096));
            if fm.get("status").map(|s| s.trim()) != Some("researching") {
                continue;
            }
            // Through `delete_page`, so the placeholder goes to the
            // trash and leaves the index — a reconcile is undoable too.
            if crate::kms::delete_page(kref, stem).is_ok() {
                cleared.push(stem.to_string());
            }
        }
    }
    if jobs.jobs.values().any(|c| !is_live(c, at)) {
        update(kref, |j| j.jobs.retain(|_, c| is_live(c, at)));
    }
    cleared
}

/// Holds a claim for as long as it is alive, beating every
/// [`HEARTBEAT_SECS`] on a background task.
///
/// A heartbeat per phase would not do: a digest round can hold one
/// phase for minutes, and a run that looks stale is a run whose page
/// another process is about to delete out from under it.
///
/// `Drop` releases the claim, so every way out of the pipeline —
/// success, error, cancel, and the panic the caller catches — ends
/// with the claim gone.
pub struct Guard {
    kref: KmsRef,
    id: String,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Guard {
    pub fn start(kref: &KmsRef, id: &str, query: &str, slug: &str, mode: &str) -> Self {
        claim(kref, id, query, slug, mode);
        let (k, i) = (kref.clone(), id.to_string());
        // Off a runtime there is nothing to beat from; the claim then
        // simply ages out, which is the safe direction to fail in.
        let task = tokio::runtime::Handle::try_current().ok().map(|h| {
            h.spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(HEARTBEAT_SECS)).await;
                    beat(&k, &i);
                }
            })
        });
        Guard {
            kref: kref.clone(),
            id: id.to_string(),
            task,
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(t) = &self.task {
            t.abort();
        }
        release(&self.kref, &self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::{create, KmsScope};

    fn placeholder(kref: &KmsRef, slug: &str) {
        std::fs::write(
            kref.pages_dir().join(format!("{slug}.md")),
            "---\ntitle: T\nstatus: researching\n---\n\nbeing written…\n",
        )
        .unwrap();
    }

    /// The whole point: a live run in another process keeps its
    /// placeholder, a dead one loses it, and a page that was actually
    /// written is never touched.
    #[test]
    fn reconcile_clears_what_died_and_leaves_what_is_running() {
        let _h = crate::research::test_helpers::scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        placeholder(&k, "alive");
        placeholder(&k, "dead");
        placeholder(&k, "never-claimed");
        std::fs::write(
            k.pages_dir().join("finished.md"),
            "---\ntitle: F\n---\n\nreal prose\n",
        )
        .unwrap();

        claim(&k, "j-alive", "q", "alive", "research");
        claim(&k, "j-dead", "q", "dead", "research");
        // Age the second claim past the staleness line.
        update(&k, |j| {
            let c = j.jobs.get_mut("j-dead").unwrap();
            c.heartbeat = now() - STALE_SECS - 1;
        });

        let mut cleared = reconcile(&k);
        cleared.sort();
        assert_eq!(cleared, vec!["dead", "never-claimed"], "{cleared:?}");
        assert!(
            k.pages_dir().join("alive.md").exists(),
            "live run clobbered"
        );
        assert!(k.pages_dir().join("finished.md").exists());
        assert!(!k.pages_dir().join("dead.md").exists());
        // The dead claim is pruned; the live one stays.
        let left = load(&k);
        assert!(left.jobs.contains_key("j-alive"));
        assert!(!left.jobs.contains_key("j-dead"));
        // A placeholder that is deleted is recoverable, like any page.
        assert!(
            crate::kms_trash::list(&k)
                .iter()
                .any(|t| t.rel == "pages/dead.md"),
            "a reconcile should be undoable"
        );
    }

    /// A second reconcile over a settled vault does nothing — it runs
    /// on every browse, so it has to be free and silent when clean.
    #[test]
    fn reconcile_is_idempotent() {
        let _h = crate::research::test_helpers::scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        placeholder(&k, "dead");
        assert_eq!(reconcile(&k), vec!["dead"]);
        assert!(reconcile(&k).is_empty());
        assert!(reconcile(&k).is_empty());
    }
}
