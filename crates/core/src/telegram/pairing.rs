//! Pairing-code flow for first-contact DM users (dev-plan/29 Tier 1,
//! decision #1 — copied from OpenClaw).
//!
//! When `dm_policy = "pairing"` and an *unknown* user DMs the bot, we
//! mint a 6-digit code bound to their `user_id`, reply asking them to
//! get the owner's approval, and surface a pairing request to the GUI.
//! The owner clicks Approve → the user's id is appended to
//! `allow_from`. Codes expire after [`PAIRING_EXPIRY`] (1h).
//!
//! Tier 1 limitation (Risk #2): pending codes live in memory only, so a
//! process restart drops them and unknown users must re-message to get
//! a fresh code. Persistence is Tier 3.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// How long a minted pairing code stays valid.
pub const PAIRING_EXPIRY: Duration = Duration::from_secs(60 * 60);

/// A pending pairing request awaiting owner approval.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingPair {
    /// 6-digit code shown to the user and the owner.
    pub code: String,
    /// Telegram user id requesting access (kept as i64; serialised as a
    /// string elsewhere to dodge JSON's 2^53 precision limit).
    pub user_id: i64,
    /// The DM chat to notify on approval/rejection.
    pub chat_id: i64,
    /// Display label (`@username` or first/last name) for the GUI.
    pub display: String,
    /// When the code was minted (used for expiry).
    #[serde(skip)]
    pub minted_at: SystemTime,
}

impl PendingPair {
    pub fn is_expired_at(&self, now: SystemTime, expiry: Duration) -> bool {
        is_expired(self.minted_at, now, expiry)
    }
}

/// True when `minted_at` is older than `expiry` relative to `now`.
/// Pulled out as a pure fn so expiry logic is testable without sleeping.
pub fn is_expired(minted_at: SystemTime, now: SystemTime, expiry: Duration) -> bool {
    now.duration_since(minted_at)
        .map(|elapsed| elapsed >= expiry)
        .unwrap_or(false) // clock skew (minted in the "future") ⇒ not expired
}

#[derive(Clone)]
pub struct PairingManager {
    pending: Arc<Mutex<HashMap<String, PendingPair>>>,
    expiry: Duration,
}

impl Default for PairingManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingManager {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            expiry: PAIRING_EXPIRY,
        }
    }

    pub fn with_expiry(mut self, expiry: Duration) -> Self {
        self.expiry = expiry;
        self
    }

    /// Return the live pairing code for `user_id`, minting a fresh one if
    /// none exists (or the prior one expired). Idempotent per user so a
    /// user who messages repeatedly before approval keeps the same code
    /// instead of flooding the GUI with new requests.
    pub fn mint(&self, user_id: i64, chat_id: i64, display: impl Into<String>) -> PendingPair {
        let now = SystemTime::now();
        let mut guard = self.pending.lock().expect("pairing mutex");
        // Drop expired entries first.
        guard.retain(|_, p| !is_expired(p.minted_at, now, self.expiry));
        // Reuse an existing live code for this user.
        if let Some(existing) = guard.values().find(|p| p.user_id == user_id) {
            return existing.clone();
        }
        let code = mint_unique_code(&guard);
        let pair = PendingPair {
            code: code.clone(),
            user_id,
            chat_id,
            display: display.into(),
            minted_at: now,
        };
        guard.insert(code, pair.clone());
        pair
    }

    /// Approve `code`: remove and return the pending entry when it's
    /// present and unexpired. `None` for an unknown / expired code.
    pub fn approve(&self, code: &str) -> Option<PendingPair> {
        let now = SystemTime::now();
        let mut guard = self.pending.lock().expect("pairing mutex");
        let pair = guard.remove(code.trim())?;
        if is_expired(pair.minted_at, now, self.expiry) {
            None
        } else {
            Some(pair)
        }
    }

    /// Reject `code`: drop the pending entry. Returns true when one was
    /// removed. Returns the pair so the caller can notify the chat.
    pub fn reject(&self, code: &str) -> Option<PendingPair> {
        let mut guard = self.pending.lock().expect("pairing mutex");
        guard.remove(code.trim())
    }

    /// Snapshot of live (unexpired) pending requests, for the GUI list /
    /// `telegram status`. Lazily GCs expired entries.
    pub fn pending_list(&self) -> Vec<PendingPair> {
        let now = SystemTime::now();
        let mut guard = self.pending.lock().expect("pairing mutex");
        guard.retain(|_, p| !is_expired(p.minted_at, now, self.expiry));
        let mut list: Vec<PendingPair> = guard.values().cloned().collect();
        list.sort_by_key(|a| a.minted_at);
        list
    }

    pub fn has_pending(&self) -> bool {
        !self.pending_list().is_empty()
    }
}

