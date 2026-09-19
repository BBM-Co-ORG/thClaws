//! HMAC-based user identity verification for multi-tenant `--serve`.
//!
//! The cloud routing layer (dev-plan/34) attaches three headers on
//! every WebSocket upgrade:
//!
//! - `X-Thclaws-User: <user_id>`
//! - `X-Thclaws-User-Ts: <unix_seconds>`
//! - `X-Thclaws-User-Proof: <hex-encoded HMAC-SHA256(secret, "<user_id>:<unix_seconds>")>`
//!
//! The pod verifies the proof against a shared HMAC secret (loaded
//! from `THCLAWS_CLOUD_HMAC_SECRET` env or the `--multi-tenant-secret`
//! CLI flag). Timestamps must be within ±5 minutes of the pod's
//! clock to prevent replay; comparison is constant-time to prevent
//! timing oracles.
//!
//! Hand-rolled HMAC-SHA256 here rather than pulling the `hmac` crate
//! — implementation is ~30 lines, well-understood, single user, no
//! API churn risk.

use sha2::{Digest, Sha256};
use std::fmt;

/// Maximum acceptable clock skew between the cloud routing layer and
/// the pod. Five minutes covers reasonable NTP drift; tighter would
/// catch more replay attempts but reject legitimate traffic on
/// poorly-synced hosts.
pub const MAX_TIMESTAMP_SKEW_SECS: i64 = 300;

/// Max length of an authenticated user id. Caps at 64 bytes so the
/// id fits cleanly in a file-path segment and in HTTP header value
/// length limits. dev-plan/34 cloud-control-plane should mint ids
/// well under this (UUIDs or 16-char slugs are typical).
pub const MAX_USER_ID_LEN: usize = 64;

/// Authenticated user identity. Constructed only via
/// [`verify_user_header`] — the type itself enforces no validation
/// invariants, but every reachable path through the public API
/// produces one only after HMAC verification, so callers can treat
/// it as a trusted identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UserId(String);

impl UserId {
    /// Test-only constructor that skips HMAC. Real code paths must
    /// go through [`verify_user_header`].
    #[cfg(test)]
    pub fn new_for_test(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Failure modes for HMAC verification. Distinct variants so the
/// caller can choose what to log and what to surface (timestamp
/// drift typically logs warn + 401; signature mismatch logs error
/// + 401 + rate-limit; malformed id 400).
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("X-Thclaws-User missing or empty")]
    UserMissing,
    #[error("X-Thclaws-User-Ts missing or unparseable")]
    TimestampMissing,
    #[error("X-Thclaws-User-Proof missing or empty")]
    ProofMissing,
    #[error("user id '{0}' exceeds {MAX_USER_ID_LEN}-byte cap")]
    UserIdTooLong(String),
    #[error("user id '{0}' contains characters outside [A-Za-z0-9_-]")]
    UserIdInvalidChars(String),
    #[error("clock skew {0}s exceeds the {MAX_TIMESTAMP_SKEW_SECS}s window")]
    ClockSkew(i64),
    #[error("HMAC proof is not valid hex")]
    ProofNotHex,
    #[error("HMAC verification failed")]
    HmacMismatch,
    #[error("X-Thclaws-User-Sig missing (pod requires the Ed25519 proof)")]
    SigMissing,
    #[error("X-Thclaws-User-Sig is not a valid 64-byte hex signature")]
    SigMalformed,
    #[error("Ed25519 signature verification failed")]
    SigMismatch,
}

/// How the pod authenticates identity headers. `Ed25519` is dev-plan/45
/// item B: the API holds the per-workspace signing key and the pod gets
/// only this verifying key (`THCLAWS_CLOUD_PUBKEY`), so nothing readable
/// inside the pod can forge a co-tenant's proof. When the pubkey is
/// configured the signature is REQUIRED — no HMAC downgrade (the HMAC
/// secret sits in the same pod env an attacker can read).
#[derive(Clone)]
pub enum IdentityVerifier {
    Hmac { secret: Vec<u8> },
    Ed25519 { key: ed25519_dalek::VerifyingKey },
}

