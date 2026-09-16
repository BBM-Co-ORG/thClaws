//! Small cross-cutting utilities that don't belong to any single
//! subsystem. Currently: cross-platform home-directory lookup.

use std::path::PathBuf;

// for Windows creation flag to hide the console window
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// The current user's home directory, in a form that works on both
/// Unix and Windows.
///
/// On Unix this is just `$HOME`. On Windows there's no `HOME` by
/// default — we fall back to `%USERPROFILE%` (set by Explorer and
/// the user profile loader on every login) and then
/// `%HOMEDRIVE%%HOMEPATH%` (used by some older tooling).
///
/// Returns `None` only if every candidate is unset or empty — in
/// practice a truly broken Windows environment; most of the
/// path-touching code in thClaws degrades gracefully in that case
/// rather than panicking.
/// The desktop's startup folder picker, answered.
///
/// Picking a folder outside the current one re-execs the app there, and the
/// "already asked" flag lives in memory, so the new process asked again and
/// the answer re-execed again: an endless picker (#211). The answer travels
/// in the environment, which both `exec` (unix) and a spawned child (Windows)
/// inherit.
const PICKER_ANSWERED: &str = "THCLAWS_PICKER_ANSWERED";

/// Did this process chain already get an answer to the folder picker?
pub fn picker_answered() -> bool {
    std::env::var(PICKER_ANSWERED).ok().as_deref() == Some("1")
}

/// Record the answer so the re-exec does not ask again.
pub fn mark_picker_answered() {
    std::env::set_var(PICKER_ANSWERED, "1");
}

/// Whether a launch still has to be told which folder it is for, and so must
/// write nothing into the one it happens to be standing in. True only for a
/// desktop launch whose picker is unanswered — the single case where the cwd
/// is the OS's choice rather than the user's (an icon launch starts in
/// Documents on Windows). Every other surface — `--cli`, `-p`, `--serve` —
/// reached its folder by `cd`-ing there, which is the user choosing it.
pub fn folder_undecided(opens_desktop: bool) -> bool {
    opens_desktop && !picker_answered()
}

/// Is `candidate` the same folder as `root`, or inside it?
///
/// Both sides are resolved the same way before comparing, which is the whole
/// point: the folder picker decides "open in place" or "restart there" from
/// this answer (#211), and a mismatch of *form* between two paths that name the
/// same place sent picks down the restart branch.
///
/// Two forms bite. Windows `canonicalize` returns a verbatim path
/// (`\\?\C:\dir`), while a path it cannot canonicalize keeps `C:\dir`. And a
/// folder that does not exist yet — one typed into the picker — cannot be
/// canonicalized at all, while its root can, so on macOS the root resolves
/// through the `/var` → `/private/var` symlink and the child does not. So:
/// canonicalize the deepest ancestor that exists, put the rest back, and strip
/// the verbatim prefix.
pub fn is_inside(root: &std::path::Path, candidate: &std::path::Path) -> bool {
    fn resolved(p: &std::path::Path) -> PathBuf {
        let mut missing: Vec<std::ffi::OsString> = Vec::new();
        let mut cur = p.to_path_buf();
        loop {
            if let Ok(c) = cur.canonicalize() {
                let mut out = PathBuf::from(strip_verbatim_prefix(&c.to_string_lossy()));
                for part in missing.iter().rev() {
                    out.push(part);
                }
                return out;
            }
            let Some(name) = cur.file_name().map(|n| n.to_os_string()) else {
                // Nothing on this path exists (a Windows path on unix, say);
                // compare the forms as given, minus the verbatim prefix.
                return PathBuf::from(strip_verbatim_prefix(&p.to_string_lossy()));
            };
            missing.push(name);
            if !cur.pop() {
                return PathBuf::from(strip_verbatim_prefix(&p.to_string_lossy()));
            }
        }
    }
    // Compared as normalised strings, not `Path::starts_with`: on unix a
    // Windows path is a single component (nothing splits on `\`), so the
    // component-wise compare answers "outside" for a path plainly inside — and
    // the Windows forms this exists for could then only be tested on Windows,
    // which is the one platform we cannot test before releasing.
    fn norm(p: &std::path::Path) -> String {
        let s = resolved(p).to_string_lossy().replace('\\', "/");
        let trimmed = s.trim_end_matches('/');
        let out = if trimmed.is_empty() { "/" } else { trimmed };
        // Windows path comparison is case-insensitive, drive letters included.
        if cfg!(windows) {
            out.to_lowercase()
        } else {
            out.to_string()
        }
    }
    let (root, candidate) = (norm(root), norm(candidate));
    candidate == root || candidate.starts_with(&format!("{root}/"))
}

