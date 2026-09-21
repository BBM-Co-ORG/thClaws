//! dev-plan/64 P3.1: nothing a knowledge base held is destroyed by the
//! command that removes it.
//!
//! A page delete, a page overwrite, a forced re-ingest and a whole-KMS drop
//! each used to end in `remove_file` / `remove_dir_all` / a bare `write`.
//! For a researcher or a writer the vault is the work; one wrong `KmsWrite`
//! by a model, one `--force` with a mistyped alias, one click on the wrong
//! row of the sidebar, and it was gone with no way back. Now each of those
//! first puts what it is about to replace under a `.trash/` folder:
//!
//! ```text
//! <kms>/.trash/<stamp>-<why>/pages/<stem>.md      one page or source
//! <scope>/.trash/<stamp>-drop-<name>/             a whole dropped KMS
//! ```
//!
//! `.`-prefixed, so the KMS lister, the search index and the project-KMS
//! migration all pass over it. Entries older than [`KEEP_DAYS`] are pruned
//! the next time something is put in.

use crate::error::{Error, Result};
use crate::kms::KmsRef;
use std::path::{Path, PathBuf};

const TRASH_DIR: &str = ".trash";
/// How long a trashed version is kept. Long enough to notice a bad research
/// run the week after; short enough that a vault rewritten daily does not
/// double in size.
pub const KEEP_DAYS: i64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Delete,
    Overwrite,
    Drop,
}

impl Why {
    fn as_str(self) -> &'static str {
        match self {
            Why::Delete => "delete",
            Why::Overwrite => "overwrite",
            Why::Drop => "drop",
        }
    }
}

/// Sortable, filesystem-safe, and unique enough that two writes in one
/// second do not share a folder.
fn stamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%S%3f").to_string()
}

fn stamp_age_days(entry: &str) -> Option<i64> {
    let ts = entry.get(..18)?;
    let t = chrono::NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%S%3f").ok()?;
    Some((chrono::Utc::now().naive_utc() - t).num_days())
}

fn prune(trash: &Path) {
    let Ok(rd) = std::fs::read_dir(trash) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if stamp_age_days(&name).is_some_and(|d| d > KEEP_DAYS) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Keep a copy of `<kms>/<rel>` (`pages/x.md`, `sources/y.md`) before it is
/// replaced. Best effort: a write that cannot be backed up still happens,
/// because the usual reason is a full disk and the write would fail too.
/// Skipped when the file does not exist or already holds `next`.
pub fn keep_before_overwrite(kref: &KmsRef, rel: &str, next: &[u8]) {
    let from = kref.root.join(rel);
    let Ok(current) = std::fs::read(&from) else {
        return;
    };
    if current == next {
        return;
    }
    let trash = kref.root.join(TRASH_DIR);
    let to = trash
        .join(format!("{}-{}", stamp(), Why::Overwrite.as_str()))
        .join(rel);
    let done = to
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .and_then(|_| std::fs::write(&to, &current));
    if let Err(e) = done {
        eprintln!("[kms] could not keep the previous {rel} before overwriting it: {e}");
        return;
    }
    prune(&trash);
}

/// Move `<kms>/<rel>` into the trash instead of deleting it. Unlike an
/// overwrite this is refused when it cannot be kept: the caller was about
/// to destroy the only copy.
pub fn move_to_trash(kref: &KmsRef, rel: &str) -> Result<PathBuf> {
    let from = kref.root.join(rel);
    let trash = kref.root.join(TRASH_DIR);
    let to = trash
        .join(format!("{}-{}", stamp(), Why::Delete.as_str()))
        .join(rel);
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)
            .map_err(|e| Error::Tool(format!("create {}: {e}", p.display())))?;
    }
    std::fs::rename(&from, &to).map_err(|e| {
        Error::Tool(format!(
            "could not move {} to the trash, so it was not deleted: {e}",
            from.display()
        ))
    })?;
    prune(&trash);
    Ok(to)
}

/// Move a whole KMS out of its scope folder. Returns where it went.
pub fn drop_kms(kref: &KmsRef) -> Result<PathBuf> {
    let scope = kref
        .root
        .parent()
        .ok_or_else(|| Error::Tool(format!("KMS '{}' has no parent directory", kref.name)))?;
    let trash = scope.join(TRASH_DIR);
    std::fs::create_dir_all(&trash)
        .map_err(|e| Error::Tool(format!("create {}: {e}", trash.display())))?;
    let to = trash.join(format!("{}-{}-{}", stamp(), Why::Drop.as_str(), kref.name));
    std::fs::rename(&kref.root, &to).map_err(|e| {
        Error::Tool(format!(
            "could not move {} to the trash, so it was not dropped: {e}",
            kref.root.display()
        ))
    })?;
    prune(&trash);
    Ok(to)
}

