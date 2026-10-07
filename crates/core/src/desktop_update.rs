//! Org-enforced desktop updates.
//!
//! Every request to the thClaws gateway (and the org-cloud sign-in / quota
//! calls) names this build in `X-Thclaws-Client: <semver>+<sha>; built=<UTC>`.
//! The org can then:
//!
//! - refuse it: HTTP 426, `X-Thclaws-Error: client_update_required`, never
//!   retried, humanized by [`client_update_message`];
//! - warn: `X-Thclaws-Update: required; by=<RFC3339|empty>; url=<download>`
//!   on a normal response, captured by [`observe`];
//! - offer a newer stable build through `GET /api/desktop/version-policy`
//!   (404 = feature off), checked by [`check_policy`].
//!
//! The rules in [`compare`] mirror the cloud's
//! `services/desktop_version.py` so the desktop and the gateway agree.
//! The resulting [`Status`] is process-global; the GUI and the CLI listen to
//! it and show one notice per level per launch.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CLIENT_HEADER: &str = "X-Thclaws-Client";
pub const UPDATE_HEADER: &str = "X-Thclaws-Update";
pub const ERROR_HEADER: &str = "X-Thclaws-Error";
pub const POLICY_PATH: &str = "/api/desktop/version-policy";

/// `<semver>+<git sha>; built=<RFC3339 UTC>` for this binary.
pub fn client_value() -> String {
    format!(
        "{}+{}; built={}",
        crate::version::VERSION,
        crate::version::GIT_SHA,
        crate::version::BUILD_TIME
    )
}

pub fn insert_client_header(headers: &mut reqwest::header::HeaderMap) {
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&client_value()) {
        headers.insert(CLIENT_HEADER, v);
    }
}

/// Name this build on a call to the org cloud (sign-in, quota, whoami).
pub fn tag_cloud(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    rb.header(CLIENT_HEADER, client_value())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum Status {
    Ok,
    /// A newer stable build exists; nothing is required.
    Available {
        version: String,
        url: String,
        sha: Option<String>,
    },
    /// This build is below the org minimum; refused from `by` on (never,
    /// under a warn-only policy, when `by` is `None`).
    Warn {
        by: Option<String>,
        url: String,
    },
    /// The org refuses this build now.
    Blocked {
        url: String,
        message: Option<String>,
    },
}

impl Status {
    fn rank(&self) -> u8 {
        match self {
            Status::Ok => 0,
            Status::Available { .. } => 1,
            Status::Warn { .. } => 2,
            Status::Blocked { .. } => 3,
        }
    }

    pub fn level(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Available { .. } => "available",
            Status::Warn { .. } => "warn",
            Status::Blocked { .. } => "blocked",
        }
    }

    /// The one-line notice for a terminal (CLI startup, GUI Terminal tab).
    pub fn notice_line(&self) -> Option<String> {
        let name = &crate::branding::current().name;
        match self {
            Status::Ok => None,
            Status::Available { version, url, .. } => Some(format!(
                "A new version of {name} is available ({version}): {url}"
            )),
            Status::Warn { by: Some(by), url } => Some(format!(
                "An update to {name} is required by {}: {url}",
                display_date(by)
            )),
            Status::Warn { by: None, url } => {
                Some(format!("An update to {name} is required: {url}"))
            }
            Status::Blocked { url, .. } => Some(format!(
                "This version of {name} is no longer allowed by your organisation. \
                 Download the update: {url}"
            )),
        }
    }
}

/// `2026-10-15T00:00:00Z` → `2026-10-15`; anything unparseable as given.
pub fn display_date(s: &str) -> String {
    parse_time(s)
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| s.to_string())
}

// ── Rules (mirror of the cloud's desktop_version.py) ───────────────────

type SemverKey = (u64, u64, u64, bool, String);

/// Sort key; a pre-release sorts below its release. `None` = not semver.
fn semver_key(v: &str) -> Option<SemverKey> {
    let v = v.trim();
    let v = v.strip_prefix('v').unwrap_or(v);
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) if !p.is_empty() => (c, Some(p)),
        Some(_) => return None,
        None => (v, None),
    };
    let mut parts = core.split('.');
    let mut num = || -> Option<u64> {
        let p = parts.next()?;
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        p.parse().ok()
    };
    let (a, b, c) = (num()?, num()?, num()?);
    if parts.next().is_some() {
        return None;
    }
    Some((a, b, c, pre.is_none(), pre.unwrap_or("").to_string()))
}