/// #210: write a line to stderr, and shrug if it fails.
///
/// `eprintln!` panics when the write returns an error. An agent's stderr is a
/// pipe to the workspace host, so the moment the host goes the next log line
/// takes the agent down with it. Anything that logs while shutting down uses
/// this instead.
pub fn log_line(msg: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{msg}");
}

/// Is this panic just a failed print? std panics with
/// `failed printing to stdout/stderr: ...` when the stream is gone (#210).
pub fn panic_is_print_failure(text: &str) -> bool {
    text.contains("failed printing to")
}

/// Keep a panic where a dead stderr cannot swallow it: append it to
/// `<workspace>/.thclaws/state/logs/panic.log`. Best effort by design — a
/// panic handler that can itself fail is how #210 became an abort.
pub fn write_panic_log(text: &str) {
    use std::io::Write as _;
    let dir = crate::workdir::workspace_root().join(".thclaws/state/logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("panic.log"))
    else {
        return;
    };
    let _ = writeln!(f, "{} {}", chrono::Utc::now().to_rfc3339(), text);
}

pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Ok(h) = std::env::var("USERPROFILE") {
            if !h.is_empty() {
                return Some(PathBuf::from(h));
            }
        }
        if let (Ok(d), Ok(p)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
            if !d.is_empty() && !p.is_empty() {
                return Some(PathBuf::from(format!("{d}{p}")));
            }
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// String form of `home_dir()` — mirrors the shape of call sites
/// that did `std::env::var("HOME").ok()?` and then used the result
/// as a `&str` / joined paths via `format!`. Prefer `home_dir()`
/// when you want a `PathBuf` directly.
pub fn home_string() -> Option<String> {
    home_dir().map(|p| p.to_string_lossy().into_owned())
}

/// Render a proportional progress bar. Example:
/// `[████████▓░░░░░░░░░░░░░░░]` for 35% over 24 cells. Half-step `▓`
/// for fractional fills. ANSI-colored: green <60%, yellow 60–80%,
/// red ≥80%.
pub fn progress_bar(pct: f64, width: usize) -> String {
    let clamped = pct.clamp(0.0, 100.0);
    let filled_f = clamped / 100.0 * width as f64;
    let full = filled_f.floor() as usize;
    let frac = filled_f - full as f64;
    let half = if frac >= 0.5 && full < width { 1 } else { 0 };
    let empty = width - full - half;
    let color = if clamped >= 80.0 {
        "\x1b[31m"
    } else if clamped >= 60.0 {
        "\x1b[33m"
    } else {
        "\x1b[32m"
    };
    let reset = "\x1b[0m";
    format!(
        "[{color}{}{}{reset}{}]",
        "█".repeat(full),
        "▓".repeat(half),
        "░".repeat(empty),
    )
}

/// Byte size in human units: `512`→`"512 B"`, `2048`→`"2.0 KB"`,
/// `5_500_000`→`"5.2 MB"`.
pub fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{} B", n)
    }
}

/// Abbreviate token counts: `200000`→`"200k"`, `1_200_000`→`"1.2M"`.
pub fn format_tokens(n: usize) -> String {
    if n >= 1_000_000 {
        let f = n as f64 / 1_000_000.0;
        if (f.round() - f).abs() < 0.05 {
            format!("{}M", f.round() as u64)
        } else {
            format!("{:.1}M", f)
        }
    } else if n >= 1_000 {
        let f = n as f64 / 1_000.0;
        if (f.round() - f).abs() < 0.05 {
            format!("{}k", f.round() as u64)
        } else {
            format!("{:.1}k", f)
        }
    } else {
        n.to_string()
    }
}

