//! `thclaws deploy` client — builds a filtered tar of the current
//! project's `.thclaws/` and ships it to a pod's `/v1/deploy*`.
//!
//! Dev-plan/28 Phase 1: replace-all upload via `POST /v1/deploy`.
//! Phase 2: `POST /v1/deploy/manifest` first to identify what the pod
//! is missing, then ship only the diff via the same `/v1/deploy`
//! endpoint. Same auth, same SSE event shape.
//!
//! Filter rules (kept in sync with the server's allow / preserve
//! lists at `api_v1/deploy.rs`):
//!
//! - Top-level entries shipped: settings.json, mcp.json, AGENTS.md,
//!   agents/, skills/, commands/, plugins/, plugins.json, prompt/,
//!   rules/, data/, agent_workflow/. memory/ added when --include-memory.
//! - Never shipped: state/ (all runtime — sessions, team, kms, usage,
//!   workflow run-state, …), .env (server would refuse anyway — defense
//!   in depth on both sides).
//! - mcp.json with stdio entries refused unless --allow-stdio-mcp
//!   (paths/binaries reference the laptop, won't resolve on the pod).

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

#[derive(Debug)]
pub struct DeployArgs {
    pub pod: String,
    pub token: Option<String>,
    pub include_memory: bool,
    pub allow_stdio_mcp: bool,
    pub dry_run: bool,
    pub full: bool,
    /// After a successful deploy, POST `/v1/restart` so the pod
    /// re-initialises MCP servers, plugin runtimes, skill caches, and
    /// the system prompt. Default-on — the snapshot the running
    /// process holds (MCP handles, system prompt, KMS load) is
    /// otherwise frozen at startup, which makes a "successful" deploy
    /// invisible to the model. Pass `--no-restart` to keep the
    /// running process up across the deploy (rare: hot config edits
    /// the snapshot doesn't read).
    pub restart: bool,
}

const ALLOWED_TOP_LEVEL: &[&str] = &[
    "settings.json",
    "mcp.json",
    "AGENTS.md",
    "agents",
    "skills",
    "commands",
    "plugins",
    "plugins.json",
    "prompt",
    "rules",
    "data",
    "agent_workflow",
];

const NEVER_SHIP: &[&str] = &["state", ".env"];

/// Whether a logged line is informational or an error — used by the
/// sink to route stdout vs stderr / regular vs error styling.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum DeployLog {
    Info,
    Warn,
    Error,
}

/// Entry point invoked by the `thclaws deploy` subcommand. Routes
/// progress to stdout/stderr (the CLI's default). For surfaces that
/// need to capture lines (the GUI's /deploy slash command), use
/// [`run_with_sink`] instead.
pub async fn run(args: DeployArgs) -> i32 {
    run_with_sink(args, |line: &str, level: DeployLog| match level {
        DeployLog::Info => println!("{line}"),
        DeployLog::Warn | DeployLog::Error => eprintln!("{line}"),
    })
    .await
}