fn sha_matches(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    a.len() >= 7 && b.len() >= 7 && (a.starts_with(&b) || b.starts_with(&a))
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// What a build says it is.
#[derive(Debug, Clone)]
pub struct Build {
    pub version: String,
    pub sha: String,
    pub built_at: String,
}

impl Build {
    pub fn this() -> Self {
        Self {
            version: crate::version::VERSION.into(),
            sha: crate::version::GIT_SHA.into(),
            built_at: crate::version::BUILD_TIME.into(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Latest {
    pub version: String,
    #[serde(default)]
    pub git_sha: Option<String>,
    #[serde(default)]
    pub built_at: Option<String>,
    #[serde(default)]
    pub download_url: Option<String>,
}

/// The body of `GET /api/desktop/version-policy`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub min_version: Option<String>,
    #[serde(default)]
    pub min_built_at: Option<String>,
    #[serde(default)]
    pub blocked_shas: Vec<String>,
    #[serde(default)]
    pub enforce_after: Option<String>,
    #[serde(default)]
    pub download_url: Option<String>,
    #[serde(default)]
    pub latest: Option<Latest>,
}

/// Why `b` is below the policy minimum, or `None`. An unparseable version
/// is below any minimum.
pub fn below_reason(p: &Policy, b: &Build) -> Option<&'static str> {
    let min = p.min_version.as_deref().and_then(semver_key);
    if min.is_none() && p.blocked_shas.is_empty() {
        return None;
    }
    let Some(have) = semver_key(&b.version) else {
        return Some("unknown_client");
    };
    if p.blocked_shas.iter().any(|s| sha_matches(&b.sha, s)) {
        return Some("blocked_build");
    }
    if let Some(want) = min {
        if have < want {
            return Some("below_min_version");
        }
        if have == want {
            if let Some(min_built) = p.min_built_at.as_deref().and_then(parse_time) {
                match parse_time(&b.built_at) {
                    Some(t) if t >= min_built => {}
                    _ => return Some("below_min_version"),
                }
            }
        }
    }
    None
}

/// Is `b` at least `lt`? `None` when either side is unknown.
pub fn on_latest(b: &Build, lt: &Latest) -> Option<bool> {
    let have = semver_key(&b.version)?;
    let want = semver_key(&lt.version)?;
    if have != want {
        return Some(have > want);
    }
    if let Some(s) = lt.git_sha.as_deref() {
        if sha_matches(&b.sha, s) {
            return Some(true);
        }
    }
    match (
        lt.built_at.as_deref().and_then(parse_time),
        parse_time(&b.built_at),
    ) {
        (Some(want), Some(have)) => Some(have >= want),
        _ => Some(true),
    }
}

/// Where `b` stands under `p` at `now`. Precedence: blocked > warn >
/// available > ok.
pub fn compare(p: &Policy, b: &Build, now: DateTime<Utc>) -> Status {
    let url = p.download_url.clone().unwrap_or_default();
    let mode = p.mode.as_deref().unwrap_or("off");
    if mode != "off" && below_reason(p, b).is_some() {
        let deadline = p.enforce_after.as_deref().and_then(parse_time);
        let enforcing = mode == "block" && deadline.is_none_or(|d| now >= d);
        return if enforcing {
            Status::Blocked { url, message: None }
        } else {
            Status::Warn {
                by: if mode == "block" {
                    p.enforce_after.clone()
                } else {
                    None
                },
                url,
            }
        };
    }
    if let Some(lt) = &p.latest {
        if on_latest(b, lt) == Some(false) {
            return Status::Available {
                version: lt.version.clone(),
                url: lt.download_url.clone().unwrap_or(url),
                sha: lt.git_sha.clone(),
            };
        }
    }
    Status::Ok
}

// ── Gateway signals ────────────────────────────────────────────────────

/// `required; by=<RFC3339 or empty>; url=<download>` → `Warn`.
pub fn parse_update_header(v: &str) -> Option<Status> {
    let mut parts = v.split(';').map(str::trim);
    if !parts.next()?.eq_ignore_ascii_case("required") {
        return None;
    }
    let (mut by, mut url) = (None, String::new());
    for p in parts {
        let Some((k, val)) = p.split_once('=') else {
            continue;
        };
        let val = val.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "by" if !val.is_empty() => by = Some(val.to_string()),
            "url" => url = val.to_string(),
            _ => {}
        }
    }
    Some(Status::Warn { by, url })
}

/// Whether a provider error is the gateway refusing this build.
pub fn is_client_update_required(raw: &str) -> bool {
    raw.contains("\"client_update_required\"")
}

/// The refusal's JSON body inside an error string like `http 426 …: {…}`.
fn refusal_body(raw: &str) -> Option<Value> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    let v: Value = serde_json::from_str(raw.get(start..=end)?).ok()?;
    let err = v
        .get("error")
        .or_else(|| v.get("detail").and_then(|d| d.get("error")))?;
    (err.get("type").and_then(Value::as_str) == Some("client_update_required")).then(|| err.clone())
}

/// The `Blocked` status a 426 refusal carries.
pub fn refusal_status(raw: &str) -> Option<Status> {
    if !is_client_update_required(raw) {
        return None;
    }
    let body = refusal_body(raw);
    let get = |k: &str| {
        body.as_ref()
            .and_then(|b| b.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    Some(Status::Blocked {
        url: get("download_url").unwrap_or_default(),
        message: get("message"),
    })
}

/// "This version of <name> (<your_version>) is no longer allowed by your
/// organisation. Download the update: <download_url>" — `None` unless `raw`
/// is the gateway's `client_update_required` refusal.
pub fn client_update_message(raw: &str) -> Option<String> {
    if !is_client_update_required(raw) {
        return None;
    }
    let body = refusal_body(raw);
    let get = |k: &str| {
        body.as_ref()
            .and_then(|b| b.get(k))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let name = &crate::branding::current().name;
    let version = get("your_version")
        .unwrap_or_else(|| format!("{}+{}", crate::version::VERSION, crate::version::GIT_SHA));
    let mut msg =
        format!("This version of {name} ({version}) is no longer allowed by your organisation.");
    if let Some(url) = get("download_url") {
        msg.push_str(&format!(" Download the update: {url}"));
    } else {
        msg.push_str(" Contact your administrator for the update.");
    }
    Some(msg)
}

// ── Process-global status ──────────────────────────────────────────────

pub type Listener = Arc<dyn Fn(&Status) + Send + Sync>;

struct State {
    /// From the version-policy check.
    policy: Status,
    /// From the gateway (warn header / 426); outranks the check until the
    /// next successful check clears it.
    gateway: Status,
    listeners: Vec<(u64, Listener)>,
}

static STATE: Mutex<State> = Mutex::new(State {
    policy: Status::Ok,
    gateway: Status::Ok,
    listeners: Vec::new(),
});

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|p| p.into_inner())
}

fn effective(s: &State) -> Status {
    if s.gateway.rank() >= s.policy.rank() {
        s.gateway.clone()
    } else {
        s.policy.clone()
    }
}

/// The current status: the higher of the last check and any gateway signal.
pub fn current() -> Status {
    effective(&lock())
}

/// Register (or replace, by `key`) a listener called on every change.
pub fn listen(key: u64, f: Listener) {
    let mut s = lock();
    s.listeners.retain(|(k, _)| *k != key);
    s.listeners.push((key, f));
}

fn update(f: impl FnOnce(&mut State)) {
    let (changed, now, listeners) = {
        let mut s = lock();
        let before = effective(&s);
        f(&mut s);
        let now = effective(&s);
        let ls: Vec<Listener> = s.listeners.iter().map(|(_, l)| l.clone()).collect();
        (before != now, now, ls)
    };
    if changed {
        for l in listeners {
            l(&now);
        }
    }
}

pub fn set_policy_status(st: Status) {
    update(|s| {
        s.policy = st;
        // A fresh check supersedes an older gateway warning; a refusal
        // stays until the gateway itself stops refusing.
        if !matches!(s.gateway, Status::Blocked { .. }) {
            s.gateway = Status::Ok;
        }
    });
}

fn set_gateway_status(st: Status) {
    update(|s| s.gateway = st);
}

/// Capture `X-Thclaws-Update` / a 426 refusal header from a gateway
/// response. Call on every gateway-capable send; other origins are ignored.
pub fn observe(resp: &reqwest::Response) {
    if !crate::gateway_turn::is_gateway_url(resp.url()) {
        return;
    }
    let h = resp.headers();
    if resp.status().as_u16() == 426
        && h.get(ERROR_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("client_update_required"))
    {
        // The body (url, message) arrives through `note_error`.
        if !matches!(current(), Status::Blocked { .. }) {
            set_gateway_status(Status::Blocked {
                url: String::new(),
                message: None,
            });
        }
        return;
    }
    if let Some(st) = h
        .get(UPDATE_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_update_header)
    {
        set_gateway_status(st);
    } else if resp.status().is_success() && lock().gateway != Status::Ok {
        // The gateway served this build without a notice: whatever it said
        // before no longer holds.
        set_gateway_status(Status::Ok);
    }
}

/// Record a 426 refusal seen as an error string (gives the download URL).
pub fn note_error(raw: &str) {
    if let Some(st) = refusal_status(raw) {
        set_gateway_status(st);
    }
}

// ── Self-check ─────────────────────────────────────────────────────────

/// The org cloud to check against: only org-policy builds have one.
pub fn policy_cloud_url() -> Option<String> {
    crate::policy::thclaws_cloud_url()
}

/// `GET <cloud>/api/desktop/version-policy` with the stored `thc_` token.
/// `Ok(None)` = feature off (404) or not signed in; `Err` = unreachable.
pub async fn fetch_policy(
    cloud_url: &str,
    timeout: std::time::Duration,
) -> Result<Option<Policy>, String> {
    let Some(token) = crate::cloud::token() else {
        return Ok(None);
    };
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = tag_cloud(client.get(format!("{}{POLICY_PATH}", cloud_url.trim_end_matches('/'))))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("version policy: {}", resp.status()));
    }
    resp.json::<Policy>()
        .await
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Run the self-check for an org-policy build and publish the result.
/// Returns the new status, or `None` when there is nothing to check
/// (no org cloud, signed out, feature off, unreachable).
pub async fn check_policy(timeout: std::time::Duration) -> Option<Status> {
    let url = policy_cloud_url()?;
    match fetch_policy(&url, timeout).await {
        Ok(Some(p)) => {
            let st = compare(&p, &Build::this(), Utc::now());
            set_policy_status(st.clone());
            Some(st)
        }
        Ok(None) => {
            set_policy_status(Status::Ok);
            None
        }
        Err(_) => None,
    }
}

/// How often a running app re-checks.
pub const RECHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(4 * 3600);

/// The CLI / print-mode startup check: never blocks startup (runs in the
/// background with a short timeout), silent when unreachable, one stderr
/// line for available / warn / blocked.
pub fn spawn_cli_check() {
    if policy_cloud_url().is_none() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async {
        if let Some(st) = check_policy(std::time::Duration::from_secs(5)).await {
            if let Some(line) = st.notice_line() {
                eprintln!("\x1b[33m{line}\x1b[0m");
            }
        }
    });
}