/// Absolutize a path for baking into a child process's argv.
///
/// `canonicalize()` alone is not enough on Windows: it ALWAYS returns a
/// verbatim (extended-length) path — `\\?\C:\Users\…`, or
/// `\\?\UNC\server\share` for a network path. Verbatim paths disable
/// Win32 path normalization, so a child that joins or compares them the
/// ordinary way silently fails every file operation — no error, just
/// nothing written. That is issue #200: a teammate spawned with a
/// verbatim `--team-dir` never wrote its status, so the lead reported
/// "launched but never booted".
///
/// Falls back to `current_dir().join(p)` when canonicalize fails (the
/// directory may not exist yet on a first team session). No-op on Unix
/// beyond canonicalize.
pub fn absolutize_for_child(p: &std::path::Path) -> String {
    let abs = p.canonicalize().unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    });
    let s = abs.to_string_lossy().into_owned();
    strip_verbatim_prefix(&s)
}

/// Strip Windows' extended-length (`\\?\`) prefix from a path string.
/// Kept separate from [`absolutize_for_child`] so it is unit-testable on
/// every platform — the string transform has no OS dependency, and the
/// bug it fixes can only be reproduced on Windows.
pub fn strip_verbatim_prefix(path: &str) -> String {
    // `\\?\UNC\server\share` is a network path — the replacement is
    // `\\server\share`, NOT `server\share`, which would be relative.
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match path.strip_prefix(r"\\?\") {
        Some(rest) => rest.to_string(),
        None => path.to_string(),
    }
}

/// Build a sync `std::process::Command` that runs `program` DIRECTLY —
/// no shell in between.
///
/// Prefer this over [`shell_command_sync`] whenever the caller already
/// knows the program and its arguments: a shell string has to be quoted,
/// and there is no quoting that works on both `/bin/sh` and `cmd.exe`.
/// POSIX `'…'` quoting is literal text to cmd.exe, and cmd.exe has no
/// equivalent of the `VAR=value cmd` env prefix at all — which is why
/// SpawnTeammate could never launch a teammate on Windows (#200).
///
/// Applies the same Windows treatment as `shell_command_sync`: the
/// `python3` shim on PATH, and the flag that suppresses a console window.
pub fn direct_command(program: &str) -> std::process::Command {
    // `mut` is only exercised by the Windows block below.
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut c = std::process::Command::new(program);

    #[cfg(target_os = "windows")]
    {
        if let Some(dir) = windows_python_shim_dir() {
            let path = std::env::var("PATH").unwrap_or_default();
            c.env("PATH", format!("{};{}", dir.display(), path));
        }
        // hide the console window
        c.creation_flags(0x08000000);
    }

    c
}

/// Build a sync `std::process::Command` that runs a shell-string in
/// the platform's default shell. On Windows this is `cmd.exe /C
/// <cmd>`; on Unix it's `/bin/sh -c <cmd>`. Centralized here so the
/// 4+ tool / hook / team / repl call sites don't each repeat the
/// `cfg!(windows)` branch.
///
/// Caveats: bash-syntax commands the agent emits (`find . -name
/// '*.rs'`, complex pipelines, `&&` chains with single-quoted args)
/// may not parse identically under cmd.exe. Power users can override
/// with the `THCLAWS_SHELL` env var (path to a shell + flag pair like
/// `bash -c`) — see [`shell_command_sync`] / [`shell_command_async`]
/// for the override path.
pub fn shell_command_sync(command: &str) -> std::process::Command {
    let (shell, flag) = shell_invocation();
    let mut c = std::process::Command::new(shell);
    c.arg(flag).arg(command);

    #[cfg(target_os = "windows")]
    {
        if let Some(dir) = windows_python_shim_dir() {
            let path = std::env::var("PATH").unwrap_or_default();
            c.env("PATH", format!("{};{}", dir.display(), path));
        }
        // hide the console window
        c.creation_flags(0x08000000);
    }

    c
}