/// Same as [`run`] but routes every progress line through `sink`.
/// Each call to `sink` is one logical event — the sink decides where
/// it goes (stdout/stderr, ViewEvent::SlashOutput, log file, etc.).
/// Lines from this function never contain ANSI escapes — callers
/// can style for their surface without parsing color codes out.
pub async fn run_with_sink<F>(args: DeployArgs, sink: F) -> i32
where
    F: Fn(&str, DeployLog),
{
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            sink(
                &format!("[deploy] cannot resolve current dir: {e}"),
                DeployLog::Error,
            );
            return 1;
        }
    };
    let thclaws_root = cwd.join(".thclaws");
    if !thclaws_root.exists() {
        sink(
            &format!(
                "[deploy] no .thclaws/ in {}: run thclaws here first to create one",
                cwd.display()
            ),
            DeployLog::Error,
        );
        return 1;
    }

    let mut candidates = match collect_files(&thclaws_root, args.include_memory) {
        Ok(c) => c,
        Err(e) => {
            sink(&format!("[deploy] scan failed: {e}"), DeployLog::Error);
            return 1;
        }
    };

    if candidates.is_empty() {
        sink(
            "[deploy] nothing to ship under .thclaws/ — bundle is empty",
            DeployLog::Warn,
        );
        return 1;
    }

    // In-memory overrides: tar entries that don't come from disk
    // verbatim. Used by the stdio-MCP-strip path so the laptop's
    // mcp.json isn't mutated; the pod just receives a filtered copy.
    let mut overrides: BTreeMap<String, Vec<u8>> = BTreeMap::new();

    // mcp.json stdio handling. Without --allow-stdio-mcp (default),
    // strip stdio entries before shipping so the pod doesn't try to
    // spawn local binaries it can't reach. With the flag, ship
    // verbatim. Either way the laptop's mcp.json is never modified.
    if candidates.contains_key("mcp.json") {
        let mcp_path = thclaws_root.join("mcp.json");
        if args.allow_stdio_mcp {
            // Just inform — the original file ships as-is.
            match scan_stdio_mcp_names(&mcp_path) {
                Ok(names) if !names.is_empty() => sink(
                    &format!(
                        "[deploy] --allow-stdio-mcp set; shipping {} stdio MCP entries verbatim (they'll log spawn errors on the pod): {}",
                        names.len(),
                        names.join(", ")
                    ),
                    DeployLog::Warn,
                ),
                Ok(_) => {}
                Err(e) => {
                    sink(&format!("[deploy] mcp.json scan: {e}"), DeployLog::Error);
                    return 1;
                }
            }
        } else {
            match filter_stdio_mcp(&mcp_path) {
                Ok(Some((filtered, stripped))) => {
                    sink(
                        &format!(
                            "[deploy] stripping {} stdio MCP entr{} from mcp.json: {} (laptop's mcp.json unchanged; use --allow-stdio-mcp to keep them on the pod)",
                            stripped.len(),
                            if stripped.len() == 1 { "y" } else { "ies" },
                            stripped.join(", ")
                        ),
                        DeployLog::Info,
                    );
                    // Re-hash the filtered bytes so the manifest diff
                    // handshake reflects the version that will actually
                    // land on the pod.
                    let mut h = Sha256::new();
                    h.update(&filtered);
                    candidates.insert(
                        "mcp.json".to_string(),
                        FileMeta {
                            size: filtered.len() as u64,
                            sha256: format!("{:x}", h.finalize()),
                        },
                    );
                    overrides.insert("mcp.json".to_string(), filtered);
                }
                Ok(None) => {} // no stdio entries — original is fine
                Err(e) => {
                    sink(&format!("[deploy] mcp.json filter: {e}"), DeployLog::Error);
                    return 1;
                }
            }
        }
    }

    if args.dry_run {
        let total_bytes: u64 = candidates.values().map(|m| m.size).sum();
        sink(
            &format!(
                "[deploy] dry run — would ship {} file(s), {} bytes:",
                candidates.len(),
                total_bytes
            ),
            DeployLog::Info,
        );
        for (path, meta) in &candidates {
            sink(
                &format!("  {} ({} bytes)", path, meta.size),
                DeployLog::Info,
            );
        }
        return 0;
    }

    let token = args
        .token
        .or_else(|| std::env::var("THCLAWS_DEPLOY_TOKEN").ok())
        .filter(|s| !s.trim().is_empty());
    let Some(token) = token else {
        sink(
            "[deploy] no token: pass --token <BEARER> or set THCLAWS_DEPLOY_TOKEN",
            DeployLog::Error,
        );
        return 1;
    };

    let base_url = args.pod.trim_end_matches('/').to_string();
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            sink(
                &format!("[deploy] reqwest build failed: {e}"),
                DeployLog::Error,
            );
            return 1;
        }
    };

    let to_ship: Vec<String> = if args.full {
        candidates.keys().cloned().collect()
    } else {
        match diff_manifest(&client, &base_url, &token, &candidates).await {
            Ok(missing) => {
                sink(
                    &format!(
                        "[deploy] diff: pod is missing {}/{} file(s)",
                        missing.len(),
                        candidates.len()
                    ),
                    DeployLog::Info,
                );
                if missing.is_empty() {
                    sink(
                        "[deploy] pod is already up to date — nothing to ship",
                        DeployLog::Info,
                    );
                    return 0;
                }
                missing
            }
            Err(e) => {
                sink(
                    &format!(
                        "[deploy] manifest handshake failed ({e}); falling back to full upload"
                    ),
                    DeployLog::Warn,
                );
                candidates.keys().cloned().collect()
            }
        }
    };

    let tar_bytes = match build_tar(&thclaws_root, &to_ship, &overrides) {
        Ok(b) => b,
        Err(e) => {
            sink(&format!("[deploy] tar build failed: {e}"), DeployLog::Error);
            return 1;
        }
    };
    sink(
        &format!(
            "[deploy] bundled {} file(s), {} bytes — uploading to {}",
            to_ship.len(),
            tar_bytes.len(),
            base_url
        ),
        DeployLog::Info,
    );

    let url = format!("{base_url}/v1/deploy");
    let resp = client
        .post(&url)
        .bearer_auth(&token)
        .header("content-type", "application/x-tar")
        .body(tar_bytes)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            sink(&format!("[deploy] upload failed: {e}"), DeployLog::Error);
            return 1;
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        sink(
            &format!(
                "[deploy] pod rejected upload: HTTP {status}: {}",
                body.chars().take(500).collect::<String>()
            ),
            DeployLog::Error,
        );
        return 1;
    }

    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            sink(
                &format!("[deploy] read SSE body failed: {e}"),
                DeployLog::Error,
            );
            return 1;
        }
    };
    render_sse(&text, &sink);

    if args.restart {
        if let Err(code) = trigger_restart_and_wait(&args.pod, &token, &client, &sink).await {
            return code;
        }
    }

    0
}