// ── "New version available" dismissal memory ───────────────────────────

fn dismissed_path() -> Option<std::path::PathBuf> {
    crate::util::home_dir().map(|h| {
        h.join(format!(
            ".config/{}/update_notice.json",
            crate::profile::app_dir_name()
        ))
    })
}

/// The release key an `Available` notice is dismissed under: its sha, else
/// its version.
pub fn available_key(st: &Status) -> Option<String> {
    match st {
        Status::Available { sha, version, .. } => Some(
            sha.clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| version.clone()),
        ),
        _ => None,
    }
}

fn read_dismissed(path: &std::path::Path) -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get("dismissed_available")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn write_dismissed(path: &std::path::Path, key: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        path,
        serde_json::json!({ "dismissed_available": key }).to_string(),
    )
}

fn is_dismissed_at(path: &std::path::Path, st: &Status) -> bool {
    match (available_key(st), read_dismissed(path)) {
        (Some(k), Some(d)) => k == d,
        _ => false,
    }
}

/// Remember that the user dismissed the "new version available" notice for
/// this release; a newer release shows again.
pub fn dismiss_available(key: &str) {
    if let Some(p) = dismissed_path() {
        let _ = write_dismissed(&p, key);
    }
}

/// The `desktop_update_status` frame for the GUI.
pub fn payload(st: &Status) -> Value {
    let dismissed = dismissed_path().is_some_and(|p| is_dismissed_at(&p, st));
    let mut v = serde_json::to_value(st).unwrap_or(Value::Null);
    if let Value::Object(m) = &mut v {
        m.insert("type".into(), "desktop_update_status".into());
        m.insert("dismissed".into(), dismissed.into());
        if let Status::Warn { by: Some(by), .. } = st {
            m.insert("by_date".into(), display_date(by).into());
        }
        if let Some(k) = available_key(st) {
            m.insert("key".into(), k.into());
        }
        m.insert(
            "name".into(),
            crate::branding::current().name.clone().into(),
        );
        m.insert(
            "your_version".into(),
            format!("{}+{}", crate::version::VERSION, crate::version::GIT_SHA).into(),
        );
    }
    v
}

