//! thClaws.cloud catalog client (dev-plan/34).
//!
//! An "AI Agent" in thClaws is a working folder. This module gives the
//! engine four CLI verbs to interact with the catalog at
//! `https://thclaws.cloud`:
//!
//! - `thclaws cloud login`   — paste a CLI token minted from the web dashboard
//! - `thclaws cloud publish` — tar the cwd (stripping secrets + sessions) and upload
//! - `thclaws cloud get`     — download an agent package and extract into a folder
//! - `thclaws cloud list`    — list your purchased / published agents
//!
//! URL precedence: `--cloud-url` flag → `THCLAWS_CLOUD_URL` env →
//! `settings.json::cloud.url` → default `https://thclaws.cloud`.
//!
//! Token precedence: `THCLAWS_CLOUD_TOKEN` env → secrets backend
//! (keychain or `~/.config/thclaws/.env`) → legacy
//! `~/.config/thclaws/cloud-token` file (kept readable so older logins
//! still work). The GUI Settings modal writes through `set_token` →
//! current secrets backend, same bundle as provider API keys.

use serde::{Deserialize, Serialize};

pub mod agent_cli;
pub mod agent_scaffold;
pub mod client;
pub mod cmd;
pub mod manifest;
pub mod pack;
pub mod wssync;

pub const DEFAULT_CLOUD_URL: &str = "https://thclaws.cloud";

const KEYCHAIN_KEY: &str = "cloud-token";
pub const ENV_TOKEN: &str = "THCLAWS_CLOUD_TOKEN";

/// On-disk shape of the `cloud` block in `.thclaws/settings.json` or
/// `~/.config/thclaws/settings.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CloudConfig {
    /// Base URL of the catalog backend. Defaults to `https://thclaws.cloud`
    /// when unset.
    pub url: Option<String>,
}

impl CloudConfig {
    pub fn resolved_url(&self) -> String {
        if let Ok(env_url) = std::env::var("THCLAWS_CLOUD_URL") {
            if !env_url.trim().is_empty() {
                return env_url.trim_end_matches('/').to_string();
            }
        }
        self.url
            .as_deref()
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_else(|| DEFAULT_CLOUD_URL.to_string())
    }
}

/// Resolve the effective cloud URL with CLI-flag override.
pub fn resolve_cloud_url(cli_override: Option<&str>, config: Option<&CloudConfig>) -> String {
    if let Some(u) = cli_override {
        let t = u.trim();
        if !t.is_empty() {
            return t.trim_end_matches('/').to_string();
        }
    }
    match config {
        Some(c) => c.resolved_url(),
        None => CloudConfig::default().resolved_url(),
    }
}

/// URL resolved purely from persisted state (no `--cloud-url` flag).
/// Used by the IPC handler that powers the Settings modal — the modal
/// only ever shows what's persisted, not anything an in-process flag
/// might have overridden.
/// #208: a thClaws.cloud CLI token is minted as `thc_` plus random bytes. A
/// value without that prefix is a mis-paste (a gateway key, an API key, a stray
/// line), and saving it would break every cloud call in silence.
pub fn looks_like_cli_token(token: &str) -> bool {
    let t = token.trim();
    t.len() > "thc_".len() && t.starts_with("thc_") && !t.contains(char::is_whitespace)
}

pub fn persisted_url() -> Option<String> {
    let project_url = crate::config::ProjectConfig::load()
        .and_then(|c| c.cloud)
        .and_then(|c| c.url);
    if let Some(u) = project_url {
        let t = u.trim();
        if !t.is_empty() {
            return Some(t.trim_end_matches('/').to_string());
        }
    }
    None
}

/// Token resolution. Order:
/// 1. `THCLAWS_CLOUD_TOKEN` env (CI override).
/// 2. The active secrets backend (keychain or `~/.config/thclaws/.env`).
/// 3. Legacy `~/.config/thclaws/cloud-token` file from earlier MVP
///    builds (kept readable so installs predating the Settings UI keep
///    working).
pub fn token() -> Option<String> {
    if let Ok(t) = std::env::var(ENV_TOKEN) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    if let Some(t) = crate::secrets::get(KEYCHAIN_KEY) {
        if !t.trim().is_empty() {
            return Some(t.trim().to_string());
        }
    }
    legacy_file_token()
}

/// Persist a CLI token via whichever backend the user picked for
/// provider API keys. Also pushes the value into the process env so
/// the in-flight CLI invocation can use it without a restart.
pub fn set_token(token: &str) -> crate::error::Result<()> {
    let backend = crate::secrets::get_backend().unwrap_or(crate::secrets::Backend::Keychain);
    match backend {
        crate::secrets::Backend::Keychain => {
            crate::secrets::set(KEYCHAIN_KEY, token)?;
        }
        crate::secrets::Backend::Dotenv => {
            crate::dotenv::upsert_user_env(ENV_TOKEN, token)?;
        }
    }
    std::env::set_var(ENV_TOKEN, token);
    // Best-effort: remove the legacy plaintext file so users migrating
    // from earlier builds end up with a single source of truth.
    let _ = clear_legacy_file();
    Ok(())
}

/// Remove the token from the active backend AND the legacy file.
/// Idempotent. Also unsets the in-process env var so the next CLI
/// call doesn't see a stale value.
pub fn clear_token() -> crate::error::Result<()> {
    let backend = crate::secrets::get_backend().unwrap_or(crate::secrets::Backend::Keychain);
    match backend {
        crate::secrets::Backend::Keychain => {
            let _ = crate::secrets::set(KEYCHAIN_KEY, "");
        }
        crate::secrets::Backend::Dotenv => {
            let _ = crate::dotenv::upsert_user_env(ENV_TOKEN, "");
        }
    }
    std::env::remove_var(ENV_TOKEN);
    let _ = clear_legacy_file();
    Ok(())
}

/// Whether the active secrets backend can durably persist a token.
/// Mirrors `remote_agent::keychain_writable` so the UI's "disabled
/// because nothing writable" branch behaves the same on both.
pub fn token_writable() -> bool {
    matches!(
        crate::secrets::get_backend(),
        Some(crate::secrets::Backend::Keychain) | Some(crate::secrets::Backend::Dotenv) | None
    )
}

fn legacy_file_path() -> std::path::PathBuf {
    crate::util::home_dir()
        .map(|h| h.join(".config").join("thclaws"))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("cloud-token")
}

fn legacy_file_token() -> Option<String> {
    std::fs::read_to_string(legacy_file_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn clear_legacy_file() -> std::io::Result<()> {
    let p = legacy_file_path();
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

#[cfg(test)]
mod cli_token_tests {
    #[test]
    fn only_a_thc_token_looks_like_a_cli_token() {
        use super::looks_like_cli_token as ok;
        assert!(ok("thc_AbCdEfGhIjKlMnOp"));
        assert!(ok("  thc_AbCdEf  "), "surrounding whitespace is trimmed");
        assert!(!ok("thc_"), "prefix alone");
        assert!(!ok("gw_v1_abc"), "a gateway key");
        assert!(!ok("sk-ant-api03-xyz"), "a provider key");
        assert!(!ok("thc_abc def"), "two things pasted together");
        assert!(!ok(""));
    }
}
