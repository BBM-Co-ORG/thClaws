//! Per-user `SharedSessionHandle` multiplexing for multi-tenant
//! `--serve` mode.
//!
//! Each authenticated user (verified via [`crate::multi_tenant::auth`])
//! gets their own [`SharedSessionHandle`] — independent agent loop,
//! independent session JSONL, independent event broadcast. Multiple
//! users sharing one pod don't observe each other's state.
//!
//! Lifecycle:
//! 1. WS upgrade arrives with HMAC-signed user-id headers.
//! 2. `handle_socket` calls [`UserSessionRegistry::get_or_spawn`].
//! 3. If absent, registry spawns a fresh `SharedSessionHandle` via
//!    [`crate::shared_session::spawn_with_approver`] (shared approver
//!    across users; modal prompts route to the cloud routing layer
//!    for now, dev-plan/35 Tier 3 may revisit).
//! 4. Handle gets cached in an LRU; future requests from the same
//!    user reuse it.
//! 5. Background task evicts idle sessions past TTL — eviction
//!    drops the handle which dropss the worker thread which finalises
//!    its session JSONL.
//!
//! Per-user state paths (session JSONL subdirs, per-user GUI-shell
//! storage, per-user usage metering) are wired through here:
//! [`UserSessionRegistry::get_or_spawn`] builds a [`UserStatePaths`]
//! from `(config.project_root, user_id)`, converts it to a
//! [`SessionRoots`], and threads it into
//! [`crate::shared_session::spawn_with_roots`] so the per-user
//! worker writes its session JSONL under
//! `<project>/.thclaws/users/<user_id>/sessions/`, its gui-shell
//! storage under `<project>/.thclaws/users/<user_id>/storage/`,
//! and its usage aggregates under
//! `<project>/.thclaws/users/<user_id>/usage/` — fully isolated
//! per user, fully recoverable on pod restart.

use super::auth::UserId;
use super::user_state::{SessionRoots, UserStatePaths};
use crate::permissions::ApprovalSink;
use crate::shared_session::{spawn_with_roots, SharedSessionHandle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One slot in the registry — the user's session handle + bookkeeping
/// for eviction decisions.
pub struct UserSession {
    pub user_id: UserId,
    pub handle: Arc<SharedSessionHandle>,
    /// Wall-clock of the most recent activity (any IPC message routed
    /// to this session). Drives idle-TTL eviction.
    last_activity: Mutex<Instant>,
}

impl UserSession {
    pub fn touch(&self) {
        if let Ok(mut lock) = self.last_activity.lock() {
            *lock = Instant::now();
        }
    }

    pub fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .ok()
            .map(|t| t.elapsed())
            .unwrap_or(Duration::ZERO)
    }
}

/// Registry of per-user session handles. Cheap to clone (internally
/// an `Arc<RwLock<...>>`), shared across all WS connections in the
/// serve process.
#[derive(Clone)]
pub struct UserSessionRegistry {
    inner: Arc<std::sync::RwLock<RegistryState>>,
    config: Arc<RegistryConfig>,
}

struct RegistryState {
    sessions: HashMap<UserId, Arc<UserSession>>,
}