impl fmt::Debug for IdentityVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hmac { .. } => f.write_str("IdentityVerifier::Hmac(<redacted>)"),
            Self::Ed25519 { .. } => f.write_str("IdentityVerifier::Ed25519(pubkey)"),
        }
    }
}

impl IdentityVerifier {
    /// Build from the pod env: `THCLAWS_CLOUD_PUBKEY` (hex, 32 bytes)
    /// wins; else the HMAC secret. `None` if the pubkey is set but
    /// malformed (fail-closed — better to refuse startup than silently
    /// downgrade to the forgeable symmetric proof).
    pub fn from_secret_and_pubkey(secret: &[u8], pubkey_hex: Option<&str>) -> Option<Self> {
        match pubkey_hex.map(str::trim).filter(|s| !s.is_empty()) {
            Some(hex) => {
                let bytes = hex_decode(hex)?;
                let arr: [u8; 32] = bytes.try_into().ok()?;
                let key = ed25519_dalek::VerifyingKey::from_bytes(&arr).ok()?;
                Some(Self::Ed25519 { key })
            }
            None => Some(Self::Hmac {
                secret: secret.to_vec(),
            }),
        }
    }
}

/// Verify all three cloud-routing headers and produce a trusted
/// [`UserId`] on success (symmetric HMAC path — see [`verify_identity`]
/// for the pubkey-aware entry point).
pub fn verify_user_header(
    user_id_header: &str,
    timestamp_secs_header: &str,
    proof_hex_header: &str,
    secret: &[u8],
    now_secs: u64,
) -> Result<UserId, AuthError> {
    verify_identity(
        user_id_header,
        timestamp_secs_header,
        proof_hex_header,
        None,
        &IdentityVerifier::Hmac {
            secret: secret.to_vec(),
        },
        now_secs,
    )
}

/// Verify the cloud-routing identity headers against the pod's
/// configured verifier. With [`IdentityVerifier::Ed25519`] the
/// `X-Thclaws-User-Sig` header (`sig_hex_header`) is REQUIRED and the
/// legacy HMAC proof is ignored; with `Hmac` the proof header is
/// checked as before.
pub fn verify_identity(
    user_id_header: &str,
    timestamp_secs_header: &str,
    proof_hex_header: &str,
    sig_hex_header: Option<&str>,
    verifier: &IdentityVerifier,
    now_secs: u64,
) -> Result<UserId, AuthError> {
    let user_id = user_id_header.trim();
    if user_id.is_empty() {
        return Err(AuthError::UserMissing);
    }
    if user_id.len() > MAX_USER_ID_LEN {
        return Err(AuthError::UserIdTooLong(user_id.to_string()));
    }
    if !user_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(AuthError::UserIdInvalidChars(user_id.to_string()));
    }

    let timestamp_secs: u64 = timestamp_secs_header
        .trim()
        .parse()
        .map_err(|_| AuthError::TimestampMissing)?;
    let skew = (now_secs as i64) - (timestamp_secs as i64);
    if skew.abs() > MAX_TIMESTAMP_SKEW_SECS {
        return Err(AuthError::ClockSkew(skew));
    }

    let message = format!("{user_id}:{timestamp_secs}");
    match verifier {
        IdentityVerifier::Hmac { secret } => {
            let proof_hex = proof_hex_header.trim();
            if proof_hex.is_empty() {
                return Err(AuthError::ProofMissing);
            }
            let provided = hex_decode(proof_hex).ok_or(AuthError::ProofNotHex)?;
            let expected = hmac_sha256(secret, message.as_bytes());
            if !constant_time_eq(&provided, &expected) {
                return Err(AuthError::HmacMismatch);
            }
        }
        IdentityVerifier::Ed25519 { key } => {
            let sig_hex = sig_hex_header.map(str::trim).unwrap_or("");
            if sig_hex.is_empty() {
                return Err(AuthError::SigMissing);
            }
            let bytes = hex_decode(sig_hex).ok_or(AuthError::SigMalformed)?;
            let sig = ed25519_dalek::Signature::from_slice(&bytes)
                .map_err(|_| AuthError::SigMalformed)?;
            use ed25519_dalek::Verifier;
            key.verify(message.as_bytes(), &sig)
                .map_err(|_| AuthError::SigMismatch)?;
        }
    }

    Ok(UserId(user_id.to_string()))
}

