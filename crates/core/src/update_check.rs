//! "There's a newer thClaws" — a once-a-day look at the public releases.
//!
//! Deliberately small and deliberately passive. It reports; it never
//! downloads and never replaces the binary someone is running. Installing is
//! the user's act, on the release page.
//!
//! Two properties matter more than the feature itself:
//!
//! - **Startup never waits on the network.** Callers read the cached answer,
//!   which is a file read, and separately kick off a refresh whose result is
//!   for the *next* launch (or, in the GUI, for whenever it lands). A version
//!   notice is worth nothing and a slow cold start costs every launch.
//! - **It is silent when it has nothing to say.** No network, rate-limited,
//!   GitHub down, a shape we don't recognise: all of it ends as "no news",
//!   never as an error in someone's face.
//!
//! The source is the public mirror's releases, which is where the binaries
//! actually are. Public tags are minor-only by release policy, so what a
//! desktop user is offered is a real release and never one of the internal
//! patch tags.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LATEST_URL: &str = "https://api.github.com/repos/thClaws/thClaws/releases/latest";
const RELEASES_PAGE: &str = "https://github.com/thClaws/thClaws/releases/latest";
/// GitHub refuses a request with no User-Agent.
const USER_AGENT: &str = concat!("thclaws/", env!("CARGO_PKG_VERSION"));
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// Short on purpose: this runs in the background, and a check that hangs for
/// a minute is a thread doing nothing useful.
const TIMEOUT: Duration = Duration::from_secs(8);

/// A newer release than the running binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// Bare version, no leading `v` — "0.132.0".
    pub version: String,
    /// Where to get it.
    pub url: String,
}

/// Whether checking is allowed at all.
///
/// Off inside our own container images: a hosted workspace's engine is
/// whatever we deployed, the user cannot change it, and telling them to
/// upgrade would be advice they cannot take. `THCLAWS_UPDATE_CHECK=0` turns
/// it off everywhere else, for anyone who would rather their editor not talk
/// to github.com.
pub fn enabled() -> bool {
    if std::env::var("THCLAWS_INSIDE_DOCKER").ok().as_deref() == Some("1") {
        return false;
    }
    !matches!(
        std::env::var("THCLAWS_UPDATE_CHECK").ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// The last answer we got, if it is still newer than what is running.
///
/// Instant — a file read, no network. Re-compares against the running version
/// rather than trusting a stored verdict, so a cache written before the user
/// upgraded does not keep nagging them afterwards.
pub fn cached() -> Option<Update> {
    if !enabled() {
        return None;
    }
    let raw = std::fs::read_to_string(cache_path()?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let tag = v.get("tag")?.as_str()?;
    newer_than_running(tag)
}

/// Fetch, cache, and return an update if there is one. Blocking; call it off
/// the path the user is waiting on.
pub async fn refresh() -> Option<Update> {
    if !enabled() || !due() {
        return None;
    }
    let v = fetch().await?;
    // A draft or prerelease is not something to send people to.
    if v.get("draft").and_then(|b| b.as_bool()) == Some(true)
        || v.get("prerelease").and_then(|b| b.as_bool()) == Some(true)
    {
        return None;
    }
    let tag = v.get("tag_name")?.as_str()?.to_string();
    write_cache(&tag);
    newer_than_running(&tag)
}

/// Decoded as JSON rather than text: the crate builds reqwest with
/// `default-features = false`, which drops the charset decoder `text()` would
/// otherwise lean on. The `json` feature is on, and GitHub sends UTF-8.
async fn fetch() -> Option<serde_json::Value> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .ok()?;
    let resp = client.get(LATEST_URL).send().await.ok()?;
    if !resp.status().is_success() {
        // 403 here is GitHub's unauthenticated rate limit. Nothing to do
        // about it and nothing worth saying; the next day's check is fine.
        return None;
    }
    resp.json::<serde_json::Value>().await.ok()
}

/// True when the cache is missing or older than `CHECK_EVERY`.
///
/// The stamp is written even when the latest release turns out to be the one
/// already running, so a user on the newest version checks once a day rather
/// than on every launch.
fn due() -> bool {
    let Some(path) = cache_path() else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return true;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return true;
    };
    let Some(at) = v.get("checked_at").and_then(|n| n.as_u64()) else {
        return true;
    };
    now_secs().saturating_sub(at) >= CHECK_EVERY.as_secs()
}

fn write_cache(tag: &str) {
    let Some(path) = cache_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::json!({ "tag": tag, "checked_at": now_secs() });
    let _ = std::fs::write(path, body.to_string());
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_path() -> Option<PathBuf> {
    let home = crate::util::home_dir()?;
    let base = if cfg!(target_os = "macos") {
        home.join("Library").join("Caches")
    } else {
        home.join(".cache")
    };
    Some(base.join("thclaws").join("update-check.json"))
}

/// `Some(update)` when `tag` names a release newer than this binary.
fn newer_than_running(tag: &str) -> Option<Update> {
    let latest = parse(tag)?;
    let running = parse(crate::version::VERSION)?;
    (latest > running).then(|| Update {
        version: tag.trim_start_matches('v').to_string(),
        url: RELEASES_PAGE.to_string(),
    })
}

/// `v0.131.0` / `0.131.0` → `(0, 131, 0)`.
///
/// Compared as numbers, not as text: "0.9.0" sorts after "0.131.0" as a
/// string, which would tell everyone on the newest build to downgrade. A tag
/// carrying anything else (a `-rc1`, a date) is refused rather than guessed
/// at — this only has to understand the shape we actually publish.
fn parse(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.trim().trim_start_matches('v').split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers_not_as_text() {
        // The bug this exists to prevent: "0.131.0" < "0.9.0" as strings, so
        // a string compare tells every up-to-date user to "upgrade" to an
        // older release.
        assert!(parse("v0.131.0") > parse("v0.9.0"));
        assert_eq!(parse("v0.131.0"), parse("0.131.0"));
        assert_eq!(parse("0.131.0"), Some((0, 131, 0)));
    }

    #[test]
    fn a_tag_we_do_not_publish_is_refused_not_guessed() {
        for tag in ["v0.131.0-api4", "v0.131", "v1.2.3.4", "nightly", ""] {
            assert_eq!(parse(tag), None, "{tag} should not parse");
        }
    }

    #[test]
    fn only_a_strictly_newer_tag_is_an_update() {
        let running = crate::version::VERSION;
        assert!(newer_than_running(running).is_none());
        assert!(newer_than_running("v0.0.1").is_none());
        // Far enough ahead to stay true whatever the crate version becomes.
        assert!(newer_than_running("v9999.0.0").is_some());
    }

    #[test]
    fn the_container_image_never_nags() {
        // A hosted runner's engine is ours, not the user's, so an upgrade
        // notice there is advice nobody can act on.
        let _guard = crate::kms::test_env_lock();
        let prev = std::env::var("THCLAWS_INSIDE_DOCKER").ok();
        std::env::set_var("THCLAWS_INSIDE_DOCKER", "1");
        assert!(!enabled());
        assert!(cached().is_none());
        match prev {
            Some(v) => std::env::set_var("THCLAWS_INSIDE_DOCKER", v),
            None => std::env::remove_var("THCLAWS_INSIDE_DOCKER"),
        }
    }
}
