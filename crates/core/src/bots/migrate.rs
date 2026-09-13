//! dev-plan/59 Step 4: workspace v2 → v3.
//!
//! **v2:** a workspace IS an agent — `<ws>/.thclaws/` holds its identity and
//! state, `<ws>/` holds its files.
//! **v3:** `<ws>/.thclaws/bots/main/` is that agent, its files included, and
//! `<ws>/.thclaws/` belongs to the host.
//!
//! The user's working files move too, for continuity rather than security: a
//! workspace today is one place — the agent, its files, its history — and
//! leaving the files at the root would hand the user an agent living
//! somewhere other than its files. Not moving does not avoid a migration; it
//! produces one the user has to notice.
//!
//! The destination lives inside one of the things being moved, so the move
//! cannot be done in place. Everything goes to a staging directory beside
//! `.thclaws/` first, and a marker file records which phase we are in — a
//! half-migrated workspace holds the user's entire tree, so being resumable
//! is not optional.
//!
//! This is NOT armed. Nothing calls it on open: until the desktop and cloud
//! startup paths run as hosts, a migrated workspace would open as the host
//! tree with no agent in it. `thclaws bots migrate` is the only caller.

use super::{bot_dir, BotDef, BotsConfig};
use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// The layout version this migration produces. Deliberately NOT
/// `ProjectConfig::CURRENT_WORKSPACE_VERSION`: bumping that would make the
/// v1→v2 pass treat every v2 workspace as stale, move nothing, and stamp it
/// v3 — marking workspaces migrated that never were.
pub const HOST_WORKSPACE_VERSION: u32 = 3;

/// The bot a migrated workspace's agent becomes.
pub const MAIN_SLUG: &str = "main";

const STAGING: &str = ".thclaws-v3-migration";
const MARKER: &str = ".thclaws-v3-migration.marker";

/// Hosted runners mount the workspace PVC twice — whole at `/workspace`, and
/// again at the engine's `$HOME` via `subPath: .home`. It is user
/// credentials, not agent content, and it is host-level: moving it into
/// `bots/main/` would break the second mount.
const KEEP_AT_ROOT: &[&str] = &[".home"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Moving root entries into staging.
    Collect,
    /// Staging into `.thclaws/bots/main/`.
    Install,
    /// Host files, identity, stored paths.
    Finalise,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Collect => "collect",
            Phase::Install => "install",
            Phase::Finalise => "finalise",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "collect" => Some(Phase::Collect),
            "install" => Some(Phase::Install),
            "finalise" => Some(Phase::Finalise),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// A v2 workspace ready to migrate.
    Migrate,
    /// A migration was interrupted; re-running continues from `Phase`.
    Resume(Phase),
    /// Already v3 — nothing to do.
    AlreadyV3,
}

#[derive(Debug)]
pub struct Plan {
    pub workspace: PathBuf,
    pub status: Status,
    /// Root entries that move into `.thclaws/bots/main/`.
    pub moves: Vec<String>,
    /// Root entries that stay where they are.
    pub keeps: Vec<String>,
    /// Schedule ids whose stored `cwd` points into this workspace and will be
    /// rewritten.
    pub schedules: Vec<String>,
    /// `true` when the workspace holds a git repository, which moves with
    /// everything else — the one thing a user is most likely to have another
    /// window open on.
    pub moves_git: bool,
}

impl Plan {
    pub fn is_noop(&self) -> bool {
        self.status == Status::AlreadyV3
    }
}

/// Read `workspaceVersion` straight from a `settings.json`, never through the
/// merged `AppConfig` — a user-level key would otherwise make every project
/// look migrated.
fn raw_workspace_version(settings: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(settings).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("workspaceVersion")?.as_u64().map(|n| n as u32)
}

fn marker_path(workspace: &Path) -> PathBuf {
    workspace.join(MARKER)
}

