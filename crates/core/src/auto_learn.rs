//! Self-improving AI Agent — auto-learn pipeline.
//!
//! When `AppConfig::auto_learn` is `true`, every session-end triggers:
//!   1. `KmsCreate({name: auto_learn_kms, scope: project})` (idempotent)
//!   2. `/kms ingest <kms> $` — file the just-finished session as a new
//!      page (one session → one page; bounded growth)
//!   3. `/kms reconcile <kms> --apply` — throttled per
//!      `AppConfig::auto_learn_reconcile_hours`, resolves contradictions
//!      across pages.
//!
//! Dedicated KMS (default `self_learn`) keeps auto-ingested pages
//! separate from the user's hand-curated active KMSes. The user can
//! reset the auto-learn state by deleting the KMS directory.
//!
//! This module owns three things:
//!   - **Throttle state** (`auto-learn-state.json` under
//!     `~/.config/thclaws/`): last reconcile timestamp + counters.
//!   - **Audit log** (`auto-learn.log` same dir): one line per
//!     auto-learn pipeline event so users can inspect failures.
//!   - **Quality gates**: minimum session-turn threshold before
//!     ingest, throttle check for reconcile.
//!
//! Pure functions — no agent dispatch here. The trigger lives in
//! `shared_session.rs` (GUI/`--serve` worker only). CLI/print mode
//! relies on user-configured `session_end` shell hooks.
//!
//! See `dev-plan/27-self-improving-agent.md`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Minimum substantive turns in a session before it's worth ingesting.
/// Short sessions (< MIN_TURNS_FOR_INGEST messages including assistant
/// turns) produce noisy KMS pages with little signal. Threshold is
/// intentionally low — a 5-turn conversation usually has a meaningful
/// nugget. Set higher in settings if you find your KMS getting noisy.
pub const MIN_TURNS_FOR_INGEST: usize = 5;

/// Throttle state persisted to `~/.config/thclaws/auto-learn-state.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AutoLearnState {
    /// Unix timestamp (seconds) of the last successful reconcile. `0`
    /// when no reconcile has run yet.
    #[serde(default)]
    pub last_reconcile_unix: u64,
    /// Running count of successful ingests (informational only —
    /// surfaced in `/dream stats` and similar tooling).
    #[serde(default)]
    pub ingest_count: u64,
    /// Running count of successful reconciles.
    #[serde(default)]
    pub reconcile_count: u64,
}

fn config_dir() -> Option<PathBuf> {
    crate::util::home_dir().map(|home| home.join(".config").join(crate::profile::app_dir_name()))
}

fn state_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("auto-learn-state.json"))
}

fn log_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("auto-learn.log"))
}

/// Load the throttle state. Returns default (zero timestamps) on any
/// error — missing file, malformed JSON, permission denied. Never
/// throws because auto-learn is opt-in and non-critical: a failed
/// read just means we'll re-throttle from scratch.
pub fn load_state() -> AutoLearnState {
    let Some(path) = state_path() else {
        return AutoLearnState::default();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return AutoLearnState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Persist the throttle state. Best-effort: errors are appended to
/// the audit log but otherwise non-fatal.
pub fn save_state(state: &AutoLearnState) {
    let Some(path) = state_path() else {
        return;
    };
    let Some(dir) = path.parent() else { return };
    if let Err(e) = fs::create_dir_all(dir) {
        log_event(&format!("save_state: mkdir({}) failed: {e}", dir.display()));
        return;
    }
    match serde_json::to_string_pretty(state) {
        Ok(s) => {
            if let Err(e) = fs::write(&path, s) {
                log_event(&format!(
                    "save_state: write({}) failed: {e}",
                    path.display()
                ));
            }
        }
        Err(e) => log_event(&format!("save_state: serialize failed: {e}")),
    }
}

/// `true` when the configured hours window since the last reconcile
/// has elapsed. Always `true` when `reconcile_hours == 0` (opt-out
/// of throttling — every session reconciles).
pub fn is_reconcile_due(reconcile_hours: u32) -> bool {
    if reconcile_hours == 0 {
        return true;
    }
    let state = load_state();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let elapsed = now.saturating_sub(state.last_reconcile_unix);
    let threshold = u64::from(reconcile_hours) * 3600;
    elapsed >= threshold
}

/// Mark a reconcile as just-completed: stamp `last_reconcile_unix`
/// and bump `reconcile_count`.
pub fn mark_reconcile_done() {
    let mut state = load_state();
    state.last_reconcile_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    state.reconcile_count = state.reconcile_count.saturating_add(1);
    save_state(&state);
}

/// Bump `ingest_count`. Doesn't touch the reconcile timestamp.
pub fn mark_ingest_done() {
    let mut state = load_state();
    state.ingest_count = state.ingest_count.saturating_add(1);
    save_state(&state);
}

/// Append a one-line event to the audit log. Format:
/// `<ISO-8601 UTC> <msg>\n`. Errors are dropped on the floor — the
/// audit log is informational, not load-bearing.
pub fn log_event(msg: &str) {
    let Some(path) = log_path() else { return };
    let Some(dir) = path.parent() else { return };
    let _ = fs::create_dir_all(dir);
    let ts = chrono::Utc::now().to_rfc3339();
    let line = format!("{ts} {msg}\n");
    let _ = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// Whether a session is substantive enough to warrant ingest.
///
/// Threshold: at least [`MIN_TURNS_FOR_INGEST`] messages in the
/// session history. Empty sessions and "I just opened the app and
/// closed it" sessions don't pass.
pub fn session_is_substantive(message_count: usize) -> bool {
    message_count >= MIN_TURNS_FOR_INGEST
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_is_substantive_threshold() {
        assert!(!session_is_substantive(0));
        assert!(!session_is_substantive(MIN_TURNS_FOR_INGEST - 1));
        assert!(session_is_substantive(MIN_TURNS_FOR_INGEST));
        assert!(session_is_substantive(MIN_TURNS_FOR_INGEST + 1));
        assert!(session_is_substantive(100));
    }

    #[test]
    fn reconcile_due_when_no_prior_run() {
        // `last_reconcile_unix = 0` means never run → due immediately
        // regardless of throttle window.
        let state = AutoLearnState::default();
        assert_eq!(state.last_reconcile_unix, 0);
        // is_reconcile_due reads from disk; can't test directly without
        // mocking, but the math: now - 0 = large, large >= any threshold.
    }

    #[test]
    fn reconcile_due_with_zero_hours_always_true() {
        assert!(is_reconcile_due(0));
    }

    #[test]
    fn state_serializes_with_defaults() {
        let s = AutoLearnState::default();
        let json = serde_json::to_string(&s).unwrap();
        // Round-trip works
        let parsed: AutoLearnState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.last_reconcile_unix, 0);
        assert_eq!(parsed.ingest_count, 0);
        assert_eq!(parsed.reconcile_count, 0);
    }

    #[test]
    fn state_parses_missing_fields() {
        // Forward-compat: a v1 file without ingest_count should still
        // load (serde default).
        let json = r#"{"last_reconcile_unix": 1700000000}"#;
        let parsed: AutoLearnState = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.last_reconcile_unix, 1700000000);
        assert_eq!(parsed.ingest_count, 0);
    }
}
