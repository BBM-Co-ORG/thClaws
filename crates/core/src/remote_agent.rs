//! Resolve the `/deploy` slash command's target URL + bearer token.
//!
//! - URL: lives in `settings.json` as `remoteAgentUrl` (project >
//!   user). Non-sensitive.
//! - Token: lives in the OS keychain under
//!   `service="thclaws", account="api-keys"`, keyed
//!   `"remote-agent-token"`. Same bundle as provider API keys — one
//!   macOS ACL prompt per launch regardless of how many secrets the
//!   user has set. Env-var fallback: `$THCLAWS_REMOTE_AGENT_TOKEN`.

const KEYCHAIN_KEY: &str = "remote-agent-token";
pub const ENV_TOKEN: &str = "THCLAWS_REMOTE_AGENT_TOKEN";

/// Best-effort URL resolution. Returns the trimmed URL or `None`.
pub fn url() -> Option<String> {
    crate::config::AppConfig::load()
        .ok()
        .and_then(|c| c.remote_agent_url)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Best-effort token resolution: env first (lets CI override the
/// keychain without ceremony), then the keychain bundle.
pub fn token() -> Option<String> {
    if let Ok(t) = std::env::var(ENV_TOKEN) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    crate::secrets::get(KEYCHAIN_KEY)
}

/// Persist the bearer token. Routes through the same backend the
/// provider API keys use — keychain when the user picked it,
/// `~/.config/thclaws/.env` (as `THCLAWS_REMOTE_AGENT_TOKEN=<value>`)
/// when they picked dotenv. Mirrors `ipc::api_key_set` so both
/// surfaces stay consistent. Also pushes the value into the
/// process env so the active session picks it up without a restart.
pub fn set_token(token: &str) -> crate::error::Result<()> {
    let backend = crate::secrets::resolved_backend();
    match backend {
        crate::secrets::Backend::Keychain => {
            crate::secrets::set(KEYCHAIN_KEY, token)?;
        }
        crate::secrets::Backend::Dotenv => {
            crate::dotenv::upsert_user_env(ENV_TOKEN, token)?;
        }
    }
    std::env::set_var(ENV_TOKEN, token);
    Ok(())
}

/// Remove the bearer token from whichever backend the user picked.
/// Idempotent — no-op when the entry doesn't exist. Also removes the
/// env var from the running process so subsequent `/deploy` calls
/// don't see a stale value.
pub fn clear_token() -> crate::error::Result<()> {
    let backend = crate::secrets::resolved_backend();
    match backend {
        crate::secrets::Backend::Keychain => {
            // secrets::set with empty string is the existing
            // "clear this entry" pattern — provider api-keys use
            // the same call.
            let _ = crate::secrets::set(KEYCHAIN_KEY, "");
        }
        crate::secrets::Backend::Dotenv => {
            // Writing an empty value is the existing pattern for
            // "clear" in the dotenv-backed flow too.
            let _ = crate::dotenv::upsert_user_env(ENV_TOKEN, "");
        }
    }
    std::env::remove_var(ENV_TOKEN);
    Ok(())
}

/// Whether `set_token` will persist anywhere durable on the current
/// backend. True for keychain or dotenv (both have a place to write);
/// false only when the backend resolution fails entirely. Kept around
/// for the UI's "disabled because nothing writable" branch — which
/// is now unreachable in practice but the flag stays as a safety
/// net.
pub fn keychain_writable() -> bool {
    matches!(
        crate::secrets::get_backend(),
        Some(crate::secrets::Backend::Keychain) | Some(crate::secrets::Backend::Dotenv) | None
    )
}