fn read_phase(workspace: &Path) -> Option<Phase> {
    let raw = std::fs::read_to_string(marker_path(workspace)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Phase::parse(v.get("phase")?.as_str()?)
}

fn write_phase(workspace: &Path, phase: Phase) -> Result<()> {
    let body = serde_json::json!({
        "marker": "thclaws-workspace-v3-migration",
        "phase": phase.as_str(),
        "workspace": workspace.display().to_string(),
        "note": "A workspace migration is in progress. Re-run `thclaws bots migrate` to finish it. Do not delete this file or the .thclaws-v3-migration/ folder by hand.",
    });
    std::fs::write(marker_path(workspace), serde_json::to_string_pretty(&body)?)?;
    Ok(())
}

/// Root entries this migration would move, in a stable order.
fn movable_entries(workspace: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(workspace)? {
        let name = entry?.file_name().to_string_lossy().to_string();
        if name == STAGING || name == MARKER || KEEP_AT_ROOT.contains(&name.as_str()) {
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

/// Schedule ids whose `cwd` is inside `workspace`. User-level store, so only
/// entries belonging to THIS workspace may be touched.
fn schedules_under(workspace: &Path) -> Vec<String> {
    let Ok(store) = crate::schedule::ScheduleStore::load() else {
        return Vec::new();
    };
    store
        .schedules
        .iter()
        .filter(|s| s.cwd.starts_with(workspace))
        .map(|s| s.id.clone())
        .collect()
}

/// `<…>/.thclaws/bots/<this>` — the same shape `context::is_bot_shelf`
/// recognises. Any directory merely named `bots` (`~/projects/bots/foo`) is an
/// ordinary workspace.
pub fn is_inside_shelf(dir: &Path) -> bool {
    let parent = dir.parent();
    parent
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "bots")
        && parent
            .and_then(|p| p.parent())
            .and_then(|g| g.file_name())
            .is_some_and(|n| n == ".thclaws")
}

/// Directories that are never a workspace, whatever they contain: the home
/// directory, anything above it, the filesystem root. The classic desktop
/// ran its engine in the cwd it was launched from — the home directory,
/// from the Dock — before the folder picker was answered, so `~/.thclaws/`
/// exists on many machines and `~` looks exactly like a v2 agent. Migrating
/// it would move the whole home directory into `~/.thclaws/bots/main/`.
pub fn is_never_a_workspace(dir: &Path) -> bool {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if dir.parent().is_none() {
        return true;
    }
    match crate::util::home_dir().and_then(|h| h.canonicalize().ok()) {
        Some(home) => home.starts_with(&dir),
        None => false,
    }
}

/// Whether the desktop should open `dir` as a workspace host: it is v3
/// already, or a v2 agent that auto-migration may upgrade. The folder
/// picker hands such a folder to a fresh process instead of pointing the
/// in-process engine at it.
pub fn opens_as_host(dir: &Path) -> bool {
    dir.join(super::CONFIG_REL).exists()
        || (auto_migrate_allowed()
            && !is_inside_shelf(dir)
            && !is_never_a_workspace(dir)
            && looks_like_v2_agent(dir))
}

/// `THCLAWS_AUTO_MIGRATE=0` turns auto-migration off anywhere; a container
/// never does it on its own; a supervised child is inside a shelf and never
/// asks.
pub fn auto_migrate_allowed() -> bool {
    std::env::var("THCLAWS_AUTO_MIGRATE").ok().as_deref() != Some("0")
        && std::env::var("THCLAWS_INSIDE_DOCKER").ok().as_deref() != Some("1")
        && std::env::var("THCLAWS_SUPERVISED").ok().as_deref() != Some("1")
}

pub fn plan(workspace: &Path) -> Result<Plan> {
    if crate::workdir::is_multiuser() {
        return Err(Error::Config(
            "workspace migration is not defined for a multiuser pod — its `.thclaws/` is shared \
             across tenants and uses a different per-user layout"
                .into(),
        ));
    }
    if !workspace.is_dir() {
        return Err(Error::Config(format!(
            "{} is not a directory",
            workspace.display()
        )));
    }
    // Migrating a bot would nest a shelf inside a shelf. The check is on the
    // path rather than on content because a bot folder looks exactly like the
    // workspace it came from.
    if is_inside_shelf(workspace) {
        return Err(Error::Config(format!(
            "{} is already a bot inside a workspace shelf — migrate the workspace above it, not \
             the bot",
            workspace.display()
        )));
    }
    if is_never_a_workspace(workspace) {
        return Err(Error::Config(format!(
            "{} is the home directory (or above it) — a workspace is a project folder, and \
             migrating this would move everything under it",
            workspace.display()
        )));
    }

    let resume = read_phase(workspace);
    let host_settings = workspace.join(".thclaws/settings.json");
    let version = raw_workspace_version(&host_settings).unwrap_or(0);
    let shelf = workspace.join(super::SHELF_REL);

    let status = match resume {
        Some(phase) => Status::Resume(phase),
        None if version >= HOST_WORKSPACE_VERSION && shelf.is_dir() => Status::AlreadyV3,
        None if version >= HOST_WORKSPACE_VERSION => {
            return Err(Error::Config(format!(
                "{} declares workspaceVersion {version} but has no {} — its bots are missing, and \
                 migrating again would move the host's own tree into a new one. Restore the shelf \
                 or fix the version by hand.",
                workspace.display(),
                super::SHELF_REL
            )));
        }
        None if shelf.is_dir() => {
            return Err(Error::Config(format!(
                "{} already has a {} folder but declares workspaceVersion {version} — refusing to \
                 migrate a tree that is neither v2 nor v3. Move the shelf aside and re-run.",
                workspace.display(),
                super::SHELF_REL
            )));
        }
        None if marker_path(workspace).exists() => {
            return Err(Error::Config(format!(
                "{} exists but could not be read as a migration marker — resolve it by hand",
                marker_path(workspace).display()
            )));
        }
        None if workspace.join(STAGING).exists() => {
            return Err(Error::Config(format!(
                "{} exists without a migration marker — an earlier migration was interrupted and \
                 its marker was removed. Move that folder aside and re-run.",
                workspace.join(STAGING).display()
            )));
        }
        None => Status::Migrate,
    };

    let moves = if status == Status::AlreadyV3 {
        Vec::new()
    } else {
        movable_entries(workspace)?
    };
    let keeps = KEEP_AT_ROOT
        .iter()
        .filter(|n| workspace.join(n).exists())
        .map(|n| n.to_string())
        .collect();

    Ok(Plan {
        moves_git: moves.iter().any(|n| n == ".git")
            || workspace.join(STAGING).join(".git").exists(),
        workspace: workspace.to_path_buf(),
        status,
        moves,
        keeps,
        schedules: schedules_under(workspace),
    })
}

#[derive(Debug, Default)]
pub struct Report {
    pub moved: usize,
    pub bot_dir: PathBuf,
    pub minted_identity: bool,
    pub rewritten_schedules: Vec<String>,
}

pub fn apply(plan: &Plan) -> Result<Report> {
    // The same lock a host holds, so a migration cannot move a tree a host
    // is serving — the gap §7.4 recorded as "nothing can detect one".
    let _lock = super::lock_workspace(&plan.workspace, "a migration")?;
    apply_locked(plan)
}

/// What happens when a v2 workspace is opened by a surface that migrates on
/// its own (dev-plan/59 §7.8).
#[derive(Debug)]
pub enum AutoOutcome {
    /// Not a v2 agent — nothing to do, and nothing was touched.
    NotV2,
    /// Already v3, possibly because another opener got there first.
    AlreadyV3,
    Migrated(Report, /* moves_git */ bool),
    /// Someone else holds the workspace and did not finish within the wait.
    Busy,
    Failed(String),
}

/// Migrate a v2 workspace the moment it is opened.
///
/// The lock is taken BEFORE the plan is made, and held through the move.
/// `apply` alone plans first and locks second, which is fine for a human
/// at a prompt but not for two openers racing: a plan made against a v2 tree
/// and applied after the other opener finished would sweep the new host root
/// — `.thclaws/` and the tombstone — into a second, nested bot. Under the
/// lock the plan sees whichever shape is true.
pub fn auto_migrate_if_v2(ws: &Path) -> AutoOutcome {
    auto_migrate_with_wait(ws, std::time::Duration::from_secs(5))
}

pub fn auto_migrate_with_wait(ws: &Path, wait: std::time::Duration) -> AutoOutcome {
    // Checked before the v2 test, which reads a v3 tree as "not v2": the
    // caller treats both as nothing-to-do, but the name should be true.
    if ws.join(super::CONFIG_REL).exists() {
        return AutoOutcome::AlreadyV3;
    }
    // A bot's folder looks exactly like a v2 workspace — it IS the workspace
    // that migrated — so the bot's own `--serve` reached this point and
    // logged "could not upgrade" on every start. Inside a shelf is not v2.
    if is_inside_shelf(ws) || is_never_a_workspace(ws) || !looks_like_v2_agent(ws) {
        return AutoOutcome::NotV2;
    }
    let deadline = std::time::Instant::now() + wait;
    let _lock = loop {
        match super::lock_workspace(ws, "the upgrade") {
            Ok(l) => break l,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => return AutoOutcome::Busy,
        }
    };
    let p = match plan(ws) {
        Ok(p) => p,
        Err(e) => return AutoOutcome::Failed(e.to_string()),
    };
    if p.is_noop() {
        return AutoOutcome::AlreadyV3;
    }
    match apply_locked(&p) {
        Ok(r) => AutoOutcome::Migrated(r, p.moves_git),
        Err(e) => AutoOutcome::Failed(e.to_string()),
    }
}

fn apply_locked(plan: &Plan) -> Result<Report> {
    let ws = &plan.workspace;
    if plan.is_noop() {
        return Ok(Report {
            bot_dir: bot_dir(ws, MAIN_SLUG),
            ..Default::default()
        });
    }
    let start = match plan.status {
        Status::Resume(p) => p,
        _ => Phase::Collect,
    };
    let mut report = Report {
        bot_dir: bot_dir(ws, MAIN_SLUG),
        ..Default::default()
    };

    let staging = ws.join(STAGING);
    if start == Phase::Collect {
        write_phase(ws, Phase::Collect)?;
        std::fs::create_dir_all(&staging)?;
        for name in movable_entries(ws)? {
            let src = ws.join(&name);
            let dst = staging.join(&name);
            if dst.exists() {
                // A previous run moved it; the root copy is the newer one only
                // if the user put it back, which is not a case to guess at.
                continue;
            }
            std::fs::rename(&src, &dst).map_err(|e| {
                Error::Config(format!(
                    "cannot move {} into the migration staging folder: {e}",
                    src.display()
                ))
            })?;
            report.moved += 1;
        }
    }

    if start <= Phase::Install {
        write_phase(ws, Phase::Install)?;
        let dest = bot_dir(ws, MAIN_SLUG);
        std::fs::create_dir_all(ws.join(super::SHELF_REL))?;
        if staging.exists() {
            if dest.exists() {
                // Resumed after a partial install: merge what is left rather
                // than clobbering what already landed.
                for name in std::fs::read_dir(&staging)?
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name())
                {
                    let src = staging.join(&name);
                    let dst = dest.join(&name);
                    if !dst.exists() {
                        std::fs::rename(&src, &dst)?;
                    }
                }
                let _ = std::fs::remove_dir(&staging);
            } else {
                std::fs::rename(&staging, &dest).map_err(|e| {
                    Error::Config(format!(
                        "cannot install the staged workspace at {}: {e}",
                        dest.display()
                    ))
                })?;
            }
        }
    }

    write_phase(ws, Phase::Finalise)?;
    install_host_files(ws)?;
    report.minted_identity = mint_identity(ws)?;
    report.rewritten_schedules = rewrite_schedules(ws)?;
    let _ = std::fs::remove_file(marker_path(ws));
    Ok(report)
}

// Phases run in order, so `start <= Phase::Install` reads naturally.
impl PartialOrd for Phase {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Phase {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        fn rank(p: &Phase) -> u8 {
            match p {
                Phase::Collect => 0,
                Phase::Install => 1,
                Phase::Finalise => 2,
            }
        }
        rank(self).cmp(&rank(other))
    }
}

/// The host's own `.thclaws/`, its bot list, and the tombstone that explains
/// the layout to a binary too old to understand it.
fn install_host_files(ws: &Path) -> Result<()> {
    let thclaws = ws.join(".thclaws");
    std::fs::create_dir_all(thclaws.join("state"))?;
    let gi = thclaws.join("state/.gitignore");
    if !gi.exists() {
        std::fs::write(&gi, "*\n")?;
    }

    let settings = thclaws.join("settings.json");
    let mut base = std::fs::read(&settings)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = base.as_object_mut() {
        obj.insert(
            "workspaceVersion".into(),
            serde_json::json!(HOST_WORKSPACE_VERSION),
        );
        obj.entry("_doc").or_insert(serde_json::json!(
            "This is the workspace HOST. It runs no agent; it supervises the bots in .thclaws/bots/. The agent that used to live here is .thclaws/bots/main/."
        ));
        std::fs::write(&settings, serde_json::to_string_pretty(obj)?)?;
    }

    let bots_json = ws.join(super::CONFIG_REL);
    if !bots_json.exists() {
        let cfg = BotsConfig {
            version: 1,
            bots: vec![BotDef {
                slug: MAIN_SLUG.into(),
                name: Some("Main".into()),
            }],
        };
        std::fs::write(&bots_json, serde_json::to_string_pretty(&cfg)?)?;
    }

    // T6: `workspaceVersion` alone is not a stop — an old binary boots
    // happily and scaffolds `state/` into the host tree. AGENTS.md is, because
    // an old binary loads it as project instructions like any other, and the
    // model then explains the situation in the user's own language instead of
    // building on top of what looks like an empty workspace.
    let tombstone = ws.join("AGENTS.md");
    if !tombstone.exists() {
        std::fs::write(&tombstone, TOMBSTONE)?;
    }
    Ok(())
}

const TOMBSTONE: &str = r#"# This workspace has been upgraded — nothing is missing

This folder is now a **workspace host**. It supervises bots; it is not a
project itself. The agent that used to live here — its files, its history,
its settings, its git repository — moved to:

    .thclaws/bots/main/

## If you are a model reading this

You are running a version of thClaws older than the one that upgraded this
workspace. **This directory is not an empty project.** You cannot see or edit
the user's work from here, and anything you create here will land outside
their project.

Do not proceed as if this were a new workspace. Tell the user, in their own
language, that:

1. Their workspace was upgraded to the multi-bot layout and nothing was lost.
2. Their project is at `.thclaws/bots/main/`.
3. Updating thClaws will open it normally again.
4. To keep working without updating, they can open `.thclaws/bots/main/`
   directly as the project folder.
"#;

/// `agent pack` refuses a folder without `agent.{id,name,description}`, so a
/// migrated workspace that never had an identity could not be published.
/// Derived from the workspace folder name — the only name the user has
/// already chosen for this thing — and only when absent.
fn mint_identity(ws: &Path) -> Result<bool> {
    let settings = bot_dir(ws, MAIN_SLUG).join(".thclaws/settings.json");
    let mut base = std::fs::read(&settings)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(obj) = base.as_object_mut() else {
        return Ok(false);
    };
    let existing = obj.get("agent").and_then(|a| a.as_object()).cloned();
    let filled = |k: &str| {
        existing
            .as_ref()
            .and_then(|a| a.get(k))
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
    };
    let (has_id, has_name, has_desc) = (filled("id"), filled("name"), filled("description"));
    if has_id && has_name && has_desc {
        return Ok(false);
    }

    let folder = ws
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| MAIN_SLUG.to_string());
    let mut agent = existing.unwrap_or_default();
    if !has_id {
        agent.insert("id".into(), serde_json::json!(slugify(&folder)));
    }
    if !has_name {
        agent.insert("name".into(), serde_json::json!(folder));
    }
    if !has_desc {
        agent.insert(
            "description".into(),
            serde_json::json!(format!(
                "{folder} — migrated from a single-agent workspace."
            )),
        );
    }
    obj.insert("agent".into(), serde_json::Value::Object(agent));
    if let Some(parent) = settings.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&settings, serde_json::to_string_pretty(obj)?)?;
    Ok(true)
}