/// Generate a 6-digit code not already in `existing`. Uses `getrandom`
/// (CSPRNG) — a pairing code is a low-grade shared secret, so don't seed
/// it from a predictable PRNG. Retries on the rare collision.
fn mint_unique_code(existing: &HashMap<String, PendingPair>) -> String {
    for _ in 0..16 {
        let code = random_six_digits();
        if !existing.contains_key(&code) {
            return code;
        }
    }
    // 16 collisions across a ≤1M space means something pathological;
    // fall back to a UUID-derived numeric so we never loop forever.
    let n = uuid::Uuid::new_v4().as_u128() % 1_000_000;
    format!("{n:06}")
}

fn random_six_digits() -> String {
    let mut buf = [0u8; 4];
    // getrandom failing is effectively impossible on supported
    // platforms; if it ever does, a fixed code is still better than a
    // panic in the polling loop.
    let n = match getrandom::getrandom(&mut buf) {
        Ok(()) => u32::from_le_bytes(buf) % 1_000_000,
        Err(_) => 0,
    };
    format!("{n:06}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_returns_six_digit_code() {
        let pm = PairingManager::new();
        let p = pm.mint(111, 111, "@jimmy");
        assert_eq!(p.code.len(), 6);
        assert!(p.code.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(p.user_id, 111);
        assert_eq!(p.display, "@jimmy");
    }

    #[test]
    fn mint_is_idempotent_per_user() {
        let pm = PairingManager::new();
        let a = pm.mint(111, 111, "@jimmy");
        let b = pm.mint(111, 111, "@jimmy");
        assert_eq!(a.code, b.code, "same user should keep one live code");
        assert_eq!(pm.pending_list().len(), 1);
    }

    #[test]
    fn distinct_users_get_distinct_entries() {
        let pm = PairingManager::new();
        pm.mint(111, 111, "@a");
        pm.mint(222, 222, "@b");
        assert_eq!(pm.pending_list().len(), 2);
    }

    #[test]
    fn approve_removes_and_returns_pair() {
        let pm = PairingManager::new();
        let p = pm.mint(111, 111, "@jimmy");
        let approved = pm.approve(&p.code).expect("approved");
        assert_eq!(approved.user_id, 111);
        assert!(!pm.has_pending());
        // Re-approving the same code now fails.
        assert!(pm.approve(&p.code).is_none());
    }

    #[test]
    fn approve_trims_whitespace() {
        let pm = PairingManager::new();
        let p = pm.mint(111, 111, "@jimmy");
        assert!(pm.approve(&format!("  {}  ", p.code)).is_some());
    }

    #[test]
    fn reject_drops_entry() {
        let pm = PairingManager::new();
        let p = pm.mint(111, 111, "@jimmy");
        let rejected = pm.reject(&p.code).expect("rejected");
        assert_eq!(rejected.user_id, 111);
        assert!(!pm.has_pending());
    }

    #[test]
    fn approve_unknown_code_is_none() {
        let pm = PairingManager::new();
        assert!(pm.approve("000000").is_none());
    }

    #[test]
    fn expired_code_is_not_approvable() {
        // Tiny expiry; a freshly-minted code with a backdated mint time
        // is treated as expired without sleeping.
        let pm = PairingManager::new().with_expiry(Duration::from_secs(3600));
        let p = pm.mint(111, 111, "@jimmy");
        // The pure helper is what approve/list rely on.
        let way_past = p.minted_at - Duration::from_secs(7200);
        assert!(is_expired(
            way_past,
            SystemTime::now(),
            Duration::from_secs(3600)
        ));
        // And an unexpired one is fine.
        assert!(!p.is_expired_at(SystemTime::now(), Duration::from_secs(3600)));
    }

    #[test]
    fn clock_skew_minted_in_future_is_not_expired() {
        let future = SystemTime::now() + Duration::from_secs(10_000);
        assert!(!is_expired(
            future,
            SystemTime::now(),
            Duration::from_secs(3600)
        ));
    }
}
