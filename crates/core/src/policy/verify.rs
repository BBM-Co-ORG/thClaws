//! Ed25519 signature verification for org policy files.
//!
//! The signed payload is a *canonicalized* JSON serialization of the
//! policy document with the `signature` field removed. Canonical form
//! means recursively-sorted object keys, no insignificant whitespace,
//! and UTF-8 encoding — produced here without an external `canonical-
//! json` dependency by walking `serde_json::Value` and emitting bytes
//! directly.
//!
//! Verification is the only operation the runtime needs. Signing lives
//! in `src/bin/policy_tool.rs` (the operator CLI) — keeping it out of
//! the main binary means a leaked private key in source code is not a
//! concern: the main binary literally has no signing code.

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::Value;

use super::error::PolicyError;
use std::path::{Path, PathBuf};

/// Pull the embedded public key (compile-time). Empty string when the
/// open-core build provides no key.
pub const EMBEDDED_PUBKEY_BASE64: &str = env!("THCLAWS_EMBEDDED_POLICY_PUBKEY");

/// Result of resolving which public key to verify against.
#[derive(Debug, Clone)]
pub enum KeySource {
    /// Compile-time embedded key. Highest trust — cannot be overridden.
    Embedded(VerifyingKey),
    /// Runtime env-var key. Used in open-core builds for testing /
    /// power-user self-locking.
    Env(VerifyingKey),
    /// File on disk at one of the conventional locations
    /// (`/etc/thclaws/policy.pub` or `~/.config/thclaws/policy.pub`).
    /// Same UX as the policy file itself — pubkey and policy live next
    /// to each other.
    File(VerifyingKey, PathBuf),
    /// No key configured anywhere. Caller must fail closed when a
    /// policy file *exists* in this state.
    None,
}

impl KeySource {
    pub fn resolve() -> Result<Self, PolicyError> {
        // 1. Compile-time embedded key — highest trust.
        if !EMBEDDED_PUBKEY_BASE64.is_empty() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(EMBEDDED_PUBKEY_BASE64)
                .map_err(|e| PolicyError::InvalidEnvKey {
                    message: format!("embedded pubkey base64 decode failed: {e}"),
                })?;
            let key = parse_pubkey(&bytes).map_err(|e| PolicyError::InvalidEnvKey {
                message: format!("embedded pubkey: {e}"),
            })?;
            return Ok(KeySource::Embedded(key));
        }
        // 2. Env var — explicit runtime override (testing / self-locking).
        if let Ok(s) = std::env::var("THCLAWS_POLICY_PUBLIC_KEY") {
            if !s.trim().is_empty() {
                let bytes = decode_text_pubkey(&s)
                    .map_err(|e| PolicyError::InvalidEnvKey { message: e })?;
                let key =
                    parse_pubkey(&bytes).map_err(|e| PolicyError::InvalidEnvKey { message: e })?;
                return Ok(KeySource::Env(key));
            }
        }
        // 3 + 4. Conventional file paths — system-wide first, then user-scoped.
        for path in pubkey_search_paths() {
            if !path.exists() {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| PolicyError::InvalidEnvKey {
                message: format!("read {path:?}: {e}"),
            })?;
            let raw = read_pubkey_bytes(&bytes).map_err(|e| PolicyError::InvalidEnvKey {
                message: format!("{path:?}: {e}"),
            })?;
            let key = parse_pubkey(&raw).map_err(|e| PolicyError::InvalidEnvKey {
                message: format!("{path:?}: {e}"),
            })?;
            return Ok(KeySource::File(key, path));
        }
        // 5. No key configured.
        Ok(KeySource::None)
    }

    pub fn key(&self) -> Option<&VerifyingKey> {
        match self {
            KeySource::Embedded(k) | KeySource::Env(k) | KeySource::File(k, _) => Some(k),
            KeySource::None => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            KeySource::Embedded(_) => "embedded (compile-time)".to_string(),
            KeySource::Env(_) => "env (THCLAWS_POLICY_PUBLIC_KEY)".to_string(),
            KeySource::File(_, p) => format!("file ({})", p.display()),
            KeySource::None => "none".to_string(),
        }
    }
}