/// Windows ships `py` (the launcher) and `python`, but not a real `python3` —
/// the bare `python3` name resolves to a Microsoft Store stub that just opens
/// the Store. Agents invoke `python3 …` (works on Linux/macOS), so on Windows we
/// shadow it with a tiny `.bat` shim that forwards to a real interpreter and
/// prepend the shim dir to the child's PATH. Detection + write happen once
/// (cached); returns `None` when no Python is present (nothing to shim).
#[cfg(target_os = "windows")]
fn windows_python_shim_dir() -> Option<&'static std::path::Path> {
    use std::os::windows::process::CommandExt;
    use std::sync::OnceLock;
    static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let on_path = |name: &str| -> bool {
            std::process::Command::new("where")
                .arg(name)
                .creation_flags(0x08000000)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        let (py_fwd, pip_fwd) = if on_path("py") {
            ("py -3", "py -3 -m pip")
        } else if on_path("python") {
            ("python", "python -m pip")
        } else {
            return None;
        };
        let dir = std::env::temp_dir().join("thclaws-python-shim");
        if std::fs::create_dir_all(&dir).is_err() {
            return None;
        }
        let _ = std::fs::write(
            dir.join("python3.bat"),
            format!("@echo off\r\n{py_fwd} %*\r\n"),
        );
        let _ = std::fs::write(
            dir.join("pip3.bat"),
            format!("@echo off\r\n{pip_fwd} %*\r\n"),
        );
        Some(dir)
    })
    .as_deref()
}

/// Re-execute the current thclaws binary in place. Used by the
/// `/reload` slash command to drop in-memory state (MCP handles,
/// system prompt, skill caches, etc.) without needing an external
/// supervisor — on-disk sessions survive, so the user can resume.
///
/// Behaviour by platform:
/// - **Unix**: `execv` the same binary with the same argv. The
///   process keeps its PID, all in-memory state is replaced. Only
///   returns on error.
/// - **Windows**: no `execv`; spawn a fresh process with the same
///   command line, then exit. The new process gets a new PID.
///
/// Mostly equivalent to a `/v1/restart` on the pod side, but
/// without involving Kubernetes — useful on a laptop where there's
/// no supervisor to bring the process back.
pub fn reexec_self() -> std::io::Error {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return e,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();

    // #211: the chosen folder rides on the working directory, so state it
    // rather than leaning on inheritance — on Windows this is a fresh spawn,
    // not an `exec`.
    let cwd = std::env::current_dir().ok();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new(&exe);
        c.args(&args);
        if let Some(dir) = &cwd {
            c.current_dir(dir);
        }
        // exec() only returns on failure; on success the current
        // process image is replaced.
        c.exec()
    }
    #[cfg(not(unix))]
    {
        let mut c = std::process::Command::new(&exe);
        c.args(&args);
        if let Some(dir) = &cwd {
            c.current_dir(dir);
        }
        let res = c.spawn();
        match res {
            Ok(_) => {
                std::process::exit(0);
            }
            Err(e) => e,
        }
    }
}

/// Async variant for tokio-based call sites (currently the Bash
/// tool). Same shell-resolution logic as [`shell_command_sync`].
pub fn shell_command_async(command: &str) -> tokio::process::Command {
    let (shell, flag) = shell_invocation();
    let mut c = tokio::process::Command::new(shell);
    c.arg(flag).arg(command);

    #[cfg(target_os = "windows")]
    {
        // Agents invoke `python3`, which Windows doesn't ship (bare `python3`
        // is a Store stub). Prepend a shim that forwards to `py -3` / `python`.
        if let Some(dir) = windows_python_shim_dir() {
            let path = std::env::var("PATH").unwrap_or_default();
            c.env("PATH", format!("{};{}", dir.display(), path));
        }
        // hide the console window
        c.creation_flags(0x08000000);
    }

    c
}

