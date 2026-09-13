//! docs/browser Phase 2 slice 3 — engine-owned Chromium + CDP attachment.
//!
//! Inverts browser ownership: instead of letting the managed
//! playwright-mcp server launch its own Chromium, the ENGINE launches
//! Chromium with a DevTools port and hands playwright-mcp a
//! `--cdp-endpoint` so the agent's tools drive the same browser. The
//! engine then attaches its own CDP session as the human-facing wire:
//!
//!   - `Page.startScreencast` → live JPEG frames into the Browser tab
//!     (replaces the ~1 fps click-through screenshots in takeover)
//!   - `Input.dispatchMouseEvent` / `Input.insertText` /
//!     `dispatchKeyEvent` → native-feeling click / type / scroll
//!     (whole strings in one shot — no more per-character press_key)
//!   - `Runtime.consoleAPICalled` / `exceptionThrown` → live console
//!     lines in the activity feed
//!
//! Graceful fallback is the design invariant: if Chromium can't be
//! found or launched, `ensure_chromium` returns `None`, playwright-mcp
//! self-launches exactly as before, and the Browser tab keeps the
//! screenshot + MCP-input path. Nothing regresses.
//!
//! Threading model: one private tokio runtime (1 worker) owns every
//! CDP websocket task. Public API is synchronous and must be called
//! from NON-tokio threads (the IPC layer's std::thread workers) — it
//! `block_on`s the private runtime. The MCP bootstrap calls only
//! `ensure_chromium`, which is plain blocking code, via
//! `spawn_blocking`.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

type Dispatch = Arc<dyn Fn(String) + Send + Sync>;

// ── Singleton state ──────────────────────────────────────────────────

struct CdpState {
    /// `http://127.0.0.1:<port>` — what playwright-mcp's
    /// `--cdp-endpoint` takes. Fixed at arm time (port reserved
    /// up-front) so the MCP server can be spawned with the endpoint
    /// BEFORE Chromium exists.
    endpoint: String,
    port: u16,
    headless: bool,
    /// Chromium is launched lazily — on the first browser tool call
    /// or takeover/screencast start — so a headed desktop doesn't pop
    /// a Chrome window at app start and an idle cloud pod doesn't pay
    /// ~150 MB for a browser nobody used.
    launched: bool,
    /// `None` when we re-attached to a Chromium a previous engine
    /// process launched (it survived the restart — sessions intact).
    child: Option<std::process::Child>,
}

static STATE: OnceLock<Mutex<Option<CdpState>>> = OnceLock::new();
static PAGE: OnceLock<Mutex<Option<Arc<PageSession>>>> = OnceLock::new();

fn state() -> &'static Mutex<Option<CdpState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn page_slot() -> &'static Mutex<Option<Arc<PageSession>>> {
    PAGE.get_or_init(|| Mutex::new(None))
}

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("browser-cdp")
            .enable_all()
            .build()
            .expect("browser-cdp runtime")
    })
}

/// Whether CDP mode is armed (endpoint reserved; Chromium launches
/// lazily on first use). The Browser tab uses this to decide
/// screencast vs screenshot mode.
pub fn cdp_active() -> bool {
    state().lock().unwrap().is_some()
}

// ── Chromium discovery + launch ──────────────────────────────────────