#[derive(Debug, Clone)]
pub struct Trashed {
    /// The entry folder's name: `<stamp>-<why>`.
    pub entry: String,
    /// `pages/x.md` or `sources/y.md`.
    pub rel: String,
    pub path: PathBuf,
    pub bytes: u64,
}

/// Everything in one KMS's trash, newest first.
pub fn list(kref: &KmsRef) -> Vec<Trashed> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(kref.root.join(TRASH_DIR)) else {
        return out;
    };
    for e in rd.flatten() {
        let entry = e.file_name().to_string_lossy().into_owned();
        for layer in ["pages", "sources"] {
            let Ok(files) = std::fs::read_dir(e.path().join(layer)) else {
                continue;
            };
            for f in files.flatten() {
                let Ok(meta) = f.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                out.push(Trashed {
                    entry: entry.clone(),
                    rel: format!("{layer}/{}", f.file_name().to_string_lossy()),
                    path: f.path(),
                    bytes: meta.len(),
                });
            }
        }
    }
    out.sort_by(|a, b| b.entry.cmp(&a.entry).then(a.rel.cmp(&b.rel)));
    out
}

/// One kept version, shaped for the GUI (dev-plan/64 P5.6). The trash
/// has existed since P3.1 and was reachable only from `/kms trash`; a
/// person who overwrote a page from the sidebar had no way to see that
/// the old text was still there.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TrashRow {
    pub entry: String,
    /// The stamp rendered for a reader: `2026-09-20 11:04:12 UTC`.
    pub when: String,
    /// `delete` | `overwrite` | `drop` — what put it here.
    pub why: String,
    /// `page` or `source`.
    pub kind: String,
    /// The file stem, which is what `restore` takes.
    pub name: String,
    pub bytes: u64,
}

/// [`list`], shaped for the GUI.
pub fn list_rows(kref: &KmsRef) -> Vec<TrashRow> {
    list(kref)
        .into_iter()
        .map(|t| {
            let (layer, file) = t.rel.split_once('/').unwrap_or(("", t.rel.as_str()));
            TrashRow {
                when: when(&t.entry),
                why: t
                    .entry
                    .split_once('-')
                    .map(|(_, w)| w.to_string())
                    .unwrap_or_default(),
                entry: t.entry.clone(),
                kind: if layer == "sources" { "source" } else { "page" }.into(),
                name: file.trim_end_matches(".md").to_string(),
                bytes: t.bytes,
            }
        })
        .collect()
}

fn when(entry: &str) -> String {
    match entry.get(..15) {
        Some(ts) => match chrono::NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%S") {
            Ok(t) => format!("{} UTC", t.format("%Y-%m-%d %H:%M:%S")),
            Err(_) => entry.to_string(),
        },
        None => entry.to_string(),
    }
}

/// `/kms trash <name>`.
pub fn apply_list(name: &str) -> Result<String> {
    let kref =
        crate::kms::resolve(name).ok_or_else(|| Error::Tool(format!("no KMS named '{name}'")))?;
    let items = list(&kref);
    if items.is_empty() {
        return Ok(format!(
            "'{}' has nothing in its trash (versions are kept {KEEP_DAYS} days).",
            kref.name
        ));
    }
    const SHOWN: usize = 40;
    let mut out = format!(
        "'{}' — {} kept version(s), newest first:\n",
        kref.name,
        items.len()
    );
    for t in items.iter().take(SHOWN) {
        let why = t.entry.rsplit('-').next().unwrap_or("");
        out.push_str(&format!(
            "  {}  {:<9}  {}  ({} B)\n",
            when(&t.entry),
            why,
            t.rel,
            t.bytes
        ));
    }
    if items.len() > SHOWN {
        out.push_str(&format!("  … and {} older\n", items.len() - SHOWN));
    }
    out.push_str(&format!(
        "Restore the newest version of a page with `/kms restore {} <page>`.",
        crate::repl::quote_slash_arg(&kref.name)
    ));
    Ok(out)
}