/// Conventional public-key search paths, in priority order. System-wide
/// path beats user-scoped — same precedence pattern as the policy file
/// itself (managed deployments override per-user state).
pub fn pubkey_search_paths() -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(2);
    out.push(PathBuf::from("/etc/thclaws/policy.pub"));
    if let Some(home) = crate::util::home_dir() {
        out.push(home.join(format!(
            ".config/{}/policy.pub",
            crate::profile::app_dir_name()
        )));
    }
    out
}

/// Decode raw public-key file bytes into the canonical 32-byte form.
/// Accepts (a) raw 32-byte files written by `thclaws-policy-tool keygen`,
/// (b) base64 text, or (c) PEM-wrapped base64. Mirrors `build.rs` and
/// `decode_text_pubkey` so all three sources accept the same forms.
fn read_pubkey_bytes(raw: &[u8]) -> Result<Vec<u8>, String> {
    if raw.len() == 32 {
        return Ok(raw.to_vec());
    }
    let text = std::str::from_utf8(raw).map_err(|_| {
        format!(
            "{} bytes and not valid UTF-8 — expected 32 raw bytes or base64/PEM text",
            raw.len()
        )
    })?;
    decode_text_pubkey(text)
}

/// Verify a policy document against the resolved key source. Returns the
/// canonical bytes that were signed (useful for re-signing tooling and
/// debug introspection).
///
/// `path` is only used to label errors; the actual file IO happens in
/// the loader.
pub fn verify_policy(
    doc: &Value,
    key_source: &KeySource,
    path: &Path,
) -> Result<Vec<u8>, PolicyError> {
    let issuer = doc
        .get("issuer")
        .and_then(|v| v.as_str())
        .unwrap_or("(no issuer)")
        .to_string();
    let signature_str = doc
        .get("signature")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PolicyError::MissingSignature {
            path: path.to_path_buf(),
        })?;
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_str)
        .map_err(|e| PolicyError::MalformedSignature {
            path: path.to_path_buf(),
            message: format!("base64 decode failed: {e}"),
        })?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|e| PolicyError::MalformedSignature {
            path: path.to_path_buf(),
            message: format!("not a valid Ed25519 signature: {e}"),
        })?;

    let canonical = canonical_signed_payload(doc);
    let key = key_source
        .key()
        .ok_or_else(|| PolicyError::NoVerificationKey {
            path: path.to_path_buf(),
        })?;
    key.verify(&canonical, &signature)
        .map_err(|_| PolicyError::SignatureMismatch {
            path: path.to_path_buf(),
            issuer,
        })?;
    Ok(canonical)
}

/// Produce the canonical-JSON byte sequence that participates in
/// signing. The `signature` top-level field is excluded; everything
/// else is serialized with keys sorted recursively.
pub fn canonical_signed_payload(doc: &Value) -> Vec<u8> {
    let cloned = strip_signature(doc.clone());
    let mut out = Vec::new();
    write_canonical(&cloned, &mut out);
    out
}

fn strip_signature(mut v: Value) -> Value {
    if let Value::Object(map) = &mut v {
        map.remove("signature");
    }
    v
}

/// Walk `Value`, emitting canonical JSON bytes. Object keys are sorted
/// lexicographically (byte-wise on UTF-8). Numbers preserve their
/// `serde_json` rendering. Whitespace is omitted entirely.
fn write_canonical(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => write_canonical_string(s, out),
        Value::Array(arr) => {
            out.push(b'[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            out.push(b'{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical_string(k, out);
                out.push(b':');
                write_canonical(&map[k], out);
            }
            out.push(b'}');
        }
    }
}