/// Construction-time configuration. `Arc` so the background evictor
/// task can hold a reference without locking the registry mutex.
pub struct RegistryConfig {
    pub max_users: usize,
    pub idle_timeout: Duration,
    /// Shared across all users — modal-style approval prompts route
    /// through the cloud routing layer (dev-plan/34) rather than the
    /// pod. Tier 3 may give each user their own approver if/when
    /// the cloud wants user-specific approval flows.
    pub approver: Arc<dyn ApprovalSink>,
    /// Cwd at serve start — passed through to
    /// `UserStatePaths::new(&project_root, user_id)` so per-user
    /// session JSONLs, storage, and usage all land under
    /// `<project_root>/.thclaws/users/<user_id>/...`. Single-tenant
    /// `--serve` does not construct a registry, so this is always
    /// set; tests use a tempdir.
    pub project_root: PathBuf,
    /// dev-plan/42: when `Some`, each user gets their own working
    /// directory at `<workspaces_base>/workspace-<user_id>/` (the
    /// "a workspace per user" model). `get_or_spawn` provisions it on
    /// first connect and roots the session's cwd + state there. When
    /// `None`, the dev-plan/35 layout applies — one shared `project_root`
    /// cwd with per-user state subtrees.
    pub workspaces_base: Option<PathBuf>,
    /// dev-plan/42: the read-only agent-definition source (the owner's
    /// agent folder, e.g. `/workspace`). When set, a freshly-provisioned
    /// per-user workspace is seeded with a copy of the def from here
    /// (AGENTS.md + `.thclaws/{settings,kms,skills,…}` + project files),
    /// marked read-only — a frozen snapshot per user. `None` → empty
    /// workspace (no shared def).
    pub def_source: Option<PathBuf>,
    /// dev-plan/42 Phase 5: the workspace owner's user id (as the cloud
    /// signs it). Their per-user workspace is seeded with a *writable* def
    /// (they author the agent and "publish" updates to guests); every
    /// other user's def is read-only. `None` → all seeded defs read-only.
    pub owner_user_id: Option<String>,
}

impl UserSessionRegistry {
    pub fn new(config: RegistryConfig) -> Self {
        Self {
            inner: Arc::new(std::sync::RwLock::new(RegistryState {
                sessions: HashMap::new(),
            })),
            config: Arc::new(config),
        }
    }

    /// Fetch the existing session for this user, or spawn a fresh
    /// one. Bumps last_activity unconditionally so the LRU evictor
    /// keeps active users alive.
    pub fn get_or_spawn(&self, user_id: &UserId, display_name: Option<&str>) -> Arc<UserSession> {
        // Fast path: read lock, hit.
        if let Ok(guard) = self.inner.read() {
            if let Some(session) = guard.sessions.get(user_id) {
                session.touch();
                return session.clone();
            }
        }

        // Slow path: write lock + double-check (another thread may
        // have spawned the same user between our read drop and
        // write acquire).
        let mut guard = self
            .inner
            .write()
            .expect("UserSessionRegistry write lock poisoned");
        if let Some(session) = guard.sessions.get(user_id) {
            session.touch();
            return session.clone();
        }

        // Cap enforcement: evict LRU if at capacity before inserting.
        if guard.sessions.len() >= self.config.max_users {
            evict_lru(&mut guard);
        }

        // dev-plan/35 Tier 1: per-user roots derived from
        // (project_root, user_id) — the SharedSessionHandle below
        // writes its session JSONL, gui-shell storage, and usage
        // tracker under <state_root>/.thclaws/users/<user_id>/...
        // instead of the cwd-relative single-tenant defaults.
        //
        // dev-plan/42: when `workspaces_base` is set, each user's
        // *working directory* is their own `workspace-<user_id>/`. We
        // provision it on first connect and root both the state paths
        // and the session cwd there. With no base (dev-plan/35 layout),
        // state lives under the one shared `project_root` and the cwd
        // stays process-global.
        let (state_root, workspace_root) = match &self.config.workspaces_base {
            Some(base) => {
                let ws = base.join(format!("workspace-{}", user_id.as_str()));
                let fresh = !ws.exists();
                if let Err(e) = std::fs::create_dir_all(&ws) {
                    eprintln!(
                        "\x1b[33m[serve] could not provision workspace for {}: {e}\x1b[0m",
                        user_id.as_str()
                    );
                }
                // dev-plan/42: seed the agent def into a brand-new per-user
                // workspace (frozen snapshot). Only on first create so a
                // returning user keeps their own files. dev-plan/42 Phase 5:
                // the OWNER's def is writable (they author + publish
                // updates); everyone else's is read-only.
                if fresh {
                    if let Some(src) = &self.config.def_source {
                        let read_only =
                            self.config.owner_user_id.as_deref() != Some(user_id.as_str());
                        seed_def_into(src, &ws, read_only);
                    }
                }
                (ws.clone(), Some(ws))
            }
            None => (self.config.project_root.clone(), None),
        };
        let user_state = UserStatePaths::new(&state_root, user_id);
        let mut roots = SessionRoots::for_user_state(&user_state);
        roots.workspace_root = workspace_root;
        // dev-plan/45 A2: outbound gateway calls in this member's turns
        // carry their id for billing attribution + per-member caps.
        roots.member_id = Some(user_id.as_str().to_string());
        roots.member_name = display_name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        let handle = Arc::new(spawn_with_roots(self.config.approver.clone(), Some(roots)));
        let session = Arc::new(UserSession {
            user_id: user_id.clone(),
            handle,
            last_activity: Mutex::new(Instant::now()),
        });
        guard.sessions.insert(user_id.clone(), session.clone());
        session
    }

