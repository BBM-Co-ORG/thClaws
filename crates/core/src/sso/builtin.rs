//! Built-in SSO providers (standard feature, not enterprise-gated).
//!
//! The enterprise SSO path (`policy::active().policies.sso`) requires a
//! signed policy file pinning the org's IdP — issuer URL, client_id,
//! optional client_secret. That stays the override for enterprises that
//! need to enforce a single sign-in surface.
//!
//! Standard users (no policy file loaded) sign in with one of the
//! built-in providers below. Each provider's issuer + scopes are
//! hardcoded; the client_id / client_secret come from environment
//! variables (loaded from `.env` at startup via `crate::dotenv`). When
//! a provider's env vars are absent the provider drops out of
//! `available()` so the UI doesn't dangle a button that errors on
//! click.
//!
//! ## Where credentials come from
//!
//! Resolved in this order at runtime, first non-empty wins:
//!
//! 1. **`std::env::var`** — populated by the shell (`export …`) OR by
//!    `crate::dotenv` loading `./.env` / `~/.config/thclaws/.env` at
//!    startup. This is the local-dev / source-build path.
//! 2. **Compile-time `BUNDLED_*` env vars** baked in by `build.rs`.
//!    Official release builds inject these in CI (see
//!    `.github/workflows/release.yml`) so the navbar "Sign in with
//!    …" buttons work out of the box on the published dmg/msi.
//!    Source builds without the CI env behave exactly like before
//!    the bundling was added (the runtime read returns `Some("")`,
//!    which the filter below drops).
//!
//! ## Why bundling is safe
//!
//! Google + Azure desktop client IDs aren't secrets — they appear in
//! every OAuth URL and id_token `aud` claim. Microsoft and Google both
//! document them as public. Azure native/public clients run PKCE and
//! have no client_secret at all (so there's no
//! `BUNDLED_AZURE_CLIENT_SECRET`). The Google client_secret is also
//! "public" in the OAuth sense; the trust boundary is the
//! gateway-side signature + `aud` verification (see
//! `thclaws-technical-manual/sso.md`).

use crate::error::{Error, Result};
use crate::policy::SsoPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinProvider {
    Google,
    /// Stubbed — env keys + issuer wired but disabled until the
    /// Azure-side OAuth client is registered. Kept in the enum so
    /// `from_id` / `available` round-trip cleanly when the user
    /// flips it on later.
    Azure,
}

impl BuiltinProvider {
    /// Short id used over IPC and in the chosen-provider persistence.
    pub fn id(&self) -> &'static str {
        match self {
            BuiltinProvider::Google => "google",
            BuiltinProvider::Azure => "azure",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            BuiltinProvider::Google => "Google",
            BuiltinProvider::Azure => "Microsoft",
        }
    }

    pub fn issuer_url(&self) -> &'static str {
        match self {
            BuiltinProvider::Google => "https://accounts.google.com",
            // Azure's common endpoint accepts any tenant (personal +
            // work/school). For a tenant-restricted app, swap `common`
            // for the tenant id at registration time.
            BuiltinProvider::Azure => "https://login.microsoftonline.com/common/v2.0",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        match s {
            "google" => Some(Self::Google),
            "azure" => Some(Self::Azure),
            _ => None,
        }
    }

    /// Env var names this provider reads its client credentials from.
    /// Both are loaded out of `.env` at startup; `_SECRET` is optional
    /// (Azure desktop apps run PKCE-only without a secret), `_ID` is
    /// required.
    fn env_keys(&self) -> (&'static str, &'static str) {
        match self {
            BuiltinProvider::Google => ("GOOGLE_CLIENT_ID", "GOOGLE_CLIENT_SECRET"),
            BuiltinProvider::Azure => ("AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET"),
        }
    }

    /// `true` when the provider's client_id is resolvable — either
    /// from `std::env::var` (local-dev `.env` path) or the compile-time
    /// `BUNDLED_*_CLIENT_ID` baked in by `build.rs` (official release
    /// builds). UI consults this to decide which buttons to render.
    pub fn is_configured(&self) -> bool {
        let (id_env, _) = self.env_keys();
        resolve_env_or_bundled(id_env).is_some()
    }

    /// Resolve into the `SsoPolicy` shape the existing
    /// `crate::sso::login` / `current_session` / `logout` API consumes.
    /// The shape is the same one EE policies use — that's deliberate
    /// so the auth flow code doesn't need to branch on "where did this
    /// policy come from".
    pub fn resolve(&self) -> Result<SsoPolicy> {
        let (id_env, secret_env) = self.env_keys();
        let client_id = resolve_env_or_bundled(id_env).ok_or_else(|| {
            Error::Config(format!(
                "{id_env} is not set — add it to .env or your environment, or rebuild with the BUNDLED_* env var"
            ))
        })?;
        let client_secret = resolve_env_or_bundled(secret_env);
        Ok(SsoPolicy {
            enabled: true,
            provider: "oidc".into(),
            issuer_url: self.issuer_url().into(),
            client_id,
            audience: None,
            client_secret,
            // Secret already resolved above — no env-name indirection.
            client_secret_env: None,
        })
    }
}