fn write_canonical_string(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\u{08}' => out.extend_from_slice(b"\\b"),
            '\u{0c}' => out.extend_from_slice(b"\\f"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

fn parse_pubkey(bytes: &[u8]) -> Result<VerifyingKey, String> {
    if bytes.len() != 32 {
        return Err(format!(
            "expected 32-byte raw Ed25519 public key, got {} bytes",
            bytes.len()
        ));
    }
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "internal: 32-byte slice cast failed".to_string())?;
    VerifyingKey::from_bytes(&arr).map_err(|e| format!("invalid Ed25519 key: {e}"))
}

/// Decode a text-form public key (raw base64 or PEM-wrapped) into
/// raw bytes. Mirrors the logic in `build.rs` so the env-var path
/// accepts the same forms operators use to produce the file.
fn decode_text_pubkey(text: &str) -> Result<Vec<u8>, String> {
    let trimmed = text.trim();
    let inner: String = if trimmed.starts_with("-----BEGIN") {
        trimmed
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("")
    } else {
        trimmed.replace(['\n', '\r', ' '], "")
    };
    base64::engine::general_purpose::STANDARD
        .decode(&inner)
        .map_err(|e| format!("base64 decode failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    fn fresh_keypair() -> (SigningKey, VerifyingKey) {
        let mut secret = [0u8; 32];
        getrandom::getrandom(&mut secret).expect("OS RNG");
        let signing = SigningKey::from_bytes(&secret);
        let verifying = signing.verifying_key();
        (signing, verifying)
    }

    fn signed_doc(signing: &SigningKey, payload: Value) -> Value {
        let canonical = {
            let mut buf = Vec::new();
            write_canonical(&payload, &mut buf);
            buf
        };
        let sig = signing.sign(&canonical);
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        let mut doc = payload;
        if let Value::Object(map) = &mut doc {
            map.insert("signature".to_string(), Value::String(sig_b64));
        }
        doc
    }

    #[test]
    fn canonical_payload_sorts_keys_recursively() {
        let v = json!({"b": 1, "a": {"y": 2, "x": [3, 4]}});
        let mut out = Vec::new();
        write_canonical(&v, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"a":{"x":[3,4],"y":2},"b":1}"#
        );
    }

    #[test]
    fn canonical_payload_strips_signature_field() {
        let v = json!({"a": 1, "signature": "should-not-affect-canonical"});
        let canonical = canonical_signed_payload(&v);
        assert_eq!(String::from_utf8(canonical).unwrap(), r#"{"a":1}"#);
    }

    #[test]
    fn round_trip_signature_verifies() {
        let (signing, verifying) = fresh_keypair();
        let doc = signed_doc(&signing, json!({"version": 1, "issuer": "test"}));
        let result = verify_policy(
            &doc,
            &KeySource::Embedded(verifying),
            std::path::Path::new("/tmp/test.json"),
        );
        assert!(result.is_ok(), "expected verify ok, got {:?}", result.err());
    }

    #[test]
    fn tampered_payload_fails_verification() {
        let (signing, verifying) = fresh_keypair();
        let mut doc = signed_doc(&signing, json!({"version": 1, "issuer": "test"}));
        // Change a signed field after signing.
        if let Value::Object(map) = &mut doc {
            map.insert("issuer".to_string(), Value::String("attacker".into()));
        }
        let result = verify_policy(
            &doc,
            &KeySource::Embedded(verifying),
            std::path::Path::new("/tmp/test.json"),
        );
        assert!(matches!(result, Err(PolicyError::SignatureMismatch { .. })));
    }

    #[test]
    fn missing_signature_field_errors_explicitly() {
        let (_, verifying) = fresh_keypair();
        let doc = json!({"version": 1, "issuer": "test"});
        let result = verify_policy(
            &doc,
            &KeySource::Embedded(verifying),
            std::path::Path::new("/tmp/test.json"),
        );
        assert!(matches!(result, Err(PolicyError::MissingSignature { .. })));
    }

    #[test]
    fn malformed_signature_field_errors_explicitly() {
        let (_, verifying) = fresh_keypair();
        let doc = json!({"version": 1, "signature": "!!!not-base64!!!"});
        let result = verify_policy(
            &doc,
            &KeySource::Embedded(verifying),
            std::path::Path::new("/tmp/test.json"),
        );
        assert!(matches!(
            result,
            Err(PolicyError::MalformedSignature { .. })
        ));
    }

    #[test]
    fn no_key_source_with_present_signature_fails_closed() {
        let (signing, _) = fresh_keypair();
        let doc = signed_doc(&signing, json!({"version": 1}));
        let result = verify_policy(
            &doc,
            &KeySource::None,
            std::path::Path::new("/tmp/test.json"),
        );
        assert!(matches!(result, Err(PolicyError::NoVerificationKey { .. })));
    }

    #[test]
    fn cross_keypair_signature_does_not_verify() {
        let (signing_a, _) = fresh_keypair();
        let (_, verifying_b) = fresh_keypair();
        let doc = signed_doc(&signing_a, json!({"version": 1, "issuer": "a"}));
        let result = verify_policy(
            &doc,
            &KeySource::Embedded(verifying_b),
            std::path::Path::new("/tmp/test.json"),
        );
        assert!(matches!(result, Err(PolicyError::SignatureMismatch { .. })));
    }

    #[test]
    fn canonical_form_normalizes_key_order_for_signing() {
        // A document signed with keys in insertion order A must verify
        // when re-serialized with keys in insertion order B — proves
        // canonicalization happens before signing.
        let (signing, verifying) = fresh_keypair();
        let payload_a = json!({"version": 1, "issuer": "test", "policies": {}});
        let signed = signed_doc(&signing, payload_a);
        // Re-serialize through round-trip to scramble insertion order.
        let s = serde_json::to_string(&signed).unwrap();
        let reparsed: Value = serde_json::from_str(&s).unwrap();
        let result = verify_policy(
            &reparsed,
            &KeySource::Embedded(verifying),
            std::path::Path::new("/tmp/test.json"),
        );
        assert!(
            result.is_ok(),
            "round-trip verify failed: {:?}",
            result.err()
        );
    }

    #[test]
    fn read_pubkey_bytes_accepts_raw_32_bytes() {
        let raw = [7u8; 32];
        let out = read_pubkey_bytes(&raw).expect("raw bytes accepted");
        assert_eq!(out, raw.to_vec());
    }

    #[test]
    fn read_pubkey_bytes_accepts_base64_text() {
        let raw = [9u8; 32];
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let out = read_pubkey_bytes(b64.as_bytes()).expect("base64 accepted");
        assert_eq!(out, raw.to_vec());
    }

    #[test]
    fn read_pubkey_bytes_accepts_pem_wrapped_text() {
        let raw = [11u8; 32];
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let pem = format!("-----BEGIN PUBLIC KEY-----\n{b64}\n-----END PUBLIC KEY-----\n");
        let out = read_pubkey_bytes(pem.as_bytes()).expect("pem accepted");
        assert_eq!(out, raw.to_vec());
    }

    #[test]
    fn read_pubkey_bytes_rejects_garbage() {
        let bytes = b"not a key, also not 32 bytes long";
        assert!(read_pubkey_bytes(bytes).is_err());
    }

    #[test]
    fn key_source_file_label_includes_path() {
        let (_, verifying) = fresh_keypair();
        let path = PathBuf::from("/tmp/test.pub");
        let src = KeySource::File(verifying, path.clone());
        assert!(src.label().contains("/tmp/test.pub"));
        assert!(src.label().starts_with("file ("));
    }

    #[test]
    fn key_source_file_acts_as_valid_verification_source() {
        // File-source keys verify the same way embedded/env keys do —
        // confirms we didn't accidentally introduce a per-variant gate
        // in `verify_policy`.
        let (signing, verifying) = fresh_keypair();
        let doc = signed_doc(&signing, json!({"version": 1, "issuer": "test"}));
        let src = KeySource::File(verifying, PathBuf::from("/tmp/whatever.pub"));
        assert!(verify_policy(&doc, &src, std::path::Path::new("/tmp/test.json")).is_ok());
    }
}
