//! Build-time version metadata, captured by `build.rs`.
//!
//! Use `version::info()` for the full struct; `version::one_line()` for a
//! terse banner suitable for the REPL header or a CLI `--version` print.

/// Cargo package version (e.g. "0.1.0").
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Sent on every request to a site we do not own. reqwest sends no
/// User-Agent at all by default, and a bare request is refused outright
/// by GitHub and by Wikipedia, whose policy requires one — so a missing
/// header is not a politeness issue, it is a 403.
pub const WEB_USER_AGENT: &str = concat!(
    "thclaws/",
    env!("CARGO_PKG_VERSION"),
    " (+https://thclaws.ai)"
);
/// Short git commit hash at build time (e.g. "a1b2c3d"), or "unknown".
pub const GIT_SHA: &str = env!("THCLAWS_GIT_SHA");
/// Branch that was checked out at build time, or "unknown".
pub const GIT_BRANCH: &str = env!("THCLAWS_GIT_BRANCH");
/// "1" if the working tree had uncommitted changes at build time.
pub const GIT_DIRTY: &str = env!("THCLAWS_GIT_DIRTY");
/// ISO-8601 UTC timestamp of when the binary was compiled.
pub const BUILD_TIME: &str = env!("THCLAWS_BUILD_TIME");
/// Cargo profile used for the build ("debug" / "release").
pub const BUILD_PROFILE: &str = env!("THCLAWS_BUILD_PROFILE");

/// Snapshot of everything in one struct.
#[derive(Debug, Clone)]
pub struct Info {
    pub version: &'static str,
    pub git_sha: &'static str,
    pub git_branch: &'static str,
    pub git_dirty: bool,
    pub build_time: &'static str,
    pub build_profile: &'static str,
}

pub fn info() -> Info {
    Info {
        version: VERSION,
        git_sha: GIT_SHA,
        git_branch: GIT_BRANCH,
        git_dirty: GIT_DIRTY == "1",
        build_time: BUILD_TIME,
        build_profile: BUILD_PROFILE,
    }
}

/// `thclaws 0.1.0 (abcd1234+dirty · release · 2026-04-14T12:00:00Z)`
pub fn one_line() -> String {
    let info = info();
    let dirty_tag = if info.git_dirty { "+dirty" } else { "" };
    format!(
        "thclaws {} ({}{} · {} · {})",
        info.version, info.git_sha, dirty_tag, info.build_profile, info.build_time
    )
}