/// Catalogue ids are lowercase letters, digits and hyphens.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        MAIN_SLUG.to_string()
    } else {
        s.chars().take(64).collect()
    }
}

/// Schedules store an absolute `cwd` and the daemon refuses one that no
/// longer exists, so every entry pointing into this workspace has to follow
/// the agent. The store is user-level: only this workspace's entries may be
/// touched.
fn rewrite_schedules(ws: &Path) -> Result<Vec<String>> {
    let Some(path) = crate::schedule::ScheduleStore::default_path() else {
        return Ok(Vec::new());
    };
    let mut store = crate::schedule::ScheduleStore::load_from(&path)?;
    let dest = bot_dir(ws, MAIN_SLUG);
    let mut touched = Vec::new();
    for s in &mut store.schedules {
        if s.cwd == dest || s.cwd.starts_with(&dest) {
            continue; // already rewritten
        }
        let Ok(rel) = s.cwd.strip_prefix(ws) else {
            continue;
        };
        // A schedule pointing at the workspace root strips to an empty path,
        // and `join("")` would leave a trailing separator in the stored value.
        s.cwd = if rel.as_os_str().is_empty() {
            dest.clone()
        } else {
            dest.join(rel)
        };
        touched.push(s.id.clone());
    }
    if !touched.is_empty() {
        store.save_to(&path)?;
    }
    Ok(touched)
}