/// Resolve `(shell, flag)` for the current host. Honors
/// `THCLAWS_SHELL` for power-user overrides — set it to a single
/// string like `"bash -c"` or `"pwsh -Command"` and we split on
/// whitespace; the first token is the executable, the second is the
/// flag.
pub(crate) fn shell_invocation() -> (String, String) {
    if let Ok(s) = std::env::var("THCLAWS_SHELL") {
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() == 2 {
            return (parts[0].to_string(), parts[1].to_string());
        }
    }
    if cfg!(windows) {
        ("cmd".to_string(), "/C".to_string())
    } else {
        ("/bin/sh".to_string(), "-c".to_string())
    }
}

#[cfg(test)]
mod tests {
    /// #211: the answer to the startup folder picker has to survive the
    /// re-exec that opening another folder performs, or the new process asks
    /// again and every answer restarts the app.
    ///
    /// The same answer gates every first-run write. A desktop opened from an
    /// icon starts in a folder the OS chose — Documents on Windows — and
    /// writing `.thclaws/` there makes `looks_like_v2_agent` true, so the next
    /// launch offers to upgrade the user's Documents. Both live on one flag,
    /// so they are pinned in one test rather than two racing over the env var.
    #[test]
    fn the_picker_answer_survives_a_restart() {
        let _g = crate::kms::test_env_lock();
        let prev = std::env::var("THCLAWS_PICKER_ANSWERED").ok();

        std::env::remove_var("THCLAWS_PICKER_ANSWERED");
        assert!(!picker_answered(), "a fresh launch asks");
        assert!(
            folder_undecided(true),
            "a desktop launch writes nothing until the picker is answered"
        );
        assert!(
            !folder_undecided(false),
            "--cli / -p / --serve chose their folder by cd-ing into it"
        );
        mark_picker_answered();
        assert!(picker_answered(), "and the restart does not");
        assert!(
            !folder_undecided(true),
            "an answered picker releases the first-run writes"
        );
        std::env::set_var("THCLAWS_PICKER_ANSWERED", "0");
        assert!(!picker_answered(), "only \"1\" counts");
        assert!(folder_undecided(true), "and holds them again");

        match prev {
            Some(v) => std::env::set_var("THCLAWS_PICKER_ANSWERED", v),
            None => std::env::remove_var("THCLAWS_PICKER_ANSWERED"),
        }
    }

    /// A folder inside the workspace opens in place; one outside restarts the
    /// app there. Windows canonicalizes to a verbatim path, so the two sides
    /// of that comparison can arrive in different forms (#211).
    #[test]
    fn inside_survives_windows_verbatim_paths() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(is_inside(root.path(), &nested));
        assert!(is_inside(root.path(), root.path()));

        let outside = tempfile::tempdir().unwrap();
        assert!(!is_inside(root.path(), outside.path()));