/// Find a Chromium/Chrome executable, in order of preference:
/// 1. `THCLAWS_BROWSER_EXECUTABLE` (explicit override)
/// 2. `PLAYWRIGHT_BROWSERS_PATH` classic layout (`chromium-<rev>/…`) —
///    the cloud runner image (`/ms-playwright`)
/// 3. the default playwright cache in the same classic layout
/// 4. branded Chrome / Chromium at well-known OS paths (what
///    playwright-mcp's default `chrome` channel uses on desktops)
pub fn find_chromium() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("THCLAWS_BROWSER_EXECUTABLE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("PLAYWRIGHT_BROWSERS_PATH") {
        roots.push(PathBuf::from(p));
    }
    if let Some(home) = crate::util::home_dir() {
        if cfg!(target_os = "macos") {
            roots.push(home.join("Library/Caches/ms-playwright"));
        } else {
            roots.push(home.join(".cache/ms-playwright"));
        }
    }
    for root in roots {
        if let Some(exe) = newest_classic_chromium(&root) {
            return Some(exe);
        }
    }

    // Branded fallbacks (Google Chrome / Edge / system chromium) are
    // OPT-IN for the engine-managed CDP path. Driving a *branded*
    // browser over CDP for the live view is unreliable in the field —
    // playwright-mcp can fail a session with "protocol error: Browser
    // context management is not supported" — whereas letting
    // playwright-mcp launch the browser ITSELF (no `--cdp-endpoint`) is
    // rock-solid. So when only a branded browser is available we return
    // None: `arm()` bails, no `--cdp-endpoint` is injected, and
    // playwright-mcp self-launches (reliable). The live view / takeover
    // needs Playwright's own Chromium (`npx playwright install
    // chromium`). Opt back into branded-over-CDP with
    // `THCLAWS_BROWSER_ALLOW_BRANDED=1`.
    if std::env::var("THCLAWS_BROWSER_ALLOW_BRANDED")
        .ok()
        .as_deref()
        != Some("1")
    {
        return None;
    }
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ]
    } else {
        &[
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
        ]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Classic ms-playwright layout: `<root>/chromium-<rev>/<platform>/…`.
/// Picks the highest revision that has a real executable.
fn newest_classic_chromium(root: &std::path::Path) -> Option<PathBuf> {
    let mut revs: Vec<(u64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(rev) = name.strip_prefix("chromium-") {
            if let Ok(n) = rev.parse::<u64>() {
                revs.push((n, entry.path()));
            }
        }
    }
    revs.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
    for (_, dir) in revs {
        let subpaths: &[&str] = if cfg!(target_os = "macos") {
            &[
                "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
                "chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium",
            ]
        } else if cfg!(target_os = "windows") {
            &["chrome-win/chrome.exe", "chrome-win64/chrome.exe"]
        } else {
            &["chrome-linux64/chrome", "chrome-linux/chrome"]
        };
        for sp in subpaths {
            let exe = dir.join(sp);
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// Arm CDP mode at MCP bootstrap: decide the DevTools endpoint NOW
/// (so playwright-mcp can be spawned with `--cdp-endpoint`), but defer
/// the actual Chromium launch to [`ensure_up`] — first browser tool
/// call or takeover. Re-attaches to a still-running Chromium from a
/// previous engine process when possible. Returns `None` (one stderr
/// note) when no Chromium executable exists → caller falls back to
/// MCP self-launch.
pub fn arm(headless: bool) -> Option<String> {
    if let Some(s) = state().lock().unwrap().as_ref() {
        return Some(s.endpoint.clone());
    }

    // A previous engine process may have left its Chromium running —
    // the profile dir records the DevTools endpoint. We must NOT
    // re-attach to it: a fresh playwright-mcp connecting over CDP into a
    // browser the previous run's playwright-mcp already drove fails
    // every time with "Browser context management is not supported"
    // (the exit-then-relaunch bug). Cookies/sessions live in the
    // persistent --user-data-dir and survive a relaunch on their own,
    // so reap the orphan and start clean. (Reap touches the network and
    // sleeps, so do it with the state lock released.)
    let profile = profile_dir();
    let endpoint_file = profile.join("devtools-endpoint");
    if let Ok(saved) = std::fs::read_to_string(&endpoint_file) {
        let saved = saved.trim().to_string();
        if !saved.is_empty() && endpoint_alive(&saved) {
            eprintln!("\x1b[2m[browser-cdp] reaping orphaned chromium at {saved}\x1b[0m");
            reap_orphan(&saved, &profile);
        }
        let _ = std::fs::remove_file(&endpoint_file);
    }
    let _ = std::fs::remove_file(profile.join("chromium.pid"));

    // Chromium must exist for CDP mode to be worth arming.
    if find_chromium().is_none() {
        eprintln!(
            "\x1b[2m[browser-cdp] no chromium executable found — live view off, playwright-mcp will launch its own browser\x1b[0m"
        );
        return None;
    }

    // Reserve a free port by binding and immediately releasing it —
    // tiny race window, acceptable: a collision surfaces as a launch
    // failure and the takeover toggle retries.
    let port = match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l.local_addr().ok().map(|a| a.port()),
        Err(_) => None,
    }?;
    let endpoint = format!("http://127.0.0.1:{port}");
    let mut guard = state().lock().unwrap();
    // Another caller may have armed while the lock was released for the
    // reap above — honor the winner.
    if let Some(s) = guard.as_ref() {
        return Some(s.endpoint.clone());
    }
    *guard = Some(CdpState {
        endpoint: endpoint.clone(),
        port,
        headless,
        launched: false,
        child: None,
    });
    Some(endpoint)
}

/// Reap a Chromium left running by a previous engine process. Graceful
/// CDP `Browser.close` first (flushes cookies and releases the profile's
/// single-instance lock cleanly), then SIGKILL by the saved PID as a
/// fallback, then clear stale `Singleton*` locks so a fresh launch on
/// the same profile won't just forward-and-exit. Best-effort throughout.
fn reap_orphan(endpoint: &str, profile: &std::path::Path) {
    let _ = rt().block_on(close_remote_browser(endpoint));
    wait_until_dead(endpoint, std::time::Duration::from_secs(4));
    if endpoint_alive(endpoint) {
        if let Ok(pid) = std::fs::read_to_string(profile.join("chromium.pid")) {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                hard_kill(pid);
            }
        }
        wait_until_dead(endpoint, std::time::Duration::from_secs(3));
    }
    // Only safe to clear the locks once the owner is actually gone —
    // removing them under a live instance risks profile corruption.
    if endpoint_alive(endpoint) {
        eprintln!(
            "\x1b[2m[browser-cdp] orphan at {endpoint} would not die — fresh launch may fail until it's killed manually\x1b[0m"
        );
    } else {
        for lock in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            let _ = std::fs::remove_file(profile.join(lock));
        }
    }
}

fn wait_until_dead(endpoint: &str, budget: std::time::Duration) {
    let deadline = std::time::Instant::now() + budget;
    while endpoint_alive(endpoint) {
        if std::time::Instant::now() > deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// Ask a running Chromium to shut down gracefully over its browser-level
/// DevTools websocket (`webSocketDebuggerUrl` from `/json/version`).
async fn close_remote_browser(endpoint: &str) -> Result<(), String> {
    use futures::{SinkExt, StreamExt};
    let body = reqwest::get(format!("{endpoint}/json/version"))
        .await
        .map_err(|e| format!("cdp /json/version: {e}"))?
        .text()
        .await
        .map_err(|e| format!("cdp /json/version body: {e}"))?;
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("cdp version parse: {e}"))?;
    let ws = v
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or("no browser ws url")?;
    let (mut sock, _) = tokio_tungstenite::connect_async(ws)
        .await
        .map_err(|e| format!("cdp connect: {e}"))?;
    sock.send(tokio_tungstenite::tungstenite::Message::Text(
        r#"{"id":1,"method":"Browser.close"}"#.to_string().into(),
    ))
    .await
    .map_err(|e| format!("cdp Browser.close: {e}"))?;
    // Let chromium act on the close before the socket drops.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), sock.next()).await;
    Ok(())
}

#[cfg(unix)]
fn hard_kill(pid: i32) {
    // SAFETY: kill(2) with a normal signal is always safe to call; a
    // dead/invalid pid simply returns ESRCH.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn hard_kill(_pid: i32) {}

/// Launch Chromium if it isn't up yet (lazy half of [`arm`]).
/// Blocking — call via `spawn_blocking` from async contexts. Cheap
/// fast-path when already launched.
pub fn ensure_up() -> Result<(), String> {
    let (port, headless, endpoint) = {
        let guard = state().lock().unwrap();
        let s = guard.as_ref().ok_or("CDP mode not armed")?;
        if s.launched {
            return Ok(());
        }
        (s.port, s.headless, s.endpoint.clone())
    };

    let exe = find_chromium().ok_or("no chromium executable")?;
    let profile = profile_dir();
    let endpoint_file = profile.join("devtools-endpoint");

    // Profile OUTSIDE the workspace on purpose: cookies/sessions must
    // never ride along when a workspace folder is published as an
    // agent or synced. Keyed by cwd so two workspaces don't share
    // logins.
    let _ = std::fs::create_dir_all(&profile);

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if headless {
        cmd.arg("--headless=new");
    }
    // Containerized chromium has no userns for its own sandbox — the
    // pod is the sandbox (same reasoning as the runner image's
    // playwright-mcp flags).
    if in_container() {
        cmd.arg("--no-sandbox");
        // The profile lives on the workspace PVC in containers — keep
        // it cookies/storage-only by pushing the (large, regenerable)
        // disk cache onto ephemeral /tmp.
        cmd.arg("--disk-cache-dir=/tmp/thclaws-browser-cache");
    }
    cmd.arg("about:blank");

    let child = cmd
        .spawn()
        .map_err(|e| format!("launch {}: {e}", exe.display()))?;

    // Record the pid so a future engine process can SIGKILL this
    // chromium if it's orphaned and won't close gracefully over CDP.
    let _ = std::fs::write(profile.join("chromium.pid"), child.id().to_string());

    // The port is ours, so poll the HTTP endpoint instead of parsing
    // stderr — robust across chromium variants and locales.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !endpoint_alive(&endpoint) {
        if std::time::Instant::now() > deadline {
            return Err("timed out waiting for DevTools endpoint".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    eprintln!(
        "\x1b[2m[browser-cdp] chromium up ({}) — devtools {endpoint}\x1b[0m",
        if headless { "headless" } else { "headed" }
    );
    let _ = std::fs::write(&endpoint_file, &endpoint);
    {
        let mut guard = state().lock().unwrap();
        if let Some(s) = guard.as_mut() {
            s.launched = true;
            s.child = Some(child);
        }
    }

    // Cookie durability (docs/browser): chromium flushes its on-disk
    // cookie store only on a ~30s timer, so an abrupt pod kill within
    // that window loses a just-completed login. We snapshot cookies to
    // a JSON file via CDP on a short timer (and restore on launch),
    // closing the window independently of chromium's flush schedule.
    // The file lives inside browser-profile/, which the publish packer
    // strips — cookies never leak into a shared agent.
    let endpoint_for_cookies = endpoint.clone();
    rt().spawn(async move {
        // Restore first (merge over whatever chromium loaded from its
        // own SQLite store — newest wins per name/domain/path).
        if let Err(e) = restore_cookies(&endpoint_for_cookies).await {
            eprintln!("\x1b[2m[browser-cdp] cookie restore: {e}\x1b[0m");
        }
        // Then snapshot periodically while this chromium lives.
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            if !cdp_active() {
                break;
            }
            let _ = snapshot_cookies(&endpoint_for_cookies).await;
        }
    });
    Ok(())
}

fn cookies_path() -> PathBuf {
    profile_dir().join("thclaws-cookies.json")
}

/// Browser-level DevTools websocket (not a page) — for the Storage
/// domain cookie methods, which are browser-scoped.
async fn browser_ws_url(endpoint: &str) -> Result<String, String> {
    let body = reqwest::get(format!("{endpoint}/json/version"))
        .await
        .map_err(|e| format!("cdp /json/version: {e}"))?
        .text()
        .await
        .map_err(|e| format!("cdp /json/version body: {e}"))?;
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("parse: {e}"))?;
    v.get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| "no browser ws url".to_string())
}

/// One-shot CDP request on the browser-level websocket. Opens, sends,
/// reads the matching reply, closes. Cheap enough for the cookie
/// snapshot/restore cadence; avoids holding a second long-lived ws.
async fn browser_call(endpoint: &str, method: &str, params: Value) -> Result<Value, String> {
    use futures::{SinkExt, StreamExt};
    let ws_url = browser_ws_url(endpoint).await?;
    let (mut stream, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| format!("cdp connect: {e}"))?;
    let frame = json!({ "id": 1, "method": method, "params": params }).to_string();
    stream
        .send(tokio_tungstenite::tungstenite::Message::Text(frame.into()))
        .await
        .map_err(|e| format!("cdp send: {e}"))?;
    let deadline = std::time::Duration::from_secs(10);
    loop {
        let msg = tokio::time::timeout(deadline, stream.next())
            .await
            .map_err(|_| format!("cdp {method}: timed out"))?
            .ok_or_else(|| format!("cdp {method}: stream closed"))?
            .map_err(|e| format!("cdp recv: {e}"))?;
        let tokio_tungstenite::tungstenite::Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
        if v.get("id").and_then(Value::as_u64) == Some(1) {
            if let Some(err) = v.get("error") {
                return Err(format!("cdp {method}: {err}"));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

async fn snapshot_cookies(endpoint: &str) -> Result<(), String> {
    let result = browser_call(endpoint, "Storage.getCookies", json!({})).await?;
    let cookies = result.get("cookies").cloned().unwrap_or(json!([]));
    let n = cookies.as_array().map(|a| a.len()).unwrap_or(0);
    if n == 0 {
        return Ok(()); // nothing to persist; don't clobber a prior snapshot
    }
    let path = cookies_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&cookies).unwrap_or_default())
        .map_err(|e| format!("write cookies: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename cookies: {e}"))?;
    Ok(())
}

async fn restore_cookies(endpoint: &str) -> Result<(), String> {
    let path = cookies_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(()); // first run — nothing saved yet
    };
    let cookies: Value = serde_json::from_slice(&bytes).map_err(|e| format!("parse: {e}"))?;
    if cookies.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return Ok(());
    }
    browser_call(
        endpoint,
        "Storage.setCookies",
        json!({ "cookies": cookies }),
    )
    .await?;
    Ok(())
}

/// Best-effort synchronous cookie flush for shutdown/pause paths.
pub fn flush_cookies() {
    let endpoint = {
        let guard = state().lock().unwrap();
        match guard.as_ref() {
            Some(s) if s.launched => s.endpoint.clone(),
            _ => return,
        }
    };
    let _ = rt().block_on(snapshot_cookies(&endpoint));
}

/// Minimal blocking health probe of a DevTools HTTP endpoint
/// (`GET /json/version`). std-only — no blocking reqwest feature.
fn endpoint_alive(endpoint: &str) -> bool {
    let Some(addr) = endpoint.strip_prefix("http://") else {
        return false;
    };
    use std::io::{Read, Write};
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(
        &match addr.parse() {
            Ok(a) => a,
            Err(_) => return false,
        },
        std::time::Duration::from_millis(800),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(800)));
    // HTTP/1.1 + Connection: close — chromium's DevTools server
    // ignores HTTP/1.0 requests entirely (verified empirically).
    let req = format!("GET /json/version HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    buf.contains("webSocketDebuggerUrl")
}

fn in_container() -> bool {
    std::env::var("THCLAWS_INSIDE_DOCKER").ok().as_deref() == Some("1")
        || std::env::var("THCLAWS_USES_GATEWAY").ok().as_deref() == Some("1")
}

fn profile_dir() -> PathBuf {
    profile_dir_for(in_container())
}

/// Where the managed browser's chromium profile lives — this is what
/// makes cookies/logins persist:
/// - **Desktop**: `~/.cache/thclaws/browser-profile/<cwd-hash>` —
///   outside the workspace so sessions can never be swept into a
///   publish/sync, persistent across restarts.
/// - **Cloud pods**: the home dir is EPHEMERAL (every restart logged
///   users out), so the profile moves to the workspace PVC at
///   `<cwd>/.thclaws/browser-profile`. Publish safety is restored by
///   the pack strip rule (`cloud/pack.rs::STRIP_PREFIXES`), and the
///   profile stays lean because the disk cache is redirected to /tmp
///   at launch.
fn profile_dir_for(container: bool) -> PathBuf {
    if container {
        return std::env::current_dir()
            .unwrap_or_default()
            .join(".thclaws/state/browser-profile");
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    cwd.hash(&mut hasher);
    let key = format!("{:016x}", hasher.finish());
    crate::util::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache/thclaws/browser-profile")
        .join(key)
}

/// Kill the engine-owned chromium (used on shutdown paths; best-effort).
pub fn shutdown() {
    flush_cookies();
    if let Some(mut s) = state().lock().unwrap().take() {
        if let Some(child) = s.child.as_mut() {
            let _ = child.kill();
        }
    }
    *page_slot().lock().unwrap() = None;
}

// ── CDP page session ─────────────────────────────────────────────────

/// One attached page target: a websocket with JSON-RPC-style calls
/// (`id`/`method`/`params` → reply by id) plus a stream of events the
/// reader task routes. Mirrors `McpClient`'s pending-map shape.
struct PageSession {
    writer: tokio::sync::Mutex<
        futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tokio_tungstenite::tungstenite::Message,
        >,
    >,
    pending: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    screencast_on: AtomicBool,
    alive: AtomicBool,
}

impl PageSession {
    /// Fire-and-forget method send — no reply registration. MUST be
    /// used from the reader task itself (e.g. screencast acks): a
    /// reply-awaiting `call` there deadlocks, because the awaited
    /// reply can only be routed by the very loop that's blocked.
    async fn notify(&self, method: &str, params: Value) {
        use futures::SinkExt;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let frame = json!({ "id": id, "method": method, "params": params }).to_string();
        let _ = self
            .writer
            .lock()
            .await
            .send(tokio_tungstenite::tungstenite::Message::Text(frame.into()))
            .await;
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        use futures::SinkExt;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let frame = json!({ "id": id, "method": method, "params": params }).to_string();
        self.writer
            .lock()
            .await
            .send(tokio_tungstenite::tungstenite::Message::Text(frame.into()))
            .await
            .map_err(|e| format!("cdp send: {e}"))?;
        match tokio::time::timeout(std::time::Duration::from_secs(15), rx).await {
            Ok(Ok(v)) => {
                if let Some(err) = v.get("error") {
                    Err(format!("cdp {method}: {err}"))
                } else {
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                }
            }
            _ => Err(format!("cdp {method}: timed out")),
        }
    }
}

/// Pick the most recently opened page target from `/json/list`.
async fn page_ws_url(endpoint: &str) -> Result<String, String> {
    let body = reqwest::get(format!("{endpoint}/json/list"))
        .await
        .map_err(|e| format!("cdp /json/list: {e}"))?
        .text()
        .await
        .map_err(|e| format!("cdp /json/list body: {e}"))?;
    let targets: Vec<Value> =
        serde_json::from_str(&body).map_err(|e| format!("cdp /json/list parse: {e}"))?;
    targets
        .iter()
        .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .and_then(|t| t.get("webSocketDebuggerUrl").and_then(Value::as_str))
        .map(String::from)
        .ok_or_else(|| "no page target".to_string())
}

/// Attach to the current page and start the live wire: screencast
/// frames + console/exception events + navigation notices, all pushed
/// through `dispatch` as frontend-ready JSON envelopes.
async fn attach_and_start(endpoint: String, dispatch: Dispatch) -> Result<(), String> {
    use futures::StreamExt;
    let ws_url = page_ws_url(&endpoint).await?;
    let (stream, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| format!("cdp connect: {e}"))?;
    let (writer, mut reader) = stream.split();

    let session = Arc::new(PageSession {
        writer: tokio::sync::Mutex::new(writer),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        screencast_on: AtomicBool::new(false),
        alive: AtomicBool::new(true),
    });

    // Reader task: route replies by id; convert events into dispatches.
    let s2 = session.clone();
    let d2 = dispatch.clone();
    rt().spawn(async move {
        while let Some(Ok(msg)) = reader.next().await {
            let tokio_tungstenite::tungstenite::Message::Text(text) = msg else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(id) = v.get("id").and_then(Value::as_u64) {
                if let Some(tx) = s2.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(v);
                }
                continue;
            }
            match v.get("method").and_then(Value::as_str) {
                Some("Page.screencastFrame") => {
                    let p = v.get("params").cloned().unwrap_or(Value::Null);
                    if let Some(data) = p.get("data").and_then(Value::as_str) {
                        d2(json!({
                            "type": "browser_frame",
                            "data": data,
                        })
                        .to_string());
                    }
                    if let Some(sid) = p.get("sessionId") {
                        // Ack AFTER forwarding — natural backpressure —
                        // but fire-and-forget: awaiting the ack's REPLY
                        // here would deadlock the reader (it's the only
                        // task that can route replies).
                        s2.notify("Page.screencastFrameAck", json!({ "sessionId": sid }))
                            .await;
                    }
                }
                Some("Runtime.consoleAPICalled") => {
                    let p = v.get("params").cloned().unwrap_or(Value::Null);
                    let level = p.get("type").and_then(Value::as_str).unwrap_or("log");
                    let text: Vec<String> = p
                        .get("args")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| {
                                    x.get("value").map(|v| match v.as_str() {
                                        Some(s) => s.to_string(),
                                        None => v.to_string(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    d2(json!({
                        "type": "browser_console",
                        "level": level,
                        "text": text.join(" "),
                    })
                    .to_string());
                }
                Some("Runtime.exceptionThrown") => {
                    let desc = v
                        .pointer("/params/exceptionDetails/exception/description")
                        .or_else(|| v.pointer("/params/exceptionDetails/text"))
                        .and_then(Value::as_str)
                        .unwrap_or("uncaught exception");
                    d2(json!({
                        "type": "browser_console",
                        "level": "error",
                        "text": desc,
                    })
                    .to_string());
                }
                Some("Page.frameNavigated") => {
                    if let Some(url) = v.pointer("/params/frame/url").and_then(Value::as_str) {
                        // Only top-level frames carry no parentId.
                        if v.pointer("/params/frame/parentId").is_none() {
                            d2(json!({ "type": "browser_nav", "url": url }).to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        s2.alive.store(false, Ordering::SeqCst);
    });

    session.call("Page.enable", json!({})).await?;
    session.call("Runtime.enable", json!({})).await?;
    session
        .call(
            "Page.startScreencast",
            json!({
                "format": "jpeg",
                "quality": 60,
                "maxWidth": 1366,
                "maxHeight": 900,
            }),
        )
        .await?;
    session.screencast_on.store(true, Ordering::SeqCst);

    *page_slot().lock().unwrap() = Some(session);
    Ok(())
}

// ── Public sync API (call from non-tokio threads only) ───────────────

/// Start (or restart) the live screencast, pushing frames + console
/// events through `dispatch`. Re-attaches to the currently active
/// page every time, so a takeover toggle recovers from closed tabs.
pub fn screencast_start(dispatch: Dispatch) -> Result<(), String> {
    ensure_up()?;
    let endpoint = state()
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.endpoint.clone())
        .ok_or("engine-owned browser not running (CDP off)")?;
    // Drop any previous session — its reader task ends when the ws does.
    if let Some(old) = page_slot().lock().unwrap().take() {
        let _ = rt().block_on(old.call("Page.stopScreencast", json!({})));
    }
    rt().block_on(attach_and_start(endpoint, dispatch))
}

pub fn screencast_stop() {
    if let Some(s) = page_slot().lock().unwrap().take() {
        let _ = rt().block_on(s.call("Page.stopScreencast", json!({})));
    }
    // A takeover session is the most likely moment a fresh login just
    // happened — snapshot now so it survives even an immediate pause.
    flush_cookies();
}

/// Native input on the live page. `kind`: click | move | wheel |
/// text | key. Coordinates are page CSS pixels (the screencast frame's
/// own space).
pub fn input(kind: &str, args: &Value) -> Result<(), String> {
    let session = page_slot()
        .lock()
        .unwrap()
        .clone()
        .ok_or("no live page session — start the screencast first")?;
    if !session.alive.load(Ordering::SeqCst) {
        return Err("live page session closed — toggle takeover to re-attach".into());
    }
    let get_f = |k: &str| args.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    rt().block_on(async {
        match kind {
            "click" => {
                let (x, y) = (get_f("x"), get_f("y"));
                let base = json!({
                    "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1,
                });
                let mut press = base.clone();
                press["type"] = json!("mousePressed");
                session.call("Input.dispatchMouseEvent", press).await?;
                let mut release = base;
                release["type"] = json!("mouseReleased");
                session.call("Input.dispatchMouseEvent", release).await?;
            }
            // The user's own gesture, relayed as it happens: a press, the
            // moves their hand makes with the button held, the release.
            // A slider CAPTCHA is a drag, and a drag is nothing but that
            // sequence — `click` alone could never move one.
            "down" | "up" => {
                session
                    .call(
                        "Input.dispatchMouseEvent",
                        json!({
                            "type": if kind == "down" { "mousePressed" } else { "mouseReleased" },
                            "x": get_f("x"), "y": get_f("y"),
                            "button": "left", "buttons": if kind == "down" { 1 } else { 0 },
                            "clickCount": 1,
                        }),
                    )
                    .await?;
            }
            "move" => {
                let held = get_f("buttons") as u32 & 1 == 1;
                let mut ev = json!({
                    "type": "mouseMoved",
                    "x": get_f("x"), "y": get_f("y"),
                    "buttons": if held { 1 } else { 0 },
                });
                if held {
                    ev["button"] = json!("left");
                }
                session.call("Input.dispatchMouseEvent", ev).await?;
            }
            "wheel" => {
                session
                    .call(
                        "Input.dispatchMouseEvent",
                        json!({
                            "type": "mouseWheel",
                            "x": get_f("x"), "y": get_f("y"),
                            "deltaX": get_f("deltaX"), "deltaY": get_f("deltaY"),
                        }),
                    )
                    .await?;
            }
            "text" => {
                let text = args.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() || text.chars().count() > 2000 {
                    return Err("text input needs 1-2000 characters".into());
                }
                session
                    .call("Input.insertText", json!({ "text": text }))
                    .await?;
            }
            "key" => {
                let key = args.get("key").and_then(Value::as_str).unwrap_or("");
                let (code, vk, text) = match key {
                    "Enter" => ("Enter", 13, Some("\r")),
                    "Tab" => ("Tab", 9, None),
                    "Backspace" => ("Backspace", 8, None),
                    "Escape" => ("Escape", 27, None),
                    "Delete" => ("Delete", 46, None),
                    "ArrowUp" => ("ArrowUp", 38, None),
                    "ArrowDown" => ("ArrowDown", 40, None),
                    "ArrowLeft" => ("ArrowLeft", 37, None),
                    "ArrowRight" => ("ArrowRight", 39, None),
                    other => return Err(format!("unsupported key: {other}")),
                };
                let mut down = json!({
                    "type": "keyDown",
                    "key": key, "code": code,
                    "windowsVirtualKeyCode": vk,
                    "nativeVirtualKeyCode": vk,
                });
                if let Some(t) = text {
                    down["text"] = json!(t);
                }
                session.call("Input.dispatchKeyEvent", down).await?;
                session
                    .call(
                        "Input.dispatchKeyEvent",
                        json!({
                            "type": "keyUp",
                            "key": key, "code": code,
                            "windowsVirtualKeyCode": vk,
                            "nativeVirtualKeyCode": vk,
                        }),
                    )
                    .await?;
            }
            other => return Err(format!("unsupported input kind: {other}")),
        }
        Ok(())
    })
}

/// One thread applies every takeover input in the order it arrived.
/// A thread per event let a drag's `up` overtake its last `move`, which
/// ends the drag short — or lands the release before the press.
fn input_queue() -> &'static Mutex<std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>> {
    static Q: OnceLock<Mutex<std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>>> = OnceLock::new();
    Q.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        std::thread::Builder::new()
            .name("browser-input".into())
            .spawn(move || {
                for job in rx {
                    job();
                }
            })
            .expect("browser-input thread");
        Mutex::new(tx)
    })
}

/// Queue one takeover input; `reply` runs on the input thread once it has
/// been applied, in arrival order with every other input.
pub fn input_queued(
    kind: String,
    args: Value,
    reply: impl FnOnce(Result<(), String>) + Send + 'static,
) {
    let job: Box<dyn FnOnce() + Send> = Box::new(move || reply(input(&kind, &args)));
    let _ = input_queue().lock().unwrap().send(job);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drag is `down`, moves, `up` — relayed on separate threads, the
    /// `up` could land first. The queue keeps arrival order.
    #[test]
    fn queued_inputs_apply_in_arrival_order() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        for i in 0..200u32 {
            let seen = seen.clone();
            let done_tx = done_tx.clone();
            let job: Box<dyn FnOnce() + Send> = Box::new(move || {
                seen.lock().unwrap().push(i);
                if i == 199 {
                    let _ = done_tx.send(());
                }
            });
            input_queue().lock().unwrap().send(job).unwrap();
        }
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("queue drained");
        assert_eq!(*seen.lock().unwrap(), (0..200).collect::<Vec<_>>());
        // With no live page a queued input reports, rather than drops, the
        // error — the tab shows it instead of a drag that silently did
        // nothing.
        let (tx, rx) = std::sync::mpsc::channel();
        input_queued("down".into(), json!({"x": 1, "y": 1}), move |r| {
            let _ = tx.send(r);
        });
        let r = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reply");
        assert!(r.is_err());
    }

    #[test]
    fn classic_layout_discovery_picks_highest_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let sub = if cfg!(target_os = "macos") {
            "chrome-mac/Chromium.app/Contents/MacOS"
        } else if cfg!(target_os = "windows") {
            "chrome-win64"
        } else {
            "chrome-linux64"
        };
        let exe_name = if cfg!(target_os = "macos") {
            "Chromium"
        } else if cfg!(target_os = "windows") {
            "chrome.exe"
        } else {
            "chrome"
        };
        for rev in ["chromium-1100", "chromium-1226"] {
            let dir = root.join(rev).join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(exe_name), b"x").unwrap();
        }
        let found = newest_classic_chromium(root).expect("found");
        assert!(found.to_string_lossy().contains("chromium-1226"));
    }

    #[test]
    fn profile_dir_placement_per_environment() {
        let cwd = std::env::current_dir().unwrap();
        // Desktop: outside the workspace (publish/sync can't sweep it).
        let desktop = profile_dir_for(false);
        assert!(
            !desktop.starts_with(&cwd),
            "desktop profile must not live in the workspace: {desktop:?}"
        );
        assert!(desktop.to_string_lossy().contains("browser-profile"));
        // Container: on the workspace PVC so logins survive pod
        // restarts — and the pack strip rule must cover that path.
        let cloud = profile_dir_for(true);
        assert!(cloud.starts_with(&cwd));
        assert!(cloud.ends_with(".thclaws/state/browser-profile"));
        assert!(crate::cloud::pack::is_strippable(std::path::Path::new(
            ".thclaws/state/browser-profile/Default/Cookies"
        )));
    }

    #[test]
    fn reap_clears_singleton_locks_for_dead_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path();
        for lock in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            std::fs::write(profile.join(lock), b"x").unwrap();
        }
        // Grab-and-release an ephemeral port so nothing is listening —
        // the "orphan" is already dead, so reap should skip the kill and
        // clear the single-instance locks that would otherwise make a
        // fresh launch forward-and-exit.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dead = format!("http://127.0.0.1:{port}");
        assert!(!endpoint_alive(&dead));
        reap_orphan(&dead, profile);
        for lock in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            assert!(
                !profile.join(lock).exists(),
                "{lock} should be cleared after reaping a dead orphan"
            );
        }
    }
}
