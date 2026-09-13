//! dev-plan/59: many bots in one workspace.
//!
//! A **bot** is a whole agent installed at `.thclaws/bots/<slug>/` — its own
//! `AGENTS.md`, `settings.json` and `.thclaws/state/` (sessions, KMS, browser
//! profile). Nothing is shared with the host or with a sibling; `bots/` is a
//! shelf, not state.
//!
//! A bot runs as its **own process**: an ordinary `thclaws --serve` with cwd
//! set to the bot's folder. Process cwd is what the engine actually reads —
//! 130 `env::current_dir()` sites against 22 `current_workdir()` ones, and
//! `config.rs` uses none of the latter — so per-process gets identity,
//! config, sandbox, MCP and permission mode right for free, where N workers
//! inside one process would each load the host's settings while writing
//! sessions to their own folder.
//!
//! The **host** supervises those processes and runs no agent, no model and no
//! MCP of its own. It reaches every bot's folder, but it is deterministic
//! code, not something a web page can talk to — which is the whole reason the
//! supervisor is not itself an agent.
//!
//! Step 3 scope: one bot, spawned from a hand-edited `.thclaws/bots.json`,
//! proxied and restarted on crash. No migration, no UI, no `/cloud get`.

pub mod desktop;
pub mod install;
pub mod migrate;
pub mod proxy;
pub mod supervisor;

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Hand-edited in Step 3; written by bot install from Step 5 on.
pub const CONFIG_REL: &str = ".thclaws/bots.json";

/// The shelf. Under `.thclaws/` rather than at the workspace root because
/// `cloud/pack.rs` strips by root-anchored prefix: one `.thclaws/bots/` entry
/// drops every bot from a publish, where a top-level `bots/` would have
/// matched no strip rule and uploaded every bot's cookies.
pub const SHELF_REL: &str = ".thclaws/bots";

/// `.thclaws/bots.json` — which bots this workspace has.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BotsConfig {
    pub version: u32,
    pub bots: Vec<BotDef>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BotDef {
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl BotDef {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.slug)
    }
}

/// A slug names a directory and is the cwd a child process is spawned in, so
/// it is restricted to one plain path component. No dots at all, which
/// answers `.`, `..` and hidden-directory questions in one rule.
pub fn validate_slug(slug: &str) -> Result<()> {
    let bad = |why: &str| {
        Err(Error::Config(format!(
            "bot slug '{slug}' is invalid: {why} (allowed: a-z, 0-9, '-', '_', starting with a letter or digit)"
        )))
    };
    if slug.is_empty() {
        return bad("empty");
    }
    if slug.len() > 64 {
        return bad("longer than 64 characters");
    }
    if !slug
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return bad("must start with a letter or digit");
    }
    if let Some(c) = slug
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        return bad(&format!("contains '{c}'"));
    }
    Ok(())
}

/// Hold the workspace's host lock. Two hosts on one workspace would both spawn
/// children and fight over `bots.json` and the address files; the migration
/// takes the same lock so it cannot move a tree a host is serving. The lock is
/// the OS's (`flock`), so a host that dies releases it without cleanup.
pub struct HostLock {
    _file: std::fs::File,
}

pub fn lock_workspace(workspace: &Path, who: &str) -> Result<HostLock> {
    use fs2::FileExt;
    let dir = workspace.join(".thclaws/state");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("host.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    if file.try_lock_exclusive().is_err() {
        return Err(Error::Config(format!(
            "{} is already open by another thClaws host — {who} cannot run alongside it. Close \
             that window or `--serve` first.",
            workspace.display()
        )));
    }
    Ok(HostLock { _file: file })
}

pub fn shelf_dir(workspace: &Path) -> PathBuf {
    workspace.join(SHELF_REL)
}

pub fn bot_dir(workspace: &Path, slug: &str) -> PathBuf {
    shelf_dir(workspace).join(slug)
}

const EXAMPLE: &str = r#"{"version": 1, "bots": [{"slug": "main", "name": "Main"}]}"#;

impl BotsConfig {
    pub fn load(workspace: &Path) -> Result<Self> {
        let path = workspace.join(CONFIG_REL);
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            Error::Config(format!(
                "cannot read {}: {e}\nStep 3 takes a hand-edited file, e.g.\n  {EXAMPLE}",
                path.display()
            ))
        })?;
        let cfg: Self = serde_json::from_str(&raw)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(Error::Config(format!(
                "{CONFIG_REL}: version {} is not supported by this build (expected 1)",
                self.version
            )));
        }
        if self.bots.is_empty() {
            return Err(Error::Config(format!(
                "{CONFIG_REL}: no bots listed — every workspace has at least one"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for def in &self.bots {
            validate_slug(&def.slug)?;
            if !seen.insert(def.slug.as_str()) {
                return Err(Error::Config(format!(
                    "{CONFIG_REL}: duplicate bot slug '{}'",
                    def.slug
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slug becomes a directory name AND a child's cwd, so traversal
    /// and separator shapes are refused rather than sanitised.
    #[test]
    fn slug_rejects_traversal_and_separators() {
        for good in ["main", "research", "book-author", "bot_2", "a"] {
            assert!(validate_slug(good).is_ok(), "{good} should be allowed");
        }
        for bad in [
            "",
            "..",
            ".",
            ".hidden",
            "a/b",
            "a\\b",
            "../escape",
            "has space",
            "-leading",
            "UPPER/../x",
            "nul\0byte",
        ] {
            assert!(validate_slug(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(validate_slug(&"a".repeat(65)).is_err());
    }

    #[test]
    fn config_rejects_duplicates_and_unknown_versions() {
        let dir = tempfile::tempdir().unwrap();
        let write = |body: &str| {
            std::fs::create_dir_all(dir.path().join(".thclaws")).unwrap();
            std::fs::write(dir.path().join(CONFIG_REL), body).unwrap();
        };

        write(r#"{"version": 1, "bots": [{"slug": "main"}]}"#);
        let cfg = BotsConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.bots[0].display_name(), "main");

        write(r#"{"version": 2, "bots": [{"slug": "main"}]}"#);
        assert!(BotsConfig::load(dir.path()).is_err());

        write(r#"{"version": 1, "bots": [{"slug": "a"}, {"slug": "a"}]}"#);
        assert!(BotsConfig::load(dir.path()).is_err());

        write(r#"{"version": 1, "bots": []}"#);
        assert!(BotsConfig::load(dir.path()).is_err());

        write(r#"{"version": 1, "bots": [{"slug": "../evil"}]}"#);
        assert!(BotsConfig::load(dir.path()).is_err());
    }

    /// A second host on the same workspace must be refused, and the refusal
    /// must go away the moment the first one is dropped.
    #[test]
    fn one_host_per_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock_workspace(dir.path(), "a host").unwrap();
        let err = lock_workspace(dir.path(), "a migration")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("already open"), "{err}");
        drop(first);
        assert!(lock_workspace(dir.path(), "a host").is_ok());
    }

    /// The message a user sees when they have not written the file yet is
    /// the only documentation Step 3 ships, so it must carry the shape.
    #[test]
    fn missing_config_explains_the_shape() {
        let dir = tempfile::tempdir().unwrap();
        let err = BotsConfig::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("bots.json"), "{err}");
        assert!(err.contains(r#""slug""#), "{err}");
    }
}