/// Bring back the newest kept version of a page. The page it replaces — if
/// there is one — goes into the trash first, by way of `write_page`, so a
/// restore can itself be undone.
pub fn restore_page(kref: &KmsRef, page: &str) -> Result<String> {
    let stem = page.trim().trim_end_matches(".md");
    let rel = format!("pages/{stem}.md");
    let live = std::fs::read(kref.root.join(&rel)).ok();
    // The newest version that differs from what is there now: an overwrite
    // keeps the old text, so the newest entry is normally the one wanted,
    // but after a restore it is the text just replaced.
    let pick = list(kref)
        .into_iter()
        .filter(|t| t.rel == rel)
        .find(|t| std::fs::read(&t.path).ok() != live);
    let Some(t) = pick else {
        return Err(Error::Tool(format!(
            "nothing in the trash of '{}' for page '{stem}' that differs from the current page",
            kref.name
        )));
    };
    let content = std::fs::read_to_string(&t.path)
        .map_err(|e| Error::Tool(format!("read {}: {e}", t.path.display())))?;
    crate::kms::write_page(kref, stem, &content)?;
    Ok(format!(
        "restored '{stem}' in '{}' to its version from {} ({} B). The page it replaced is in the trash.",
        kref.name,
        when(&t.entry),
        t.bytes
    ))
}

/// Bring back a dropped KMS: the newest `…-drop-<name>` in the project
/// scope, then the user scope. Refused when the name is taken again.
pub fn restore_kms(name: &str) -> Result<String> {
    if crate::kms::resolve_exact(name).is_some() {
        return Err(Error::Tool(format!(
            "a KMS named '{name}' exists — rename it first, or name a page to restore"
        )));
    }
    let suffix = format!("-{}-{name}", Why::Drop.as_str());
    for scope_root in crate::kms::writable_scope_roots() {
        let trash = scope_root.join(TRASH_DIR);
        let Ok(rd) = std::fs::read_dir(&trash) else {
            continue;
        };
        let mut hits: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(&suffix))
            })
            .collect();
        hits.sort();
        let Some(from) = hits.pop() else { continue };
        let to = scope_root.join(name);
        std::fs::rename(&from, &to)
            .map_err(|e| Error::Tool(format!("restore {}: {e}", from.display())))?;
        return Ok(format!(
            "restored KMS '{name}' to {}. Attach it with `/kms use {}`.",
            to.display(),
            crate::repl::quote_slash_arg(name)
        ));
    }
    Err(Error::Tool(format!(
        "no dropped KMS named '{name}' in the trash (drops are kept {KEEP_DAYS} days)"
    )))
}

