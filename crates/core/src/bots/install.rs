//! dev-plan/59 Step 5: install a catalogue agent as a bot.
//!
//! Installing a bot is a **host** action, not a slash command. The plan
//! originally wrote this as "`/cloud get` installs to `.thclaws/bots/<slug>/`",
//! which predates the decision that the host runs no agent: a bot's sandbox
//! root is its own folder, so a bot cannot write to a sibling's — and that is
//! §4 working, not a gap. `/cloud get` run inside a bot still replaces that
//! bot, which is the coherent meaning of "get" from where it stands.
//!
//! The download itself is not re-implemented here. `cloud::cmd::get_lines`
//! carries the checks that matter — fail-closed on a missing SHA-256 or
//! agent-UUID header, refuse to overwrite a folder bound to a different
//! agent, carry the installer's own settings across the extraction — and
//! re-deriving any of those would be one copy drifting from the other.

use super::{bot_dir, BotDef, BotsConfig, CONFIG_REL};
use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Installed {
    pub slug: String,
    pub dir: PathBuf,
    /// The installer's progress log, as `/cloud get` renders it.
    pub lines: Vec<String>,
    /// `false` when the bot was already listed — the install was an update.
    pub newly_registered: bool,
}

/// Download `slug` from the catalogue into `.thclaws/bots/<slug>/` and list it
/// in `.thclaws/bots.json`. The registration only happens when the extraction
/// actually succeeded: a bot listed but not installed would be a child the
/// supervisor tries to spawn in an empty folder.
pub async fn install(
    workspace: &Path,
    slug: &str,
    version: Option<&str>,
    force: bool,
) -> Result<Installed> {
    super::validate_slug(slug)?;
    if !workspace.join(CONFIG_REL).exists() {
        return Err(Error::Config(format!(
            "{} is not a workspace that holds agents — `thclaws bots migrate` converts one, or start a \
             host in an empty directory.",
            workspace.display()
        )));
    }
    let dir = bot_dir(workspace, slug);
    // Whether the shelf already had this folder decides what a failure may
    // clean up: a failed FIRST install should leave the shelf exactly as it
    // was, but a failed UPDATE must not delete a bot's sessions and logins.
    let is_new = !dir.exists();
    std::fs::create_dir_all(&dir)?;

    let url = crate::cloud::persisted_url();
    let outcome = crate::cloud::cmd::get_lines(
        slug.to_string(),
        dir.clone(),
        version.map(str::to_string),
        force,
        url.as_deref(),
        None,
    )
    .await;

    if !outcome.ok {
        if is_new {
            // Nothing of the user's is in there — leaving it would put an
            // empty or half-extracted folder on the shelf for the host UI to
            // show as a bot.
            let _ = std::fs::remove_dir_all(&dir);
        }
        return Err(Error::Tool(outcome.lines.join("\n")));
    }

    seed_workspace_gateway_choice(workspace, &dir);
    let newly_registered = register(workspace, slug)?;
    Ok(Installed {
        slug: slug.to_string(),
        dir,
        lines: outcome.lines,
        newly_registered,
    })
}

/// Add a bot with no agent in it — the same thing as opening thClaws on a
/// new, empty folder. The bot's `--serve` writes its own settings template on
/// first start; this only makes the folder, gives it the workspace's gateway
/// choice so it doesn't open on "no API key", and lists it.
///
/// Never reuses a folder: one already on the shelf holds some bot's sessions,
/// and "empty" must not mean "adopt whatever was there".
pub fn create_blank(workspace: &Path, slug: &str) -> Result<Installed> {
    super::validate_slug(slug)?;
    if !workspace.join(CONFIG_REL).exists() {
        return Err(Error::Config(format!(
            "{} is not a workspace that holds agents — `thclaws bots migrate` converts one, or start a \
             host in an empty directory.",
            workspace.display()
        )));
    }
    let dir = bot_dir(workspace, slug);
    if dir.exists()
        || BotsConfig::load(workspace)?
            .bots
            .iter()
            .any(|b| b.slug == slug)
    {
        return Err(Error::Config(format!(
            "an agent named '{slug}' already exists in this workspace — pick another name"
        )));
    }
    std::fs::create_dir_all(&dir)?;
    // The first-run file a new folder gets, written before the gateway seed:
    // seeded first, the seed's two-key file made the engine see "settings
    // already exist" and skip the documented template.
    crate::config::ProjectConfig::ensure_default_exists_in(&dir);
    seed_workspace_gateway_choice(workspace, &dir);
    let newly_registered = register(workspace, slug)?;
    Ok(Installed {
        slug: slug.to_string(),
        lines: vec![format!("Created a blank agent at {}.", dir.display())],
        dir,
        newly_registered,
    })
}