/// Mint a fresh v3 workspace: a host with one empty bot. Used when
/// `--supervisor` is pointed at a directory that has no workspace in it yet —
/// never at one that already holds a v2 agent, which needs [`apply`].
pub fn mint_new_workspace(ws: &Path, slug: &str) -> Result<PathBuf> {
    super::validate_slug(slug)?;
    let dest = bot_dir(ws, slug);
    std::fs::create_dir_all(&dest)?;
    std::fs::create_dir_all(ws.join(".thclaws/state"))?;
    let bots_json = ws.join(super::CONFIG_REL);
    if !bots_json.exists() {
        let cfg = BotsConfig {
            version: 1,
            bots: vec![BotDef {
                slug: slug.to_string(),
                name: None,
            }],
        };
        std::fs::write(&bots_json, serde_json::to_string_pretty(&cfg)?)?;
    }
    let settings = ws.join(".thclaws/settings.json");
    if !settings.exists() {
        std::fs::write(
            &settings,
            serde_json::to_string_pretty(&serde_json::json!({
                "workspaceVersion": HOST_WORKSPACE_VERSION,
                "_doc": "This is the workspace HOST. It runs no agent; it supervises the bots in .thclaws/bots/.",
            }))?,
        )?;
    }
    Ok(dest)
}