        // A sibling whose name merely starts with the root's name is outside.
        let sibling = root.path().with_file_name(format!(
            "{}-more",
            root.path().file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(!is_inside(root.path(), &sibling));
        let _ = std::fs::remove_dir_all(&sibling);

        // A folder that does not exist cannot be canonicalized; it must still
        // compare against a canonicalized root rather than reading as outside.
        assert!(is_inside(root.path(), &root.path().join("not-yet")));

        // The verbatim forms Windows produces, compared with plain ones.
        let verbatim = std::path::Path::new("\\\\?\\C:\\projects\\book");
        let plain_root = std::path::Path::new("C:\\projects");
        let plain_other = std::path::Path::new("C:\\other");
        assert!(is_inside(plain_root, verbatim));
        assert!(!is_inside(plain_other, verbatim));
        let unc = std::path::Path::new("\\\\?\\UNC\\server\\share\\dir");
        assert!(is_inside(std::path::Path::new("\\\\server\\share"), unc));
    }

    /// #210: std panics with "failed printing to stderr" when the pipe is
    /// gone. That panic must be told apart from a real one, or the agent
    /// aborts while its host is simply shutting down.
    #[test]
    fn a_failed_print_is_recognised_and_a_real_panic_is_not() {
        assert!(panic_is_print_failure(
            "failed printing to stderr: Broken pipe (os error 32)"
        ));
        assert!(panic_is_print_failure("failed printing to stdout: x"));
        assert!(!panic_is_print_failure("index out of bounds: the len is 3"));
        assert!(!panic_is_print_failure(""));
    }

    /// A panic caused by a dead stderr is invisible unless it is written
    /// somewhere else.
    #[test]
    fn a_panic_is_written_under_the_workspace() {
        let _g = crate::kms::test_env_lock();
        let prev = std::env::var("THCLAWS_WORKSPACE_ROOT").ok();
        let ws = tempfile::tempdir().unwrap();
        std::env::set_var("THCLAWS_WORKSPACE_ROOT", ws.path());

        write_panic_log("panic at src/x.rs:1:1: boom");
        write_panic_log("panic at src/x.rs:2:2: again");
        let log = ws.path().join(".thclaws/state/logs/panic.log");
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(body.contains("boom") && body.contains("again"), "{body}");
        assert_eq!(body.lines().count(), 2, "appended, not overwritten");

        match prev {
            Some(v) => std::env::set_var("THCLAWS_WORKSPACE_ROOT", v),
            None => std::env::remove_var("THCLAWS_WORKSPACE_ROOT"),
        }
    }

    /// Logging must never be the thing that kills the process.
    #[test]
    fn log_line_survives_a_stderr_it_cannot_write_to() {
        log_line("a line that may or may not land");
    }

    use super::*;

    #[test]
    fn home_dir_returns_something_on_dev_machine() {
        // Dev machines set HOME (Unix) or USERPROFILE (Windows). In
        // CI this could fail if a sandboxed runner strips env — we
        // allow `None` there, but don't crash.
        let _ = home_dir();
    }

    /// Issue #200: Rust's `canonicalize()` on Windows ALWAYS returns a
    /// verbatim path. A teammate handed `\\?\C:\…` as `--team-dir` wrote
    /// nothing — no error, just a status stuck on "spawning".
    #[test]
    fn strip_verbatim_prefix_unwraps_windows_extended_paths() {
        assert_eq!(
            strip_verbatim_prefix(r"\\?\C:\Users\dev\proj\.thclaws\team"),
            r"C:\Users\dev\proj\.thclaws\team"
        );
        // A UNC path must stay UNC — `server\share` alone is relative.
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\server\share\team"),
            r"\\server\share\team"
        );
        // Already-plain paths pass through untouched, on every platform.
        assert_eq!(strip_verbatim_prefix(r"C:\Users\dev"), r"C:\Users\dev");
        assert_eq!(strip_verbatim_prefix("/home/dev/proj"), "/home/dev/proj");
        assert_eq!(strip_verbatim_prefix(""), "");
    }

    /// The two team_dir call sites (spawn argv and the `kill_my_teammates`
    /// matcher) must agree, so both go through this helper.
    #[test]
    fn absolutize_for_child_is_absolute_and_not_verbatim() {
        let cwd = std::env::current_dir().expect("cwd");
        let got = absolutize_for_child(&cwd);
        assert!(!got.starts_with(r"\\?\"), "verbatim prefix leaked: {got}");
        assert!(
            std::path::Path::new(&got).is_absolute(),
            "not absolute: {got}"
        );
    }

    #[test]
    fn shell_invocation_picks_platform_default() {
        // Clear any THCLAWS_SHELL override so we test the default.
        let saved = std::env::var("THCLAWS_SHELL").ok();
        std::env::remove_var("THCLAWS_SHELL");
        let (shell, flag) = shell_invocation();
        if cfg!(windows) {
            assert_eq!(shell, "cmd");
            assert_eq!(flag, "/C");
        } else {
            assert_eq!(shell, "/bin/sh");
            assert_eq!(flag, "-c");
        }
        if let Some(v) = saved {
            std::env::set_var("THCLAWS_SHELL", v);
        }
    }

    #[test]
    fn thclaws_shell_override_works() {
        let saved = std::env::var("THCLAWS_SHELL").ok();
        std::env::set_var("THCLAWS_SHELL", "bash -c");
        let (shell, flag) = shell_invocation();
        assert_eq!(shell, "bash");
        assert_eq!(flag, "-c");
        std::env::remove_var("THCLAWS_SHELL");
        if let Some(v) = saved {
            std::env::set_var("THCLAWS_SHELL", v);
        }
    }
}
