//! Error type for the policy subsystem.
//!
//! Every variant maps to a *fail-closed* condition at startup. The binary
//! refuses to start when any of these surface — silent fallback would
//! defeat the whole purpose of an enforceable policy file.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// This build requires a policy and none was found — not on disk, and
    /// none embedded. Reached only by an enterprise build; open-core runs
    /// unrestricted when no policy exists.
    #[error(
        "this build requires an organization policy, but none was found \
         (looked at THCLAWS_POLICY_FILE, /etc/thclaws/policy.json, \
         ~/.config/thclaws/policy.json, and the built-in copy)"
    )]
    PolicyRequired,

    /// File was readable but didn't parse as valid JSON.
    #[error("policy file at {path:?} is not valid JSON: {source}")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// File parsed but is missing the `signature` field.
    #[error("policy file at {path:?} has no signature field — refusing to apply unsigned policy")]
    MissingSignature { path: PathBuf },

    /// Signature field is present but not valid base64.
    #[error("policy file at {path:?} has malformed signature: {message}")]
    MalformedSignature { path: PathBuf, message: String },

    /// Signature was syntactically OK but didn't verify against the
    /// configured public key.
    #[error(
        "policy file at {path:?} failed signature verification — issuer {issuer:?} not authorized for this build"
    )]
    SignatureMismatch { path: PathBuf, issuer: String },

    /// Verification key isn't available (no compile-time embed, no env var).
    /// Holds a policy file but can't trust it — fail closed.
    #[error(
        "policy file at {path:?} requires verification but no public key is configured (build embedded none and THCLAWS_POLICY_PUBLIC_KEY is unset)"
    )]
    NoVerificationKey { path: PathBuf },

    /// Public key from env var couldn't be decoded.
    #[error("THCLAWS_POLICY_PUBLIC_KEY is set but unusable: {message}")]
    InvalidEnvKey { message: String },

    /// `expires_at` is in the past relative to the host clock.
    #[error("policy file at {path:?} expired on {expires_at} — contact admin for renewal")]
    Expired { path: PathBuf, expires_at: String },

    /// `binding.binary_fingerprint` is set but doesn't match this binary.
    /// Prevents lifting a customer's policy onto a non-customer build.
    #[error(
        "policy file at {path:?} is bound to a different binary fingerprint ({expected}) than the one running"
    )]
    BindingMismatch { path: PathBuf, expected: String },

    /// Schema version we don't know how to interpret. Forward-compat
    /// guard — newer policies on older binaries refuse rather than
    /// silently skipping unknown blocks.
    #[error(
        "policy file at {path:?} declares version {got} but this build only understands version {supported}"
    )]
    UnsupportedVersion {
        path: PathBuf,
        got: u32,
        supported: u32,
    },

    /// IO error reading the policy file (permissions, etc.).
    #[error("could not read policy file at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Policy parsed and verified but a sub-policy is enabled without
    /// the fields needed to enforce it. Examples:
    ///   - `gateway.enabled: true` but `gateway.url` is empty → would
    ///     fail open at provider construction.
    ///   - `sso.enabled: true` but `sso.issuer_url` is empty → no way
    ///     to do OIDC discovery.
    ///
    /// We refuse to start rather than silently bypass.
    #[error("policy file at {path:?} has invalid config: {message}")]
    InvalidConfig { path: PathBuf, message: String },
}

impl PolicyError {
    /// Render the error as a multi-line "refuse to start" message
    /// suitable for printing to stderr at startup.
    pub fn refuse_message(&self) -> String {
        // The missing-policy case is the one an ordinary employee will
        // actually hit — usually because the file was deleted — so it
        // gets instructions they can act on rather than a description of
        // the failure.
        if matches!(self, PolicyError::PolicyRequired) {
            return format!(
                "thClaws refused to start: this copy is configured for your organization \
                 and its policy file is missing.\n  {self}\n\n\
                 Ask whoever provided thClaws for your organization's policy.json, then \
                 save it as one of:\n  \
                 /etc/thclaws/policy.json          (all users on this machine)\n  \
                 ~/.config/thclaws/policy.json     (just you)\n\n\
                 Nothing else needs installing — the file alone is enough."
            );
        }
        format!(
            "thClaws refused to start due to a policy enforcement failure:\n  {self}\n\n\
             If you are an end user, contact your organization's administrator.\n\
             If you are testing, remove the policy file or set THCLAWS_POLICY_PUBLIC_KEY correctly."
        )
    }
}