/// Give a freshly installed bot the workspace's gateway choice.
///
/// `gatewayProxy` is a project-level setting and stays one — a bot may
/// legitimately be pinned to BYOK. But a bot installed into a workspace whose
/// user is logged in and routing through the gateway should not open saying
/// "no API key": the credential is user-level and already reachable, only the
/// routing choice was missing. Copied at install, never afterwards, so a bot
/// the user later re-points keeps its own answer.
fn seed_workspace_gateway_choice(workspace: &Path, dest: &Path) {
    let flag_in = |path: PathBuf| -> Option<bool> {
        std::fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
            .and_then(|v| v.get("gatewayProxy").and_then(|b| b.as_bool()))
    };
    let host_on = flag_in(workspace.join(".thclaws/settings.json"))
        // After a migration the workspace's original agent — and the choice
        // the user made in it — is `bots[0]`, not the host root, which the
        // migration leaves minimal. The first cut read only the host and so
        // never seeded for exactly the workspace that reported the bug.
        .or_else(|| {
            BotsConfig::load(workspace)
                .ok()
                .and_then(|c| c.bots.first().map(|b| b.slug.clone()))
                .map(|slug| bot_dir(workspace, &slug).join(".thclaws/settings.json"))
                .and_then(flag_in)
        })
        .or_else(|| {
            crate::config::AppConfig::load()
                .ok()
                .map(|c| c.gateway_proxy)
        })
        .unwrap_or(false);
    if !host_on {
        return;
    }
    let path = dest.join(".thclaws/settings.json");
    let mut base = std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(obj) = base.as_object_mut() else {
        return;
    };
    // A bundle that ships its own choice keeps it. A choice is a boolean
    // flag or a non-empty provider list — the same reading `apply_to` uses.
    // Not a bare key: the catalogue's own hello-world ships
    // `"gatewayUseFor": null`, and treating that null as a decision is why the
    // seed never fired on the real workspace even though its test passed.
    let pinned = obj.get("gatewayProxy").and_then(|v| v.as_bool()).is_some()
        || obj
            .get("gatewayUseFor")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
    if pinned {
        return;
    }
    obj.insert("gatewayProxy".into(), serde_json::json!(true));
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(body) = serde_json::to_string_pretty(obj) {
        let _ = std::fs::write(&path, body);
    }
}