/// Hit `POST /v1/restart` and then poll `GET /healthz` until the pod
/// reports healthy again (or we time out). Surfaces each step via the
/// sink so the user sees progress, not a silent gap.
async fn trigger_restart_and_wait<F>(
    pod_url: &str,
    token: &str,
    client: &reqwest::Client,
    sink: &F,
) -> Result<(), i32>
where
    F: Fn(&str, DeployLog),
{
    sink("[deploy] requesting pod restart...", DeployLog::Info);

    let restart_url = format!("{}/v1/restart", pod_url.trim_end_matches('/'));
    let resp = client.post(&restart_url).bearer_auth(token).send().await;
    match resp {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            sink(
                &format!(
                    "[deploy] pod refused restart: HTTP {} (deploy succeeded but the pod kept its pre-deploy snapshot)",
                    r.status()
                ),
                DeployLog::Error,
            );
            return Err(1);
        }
        Err(e) => {
            sink(
                &format!("[deploy] restart request failed: {e}"),
                DeployLog::Error,
            );
            return Err(1);
        }
    }

    // The pod begins shutting down ~1s after the /v1/restart 200. Give
    // the supervisor (k8s, systemd, docker, ...) a moment to actually
    // tear it down before we start health-polling — otherwise the
    // first poll might catch the OLD process still answering.
    sink(
        "[deploy] pod restarting — waiting for /healthz to come back...",
        DeployLog::Info,
    );
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let healthz_url = format!("{}/healthz", pod_url.trim_end_matches('/'));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut last_status: Option<String> = None;
    while std::time::Instant::now() < deadline {
        match client.get(&healthz_url).send().await {
            Ok(r) if r.status().is_success() => {
                sink("[deploy] pod is back up", DeployLog::Info);
                return Ok(());
            }
            Ok(r) => {
                last_status = Some(format!("HTTP {}", r.status()));
            }
            Err(e) => {
                last_status = Some(e.to_string());
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    sink(
        &format!(
            "[deploy] pod did not return to healthy within 120s (last: {})",
            last_status.unwrap_or_else(|| "no response".to_string())
        ),
        DeployLog::Error,
    );
    Err(1)
}

struct FileMeta {
    size: u64,
    sha256: String,
}

fn collect_files(
    thclaws_root: &Path,
    include_memory: bool,
) -> std::io::Result<BTreeMap<String, FileMeta>> {
    let mut out: BTreeMap<String, FileMeta> = BTreeMap::new();

    // Walk the .thclaws/ allow-list as before.
    for top in ALLOWED_TOP_LEVEL.iter().chain(if include_memory {
        ["memory"].iter()
    } else {
        [].iter()
    }) {
        let path = thclaws_root.join(top);
        if !path.exists() {
            continue;
        }
        if path.is_file() {
            let rel = (*top).to_string();
            if NEVER_SHIP.contains(top) {
                continue;
            }
            insert_file(&mut out, &rel, &path)?;
        } else if path.is_dir() {
            for entry in walkdir::WalkDir::new(&path) {
                let entry = entry.map_err(std::io::Error::other)?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let abs = entry.path();
                let rel = abs
                    .strip_prefix(thclaws_root)
                    .map_err(std::io::Error::other)?
                    .to_string_lossy()
                    .replace('\\', "/");
                // Defense in depth: skip never-ship even if walked from
                // an allowed parent (won't happen with current consts
                // but cheap).
                let first = rel.split('/').next().unwrap_or("");
                if NEVER_SHIP.contains(&first) {
                    continue;
                }
                insert_file(&mut out, &rel, abs)?;
            }
        }
    }

    // Also pick up project-root AGENTS.md / CLAUDE.md (the conventional
    // location for the agents.md standard) and ship them under the
    // `__root__/` prefix. The server treats this prefix specially:
    // entries land at /workspace/<NAME> on the pod, NOT inside
    // /workspace/.thclaws/, so the pod's ProjectContext::discover walk
    // (which expects AGENTS.md at the project root, the same way it
    // does on the laptop) finds them where it expects.
    //
    // Why a fake-looking prefix rather than separate endpoints / a
    // sibling tar: path-safety stays uniform (no '..' games), the
    // single tar + single manifest endpoint round-trip still works,
    // and the strict allow-list of names under `__root__/` (server
    // side) keeps blast radius tight — only AGENTS.md and CLAUDE.md
    // can land outside .thclaws/, never anything else.
    let Some(project_root) = thclaws_root.parent() else {
        return Ok(out);
    };
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let root_path = project_root.join(name);
        if root_path.is_file() {
            insert_file(&mut out, &format!("{ROOT_PREFIX}/{name}"), &root_path)?;
        }
    }

    Ok(out)
}

/// Tar-path prefix marking entries that land at the pod's workspace
/// root (alongside the project code) instead of inside .thclaws/. The
/// server enforces a strict allow-list of names under this prefix.
pub const ROOT_PREFIX: &str = "__root__";

fn insert_file(out: &mut BTreeMap<String, FileMeta>, rel: &str, abs: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(abs)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    out.insert(
        rel.to_string(),
        FileMeta {
            size: bytes.len() as u64,
            sha256: format!("{:x}", h.finalize()),
        },
    );
    Ok(())
}

/// Scan `mcp.json` and return the names of stdio-transport entries.
/// Empty vec when none. Used to decide whether to strip-on-ship
/// (default) or refuse the upload (legacy behavior; not used now —
/// kept the error path off since the user picked filter-on-strip as
/// the default).
fn scan_stdio_mcp_names(path: &Path) -> Result<Vec<String>, String> {
    let body = std::fs::read_to_string(path).map_err(|e| format!("read mcp.json: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("parse mcp.json: {e}"))?;
    let Some(servers) = v.get("mcpServers").and_then(|s| s.as_object()) else {
        return Ok(Vec::new());
    };
    let stdio: Vec<String> = servers
        .iter()
        .filter(|(_, cfg)| {
            cfg.get("transport")
                .and_then(|t| t.as_str())
                .map(|t| t == "stdio")
                .unwrap_or(true)
        })
        .map(|(name, _)| name.clone())
        .collect();
    Ok(stdio)
}

/// Build a copy of `mcp.json` with stdio-transport entries removed.
/// Returns the new bytes + the names of entries that got dropped.
/// `None` when the source file has no stdio entries (no rewrite
/// needed; caller ships the original).
type FilteredMcpConfig = (Vec<u8>, Vec<String>);

fn filter_stdio_mcp(path: &Path) -> Result<Option<FilteredMcpConfig>, String> {
    let body = std::fs::read_to_string(path).map_err(|e| format!("read mcp.json: {e}"))?;
    let mut v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("parse mcp.json: {e}"))?;
    let Some(servers) = v.get_mut("mcpServers").and_then(|s| s.as_object_mut()) else {
        return Ok(None);
    };
    let stripped: Vec<String> = servers
        .iter()
        .filter(|(_, cfg)| {
            cfg.get("transport")
                .and_then(|t| t.as_str())
                .map(|t| t == "stdio")
                .unwrap_or(true)
        })
        .map(|(name, _)| name.clone())
        .collect();
    if stripped.is_empty() {
        return Ok(None);
    }
    for name in &stripped {
        servers.remove(name);
    }
    let new_body =
        serde_json::to_vec_pretty(&v).map_err(|e| format!("serialize filtered mcp.json: {e}"))?;
    Ok(Some((new_body, stripped)))
}

// Legacy stub kept for the call-site below; superseded by the
// scan + filter helpers above.
#[allow(dead_code)]
fn scan_mcp_json(path: &Path) -> Result<(), String> {
    let body = std::fs::read_to_string(path).map_err(|e| format!("read mcp.json: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("parse mcp.json: {e}"))?;
    let Some(servers) = v.get("mcpServers").and_then(|s| s.as_object()) else {
        return Ok(());
    };
    let stdio: Vec<String> = servers
        .iter()
        .filter(|(_, cfg)| {
            cfg.get("transport")
                .and_then(|t| t.as_str())
                .map(|t| t == "stdio")
                .unwrap_or(true)
        })
        .map(|(name, _)| name.clone())
        .collect();
    if !stdio.is_empty() {
        return Err(format!(
            "mcp.json contains stdio MCP servers that won't resolve on the pod: {} \
             (each spawns a local binary)",
            stdio.join(", ")
        ));
    }
    Ok(())
}

async fn diff_manifest(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    candidates: &BTreeMap<String, FileMeta>,
) -> Result<Vec<String>, String> {
    let files: Vec<serde_json::Value> = candidates
        .iter()
        .map(|(path, meta)| {
            serde_json::json!({
                "path": path,
                "sha256": meta.sha256,
            })
        })
        .collect();
    let url = format!("{base_url}/v1/deploy/manifest");
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "files": files }))
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "HTTP {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        ));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    let missing = body
        .get("missing")
        .and_then(|m| m.as_array())
        .ok_or("manifest response missing `missing` array")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    Ok(missing)
}

fn build_tar(
    thclaws_root: &Path,
    paths: &[String],
    overrides: &BTreeMap<String, Vec<u8>>,
) -> std::io::Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    {
        let mut builder = tar::Builder::new(&mut buf);
        for rel in paths {
            // overrides take precedence — used by the stdio-MCP-strip
            // path to ship a filtered mcp.json without mutating the
            // file on disk.
            if let Some(bytes) = overrides.get(rel) {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_mtime(now_secs());
                header.set_cksum();
                builder.append_data(&mut header, rel, bytes.as_slice())?;
                continue;
            }

            // Most entries are under .thclaws/. Entries prefixed with
            // `__root__/` (currently only AGENTS.md / CLAUDE.md) sit at
            // the project root one level up — see collect_files for
            // the rationale.
            let abs = if let Some(stripped) = rel.strip_prefix(&format!("{ROOT_PREFIX}/")) {
                match thclaws_root.parent() {
                    Some(p) => p.join(stripped),
                    None => continue,
                }
            } else {
                thclaws_root.join(rel)
            };
            if !abs.is_file() {
                continue;
            }
            let mut f = std::fs::File::open(&abs)?;
            let mut header = tar::Header::new_gnu();
            let metadata = f.metadata()?;
            header.set_size(metadata.len());
            header.set_mode(0o644);
            header.set_mtime(
                metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            );
            header.set_cksum();
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            f.read_to_end(&mut bytes)?;
            builder.append_data(&mut header, rel, bytes.as_slice())?;
        }
        builder.finish()?;
    }
    Ok(buf)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn render_sse<F>(text: &str, sink: &F)
where
    F: Fn(&str, DeployLog),
{
    let mut event: Option<String> = None;
    let mut data: Option<String> = None;
    let emit = |e: &str, d: &str, sink: &F| {
        let summary = summarize(d);
        let level = if e == "error" {
            DeployLog::Error
        } else {
            DeployLog::Info
        };
        sink(&format!("[deploy] {e}: {summary}"), level);
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data = Some(rest.trim().to_string());
        } else if line.trim().is_empty() {
            if let (Some(e), Some(d)) = (event.take(), data.take()) {
                emit(&e, &d, sink);
            }
        }
    }
    if let (Some(e), Some(d)) = (event, data) {
        emit(&e, &d, sink);
    }
}

fn summarize(json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    if let Some(obj) = v.as_object() {
        let parts: Vec<String> = obj
            .iter()
            .filter(|(_, val)| !val.is_null())
            .map(|(k, val)| format!("{k}={val}"))
            .collect();
        return parts.join(" ");
    }
    json.to_string()
}
