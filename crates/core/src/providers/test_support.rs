use std::ffi::OsString;
use std::sync::MutexGuard;

/// Keeps credential tests independent of the developer's keychain, cloud login,
/// and exported keys. The shared lock covers the entire fixture lifetime.
pub(crate) struct CredentialEnv {
    saved: Vec<(&'static str, Option<OsString>)>,
    _home: tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
}

impl CredentialEnv {
    pub(crate) fn new() -> Self {
        let lock = crate::kms::test_env_lock();
        let home = tempfile::tempdir().expect("credential test home");
        let mut keys: Vec<_> = super::ProviderKind::ALL
            .iter()
            .filter_map(|kind| kind.api_key_env())
            .collect();
        keys.extend([
            "HOME",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
            "THCLAWS_DISABLE_KEYCHAIN",
            "THCLAWS_GATEWAY_API_KEY",
            "THCLAWS_GATEWAY_BASE_URL",
            "THCLAWS_CLOUD_TOKEN",
            "THCLAWS_SHARED_AGENT_DIR",
            "LITELLM_BASE_URL",
            "OPENAI_COMPAT_BASE_URL",
        ]);
        keys.sort_unstable();
        keys.dedup();
        let saved = keys
            .iter()
            .map(|&key| (key, std::env::var_os(key)))
            .collect();
        let guard = Self {
            saved,
            _home: home,
            _lock: lock,
        };
        for key in keys {
            std::env::remove_var(key);
        }
        std::env::set_var("THCLAWS_DISABLE_KEYCHAIN", "1");
        std::env::set_var("HOME", guard._home.path());
        std::env::set_var("USERPROFILE", guard._home.path());
        std::env::set_var("XDG_CONFIG_HOME", guard._home.path());
        guard
    }
}

impl Drop for CredentialEnv {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
