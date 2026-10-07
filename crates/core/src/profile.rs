//! Which user-level profile this build keeps its state in.
//!
//! A per-customer Enterprise build (`THCLAWS_CUSTOMER=<id>` at build time)
//! lives in `~/.config/thclaws-<id>` and the keychain service
//! `thclaws-<id>`, so a regular thClaws on the same machine — its provider
//! keys, settings, cloud token — is invisible to it and vice versa. Without
//! a customer every path is exactly what it always was (`thclaws`).
//! Project-level `.thclaws/` folders are per-workspace and stay shared.

use std::path::{Path, PathBuf};

const CUSTOMER: &str = env!("THCLAWS_CUSTOMER");
/// A literal, so `ee-verify` can find `thclaws-<customer>` in the binary.
const CUSTOMER_DIR: &str = concat!("thclaws-", env!("THCLAWS_CUSTOMER"));

/// The customer id this build was made for, if any.
pub fn customer() -> Option<&'static str> {
    (!CUSTOMER.is_empty()).then_some(CUSTOMER)
}

/// `thclaws`, or `thclaws-<customer>`: the directory name under
/// `~/.config` / `~/.cache` / the platform data dirs.
pub fn app_dir_name() -> &'static str {
    if CUSTOMER.is_empty() {
        "thclaws"
    } else {
        CUSTOMER_DIR
    }
}

/// Whether user-home Claude Code content (`~/.claude/mcp.json`, agents,
/// commands, skills, CLAUDE.md) is read. Not by a customer build: MCP
/// servers and skills from there would run outside the org's policy.
/// Project-level `.claude/` in a workspace still loads.
pub fn reads_claude_home() -> bool {
    customer().is_none()
}

/// The keychain service every secret of this profile is filed under.
pub fn keychain_service() -> &'static str {
    app_dir_name()
}

/// `<home>/.config/<app dir>/<sub>`, joined as one relative string so a
/// regular build produces the same bytes as the old `".config/thclaws/…"`
/// literals did.
pub fn config_path(home: &Path, sub: &str) -> PathBuf {
    if sub.is_empty() {
        home.join(format!(".config/{}", app_dir_name()))
    } else {
        home.join(format!(".config/{}/{sub}", app_dir_name()))
    }
}

/// `~/.config/<app dir>` for display (`/cloud doctor`).
pub fn config_dir() -> Option<PathBuf> {
    crate::util::home_dir().map(|h| config_path(&h, ""))
}

#[cfg(test)]
fn dir_name(customer: &str) -> String {
    if customer.is_empty() {
        "thclaws".to_string()
    } else {
        format!("thclaws-{customer}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_regular_build_keeps_the_thclaws_name() {
        assert_eq!(dir_name(""), "thclaws");
    }

    #[test]
    fn a_customer_build_gets_its_own_name() {
        assert_eq!(dir_name("sis"), "thclaws-sis");
        assert_eq!(dir_name("acme-2"), "thclaws-acme-2");
    }

    #[test]
    fn this_build_uses_the_mapping() {
        assert_eq!(app_dir_name(), dir_name(CUSTOMER));
        assert_eq!(keychain_service(), app_dir_name());
    }

    #[test]
    fn config_path_matches_the_old_literal_for_a_regular_build() {
        if customer().is_some() {
            return;
        }
        let home = Path::new("/home/u");
        assert_eq!(
            config_path(home, "settings.json"),
            home.join(".config/thclaws/settings.json")
        );
        assert_eq!(config_path(home, ""), home.join(".config/thclaws"));
    }
}
