//! Per-KMS source registry: a URL gets one citation index for the life
//! of the knowledge base, so `[N]` in a note written last month still
//! resolves after this month's update merges new claims into it.
//! Stored at `<kms>/.research/sources.json`.

use crate::error::Result;
use crate::kms::KmsRef;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SourceRegistry {
    #[serde(default)]
    next: u32,
    /// url → (index, title)
    #[serde(default)]
    by_url: BTreeMap<String, (u32, String)>,
}

impl SourceRegistry {
    fn path(kref: &KmsRef) -> PathBuf {
        kref.root.join(".research").join("sources.json")
    }

    pub fn load(kref: &KmsRef) -> Self {
        std::fs::read_to_string(Self::path(kref))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, kref: &KmsRef) -> Result<()> {
        let p = Self::path(kref);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)
                .map_err(|e| crate::error::Error::Tool(format!("create {}: {e}", d.display())))?;
        }
        crate::kms::write_file(&p, serde_json::to_string_pretty(self).unwrap_or_default())
            .map_err(|e| crate::error::Error::Tool(format!("write {}: {e}", p.display())))
    }

    /// Index for `url`, allocating the next one on first sight. A
    /// non-empty title refreshes the stored one.
    ///
    /// Lookup is by [`canonical_url`](super::source_quality::canonical_url)
    /// so `?utm_source=…`, a fragment or a trailing slash cannot mint a
    /// second citation number for a page already in the registry. The
    /// key stored on disk stays the URL as first seen — it is what the
    /// `## Sources` block links to and what names the archived file, and
    /// rewriting it would break every `[N]` in every existing note.
    pub fn index_for(&mut self, url: &str, title: &str) -> u32 {
        if let Some(key) = self.key_for(url) {
            if let Some((i, t)) = self.by_url.get_mut(&key) {
                if !title.trim().is_empty() {
                    *t = title.to_string();
                }
                return *i;
            }
        }
        self.next += 1;
        self.by_url
            .insert(url.to_string(), (self.next, title.to_string()));
        self.next
    }

    /// The stored key naming the same document as `url`, if any.
    fn key_for(&self, url: &str) -> Option<String> {
        if self.by_url.contains_key(url) {
            return Some(url.to_string());
        }
        let canon = super::source_quality::canonical_url(url);
        self.by_url
            .keys()
            .find(|k| super::source_quality::canonical_url(k) == canon)
            .cloned()
    }

    pub fn get(&self, url: &str) -> Option<u32> {
        self.key_for(url)
            .and_then(|k| self.by_url.get(&k).map(|(i, _)| *i))
    }

    /// `(index, title, url)` for every registered source — what the
    /// citation helpers need to rebuild a Sources block that covers
    /// indices from earlier runs too.
    pub fn meta(&self) -> Vec<(u32, String, String)> {
        let mut v: Vec<(u32, String, String)> = self
            .by_url
            .iter()
            .map(|(url, (i, t))| (*i, t.clone(), url.clone()))
            .collect();
        v.sort_by_key(|m| m.0);
        v
    }

    pub fn len(&self) -> usize {
        self.by_url.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_url.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_are_stable_across_loads() {
        let _h = crate::research::test_helpers::scoped_home();
        let kref = crate::kms::create("reg-rt", crate::kms::KmsScope::Project).unwrap();
        let mut r = SourceRegistry::load(&kref);
        assert_eq!(r.index_for("https://a", "A"), 1);
        assert_eq!(r.index_for("https://b", "B"), 2);
        assert_eq!(r.index_for("https://a", ""), 1);
        r.save(&kref).unwrap();
        let mut r2 = SourceRegistry::load(&kref);
        assert_eq!(r2.index_for("https://c", "C"), 3);
        assert_eq!(r2.get("https://b"), Some(2));
        assert_eq!(r2.meta()[0], (1, "A".to_string(), "https://a".to_string()));
    }

    #[test]
    fn url_variants_share_one_citation_index() {
        let _h = crate::research::test_helpers::scoped_home();
        let kref = crate::kms::create("reg-canon", crate::kms::KmsScope::Project).unwrap();
        let mut r = SourceRegistry::load(&kref);
        assert_eq!(r.index_for("https://ex.com/post", "A"), 1);
        assert_eq!(r.index_for("https://www.ex.com/post/?utm_source=x", "A"), 1);
        assert_eq!(r.index_for("http://ex.com/post#top", "A"), 1);
        assert_eq!(r.get("https://ex.com/post/"), Some(1));
        assert_eq!(r.len(), 1, "one document, one entry");
        // A different page still gets its own index.
        assert_eq!(r.index_for("https://ex.com/other", "B"), 2);
        // The stored URL stays the one first seen — it names the archive.
        assert_eq!(r.meta()[0].2, "https://ex.com/post");
    }
}