    /// Drop a user's session handle (called from admin endpoints —
    /// dev-plan/34 "evict on dispute / ban" path). Idempotent.
    /// Returns true if a session was actually removed. The dropped
    /// handle's worker thread observes channel close and exits,
    /// finalising its session JSONL via Drop on SharedSessionHandle.
    pub fn evict(&self, user_id: &UserId) -> bool {
        if let Ok(mut guard) = self.inner.write() {
            guard.sessions.remove(user_id).is_some()
        } else {
            false
        }
    }

    /// Count of currently-resident sessions. For metrics + tests.
    pub fn active_user_count(&self) -> usize {
        self.inner.read().map(|g| g.sessions.len()).unwrap_or(0)
    }

    /// Snapshot user-ids — for ops endpoints (`/admin/users`).
    pub fn active_user_ids(&self) -> Vec<UserId> {
        self.inner
            .read()
            .map(|g| g.sessions.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Spawn a background evictor that wakes every `interval` to
    /// drop sessions idle past the configured TTL. Returns the
    /// JoinHandle so the caller (server::run) can shut it down on
    /// process exit. Sweep cost is O(active_users), cheap.
    pub fn spawn_evictor(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let registry = self.clone();
        let idle_timeout = self.config.idle_timeout;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // Skip the immediate first tick so we don't churn at boot.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                registry.sweep_idle(idle_timeout);
            }
        })
    }

    fn sweep_idle(&self, idle_timeout: Duration) {
        let to_evict: Vec<UserId> = match self.inner.read() {
            Ok(guard) => guard
                .sessions
                .iter()
                .filter(|(_, s)| s.idle_for() > idle_timeout)
                .map(|(id, _)| id.clone())
                .collect(),
            Err(_) => return,
        };
        if to_evict.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.inner.write() {
            for id in to_evict {
                guard.sessions.remove(&id);
            }
        }
    }
}

/// Drop the single least-recently-active session. Called holding
/// the write lock when at capacity. No-op if the registry is empty
/// (shouldn't happen — caller checks `len() >= max_users` first).
fn evict_lru(state: &mut RegistryState) {
    // LRU = longest idle = MAX idle_for (oldest last_activity).
    // Easy bug to write as min — `idle_for` is "how long since
    // touched", so "least recently used" wants the biggest one.
    let oldest = state
        .sessions
        .iter()
        .max_by_key(|(_, s)| s.idle_for())
        .map(|(id, _)| id.clone());
    if let Some(id) = oldest {
        state.sessions.remove(&id);
    }
}