/// HMAC-SHA256 per RFC 2104. Block size for SHA-256 is 64 bytes.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut key_block = [0u8; BLOCK_SIZE];

    if key.len() > BLOCK_SIZE {
        // Long key: hash it first, then zero-pad.
        let hashed = Sha256::digest(key);
        key_block[..32].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut o_pad = [0u8; BLOCK_SIZE];
    let mut i_pad = [0u8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        o_pad[i] = key_block[i] ^ 0x5c;
        i_pad[i] = key_block[i] ^ 0x36;
    }

    let mut inner = Sha256::new();
    inner.update(i_pad);
    inner.update(message);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(o_pad);
    outer.update(inner_hash);
    let outer_hash = outer.finalize();

    let mut out = [0u8; 32];
    out.copy_from_slice(&outer_hash);
    out
}

/// Constant-time byte comparison. Returns false if lengths differ
/// (length leak is acceptable — it's not the secret).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Decode lowercase hex to bytes. Returns `None` on any non-hex
/// character or odd length.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut chars = s.chars();
    while let (Some(hi), Some(lo)) = (chars.next(), chars.next()) {
        out.push(hex_nibble(hi)? << 4 | hex_nibble(lo)?);
    }
    Some(out)
}

fn hex_nibble(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some(c as u8 - b'0'),
        'a'..='f' => Some(c as u8 - b'a' + 10),
        'A'..='F' => Some(c as u8 - b'A' + 10),
        _ => None,
    }
}

/// Convenience for tests / cloud-side proof generation: hex-encode
/// raw bytes lowercase.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(hex_char_low(b >> 4));
        out.push(hex_char_low(b & 0x0f));
    }
    out
}

fn hex_char_low(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!("0..=15 only"),
    }
}