/// One terminal notice per level per launch.
pub fn first_notice_for_level(st: &Status) -> bool {
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    if matches!(st, Status::Ok) {
        return false;
    }
    let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
    if seen.contains(&st.level()) {
        return false;
    }
    seen.push(st.level());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(v: &str, sha: &str, built: &str) -> Build {
        Build {
            version: v.into(),
            sha: sha.into(),
            built_at: built.into(),
        }
    }

    fn t(s: &str) -> DateTime<Utc> {
        parse_time(s).unwrap()
    }

    fn policy(min: Option<&str>, min_built: Option<&str>) -> Policy {
        Policy {
            mode: Some("block".into()),
            min_version: min.map(Into::into),
            min_built_at: min_built.map(Into::into),
            download_url: Some("https://sis.thclaws.ai/download".into()),
            ..Default::default()
        }
    }

    const NOW: &str = "2026-10-01T00:00:00Z";

    #[test]
    fn client_value_names_version_sha_and_build_time() {
        let v = client_value();
        assert!(v.starts_with(&format!(
            "{}+{}; built=",
            crate::version::VERSION,
            crate::version::GIT_SHA
        )));
        assert!(reqwest::header::HeaderValue::from_str(&v).is_ok());
    }

    #[test]
    fn below_min_version_blocks() {
        let p = policy(Some("0.140.0"), None);
        let old = build("0.139.2", "abcdef1", "2026-09-30T00:00:00Z");
        let new = build("0.140.0", "abcdef1", "2026-09-30T00:00:00Z");
        assert!(matches!(compare(&p, &old, t(NOW)), Status::Blocked { .. }));
        assert_eq!(compare(&p, &new, t(NOW)), Status::Ok);
        assert_eq!(
            compare(&p, &build("0.141.0", "x", "garbage"), t(NOW)),
            Status::Ok
        );
    }

    #[test]
    fn min_built_at_breaks_the_tie_at_min_version() {
        let p = policy(Some("0.140.0"), Some("2026-09-30T12:00:00Z"));
        let early = build("0.140.0", "abcdef1", "2026-09-30T11:59:59Z");
        let late = build("0.140.0", "abcdef1", "2026-09-30T12:00:00Z");
        let unknown = build("0.140.0", "abcdef1", "unknown");
        assert!(matches!(
            compare(&p, &early, t(NOW)),
            Status::Blocked { .. }
        ));
        assert_eq!(compare(&p, &late, t(NOW)), Status::Ok);
        assert!(matches!(
            compare(&p, &unknown, t(NOW)),
            Status::Blocked { .. }
        ));
        // A newer version never needs the tie-break.
        assert_eq!(
            compare(
                &p,
                &build("0.140.1", "abcdef1", "2020-01-01T00:00:00Z"),
                t(NOW)
            ),
            Status::Ok
        );
    }

    #[test]
    fn a_blocked_sha_is_blocked_by_prefix() {
        let mut p = policy(None, None);
        p.blocked_shas = vec!["e5f1195c".into()];
        assert!(matches!(
            compare(&p, &build("9.9.9", "e5f1195", NOW), t(NOW)),
            Status::Blocked { .. }
        ));
        assert!(matches!(
            compare(&p, &build("9.9.9", "e5f1195cdeadbeef", NOW), t(NOW)),
            Status::Blocked { .. }
        ));
        assert_eq!(
            compare(&p, &build("9.9.9", "e5f119", NOW), t(NOW)),
            Status::Ok
        );
        assert_eq!(
            compare(&p, &build("9.9.9", "88b628fc", NOW), t(NOW)),
            Status::Ok
        );
    }

    #[test]
    fn unparseable_versions_are_below() {
        let p = policy(Some("0.140.0"), None);
        for v in ["unknown", "", "0.140", "0.140.0.1", "v0.x.0"] {
            assert!(
                matches!(
                    compare(&p, &build(v, "abcdef1", NOW), t(NOW)),
                    Status::Blocked { .. }
                ),
                "{v:?}"
            );
        }
        // A pre-release sorts below its release.
        assert!(matches!(
            compare(&p, &build("0.140.0-rc1", "abcdef1", NOW), t(NOW)),
            Status::Blocked { .. }
        ));
        // A policy with no criteria enforces nothing.
        assert_eq!(
            compare(&policy(None, None), &build("junk", "", ""), t(NOW)),
            Status::Ok
        );
    }

    #[test]
    fn enforce_after_and_mode_turn_block_into_warn() {
        let mut p = policy(Some("0.140.0"), None);
        p.enforce_after = Some("2026-10-15T00:00:00Z".into());
        let old = build("0.139.0", "abcdef1", NOW);
        assert_eq!(
            compare(&p, &old, t(NOW)),
            Status::Warn {
                by: Some("2026-10-15T00:00:00Z".into()),
                url: "https://sis.thclaws.ai/download".into()
            }
        );
        assert!(matches!(
            compare(&p, &old, t("2026-10-15T00:00:00Z")),
            Status::Blocked { .. }
        ));
        p.mode = Some("warn".into());
        assert!(matches!(
            compare(&p, &old, t("2027-01-01T00:00:00Z")),
            Status::Warn { by: None, .. }
        ));
        p.mode = Some("off".into());
        assert_eq!(compare(&p, &old, t(NOW)), Status::Ok);
    }

    fn latest(v: &str, sha: Option<&str>, built: &str) -> Latest {
        Latest {
            version: v.into(),
            git_sha: sha.map(Into::into),
            built_at: Some(built.into()),
            download_url: Some("https://sis.thclaws.ai/download".into()),
        }
    }

    #[test]
    fn precedence_is_blocked_then_warn_then_available_then_ok() {
        let lt = latest("0.141.0", Some("1111111"), "2026-09-30T00:00:00Z");
        let old = build("0.139.0", "abcdef1", "2026-09-01T00:00:00Z");

        let mut p = policy(Some("0.140.0"), None);
        p.latest = Some(lt.clone());
        assert!(matches!(compare(&p, &old, t(NOW)), Status::Blocked { .. }));

        p.enforce_after = Some("2026-12-01T00:00:00Z".into());
        assert!(matches!(compare(&p, &old, t(NOW)), Status::Warn { .. }));

        p.min_version = None;
        assert_eq!(
            compare(&p, &old, t(NOW)),
            Status::Available {
                version: "0.141.0".into(),
                url: "https://sis.thclaws.ai/download".into(),
                sha: Some("1111111".into()),
            }
        );

        let on = build("0.141.0", "1111111", "2026-09-30T00:00:00Z");
        assert_eq!(compare(&p, &on, t(NOW)), Status::Ok);
        p.latest = None;
        assert_eq!(compare(&p, &old, t(NOW)), Status::Ok);
    }

    #[test]
    fn on_latest_uses_version_then_sha_then_build_time() {
        let lt = latest("0.141.0", Some("1111111"), "2026-09-30T00:00:00Z");
        assert_eq!(
            on_latest(&build("0.142.0", "x", "2020-01-01T00:00:00Z"), &lt),
            Some(true)
        );
        assert_eq!(
            on_latest(&build("0.140.9", "x", "2027-01-01T00:00:00Z"), &lt),
            Some(false)
        );
        // Same git sha = current, whatever the clock says.
        assert_eq!(
            on_latest(&build("0.141.0", "1111111abc", "2020-01-01T00:00:00Z"), &lt),
            Some(true)
        );
        assert_eq!(
            on_latest(&build("0.141.0", "2222222", "2026-09-29T00:00:00Z"), &lt),
            Some(false)
        );
        assert_eq!(
            on_latest(&build("0.141.0", "2222222", "2026-09-30T00:00:00Z"), &lt),
            Some(true)
        );
        assert_eq!(on_latest(&build("junk", "2222222", NOW), &lt), None);
    }

    #[test]
    fn update_header_parses_by_and_url() {
        assert_eq!(
            parse_update_header("required; by=2026-10-15T00:00:00Z; url=https://x.test/download"),
            Some(Status::Warn {
                by: Some("2026-10-15T00:00:00Z".into()),
                url: "https://x.test/download".into()
            })
        );
        assert_eq!(
            parse_update_header("required; by=; url=https://x.test/d?a=1"),
            Some(Status::Warn {
                by: None,
                url: "https://x.test/d?a=1".into()
            })
        );
        assert_eq!(parse_update_header("optional; url=x"), None);
        assert_eq!(parse_update_header(""), None);
    }

    #[test]
    fn a_426_refusal_is_humanized_and_parsed() {
        let raw = r#"http 426 Upgrade Required: {"error":{"type":"client_update_required","message":"This thClaws version is no longer allowed. Update to 0.140.0 or later from https://sis.thclaws.ai/download","min_version":"0.140.0","min_built_at":null,"download_url":"https://sis.thclaws.ai/download","your_version":"0.139.0+abcdef1"}}"#;
        assert!(is_client_update_required(raw));
        let name = &crate::branding::current().name;
        assert_eq!(
            client_update_message(raw).unwrap(),
            format!(
                "This version of {name} (0.139.0+abcdef1) is no longer allowed by your \
                 organisation. Download the update: https://sis.thclaws.ai/download"
            )
        );
        assert!(matches!(
            refusal_status(raw),
            Some(Status::Blocked { url, message: Some(_) }) if url == "https://sis.thclaws.ai/download"
        ));
        assert!(client_update_message(r#"{"error":{"type":"quota_exceeded"}}"#).is_none());
        // A body we cannot parse still yields a message.
        assert!(
            client_update_message("http 426: \"client_update_required\"")
                .unwrap()
                .contains("no longer allowed")
        );
    }

    #[test]
    fn dismissal_is_remembered_per_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/update_notice.json");
        let a = Status::Available {
            version: "0.141.0".into(),
            url: "u".into(),
            sha: Some("1111111".into()),
        };
        let b = Status::Available {
            version: "0.142.0".into(),
            url: "u".into(),
            sha: Some("2222222".into()),
        };
        assert!(!is_dismissed_at(&path, &a));
        write_dismissed(&path, &available_key(&a).unwrap()).unwrap();
        assert!(is_dismissed_at(&path, &a));
        assert!(!is_dismissed_at(&path, &b), "a newer release shows again");
        // Only the "available" level is dismissible for good.
        let w = Status::Warn {
            by: None,
            url: "u".into(),
        };
        assert!(!is_dismissed_at(&path, &w));
        // No sha pinned: the version is the key.
        let pinned = Status::Available {
            version: "0.141.0".into(),
            url: "u".into(),
            sha: None,
        };
        assert_eq!(available_key(&pinned).as_deref(), Some("0.141.0"));
    }

    #[test]
    fn notices_name_the_level() {
        let w = Status::Warn {
            by: Some("2026-10-15T00:00:00Z".into()),
            url: "https://d".into(),
        };
        assert!(w
            .notice_line()
            .unwrap()
            .contains("required by 2026-10-15: https://d"));
        assert!(Status::Ok.notice_line().is_none());
        let p = payload(&w);
        assert_eq!(p["type"], "desktop_update_status");
        assert_eq!(p["level"], "warn");
        assert_eq!(p["by_date"], "2026-10-15");
    }
}