/// Add `slug` to `.thclaws/bots.json`. `true` when it was not already there.
pub fn register(workspace: &Path, slug: &str) -> Result<bool> {
    super::validate_slug(slug)?;
    let path = workspace.join(CONFIG_REL);
    let mut cfg = BotsConfig::load(workspace)?;
    if cfg.bots.iter().any(|b| b.slug == slug) {
        return Ok(false);
    }
    cfg.bots.push(BotDef {
        slug: slug.to_string(),
        name: None,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
    Ok(true)
}

/// Can `slug` be removed at all? Separated from [`deregister`] so a caller
/// that has to stop a process first can find out BEFORE stopping it — the
/// first cut stopped the bot and then refused, leaving a live host holding a
/// dead child.
pub fn can_deregister(workspace: &Path, slug: &str) -> Result<bool> {
    let cfg = BotsConfig::load(workspace)?;
    if !cfg.bots.iter().any(|b| b.slug == slug) {
        return Ok(false);
    }
    if cfg.bots.len() == 1 {
        return Err(Error::Config(format!(
            "'{slug}' is the only agent in this workspace, and a workspace always has at least one \
             — install another before removing it."
        )));
    }
    Ok(true)
}

/// Remove `slug` from `.thclaws/bots.json`. The folder is left on disk unless
/// `purge`: a bot's folder holds its sessions, its KMS and its browser
/// logins, so deleting it is a separate decision from "stop listing it".
pub fn deregister(workspace: &Path, slug: &str, purge: bool) -> Result<bool> {
    if !can_deregister(workspace, slug)? {
        return Ok(false);
    }
    let path = workspace.join(CONFIG_REL);
    let mut cfg = BotsConfig::load(workspace)?;
    cfg.bots.retain(|b| b.slug != slug);
    std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
    if purge {
        let dir = bot_dir(workspace, slug);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v3_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        super::super::migrate::mint_new_workspace(dir.path(), "main").unwrap();
        dir
    }

    #[test]
    fn a_blank_bot_is_an_empty_listed_folder() {
        let ws = v3_workspace();
        let made = create_blank(ws.path(), "scratch").unwrap();
        assert!(made.newly_registered);
        assert_eq!(made.dir, bot_dir(ws.path(), "scratch"));
        assert!(made.dir.is_dir());
        // No agent: nothing the catalogue would have put there.
        assert!(!made.dir.join("AGENTS.md").exists());
        assert!(!made.dir.join("manifest.json").exists());
        // …but the same documented settings file a new folder gets.
        let settings = std::fs::read_to_string(made.dir.join(".thclaws/settings.json")).unwrap();
        assert!(settings.contains("\"_doc\""), "the full first-run template");
        serde_json::from_str::<serde_json::Value>(&settings).expect("valid JSON");
        let cfg = BotsConfig::load(ws.path()).unwrap();
        assert_eq!(
            cfg.bots.iter().map(|b| b.slug.as_str()).collect::<Vec<_>>(),
            vec!["main", "scratch"]
        );
    }

    #[test]
    fn a_blank_bot_never_takes_over_an_existing_folder() {
        let ws = v3_workspace();
        let dir = bot_dir(ws.path(), "old");
        std::fs::create_dir_all(dir.join(".thclaws/sessions")).unwrap();
        std::fs::write(dir.join(".thclaws/sessions/s.jsonl"), "{}").unwrap();
        let err = create_blank(ws.path(), "old").unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(
            dir.join(".thclaws/sessions/s.jsonl").exists(),
            "left untouched"
        );
        assert!(!BotsConfig::load(ws.path())
            .unwrap()
            .bots
            .iter()
            .any(|b| b.slug == "old"));
        // A listed slug is taken too, folder or not.
        assert!(create_blank(ws.path(), "main").is_err());
        // And the slug rules still apply.
        assert!(create_blank(ws.path(), "../up").is_err());
    }

    #[test]
    fn a_blank_bot_needs_a_multi_bot_workspace() {
        let plain = tempfile::tempdir().unwrap();
        let err = create_blank(plain.path(), "scratch")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a workspace that holds agents"), "{err}");
        assert!(!plain.path().join(".thclaws").exists());
    }

    #[test]
    fn register_is_idempotent_and_ordered() {
        let ws = v3_workspace();
        assert!(register(ws.path(), "research").unwrap());
        assert!(!register(ws.path(), "research").unwrap());
        let cfg = BotsConfig::load(ws.path()).unwrap();
        assert_eq!(
            cfg.bots.iter().map(|b| b.slug.as_str()).collect::<Vec<_>>(),
            vec!["main", "research"]
        );
    }

    #[test]
    fn register_refuses_a_slug_that_is_not_a_folder_name() {
        let ws = v3_workspace();
        assert!(register(ws.path(), "../escape").is_err());
        assert!(register(ws.path(), "a/b").is_err());
    }

    /// A bot's folder holds its sessions, KMS and browser logins, so removing
    /// it from the list and deleting it are two different decisions.
    #[test]
    fn deregister_keeps_the_folder_unless_purge() {
        let ws = v3_workspace();
        register(ws.path(), "research").unwrap();
        let dir = bot_dir(ws.path(), "research");
        std::fs::create_dir_all(dir.join(".thclaws/state")).unwrap();
        std::fs::write(dir.join(".thclaws/state/sessions.jsonl"), "{}").unwrap();

        assert!(deregister(ws.path(), "research", false).unwrap());
        assert!(dir.exists(), "the folder outlives being delisted");
        assert!(!deregister(ws.path(), "research", false).unwrap());

        register(ws.path(), "research").unwrap();
        assert!(deregister(ws.path(), "research", true).unwrap());
        assert!(!dir.exists());
    }

    /// Every workspace has a host and at least one bot; removing the last one
    /// would leave a host with nothing to supervise.
    #[test]
    fn the_last_bot_cannot_be_removed() {
        let ws = v3_workspace();
        let err = deregister(ws.path(), "main", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("only agent"), "{err}");
        assert_eq!(BotsConfig::load(ws.path()).unwrap().bots.len(), 1);
    }

    /// A download that never lands must leave the shelf exactly as it was —
    /// an empty folder there is a bot as far as the host UI is concerned.
    #[tokio::test]
    async fn a_failed_first_install_leaves_no_folder_behind() {
        let ws = v3_workspace();
        // No token configured, so the download refuses before any bytes move.
        let _g = crate::kms::test_env_lock();
        let prev = std::env::var("THCLAWS_CLOUD_TOKEN").ok();
        std::env::set_var("THCLAWS_CLOUD_TOKEN", "");
        std::env::set_var("THCLAWS_DISABLE_KEYCHAIN", "1");

        let before: Vec<_> = std::fs::read_dir(ws.path().join(super::super::SHELF_REL))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        let _ = install(ws.path(), "no-such-agent", None, false).await;
        let after: Vec<_> = std::fs::read_dir(ws.path().join(super::super::SHELF_REL))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        assert_eq!(before.len(), after.len(), "{before:?} -> {after:?}");
        assert_eq!(
            BotsConfig::load(ws.path()).unwrap().bots.len(),
            1,
            "a failed install must not register a bot"
        );

        match prev {
            Some(v) => std::env::set_var("THCLAWS_CLOUD_TOKEN", v),
            None => std::env::remove_var("THCLAWS_CLOUD_TOKEN"),
        }
    }

    /// A bot installed into a workspace that routes through the gateway
    /// should not open saying "no API key". The credential is user-level and
    /// already reachable; only the routing choice was missing.
    #[test]
    fn a_new_bot_inherits_the_workspaces_gateway_choice() {
        let ws = v3_workspace();
        std::fs::write(
            ws.path().join(".thclaws/settings.json"),
            r#"{"workspaceVersion":3,"gatewayProxy":true}"#,
        )
        .unwrap();
        let bot = bot_dir(ws.path(), "fresh");
        std::fs::create_dir_all(bot.join(".thclaws")).unwrap();
        std::fs::write(bot.join(".thclaws/settings.json"), r#"{"model":"x"}"#).unwrap();

        seed_workspace_gateway_choice(ws.path(), &bot);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(bot.join(".thclaws/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["gatewayProxy"], true);
        assert_eq!(v["model"], "x", "the bundle's own keys survive");

        // A bundle that ships its own answer keeps it.
        let pinned = bot_dir(ws.path(), "pinned");
        std::fs::create_dir_all(pinned.join(".thclaws")).unwrap();
        std::fs::write(
            pinned.join(".thclaws/settings.json"),
            r#"{"gatewayProxy":false}"#,
        )
        .unwrap();
        seed_workspace_gateway_choice(ws.path(), &pinned);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(pinned.join(".thclaws/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["gatewayProxy"], false);

        // The catalogue's hello-world ships `"gatewayUseFor": null` — a key
        // with no choice in it. It must still be seeded.
        let nullish = bot_dir(ws.path(), "nullish");
        std::fs::create_dir_all(nullish.join(".thclaws")).unwrap();
        std::fs::write(
            nullish.join(".thclaws/settings.json"),
            r#"{"gatewayUseFor":null,"model":"m"}"#,
        )
        .unwrap();
        seed_workspace_gateway_choice(ws.path(), &nullish);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(nullish.join(".thclaws/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["gatewayProxy"], true, "a null key is not a decision");

        // The migrated shape: the host root is minimal and the flag lives in
        // bots[0]. This is the workspace that reported the bug, and the first
        // cut of the seed read only the host and never fired for it.
        let mig = v3_workspace();
        std::fs::write(
            mig.path().join(".thclaws/settings.json"),
            r#"{"workspaceVersion":3}"#,
        )
        .unwrap();
        let main = bot_dir(mig.path(), "main");
        std::fs::create_dir_all(main.join(".thclaws")).unwrap();
        std::fs::write(
            main.join(".thclaws/settings.json"),
            r#"{"workspaceVersion":2,"gatewayProxy":true}"#,
        )
        .unwrap();
        let newbie = bot_dir(mig.path(), "newbie");
        std::fs::create_dir_all(newbie.join(".thclaws")).unwrap();
        std::fs::write(newbie.join(".thclaws/settings.json"), "{}").unwrap();
        seed_workspace_gateway_choice(mig.path(), &newbie);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(newbie.join(".thclaws/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["gatewayProxy"], true, "must follow the original agent");

        // A workspace that is not on the gateway seeds nothing.
        let off = v3_workspace();
        let b = bot_dir(off.path(), "b");
        std::fs::create_dir_all(b.join(".thclaws")).unwrap();
        std::fs::write(b.join(".thclaws/settings.json"), "{}").unwrap();
        seed_workspace_gateway_choice(off.path(), &b);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(b.join(".thclaws/settings.json")).unwrap(),
        )
        .unwrap();
        assert!(v.get("gatewayProxy").is_none());
    }

    #[tokio::test]
    async fn install_refuses_a_workspace_that_is_not_v3() {
        let dir = tempfile::tempdir().unwrap();
        let err = install(dir.path(), "demo", None, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a workspace that holds agents"), "{err}");
    }
}