/// Test/utility: produce a proof header value the way the cloud
/// would, so callers (and tests) can construct valid requests.
pub fn sign_user_header(user_id: &str, timestamp_secs: u64, secret: &[u8]) -> String {
    let message = format!("{user_id}:{timestamp_secs}");
    let mac = hmac_sha256(secret, message.as_bytes());
    hex_encode(&mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"super-secret-shared-with-cloud-only";

    #[test]
    fn round_trip_sign_then_verify() {
        let now = 1_700_000_000u64;
        let user = "usr_abc123";
        let proof = sign_user_header(user, now, SECRET);
        let result = verify_user_header(user, &now.to_string(), &proof, SECRET, now).unwrap();
        assert_eq!(result.as_str(), user);
    }

    #[test]
    fn rejects_forged_proof() {
        let now = 1_700_000_000u64;
        let user = "usr_abc123";
        let proof = sign_user_header(user, now, SECRET);
        // Flip one bit.
        let mut bad = proof.clone();
        bad.replace_range(0..1, "f");
        let err = verify_user_header(user, &now.to_string(), &bad, SECRET, now).unwrap_err();
        assert!(matches!(err, AuthError::HmacMismatch));
    }

    #[test]
    fn rejects_wrong_secret() {
        let now = 1_700_000_000u64;
        let user = "usr_abc123";
        let proof = sign_user_header(user, now, SECRET);
        let err = verify_user_header(user, &now.to_string(), &proof, b"other", now).unwrap_err();
        assert!(matches!(err, AuthError::HmacMismatch));
    }

    #[test]
    fn rejects_user_id_mismatch() {
        // Cloud signed for user A but the header claims user B —
        // HMAC inputs differ, signature fails.
        let now = 1_700_000_000u64;
        let proof = sign_user_header("alice", now, SECRET);
        let err = verify_user_header("bob", &now.to_string(), &proof, SECRET, now).unwrap_err();
        assert!(matches!(err, AuthError::HmacMismatch));
    }

    #[test]
    fn rejects_replay_past_skew_window() {
        let signed_at = 1_700_000_000u64;
        let now = signed_at + (MAX_TIMESTAMP_SKEW_SECS as u64) + 1;
        let user = "usr_replay";
        let proof = sign_user_header(user, signed_at, SECRET);
        let err =
            verify_user_header(user, &signed_at.to_string(), &proof, SECRET, now).unwrap_err();
        assert!(matches!(err, AuthError::ClockSkew(_)));
    }

    #[test]
    fn rejects_future_timestamp_past_skew_window() {
        // Cloud's clock ahead of pod's by > 5min — also rejected.
        let signed_at = 1_700_000_000u64 + (MAX_TIMESTAMP_SKEW_SECS as u64) + 1;
        let now = 1_700_000_000u64;
        let user = "usr_skew";
        let proof = sign_user_header(user, signed_at, SECRET);
        let err =
            verify_user_header(user, &signed_at.to_string(), &proof, SECRET, now).unwrap_err();
        assert!(matches!(err, AuthError::ClockSkew(_)));
    }

    #[test]
    fn accepts_timestamp_within_skew_window() {
        let signed_at = 1_700_000_000u64;
        // 4 minutes ahead — within window.
        let now = signed_at + 240;
        let user = "usr_skew_ok";
        let proof = sign_user_header(user, signed_at, SECRET);
        assert!(verify_user_header(user, &signed_at.to_string(), &proof, SECRET, now).is_ok());
    }

    #[test]
    fn rejects_empty_user() {
        let now = 1_700_000_000u64;
        let err = verify_user_header("", &now.to_string(), "00", SECRET, now).unwrap_err();
        assert!(matches!(err, AuthError::UserMissing));
    }

    #[test]
    fn rejects_user_id_too_long() {
        let now = 1_700_000_000u64;
        let big = "a".repeat(MAX_USER_ID_LEN + 1);
        let err = verify_user_header(&big, &now.to_string(), "00", SECRET, now).unwrap_err();
        assert!(matches!(err, AuthError::UserIdTooLong(_)));
    }

    #[test]
    fn rejects_user_id_with_bad_chars() {
        let now = 1_700_000_000u64;
        for bad in ["abc/def", "../etc", "a b c", "abc.def", "abc:def"] {
            let err = verify_user_header(bad, &now.to_string(), "00", SECRET, now).unwrap_err();
            assert!(
                matches!(err, AuthError::UserIdInvalidChars(_)),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_non_hex_proof() {
        let now = 1_700_000_000u64;
        let err = verify_user_header("usr", &now.to_string(), "not-hex!", SECRET, now).unwrap_err();
        assert!(matches!(err, AuthError::ProofNotHex));
    }

    #[test]
    fn rejects_unparseable_timestamp() {
        let now = 1_700_000_000u64;
        let err = verify_user_header("usr", "not-a-number", "00", SECRET, now).unwrap_err();
        assert!(matches!(err, AuthError::TimestampMissing));
    }

    #[test]
    fn hmac_sha256_matches_known_rfc4231_test_case() {
        // RFC 4231 Test Case 1: Key = 20 bytes of 0x0b, Data = "Hi There".
        let key = [0x0b; 20];
        let mac = hmac_sha256(&key, b"Hi There");
        let hex = hex_encode(&mac);
        // Expected from RFC 4231:
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_sha256_handles_long_key() {
        // RFC 4231 Test Case 6: Key = 131 bytes of 0xaa
        // (longer than block size, gets pre-hashed).
        let key = [0xaa; 131];
        let mac = hmac_sha256(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        let hex = hex_encode(&mac);
        assert_eq!(
            hex,
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn hex_decode_round_trip() {
        let bytes = b"\x00\x01\xff\xfe\xab\xcd";
        assert_eq!(hex_decode(&hex_encode(bytes)).unwrap(), bytes);
    }

    #[test]
    fn hex_decode_rejects_odd_length() {
        assert!(hex_decode("abc").is_none());
    }

    #[test]
    fn hex_decode_rejects_non_hex_chars() {
        assert!(hex_decode("zz").is_none());
    }

    #[test]
    fn constant_time_eq_lengths_differ() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    fn ed25519_pair() -> (ed25519_dalek::SigningKey, IdentityVerifier) {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let v = IdentityVerifier::Ed25519 {
            key: sk.verifying_key(),
        };
        (sk, v)
    }

    fn ed25519_sign(sk: &ed25519_dalek::SigningKey, user: &str, ts: u64) -> String {
        use ed25519_dalek::Signer;
        hex_encode(&sk.sign(format!("{user}:{ts}").as_bytes()).to_bytes())
    }

    #[test]
    fn ed25519_round_trip_sign_then_verify() {
        let (sk, v) = ed25519_pair();
        let now = 1_700_000_000u64;
        let sig = ed25519_sign(&sk, "usr_abc123", now);
        let got = verify_identity("usr_abc123", &now.to_string(), "", Some(&sig), &v, now).unwrap();
        assert_eq!(got.as_str(), "usr_abc123");
    }

    #[test]
    fn ed25519_requires_sig_and_ignores_hmac_proof() {
        // A valid HMAC proof must NOT satisfy a pubkey-configured pod —
        // the HMAC secret lives in the same env an attacker can read,
        // so accepting it would be a downgrade hole.
        let (_sk, v) = ed25519_pair();
        let now = 1_700_000_000u64;
        let hmac_proof = sign_user_header("usr_abc123", now, SECRET);
        let err = verify_identity("usr_abc123", &now.to_string(), &hmac_proof, None, &v, now)
            .unwrap_err();
        assert!(matches!(err, AuthError::SigMissing));
    }

    #[test]
    fn ed25519_rejects_forged_or_cross_user_sig() {
        let (sk, v) = ed25519_pair();
        let now = 1_700_000_000u64;
        // Signature for alice presented as bob.
        let sig = ed25519_sign(&sk, "alice", now);
        let err = verify_identity("bob", &now.to_string(), "", Some(&sig), &v, now).unwrap_err();
        assert!(matches!(err, AuthError::SigMismatch));
        // Wrong key entirely.
        let other = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let sig = ed25519_sign(&other, "alice", now);
        let err = verify_identity("alice", &now.to_string(), "", Some(&sig), &v, now).unwrap_err();
        assert!(matches!(err, AuthError::SigMismatch));
        // Garbage sig.
        let err = verify_identity("alice", &now.to_string(), "", Some("zz"), &v, now).unwrap_err();
        assert!(matches!(err, AuthError::SigMalformed));
    }

    #[test]
    fn ed25519_cross_verifies_python_cryptography_signature() {
        // Golden vectors produced by the API side
        // (auth/multiuser.py::derive_workspace_ed25519 with master
        // "master", workspace id "ws1", message "alice:1700000000") —
        // pins the cross-language wire contract.
        let pub_hex = "a1c65ea8060f044cd5039d0cc1587adaa08eacadf375616161053d959a66e392";
        let sig_hex = "7f3a87e3137994cf095150d0e5134ad90f8bf2e8e73f15f860ed1ab97e789ec8cdff1e547ab545291c03cc8b317cf684422c1cb4a3d59d96539b117253eb530b";
        let v = IdentityVerifier::from_secret_and_pubkey(b"", Some(pub_hex)).unwrap();
        let now = 1_700_000_000u64;
        let got = verify_identity("alice", &now.to_string(), "", Some(sig_hex), &v, now).unwrap();
        assert_eq!(got.as_str(), "alice");
        // Same sig for a different user must fail.
        assert!(verify_identity("bob", &now.to_string(), "", Some(sig_hex), &v, now).is_err());
    }

    #[test]
    fn verifier_from_env_prefers_pubkey_and_fails_closed_on_garbage() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let pub_hex = hex_encode(sk.verifying_key().as_bytes());
        assert!(matches!(
            IdentityVerifier::from_secret_and_pubkey(SECRET, Some(&pub_hex)),
            Some(IdentityVerifier::Ed25519 { .. })
        ));
        assert!(matches!(
            IdentityVerifier::from_secret_and_pubkey(SECRET, None),
            Some(IdentityVerifier::Hmac { .. })
        ));
        assert!(matches!(
            IdentityVerifier::from_secret_and_pubkey(SECRET, Some("")),
            Some(IdentityVerifier::Hmac { .. })
        ));
        // Malformed pubkey must NOT silently downgrade to HMAC.
        assert!(IdentityVerifier::from_secret_and_pubkey(SECRET, Some("nothex")).is_none());
        assert!(IdentityVerifier::from_secret_and_pubkey(SECRET, Some("aabb")).is_none());
    }
}