/// Resolve a credential value by checking runtime env first (so
/// local-dev `.env` and shell `export` win) then falling back to the
/// compile-time `BUNDLED_<name>` value baked in by `build.rs`.
/// Returns `None` only when both are empty / unset.
fn resolve_env_or_bundled(name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(name) {
        let trimmed = v.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    bundled_value(name).map(str::to_string)
}

/// Look up the compile-time `BUNDLED_<name>` value. Each variant is
/// a separate `env!()` so the lookup table stays a `const`-foldable
/// `match` — no runtime allocation, no hashmap, no surprises if
/// `build.rs` forgets to emit one (compile error instead of silent
/// `None` for that key).
fn bundled_value(name: &str) -> Option<&'static str> {
    let raw = match name {
        "GOOGLE_CLIENT_ID" => env!("BUNDLED_GOOGLE_CLIENT_ID"),
        "GOOGLE_CLIENT_SECRET" => env!("BUNDLED_GOOGLE_CLIENT_SECRET"),
        "AZURE_CLIENT_ID" => env!("BUNDLED_AZURE_CLIENT_ID"),
        // No `BUNDLED_AZURE_CLIENT_SECRET` — Azure native/public clients
        // run PKCE without a secret. Return empty so the secret-resolve
        // path falls through to None cleanly.
        "AZURE_CLIENT_SECRET" => "",
        _ => "",
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// All providers whose client_id env var is set right now. Empty when
/// the user hasn't configured any OAuth app — the navbar Login button
/// stays present but its dropdown collapses to a single "configure"
/// hint.
pub fn available() -> Vec<BuiltinProvider> {
    [BuiltinProvider::Google, BuiltinProvider::Azure]
        .into_iter()
        .filter(|p| p.is_configured())
        .collect()
}

/// Find the first builtin (in `available()` order) that has a stored
/// session in the keychain. Used by the state payload to decide
/// "logged in as X" without requiring a separate chosen-provider
/// persistence file.
///
/// **Per-process memo (May 2026):** the navbar `LoginButton` re-fetches
/// `sso_status` on window-focus + dropdown-open + react-strict-mode
/// double-mount, so this can fire dozens of times across a session.
/// Each call walks `storage::load(...)` which, when the marker file
/// names a known issuer, hits the OS keychain. On macOS, a freshly-
/// rebuilt binary has a new code signature → every keychain access
/// triggers an ACL prompt regardless of any "Always Allow" the user
/// previously granted. Result: a developer running `make install`
/// then launching the GUI sees 20+ prompts in rapid succession
/// (reported May 2026).
///
/// Fix: memoize the result for the lifetime of the process. The
/// FIRST call walks storage; subsequent calls return the cached
/// answer instantly. Sign-in / sign-out paths invalidate via
/// [`invalidate_session_memo`] so the UI reflects state changes
/// without restart. Expired sessions also re-walk so refresh stays
/// timely.
pub fn current_session_any() -> Option<(BuiltinProvider, super::Session)> {
    {
        let guard = session_memo().read().expect("session_memo read");
        if let Some(cached) = guard.as_ref() {
            // Any of (a) no-session memo, (b) live session — return
            // without touching keychain.
            return cached.clone().filter(|(_, s)| !s.is_expired());
        }
    }
    let resolved: Option<(BuiltinProvider, super::Session)> = (|| {
        for p in available() {
            let Ok(policy) = p.resolve() else { continue };
            if let Some(s) = super::storage::load(&policy.issuer_url) {
                return Some((p, s));
            }
        }
        None
    })();
    *session_memo().write().expect("session_memo write") = Some(resolved.clone());
    resolved
}

/// Invalidate the [`current_session_any`] memo. Called by
/// `sso::login` after a successful sign-in and by `sso::logout`
/// after a clear, so the next `build_state_payload()` re-walks
/// storage and observes the new state.
pub fn invalidate_session_memo() {
    *session_memo().write().expect("session_memo write") = None;
}

fn session_memo() -> &'static std::sync::RwLock<Option<Option<(BuiltinProvider, super::Session)>>> {
    use std::sync::OnceLock;
    type CachedSession = Option<Option<(BuiltinProvider, super::Session)>>;
    static MEMO: OnceLock<std::sync::RwLock<CachedSession>> = OnceLock::new();
    MEMO.get_or_init(|| std::sync::RwLock::new(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // bundled_value / resolve_env_or_bundled mutate via std::env::var.
    // Serialise the tests so a sibling test doesn't observe a flapping
    // env state. Cheap; only this small module is affected.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn bundled_value_returns_none_for_empty_or_unknown() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Empty bundled (the default for source builds without the CI
        // env) → None. Unknown name → None.
        assert!(bundled_value("DOES_NOT_EXIST").is_none());
        // `AZURE_CLIENT_SECRET` is explicitly empty-mapped in
        // `bundled_value` (no secret for Azure public clients).
        assert!(bundled_value("AZURE_CLIENT_SECRET").is_none());
    }

    #[test]
    fn resolve_env_or_bundled_prefers_env_over_bundled() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("GOOGLE_CLIENT_ID", "from-env");
        let resolved = resolve_env_or_bundled("GOOGLE_CLIENT_ID");
        std::env::remove_var("GOOGLE_CLIENT_ID");
        // Env wins even if BUNDLED_GOOGLE_CLIENT_ID was injected at
        // compile time (this test passes either way).
        assert_eq!(resolved.as_deref(), Some("from-env"));
    }

    #[test]
    fn resolve_env_or_bundled_treats_blank_env_as_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("GOOGLE_CLIENT_ID", "   ");
        let resolved = resolve_env_or_bundled("GOOGLE_CLIENT_ID");
        std::env::remove_var("GOOGLE_CLIENT_ID");
        // Blank env falls through to bundled (which may be empty too
        // in the source-build case). Either way, never returns the
        // blank string itself.
        match resolved {
            Some(s) => assert!(!s.trim().is_empty()),
            None => {} // source build, no bundled value — fine
        }
    }

    #[test]
    fn is_configured_returns_true_when_env_is_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("AZURE_CLIENT_ID", "test-azure-id");
        let ok = BuiltinProvider::Azure.is_configured();
        std::env::remove_var("AZURE_CLIENT_ID");
        assert!(ok);
    }

    #[test]
    fn resolve_carries_through_env_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("AZURE_CLIENT_ID", "test-azure-id");
        let policy = BuiltinProvider::Azure.resolve().expect("resolve");
        std::env::remove_var("AZURE_CLIENT_ID");
        assert_eq!(policy.client_id, "test-azure-id");
        // Azure public client → no secret returned even if env is unset.
        assert!(policy.client_secret.is_none());
    }
}