/// dev-plan/42: copy the agent definition from `src` into a freshly
/// provisioned per-user workspace `dst`, then mark every copied file
/// read-only (a frozen snapshot — the company agent's instructions/KMS/
/// skills are locked to the guest; their own work lives elsewhere in the
/// workspace, writable). Excludes the per-user dirs themselves (so we
/// never recurse `dst` back into itself), member-private state, build
/// artefacts, and secrets — mirrors the cloud pack/brain strip rules.
///
/// Best-effort: a copy/permission failure is logged, not fatal — a
/// missing def file just means the guest's agent is thinner, never a
/// crash or a security downgrade (gateway-force + isolation are enforced
/// elsewhere).
///
/// dev-plan/42 Phase 5: `read_only` marks copied def files `0444` (the
/// guest can't change the company agent). The OWNER seeds with
/// `read_only = false` so they can author the def and "publish" updates.
fn seed_def_into(src: &std::path::Path, dst: &std::path::Path, read_only: bool) {
    fn excluded(rel: &str) -> bool {
        const PREFIXES: &[&str] = &[
            ".users/",
            ".thclaws/users/",
            // Workspace v2 moved every runtime-state entry under `state/`.
            // The three legacy prefixes below stopped matching anything the
            // moment that landed, so seeding a member's workspace copied the
            // owner's cookie jar, sessions and KMS into it. `cloud/pack.rs`
            // was updated for the move; this list was not (dev-plan/65 #7).
            // Kept alongside for a workspace not yet opened since the move.
            ".thclaws/state/",
            ".thclaws/sessions/",
            ".thclaws/browser-profile/",
            ".thclaws/cache/",
            ".thclaws/kms/data/",
            // Same reason as `cloud/pack.rs`: playwright-mcp's per-navigation
            // page dumps are the owner's browsing, not agent content.
            ".playwright-mcp/",
            "output/users/",
            ".git/",
            "node_modules/",
            "target/",
            "__pycache__/",
        ];
        if PREFIXES.iter().any(|p| rel.starts_with(p)) {
            return true;
        }
        if rel.ends_with(".env") || rel.ends_with(".key") {
            return true;
        }
        rel.to_lowercase().contains("_secret")
    }

    let walker = walkdir::WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        // Prune heavy / private dirs so we don't descend into them (esp.
        // `.users`, which lives under `src` and would otherwise recurse).
        .filter_entry(|e| match e.path().strip_prefix(src) {
            Ok(rel) if e.file_type().is_dir() => {
                let mut s = rel.to_string_lossy().replace('\\', "/");
                if !s.is_empty() {
                    s.push('/');
                }
                !excluded(&s)
            }
            _ => true,
        });

    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(src) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if excluded(&rel_str) {
            continue;
        }
        let out = dst.join(rel);
        if let Some(parent) = out.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::copy(entry.path(), &out) {
            Ok(_) => {
                // Cross-platform read-only (unix: clears write bits → 0o444;
                // Windows: sets the read-only attribute). Avoids the
                // unix-only PermissionsExt that broke the Windows release.
                if read_only {
                    if let Ok(meta) = std::fs::metadata(&out) {
                        let mut perms = meta.permissions();
                        perms.set_readonly(true);
                        let _ = std::fs::set_permissions(&out, perms);
                    }
                }
            }
            Err(e) => eprintln!(
                "\x1b[33m[serve] seed copy failed for {}: {e}\x1b[0m",
                rel_str
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    /// The display name is advisory and lives only on the session's
    /// roots — it must never affect which user id a session belongs to,
    /// and an absent/blank one must leave no name at all (desktop and
    /// pre-header cloud builds both hit that path).
    #[test]
    fn display_name_rides_along_without_touching_identity() {
        let reg = UserSessionRegistry::new(config(8, Duration::from_secs(60)));
        let roots = |s: &Arc<UserSession>| s.handle.session_roots.clone().expect("roots");

        let alice = UserId::new_for_test("alice");
        let s1 = reg.get_or_spawn(&alice, Some("จิมมี่ Panutat"));
        assert_eq!(roots(&s1).member_id.as_deref(), Some("alice"));
        assert_eq!(
            roots(&s1).member_name.as_deref(),
            Some("จิมมี่ Panutat"),
            "a Thai name must survive intact"
        );

        // Blank / whitespace is the same as absent — a chat surface
        // should show no name, not an empty one.
        let bob = UserId::new_for_test("bob");
        assert_eq!(
            roots(&reg.get_or_spawn(&bob, Some("   "))).member_name,
            None
        );

        // Desktop / pre-header cloud: no name at all.
        let carol = UserId::new_for_test("carol");
        let s3 = reg.get_or_spawn(&carol, None);
        assert_eq!(roots(&s3).member_name, None);
        assert_eq!(roots(&s3).member_id.as_deref(), Some("carol"));
    }

    use super::*;
    use crate::permissions::AutoApprover;

    fn config(max_users: usize, idle_timeout: Duration) -> RegistryConfig {
        // Existing unit tests don't exercise the per-user roots
        // (just registry mechanics) so std::env::temp_dir() is a
        // safe shared root — UserSessionRegistry only stores it,
        // the spawned worker thread is the only consumer and it
        // gets dropped at test end before writing anything that
        // would collide.
        config_with_root(max_users, idle_timeout, std::env::temp_dir())
    }

    fn config_with_root(
        max_users: usize,
        idle_timeout: Duration,
        project_root: PathBuf,
    ) -> RegistryConfig {
        RegistryConfig {
            max_users,
            idle_timeout,
            approver: Arc::new(AutoApprover),
            project_root,
            workspaces_base: None,
            def_source: None,
            owner_user_id: None,
        }
    }

    // dev-plan/42: registry config with per-user working directories.
    fn config_with_workspaces_base(base: PathBuf) -> RegistryConfig {
        RegistryConfig {
            max_users: 10,
            idle_timeout: Duration::from_secs(60),
            approver: Arc::new(AutoApprover),
            project_root: base.clone(),
            workspaces_base: Some(base),
            def_source: None,
            owner_user_id: None,
        }
    }

    #[test]
    fn seed_def_into_copies_def_excludes_private_and_locks_readonly() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        let s = src.path();
        std::fs::write(s.join("AGENTS.md"), b"# agent def").unwrap();
        std::fs::create_dir_all(s.join(".thclaws/kms")).unwrap();
        std::fs::write(s.join(".thclaws/kms/index.bin"), b"idx").unwrap();
        std::fs::create_dir_all(s.join(".thclaws/sessions")).unwrap();
        std::fs::write(s.join(".thclaws/sessions/sess.jsonl"), b"private chat").unwrap();
        std::fs::create_dir_all(s.join(".users/workspace-bob")).unwrap();
        std::fs::write(s.join(".users/workspace-bob/file.txt"), b"bob's").unwrap();
        std::fs::write(s.join(".env"), b"OPENAI_API_KEY=sk-x").unwrap();

        let dst = tempfile::tempdir().unwrap();
        let d = dst.path();
        seed_def_into(s, d, true);

        // Def parts are copied…
        assert!(d.join("AGENTS.md").is_file(), "AGENTS.md seeded");
        assert!(
            d.join(".thclaws/kms/index.bin").is_file(),
            "kms index seeded"
        );
        // …private / recursive / secret parts are NOT.
        assert!(
            !d.join(".thclaws/sessions/sess.jsonl").exists(),
            "sessions excluded"
        );
        assert!(
            !d.join(".users").exists(),
            "per-user dirs excluded (no recursion)"
        );
        assert!(!d.join(".env").exists(), ".env excluded");
        // Seeded def is locked read-only (frozen snapshot).
        let mode = std::fs::metadata(d.join("AGENTS.md"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o444, "seeded def file is read-only");
    }

    /// The test above pins the PRE-v2 paths, which is how the exclusion list
    /// went stale unnoticed: once runtime state moved under
    /// `.thclaws/state/`, `.thclaws/browser-profile/` and friends matched
    /// nothing and the owner's cookie jar, sessions and KMS were seeded into
    /// every member's workspace (dev-plan/65 #7). Pin the CURRENT layout.
    #[test]
    fn seed_def_into_excludes_the_current_state_layout() {
        let src = tempfile::tempdir().unwrap();
        let s = src.path();
        std::fs::write(s.join("AGENTS.md"), b"# agent def").unwrap();
        for rel in [
            ".thclaws/state/browser-profile/Default/Cookies",
            ".thclaws/state/sessions/sess.jsonl",
            ".thclaws/state/kms/Vault/page.md",
            ".thclaws/state/usage.jsonl",
        ] {
            let p = s.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"private").unwrap();
        }

        let dst = tempfile::tempdir().unwrap();
        let d = dst.path();
        seed_def_into(s, d, true);

        assert!(d.join("AGENTS.md").is_file(), "def still seeded");
        assert!(
            !d.join(".thclaws/state").exists(),
            "nothing under .thclaws/state/ may be seeded into a member's workspace"
        );
    }

    // dev-plan/42 Phase 5: the owner seeds with read_only=false so they
    // can edit the agent def + publish updates.
    #[test]
    fn seed_def_into_owner_gets_writable_def() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("AGENTS.md"), b"# def").unwrap();
        let dst = tempfile::tempdir().unwrap();
        seed_def_into(src.path(), dst.path(), /* read_only */ false);
        let mode = std::fs::metadata(dst.path().join("AGENTS.md"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o200, 0, "owner's seeded def must be writable");
    }

    #[test]
    fn per_user_workspaces_are_provisioned_and_distinct() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let reg = UserSessionRegistry::new(config_with_workspaces_base(base.clone()));

        let alice = UserId::new_for_test("alice");
        let bob = UserId::new_for_test("bob");
        let _ = reg.get_or_spawn(&alice, None);
        let _ = reg.get_or_spawn(&bob, None);

        // dev-plan/42: first connect provisions <base>/workspace-<id>/,
        // one per user, and they are distinct directories.
        let alice_ws = base.join(format!("workspace-{}", alice.as_str()));
        let bob_ws = base.join(format!("workspace-{}", bob.as_str()));
        assert!(alice_ws.is_dir(), "alice's workspace dir provisioned");
        assert!(bob_ws.is_dir(), "bob's workspace dir provisioned");
        assert_ne!(alice_ws, bob_ws, "each user gets their own workspace");
    }

    #[test]
    fn get_or_spawn_returns_same_handle_per_user() {
        let reg = UserSessionRegistry::new(config(10, Duration::from_secs(60)));
        let alice = UserId::new_for_test("alice");
        let a = reg.get_or_spawn(&alice, None);
        let b = reg.get_or_spawn(&alice, None);
        assert!(Arc::ptr_eq(&a, &b), "same user → same session arc");
        assert_eq!(reg.active_user_count(), 1);
    }

    #[test]
    fn different_users_get_different_handles() {
        let reg = UserSessionRegistry::new(config(10, Duration::from_secs(60)));
        let a = reg.get_or_spawn(&UserId::new_for_test("alice"), None);
        let b = reg.get_or_spawn(&UserId::new_for_test("bob"), None);
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a.handle, &b.handle));
        assert_eq!(reg.active_user_count(), 2);
    }

    #[test]
    fn evict_removes_user() {
        let reg = UserSessionRegistry::new(config(10, Duration::from_secs(60)));
        let alice = UserId::new_for_test("alice");
        reg.get_or_spawn(&alice, None);
        assert_eq!(reg.active_user_count(), 1);
        assert!(reg.evict(&alice));
        assert_eq!(reg.active_user_count(), 0);
        // Idempotent: second evict returns false.
        assert!(!reg.evict(&alice));
    }

    #[test]
    fn capacity_triggers_lru_eviction() {
        let reg = UserSessionRegistry::new(config(2, Duration::from_secs(60)));
        let a = UserId::new_for_test("a");
        let b = UserId::new_for_test("b");
        let c = UserId::new_for_test("c");
        reg.get_or_spawn(&a, None);
        std::thread::sleep(Duration::from_millis(10));
        reg.get_or_spawn(&b, None);
        std::thread::sleep(Duration::from_millis(10));
        // c arrives → at cap (2) → a should be evicted (oldest).
        reg.get_or_spawn(&c, None);
        let active: Vec<_> = reg.active_user_ids().into_iter().collect();
        assert_eq!(active.len(), 2);
        assert!(active.contains(&b));
        assert!(active.contains(&c));
        assert!(!active.contains(&a), "LRU should have dropped a");
    }

    #[test]
    fn touch_updates_lru_so_active_user_survives() {
        let reg = UserSessionRegistry::new(config(2, Duration::from_secs(60)));
        let a = UserId::new_for_test("a");
        let b = UserId::new_for_test("b");
        let c = UserId::new_for_test("c");
        reg.get_or_spawn(&a, None);
        std::thread::sleep(Duration::from_millis(10));
        reg.get_or_spawn(&b, None);
        std::thread::sleep(Duration::from_millis(10));
        // Re-touching a refreshes its activity — now b is oldest.
        reg.get_or_spawn(&a, None);
        std::thread::sleep(Duration::from_millis(10));
        reg.get_or_spawn(&c, None);
        let active = reg.active_user_ids();
        assert!(active.contains(&a), "a should survive — recently touched");
        assert!(active.contains(&c));
        assert!(!active.contains(&b), "b is now the LRU");
    }

    /// dev-plan/35 Tier 1 acceptance: every user's spawned
    /// SharedSessionHandle carries a `session_roots` that points at
    /// THEIR per-user subtree under the registry's project_root.
    /// Without this, the registry's "per-user isolation" is purely
    /// in-memory — restart loses everyone, and two users can collide
    /// on storage keys. With it, the worker's own SessionStore /
    /// gui-shell storage / usage tracker write to disjoint paths.
    #[test]
    fn per_user_handles_carry_distinct_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = UserSessionRegistry::new(config_with_root(
            10,
            Duration::from_secs(60),
            tmp.path().to_path_buf(),
        ));
        let alice = UserId::new_for_test("alice");
        let bob = UserId::new_for_test("bob");
        let a = reg.get_or_spawn(&alice, None);
        let b = reg.get_or_spawn(&bob, None);

        let a_roots = a.handle.session_roots.as_ref().expect("alice roots set");
        let b_roots = b.handle.session_roots.as_ref().expect("bob roots set");

        // Each lands under its own user-id segment.
        let a_expected = tmp.path().join(".thclaws/users/alice/sessions");
        let b_expected = tmp.path().join(".thclaws/users/bob/sessions");
        assert_eq!(a_roots.sessions_dir, a_expected);
        assert_eq!(b_roots.sessions_dir, b_expected);

        // storage_dir + usage_dir are siblings under the same user
        // prefix — they must not collide either.
        assert!(a_roots
            .storage_dir
            .starts_with(tmp.path().join(".thclaws/users/alice")));
        assert!(b_roots
            .storage_dir
            .starts_with(tmp.path().join(".thclaws/users/bob")));
        assert!(a_roots
            .usage_dir
            .starts_with(tmp.path().join(".thclaws/users/alice")));
        assert!(b_roots
            .usage_dir
            .starts_with(tmp.path().join(".thclaws/users/bob")));
    }

    /// dev-plan/35 Tier 1 "done means": kill the pod (drop the
    /// registry), restart against the same project dir, the user's
    /// session JSONLs and gui-shell storage are still on disk and
    /// the new registry recovers them when the user reconnects.
    /// This test simulates the on-disk side end-to-end:
    /// 1. Build registry; spawn alice; write a session JSONL +
    ///    storage value via her roots.
    /// 2. Drop registry + handle (worker thread tears down).
    /// 3. Build a NEW registry on the same tempdir; spawn alice
    ///    again. The new roots must be byte-identical so the new
    ///    worker's SessionStore + storage handlers see the
    ///    pre-restart files.
    #[test]
    fn restart_recovery_user_sees_prior_session_on_new_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let alice = UserId::new_for_test("alice");

        // --- pod 1 ---
        let pre_roots = {
            let reg = UserSessionRegistry::new(config_with_root(
                10,
                Duration::from_secs(60),
                project_root.clone(),
            ));
            let a = reg.get_or_spawn(&alice, None);
            let roots = a.handle.session_roots.as_ref().unwrap().clone();

            // Write a session JSONL via the per-user SessionStore at
            // the same path the worker would use. Mirrors what
            // shared_session.rs:1683 does on its first turn.
            std::fs::create_dir_all(&roots.sessions_dir).unwrap();
            let store = crate::session::SessionStore::new(roots.sessions_dir.clone());
            let mut sess = crate::session::Session::new("test-model", "/tmp/proj");
            let sess_id = sess.id.clone();
            let path = store.path_for(&sess_id);
            sess.append_to(&path).unwrap();
            assert!(path.exists(), "pre-restart session jsonl must exist");

            // Write a gui-shell storage value via the per-user override.
            crate::gui_shell::storage::set_in(
                &roots.storage_dir,
                "chatbot",
                &sess_id,
                "msg-1",
                serde_json::json!("from-pod-1"),
            )
            .unwrap();

            roots
            // reg + handle drop here — worker thread exits, just like
            // a pod restart.
        };

        // --- pod 2 (fresh registry, same project_root) ---
        let reg2 = UserSessionRegistry::new(config_with_root(
            10,
            Duration::from_secs(60),
            project_root.clone(),
        ));
        let a2 = reg2.get_or_spawn(&alice, None);
        let post_roots = a2.handle.session_roots.as_ref().unwrap();

        // The recovered roots are byte-identical — the new worker
        // sees the same on-disk subtree the old one wrote to.
        assert_eq!(pre_roots.sessions_dir, post_roots.sessions_dir);
        assert_eq!(pre_roots.storage_dir, post_roots.storage_dir);

        // Files written before restart are visible to the new pod's
        // SessionStore + storage handler.
        let store_after = crate::session::SessionStore::new(post_roots.sessions_dir.clone());
        let sessions: Vec<_> = store_after.list().unwrap();
        assert_eq!(sessions.len(), 1, "alice's prior session must persist");
        let sess_id = sessions[0].id.clone();
        let recovered = crate::gui_shell::storage::get_in(
            &post_roots.storage_dir,
            "chatbot",
            &sess_id,
            "msg-1",
        )
        .unwrap();
        assert_eq!(recovered, serde_json::json!("from-pod-1"));
    }

    /// dev-plan/35 Tier 1 "done means": 50 concurrent users on the
    /// same Agent run without state corruption. We can't soak for an
    /// hour in a unit test, but we CAN hammer the registry from many
    /// threads in parallel and verify (a) no panic, (b) every user
    /// gets back the same Arc across calls (no double-spawn races),
    /// (c) each user's roots are distinct, (d) no cross-user leakage.
    /// This is the closest practical check to a real soak — the
    /// registry's RwLock + double-check spawn path is the actual
    /// concurrency surface we need to prove.
    #[test]
    fn concurrent_50_users_no_cross_leakage_or_double_spawn() {
        use std::sync::Arc as StdArc;
        let tmp = tempfile::tempdir().unwrap();
        let reg = StdArc::new(UserSessionRegistry::new(config_with_root(
            100,
            Duration::from_secs(60),
            tmp.path().to_path_buf(),
        )));

        let mut handles = Vec::new();
        // 50 users × 4 threads each — exercises (a) initial spawn
        // races between threads asking for the same user, and (b)
        // distinct-user spawn fan-out under lock contention.
        for u in 0..50 {
            for _t in 0..4 {
                let reg = reg.clone();
                let user_id = UserId::new_for_test(&format!("u{u:02}"));
                handles.push(std::thread::spawn(move || reg.get_or_spawn(&user_id, None)));
            }
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // (a) 200 calls, 50 users → exactly 50 distinct UserSessions.
        assert_eq!(reg.active_user_count(), 50);

        // (b) for each user, all 4 racing calls returned the SAME
        // Arc — double-check spawn worked, no leak.
        for u in 0..50 {
            let id = UserId::new_for_test(&format!("u{u:02}"));
            let same: Vec<_> = results.iter().filter(|s| s.user_id == id).collect();
            assert_eq!(same.len(), 4, "u{u:02} should have 4 entries in results");
            for s in &same[1..] {
                assert!(
                    Arc::ptr_eq(s, same[0]),
                    "u{u:02} returned distinct Arcs — double-spawn race"
                );
            }
        }

        // (c) every user's roots are distinct + cleanly disjoint.
        let mut session_dirs: Vec<PathBuf> = Vec::new();
        for u in 0..50 {
            let id = UserId::new_for_test(&format!("u{u:02}"));
            let s = reg.get_or_spawn(&id, None);
            let r = s.handle.session_roots.as_ref().unwrap();
            session_dirs.push(r.sessions_dir.clone());
        }
        let mut sorted = session_dirs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            50,
            "every user must have a unique sessions_dir"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evictor_sweeps_idle_sessions() {
        let reg = UserSessionRegistry::new(config(10, Duration::from_millis(50)));
        reg.get_or_spawn(&UserId::new_for_test("idle"), None);
        assert_eq!(reg.active_user_count(), 1);
        let evictor = reg.spawn_evictor(Duration::from_millis(20));
        // Wait long enough for idle to exceed 50ms + a sweep cycle.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(reg.active_user_count(), 0);
        evictor.abort();
    }
}