/// Does this directory hold a v2 workspace — an agent at its root that a
/// supervisor must not silently step over?
pub fn looks_like_v2_agent(ws: &Path) -> bool {
    if ws.join(super::CONFIG_REL).exists() {
        return false;
    }
    ws.join("AGENTS.md").exists()
        || ws.join("manifest.json").exists()
        || ws.join(".thclaws/settings.json").exists()
        || ws.join(".thclaws/state").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// `~/.thclaws/` exists on most desktops (the classic app ran in the
    /// Dock's cwd before the picker was answered), so `~` passes every
    /// content check. It must never be migrated — automatically or by hand.
    #[test]
    fn home_and_above_are_never_a_workspace() {
        let _g = crate::kms::test_env_lock();
        let fake_home = tempfile::tempdir().unwrap();
        let home = fake_home.path().join("Users").join("someone");
        std::fs::create_dir_all(home.join(".thclaws/state")).unwrap();
        std::fs::write(home.join(".thclaws/settings.json"), "{}").unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);

        assert!(
            looks_like_v2_agent(&home),
            "the shape that made this dangerous"
        );
        assert!(is_never_a_workspace(&home));
        assert!(is_never_a_workspace(home.parent().unwrap()));
        assert!(is_never_a_workspace(Path::new("/")));
        assert!(matches!(
            auto_migrate_with_wait(&home, Duration::from_millis(10)),
            AutoOutcome::NotV2
        ));
        assert!(
            !home.join(".thclaws/bots").exists() && !home.join(STAGING).exists(),
            "nothing moved"
        );
        let err = plan(&home).unwrap_err().to_string();
        assert!(err.contains("home directory"), "{err}");
        assert!(!opens_as_host(&home));

        // A project under the home directory is an ordinary workspace.
        let proj = home.join("projects").join("thing");
        std::fs::create_dir_all(proj.join(".thclaws")).unwrap();
        std::fs::write(proj.join("AGENTS.md"), "# t").unwrap();
        assert!(!is_never_a_workspace(&proj));
        assert!(opens_as_host(&proj));

        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    /// A workspace shaped like a real one: project files, a git repo, the
    /// agent's `.thclaws/` with runtime state in it, and the hosted-runner
    /// `.home/` that must not move.
    fn v2_workspace(name: &str) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join(name);
        let w = |rel: &str, body: &str| {
            let p = ws.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        w("AGENTS.md", "# The agent\n");
        w("manifest.json", r#"{"name":"demo","version":"1.0.0"}"#);
        w("output/report.md", "findings\n");
        w("src/main.py", "print('hi')\n");
        w(".gitignore", "target/\n");
        w(".git/config", "[core]\n");
        w(".git/objects/ab/cdef", "blob");
        w(".thclaws/settings.json", r#"{"workspaceVersion":2}"#);
        w(".thclaws/state/sessions/sess-1.jsonl", "{}\n");
        w(
            ".thclaws/state/browser-profile/Cookies",
            "a real login lives here",
        );
        w(".home/.config/thclaws/settings.json", "{}");
        (root, ws)
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn moves_the_whole_workspace_under_main_and_leaves_a_host_behind() {
        let (_root, ws) = v2_workspace("my-project");
        let plan = plan(&ws).unwrap();
        assert_eq!(plan.status, Status::Migrate);
        assert!(plan.moves_git, "the repo moves and the user must be told");
        assert!(plan.keeps.contains(&".home".to_string()));

        let report = apply(&plan).unwrap();
        let bot = ws.join(".thclaws/bots/main");
        assert_eq!(report.bot_dir, bot);

        // The agent, its files, its state and its repo all land together.
        for rel in [
            "AGENTS.md",
            "manifest.json",
            "output/report.md",
            "src/main.py",
            ".gitignore",
            ".git/config",
            ".git/objects/ab/cdef",
            ".thclaws/settings.json",
            ".thclaws/state/sessions/sess-1.jsonl",
            ".thclaws/state/browser-profile/Cookies",
        ] {
            assert!(bot.join(rel).exists(), "bot should hold {rel}");
        }
        assert_eq!(read(&bot.join("output/report.md")), "findings\n");

        // `.home/` is the runner's second mount point, not agent content.
        assert!(ws.join(".home/.config/thclaws/settings.json").exists());
        assert!(!bot.join(".home").exists());

        // The host is left with its own minimal tree.
        let host: serde_json::Value =
            serde_json::from_str(&read(&ws.join(".thclaws/settings.json"))).unwrap();
        assert_eq!(host["workspaceVersion"], 3);
        let bots: serde_json::Value =
            serde_json::from_str(&read(&ws.join(".thclaws/bots.json"))).unwrap();
        assert_eq!(bots["bots"][0]["slug"], "main");
        assert!(ws.join(".thclaws/state").is_dir());

        // T6: the tombstone, not the version field, is what stops an older
        // binary treating this as an empty project.
        let tomb = read(&ws.join("AGENTS.md"));
        assert!(tomb.contains(".thclaws/bots/main"), "{tomb}");
        assert!(tomb.contains("nothing is missing") || tomb.contains("nothing was lost"));

        // No litter.
        assert!(!ws.join(STAGING).exists());
        assert!(!ws.join(MARKER).exists());
    }

    #[test]
    fn a_second_run_is_a_no_op() {
        let (_root, ws) = v2_workspace("my-project");
        apply(&plan(&ws).unwrap()).unwrap();
        let before = read(&ws.join(".thclaws/bots/main/output/report.md"));

        let again = plan(&ws).unwrap();
        assert_eq!(again.status, Status::AlreadyV3);
        assert!(again.is_noop());
        let report = apply(&again).unwrap();
        assert_eq!(report.moved, 0);
        assert_eq!(
            read(&ws.join(".thclaws/bots/main/output/report.md")),
            before
        );
        // Not nested a second level down.
        assert!(!ws.join(".thclaws/bots/main/.thclaws/bots").exists());
    }

    /// A half-migrated workspace holds the user's entire tree, so being
    /// resumable is the whole point. Each case is the state a `kill -9` would
    /// leave behind at that phase.
    #[test]
    fn resumes_from_a_kill_at_any_phase() {
        // Killed during collect: some entries moved, marker says collect.
        let (_r1, ws) = v2_workspace("proj");
        write_phase(&ws, Phase::Collect).unwrap();
        std::fs::create_dir_all(ws.join(STAGING)).unwrap();
        std::fs::rename(ws.join("output"), ws.join(STAGING).join("output")).unwrap();
        std::fs::rename(ws.join(".git"), ws.join(STAGING).join(".git")).unwrap();
        let p = plan(&ws).unwrap();
        assert_eq!(p.status, Status::Resume(Phase::Collect));
        apply(&p).unwrap();
        let bot = ws.join(".thclaws/bots/main");
        assert!(bot.join("output/report.md").exists());
        assert!(bot.join(".git/config").exists());
        assert!(bot.join("AGENTS.md").exists());
        assert!(!ws.join(STAGING).exists());

        // Killed during install: everything staged, nothing installed.
        let (_r2, ws) = v2_workspace("proj");
        std::fs::create_dir_all(ws.join(STAGING)).unwrap();
        for name in movable_entries(&ws).unwrap() {
            std::fs::rename(ws.join(&name), ws.join(STAGING).join(&name)).unwrap();
        }
        write_phase(&ws, Phase::Install).unwrap();
        let p = plan(&ws).unwrap();
        assert_eq!(p.status, Status::Resume(Phase::Install));
        apply(&p).unwrap();
        assert!(ws.join(".thclaws/bots/main/src/main.py").exists());
        assert!(ws.join(".thclaws/bots.json").exists());

        // Killed during install AFTER a partial merge: both dirs present.
        let (_r3, ws) = v2_workspace("proj");
        std::fs::create_dir_all(ws.join(STAGING)).unwrap();
        for name in movable_entries(&ws).unwrap() {
            std::fs::rename(ws.join(&name), ws.join(STAGING).join(&name)).unwrap();
        }
        let bot = ws.join(".thclaws/bots/main");
        std::fs::create_dir_all(&bot).unwrap();
        std::fs::rename(ws.join(STAGING).join("src"), bot.join("src")).unwrap();
        write_phase(&ws, Phase::Install).unwrap();
        apply(&plan(&ws).unwrap()).unwrap();
        assert!(bot.join("src/main.py").exists());
        assert!(bot.join("AGENTS.md").exists());
        assert!(!ws.join(STAGING).exists());

        // Killed during finalise: the move is done, host files are not.
        let (_r4, ws) = v2_workspace("proj");
        std::fs::create_dir_all(ws.join(STAGING)).unwrap();
        for name in movable_entries(&ws).unwrap() {
            std::fs::rename(ws.join(&name), ws.join(STAGING).join(&name)).unwrap();
        }
        let bot = ws.join(".thclaws/bots/main");
        std::fs::create_dir_all(bot.parent().unwrap()).unwrap();
        std::fs::rename(ws.join(STAGING), &bot).unwrap();
        write_phase(&ws, Phase::Finalise).unwrap();
        let p = plan(&ws).unwrap();
        assert_eq!(p.status, Status::Resume(Phase::Finalise));
        apply(&p).unwrap();
        assert!(ws.join(".thclaws/bots.json").exists());
        assert!(read(&ws.join("AGENTS.md")).contains(".thclaws/bots/main"));
        // Crucially: resuming at finalise must NOT sweep the host tree it
        // just built into the bot.
        assert!(ws.join(".thclaws/bots/main/AGENTS.md").exists());
        assert!(!ws.join(".thclaws/bots/main/.thclaws/bots").exists());
    }

    #[test]
    fn refuses_a_tree_it_cannot_read_safely() {
        // A bot is not a workspace.
        let (_r, ws) = v2_workspace("proj");
        let bot = ws.join(".thclaws/bots/research");
        std::fs::create_dir_all(&bot).unwrap();
        assert!(plan(&bot).is_err());

        // A shelf without a v3 stamp is neither shape.
        let err = plan(&ws).unwrap_err().to_string();
        assert!(err.contains("neither v2 nor v3"), "{err}");

        // A directory that merely sits under something called `bots` is an
        // ordinary workspace — the refusal is about `.thclaws/bots` only.
        let plain = tempfile::tempdir().unwrap();
        let proj = plain.path().join("bots/myproj");
        std::fs::create_dir_all(proj.join(".thclaws")).unwrap();
        std::fs::write(proj.join("AGENTS.md"), "x").unwrap();
        assert_eq!(plan(&proj).unwrap().status, Status::Migrate);

        // A v3 stamp with no shelf is a broken tree, not a v2 one: migrating
        // would move the host's own `.thclaws/` into a fresh `bots/main/`.
        let (_r3, ws3) = v2_workspace("proj");
        std::fs::write(
            ws3.join(".thclaws/settings.json"),
            r#"{"workspaceVersion":3}"#,
        )
        .unwrap();
        let err = plan(&ws3).unwrap_err().to_string();
        assert!(err.contains("bots are missing"), "{err}");

        // Staging with no marker means someone removed the marker by hand.
        let (_r2, ws2) = v2_workspace("proj");
        std::fs::create_dir_all(ws2.join(STAGING)).unwrap();
        let err = plan(&ws2).unwrap_err().to_string();
        assert!(err.contains("without a migration marker"), "{err}");
    }

    /// `agent pack` refuses a folder with no `agent.{id,name,description}`,
    /// so a migrated workspace that never had one could not be published.
    #[test]
    fn mints_an_identity_only_when_one_is_missing() {
        let (_r, ws) = v2_workspace("My Cool Project");
        let report = apply(&plan(&ws).unwrap()).unwrap();
        assert!(report.minted_identity);
        let s: serde_json::Value =
            serde_json::from_str(&read(&ws.join(".thclaws/bots/main/.thclaws/settings.json")))
                .unwrap();
        assert_eq!(s["agent"]["id"], "my-cool-project");
        assert_eq!(s["agent"]["name"], "My Cool Project");
        assert!(s["agent"]["description"].as_str().is_some());
        // The version the agent carried is untouched.
        assert_eq!(s["workspaceVersion"], 2);

        // An agent that already has an identity keeps it exactly.
        let (_r2, ws2) = v2_workspace("other");
        std::fs::write(
            ws2.join(".thclaws/settings.json"),
            r#"{"workspaceVersion":2,"agent":{"id":"chosen","name":"Chosen","description":"d","uuid":"u-1"}}"#,
        )
        .unwrap();
        let report = apply(&plan(&ws2).unwrap()).unwrap();
        assert!(!report.minted_identity);
        let s: serde_json::Value = serde_json::from_str(&read(
            &ws2.join(".thclaws/bots/main/.thclaws/settings.json"),
        ))
        .unwrap();
        assert_eq!(s["agent"]["id"], "chosen");
        assert_eq!(s["agent"]["uuid"], "u-1");
    }

    #[test]
    fn slugify_produces_catalogue_safe_ids() {
        assert_eq!(slugify("My Cool Project"), "my-cool-project");
        assert_eq!(slugify("__weird__"), "weird");
        assert_eq!(slugify("ไทย"), "main");
        assert_eq!(slugify(""), "main");
        assert_eq!(slugify("a.b_c"), "a-b-c");
    }

    /// The schedule store is user-level and its daemon refuses a `cwd` that no
    /// longer exists, so this workspace's entries must follow the agent — and
    /// nobody else's may be touched.
    #[test]
    fn rewrites_only_this_workspaces_schedules() {
        let _g = crate::kms::test_env_lock();
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", home.path());

        let (_r, ws) = v2_workspace("proj");
        let other = home.path().join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        let store = serde_json::json!({
            "version": 1,
            "schedules": [
                {"id":"mine","cron":"0 9 * * *","prompt":"p","cwd": ws.display().to_string(),"enabled":true},
                {"id":"nested","cron":"0 9 * * *","prompt":"p","cwd": ws.join("output").display().to_string(),"enabled":true},
                {"id":"theirs","cron":"0 9 * * *","prompt":"p","cwd": other.display().to_string(),"enabled":true}
            ]
        });
        let path = home.path().join(".config/thclaws/schedules.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&store).unwrap()).unwrap();

        let p = plan(&ws).unwrap();
        assert_eq!(p.schedules, vec!["mine".to_string(), "nested".to_string()]);
        let report = apply(&p).unwrap();
        assert_eq!(report.rewritten_schedules.len(), 2);

        let after: serde_json::Value = serde_json::from_str(&read(&path)).unwrap();
        let cwd = |i: usize| after["schedules"][i]["cwd"].as_str().unwrap().to_string();
        let bot = ws.join(".thclaws/bots/main");
        assert_eq!(cwd(0), bot.display().to_string());
        assert_eq!(cwd(1), bot.join("output").display().to_string());
        assert_eq!(cwd(2), other.display().to_string());

        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    /// The automatic entry point: a v2 workspace becomes v3 on first open, a
    /// second open finds it so, something else is left alone, and a holder
    /// of the lock makes it wait and then give up rather than move anything.
    #[test]
    fn auto_migrate_runs_once_and_yields_to_a_holder() {
        let (_r, ws) = v2_workspace("proj");
        let wait = Duration::from_millis(200);
        assert!(matches!(
            auto_migrate_with_wait(&ws, wait),
            AutoOutcome::Migrated(_, true)
        ));
        assert!(ws.join(super::super::CONFIG_REL).exists());
        assert!(matches!(
            auto_migrate_with_wait(&ws, wait),
            AutoOutcome::AlreadyV3
        ));

        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(
            auto_migrate_with_wait(empty.path(), wait),
            AutoOutcome::NotV2
        ));

        // The bot's own folder, as its own `--serve` sees it: a v2-shaped tree
        // inside a shelf. Silently not v2 — not a failure to report.
        let bot = ws.join(".thclaws/bots/main");
        assert!(bot.join("AGENTS.md").exists());
        assert!(matches!(
            auto_migrate_with_wait(&bot, wait),
            AutoOutcome::NotV2
        ));
        assert!(
            !empty.path().join(".thclaws").exists(),
            "nothing minted, nothing moved"
        );

        let (_r2, held) = v2_workspace("held");
        let _holder = super::super::lock_workspace(&held, "a host").unwrap();
        let t0 = Instant::now();
        assert!(matches!(
            auto_migrate_with_wait(&held, wait),
            AutoOutcome::Busy
        ));
        assert!(
            t0.elapsed() >= wait,
            "it waited for the holder before giving up"
        );
        assert!(held.join("AGENTS.md").exists() && !held.join(".thclaws/bots").exists());
    }

    #[test]
    fn a_new_workspace_is_minted_as_a_host_plus_one_bot() {
        let dir = tempfile::tempdir().unwrap();
        let dest = mint_new_workspace(dir.path(), MAIN_SLUG).unwrap();
        assert!(dest.ends_with(".thclaws/bots/main"));
        assert!(dest.is_dir());
        let cfg = BotsConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.bots[0].slug, "main");
        assert!(!looks_like_v2_agent(dir.path()));
    }

    /// The one thing `--supervisor` must never do is start a host over an
    /// agent that still lives at the root.
    #[test]
    fn a_v2_workspace_is_recognised_before_a_host_starts_over_it() {
        let (_r, ws) = v2_workspace("proj");
        assert!(looks_like_v2_agent(&ws));
        apply(&plan(&ws).unwrap()).unwrap();
        assert!(!looks_like_v2_agent(&ws), "a migrated workspace is not v2");

        let empty = tempfile::tempdir().unwrap();
        assert!(!looks_like_v2_agent(empty.path()));
    }
}