/// `/kms restore <name> [page]`.
pub fn apply_restore(name: &str, page: Option<&str>) -> Result<String> {
    match page {
        Some(p) => {
            let kref = crate::kms::resolve(name)
                .ok_or_else(|| Error::Tool(format!("no KMS named '{name}'")))?;
            restore_page(&kref, p)
        }
        None => restore_kms(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::{create, delete_page, write_page, KmsScope};

    /// What the sidebar's Trash section renders. The stem is what a
    /// restore takes, so it must come back without its extension, and
    /// `why` must survive a Thai name full of nothing the splitter can
    /// use except the one `-` the stamp puts there.
    #[test]
    fn list_rows_says_what_each_kept_version_is_and_why() {
        let _h = crate::research::test_helpers::scoped_home();
        let k = create("งานวิจัย", KmsScope::Project).unwrap();
        let page = "ยุคที่ความฉลาดล้นเหลือ";
        write_page(&k, page, "---\ntitle: ยุค\n---\n\n# ยุค\n\nฉบับเต็ม\n").unwrap();
        write_page(&k, page, "---\ntitle: ยุค\n---\n\n# ยุค\n\nครึ่งเดียว\n").unwrap();
        write_page(&k, "notes", "---\ntitle: N\n---\n\n# N\n\none\n").unwrap();
        delete_page(&k, "notes").unwrap();

        let rows = list_rows(&k);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let by_name = |n: &str| rows.iter().find(|r| r.name == n).cloned().unwrap();
        let over = by_name(page);
        assert_eq!(over.kind, "page");
        assert_eq!(over.why, "overwrite");
        assert!(over.bytes > 0);
        // `when` is the stamp rendered, not the raw folder name.
        assert!(
            over.when.contains('-') && over.when.ends_with("UTC"),
            "unrendered stamp: {}",
            over.when
        );
        assert_eq!(by_name("notes").why, "delete");
        // The stem is what `restore_page` takes — a round trip proves it.
        restore_page(&k, &over.name).unwrap();
        assert!(
            std::fs::read_to_string(k.pages_dir().join(format!("{page}.md")))
                .unwrap()
                .contains("ฉบับเต็ม")
        );
    }

    /// The three ways a page's text used to be destroyed — a model's
    /// `KmsWrite` over it, a delete, a drop of the whole base — each leave
    /// it recoverable, and recovering is itself undoable.
    #[test]
    fn nothing_a_kms_held_is_destroyed_by_the_command_that_removes_it() {
        let _h = crate::research::test_helpers::scoped_home();
        let k = create("งานวิจัย", KmsScope::Project).unwrap();
        let v1 = "---\ntitle: ยุค\n---\n\n# ยุค\n\nฉบับเต็ม สามหมื่นตัวอักษร\n";
        write_page(&k, "ยุคที่ความฉลาดล้นเหลือ", v1).unwrap();
        assert!(list(&k).is_empty(), "a first write replaces nothing");
        let on_disk_v1 =
            std::fs::read_to_string(k.pages_dir().join("ยุคที่ความฉลาดล้นเหลือ.md")).unwrap();

        // The accident: a rewrite from half a read.
        write_page(
            &k,
            "ยุคที่ความฉลาดล้นเหลือ",
            "---\ntitle: ยุค\n---\n\n# ยุค\n\nครึ่งเดียว\n",
        )
        .unwrap();
        let kept = list(&k);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].rel, "pages/ยุคที่ความฉลาดล้นเหลือ.md");
        assert!(kept[0].entry.ends_with("-overwrite"));

        let msg = restore_page(&k, "ยุคที่ความฉลาดล้นเหลือ").unwrap();
        assert!(msg.contains("restored"), "{msg}");
        let back = std::fs::read_to_string(k.pages_dir().join("ยุคที่ความฉลาดล้นเหลือ.md")).unwrap();
        assert_eq!(back, on_disk_v1);
        assert_eq!(list(&k).len(), 2, "the half page went into the trash too");
        // Undo the undo: the newest version that differs is the half page.
        restore_page(&k, "ยุคที่ความฉลาดล้นเหลือ").unwrap();
        assert!(
            std::fs::read_to_string(k.pages_dir().join("ยุคที่ความฉลาดล้นเหลือ.md"))
                .unwrap()
                .contains("ครึ่งเดียว")
        );
        restore_page(&k, "ยุคที่ความฉลาดล้นเหลือ").unwrap();

        // Writing the same bytes again keeps nothing new.
        let n = list(&k).len();
        write_page(&k, "ยุคที่ความฉลาดล้นเหลือ", &on_disk_v1).unwrap();
        assert_eq!(list(&k).len(), n);

        delete_page(&k, "ยุคที่ความฉลาดล้นเหลือ").unwrap();
        assert!(!k.pages_dir().join("ยุคที่ความฉลาดล้นเหลือ.md").exists());
        assert!(list(&k)[0].entry.ends_with("-delete"));
        restore_page(&k, "ยุคที่ความฉลาดล้นเหลือ").unwrap();
        assert_eq!(
            std::fs::read_to_string(k.pages_dir().join("ยุคที่ความฉลาดล้นเหลือ.md")).unwrap(),
            on_disk_v1
        );

        // The trash is not a page, a source, or a knowledge base.
        assert_eq!(crate::kms::page_count(&k), 1);
        let report = crate::kms::remove("งานวิจัย").unwrap();
        assert!(crate::kms::resolve("งานวิจัย").is_none());
        assert!(report.trashed.join("pages/ยุคที่ความฉลาดล้นเหลือ.md").is_file());
        assert!(crate::kms::list_all()
            .iter()
            .all(|k| !k.name.starts_with('.')));

        assert!(apply_restore("งานวิจัย", None)
            .unwrap()
            .contains("restored KMS"));
        let k = crate::kms::resolve("งานวิจัย").expect("it is back");
        assert_eq!(
            std::fs::read_to_string(k.pages_dir().join("ยุคที่ความฉลาดล้นเหลือ.md")).unwrap(),
            on_disk_v1
        );
        assert!(!list(&k).is_empty(), "with its own trash intact");
        assert!(
            apply_restore("งานวิจัย", None).is_err(),
            "the name is taken again"
        );
        assert!(apply_list("งานวิจัย").unwrap().contains("/kms restore"));
    }

    #[test]
    fn old_trash_is_pruned_and_new_trash_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("20200101T000000000-overwrite");
        let new = tmp.path().join(format!("{}-overwrite", stamp()));
        let odd = tmp.path().join("not-a-stamp");
        for d in [&old, &new, &odd] {
            std::fs::create_dir_all(d).unwrap();
        }
        prune(tmp.path());
        assert!(!old.exists() && new.exists() && odd.exists());
    }
}
