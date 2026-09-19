//! Session-scoped artifacts + inputs for external orchestrators
//! (dev-plan: job-artifacts).
//!
//! The gap this closes: a control plane can dispatch work to many thClaws
//! workers via `POST /agent/run`, but moving the RESULTING FILES between
//! machines had no Bearer-authenticated, job-scoped path — only the
//! workspace-sync surface, which is whole-workspace and trusts the network
//! layer (tunnel / ForwardAuth). Three endpoints under `/v1` fix that:
//!
//! - `GET  /v1/sessions/{sid}/artifacts`        — the run's frozen manifest
//! - `GET  /v1/sessions/{sid}/artifacts/{aid}`  — one snapshotted file
//! - `POST /v1/inputs`                          — place input files into a
//!   workspace before/with a dispatch (prefix-jailed, size-capped)
//!
//! **Atomicity**: `agent_run` accepts `collect_files: ["reports/*.pdf"]`.
//! When the run finishes, matching files are COPIED into
//! `<workspace>/.thclaws/state/artifacts/<session_id>/files/` and hashed —
//! the manifest and the bytes a later GET serves are the snapshot, so a
//! file being edited after the run can never race the download (the old
//! manifest→export two-step could). `state/` is gitignored + pack-stripped.
//!
//! **Least privilege**: everything is scoped to one session id — an
//! orchestrator holding the Bearer token reads that run's declared outputs,
//! not the whole workspace.

use axum::extract::{Path as AxPath, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::errors::OpenAiError;
use super::AuthOk;

/// Caps — server-side, non-negotiable (the proposer explicitly asked for
/// server-enforced limits rather than trusting the client).
const MAX_ARTIFACT_FILES: usize = 256;
const MAX_ARTIFACT_TOTAL_BYTES: u64 = 300 * 1024 * 1024; // parity with sync/push
const MAX_INPUT_FILES: usize = 100;
const MAX_INPUT_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const INPUTS_BODY_LIMIT_BYTES: usize = 96 * 1024 * 1024; // base64 overhead over 64MB

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArtifactEntry {
    pub id: String,
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Outcome of a run's snapshot. Before this existed a manifest could only be
/// written on success, so a manifest with no `status` is `Completed` — which
/// keeps older manifests readable instead of defaulting them to a failure
/// they never had.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotStatus {
    /// The snapshot ran. `artifacts` may still be empty — a pattern that
    /// matched nothing is a completed snapshot of nothing, not a failure.
    #[default]
    Completed,
    /// Requested but not written; `error` says why. The run itself still
    /// succeeded — collection is non-fatal by design.
    Failed,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArtifactManifest {
    pub session_id: String,
    pub collected_at: String,
    pub patterns: Vec<String>,
    pub artifacts: Vec<ArtifactEntry>,
    /// Files that matched but were skipped by the caps, so a truncated
    /// collection is visible instead of silently partial.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    #[serde(default)]
    pub status: SnapshotStatus,
    /// Present only when `status` is `failed`. Carries the collection error
    /// so a caller learns the outcome from the API instead of the daemon's
    /// stderr (#191).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn artifacts_root(workspace: &Path, session_id: &str) -> PathBuf {
    workspace
        .join(".thclaws")
        .join("state")
        .join("artifacts")
        .join(session_id)
}

/// Session ids come from URL segments — reject anything that isn't the
/// engine's own id shape before it touches a filesystem path.
fn safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Snapshot the files matching `patterns` into the session's artifact
/// store and write the frozen manifest. Called at run end (all three
/// agent_run paths). Failures are logged, never fatal to the run.
pub(crate) fn snapshot_artifacts(
    workspace: &Path,
    session_id: &str,
    patterns: &[String],
) -> std::io::Result<ArtifactManifest> {
    let mut builder = globset::GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = globset::Glob::new(p) {
            builder.add(g);
        }
    }
    let set = builder
        .build()
        .map_err(|e| std::io::Error::other(format!("bad glob set: {e}")))?;

    let root = artifacts_root(workspace, session_id);
    let files_dir = root.join("files");
    std::fs::create_dir_all(&files_dir)?;

    let mut artifacts: Vec<ArtifactEntry> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut total: u64 = 0;

    for entry in walkdir::WalkDir::new(workspace)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            // Never descend into runtime/VCS/dep trees — artifacts are
            // workspace outputs, and .thclaws would recurse into our own
            // snapshot directory.
            !(name == ".thclaws" || name == ".git" || name == "node_modules")
        })
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(workspace) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !set.is_match(rel) {
            continue;
        }
        let rel_str = rel.to_string_lossy().to_string();
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if artifacts.len() >= MAX_ARTIFACT_FILES || total + size > MAX_ARTIFACT_TOTAL_BYTES {
            skipped.push(rel_str);
            continue;
        }
        let bytes = match std::fs::read(entry.path()) {
            Ok(b) => b,
            Err(_) => {
                skipped.push(rel_str);
                continue;
            }
        };
        let sha = hex(&Sha256::digest(&bytes));
        let dest = files_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &bytes)?;
        total += bytes.len() as u64;
        artifacts.push(ArtifactEntry {
            id: format!("a{}", artifacts.len() + 1),
            path: rel_str,
            size: bytes.len() as u64,
            sha256: sha,
        });
    }
    // Deterministic ordering (walkdir order is fs-dependent): sort by
    // path, then re-assign ids so `a1` is stable across identical runs.
    artifacts.sort_by(|a, b| a.path.cmp(&b.path));
    for (i, a) in artifacts.iter_mut().enumerate() {
        a.id = format!("a{}", i + 1);
    }

    let manifest = ArtifactManifest {
        session_id: session_id.to_string(),
        collected_at: chrono::Utc::now().to_rfc3339(),
        patterns: patterns.to_vec(),
        artifacts,
        skipped,
        status: SnapshotStatus::Completed,
        error: None,
    };
    std::fs::write(
        root.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(manifest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceQuery {
    pub workspace_dir: Option<String>,
}

fn resolve_workspace(q: &WorkspaceQuery) -> Result<PathBuf, Box<Response>> {
    match q
        .workspace_dir
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(raw) => crate::agent_runtime::validate_workspace_dir(raw).map_err(|msg| {
            (
                StatusCode::BAD_REQUEST,
                Json(OpenAiError::invalid_request(msg, "invalid_workspace_dir")),
            )
                .into_response()
                .into()
        }),
        None => std::env::current_dir().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(OpenAiError::server_error(format!("daemon CWD: {e}"))),
            )
                .into_response()
                .into()
        }),
    }
}

/// `GET /v1/sessions/{sid}/artifacts` — the frozen manifest.
pub async fn get_manifest(
    _auth: AuthOk,
    AxPath(sid): AxPath<String>,
    Query(q): Query<WorkspaceQuery>,
) -> Result<Response, Response> {
    let ws = resolve_workspace(&q).map_err(|response| *response)?;
    if !safe_session_id(&sid) {
        return Err(bad_id());
    }
    let path = artifacts_root(&ws, &sid).join("manifest.json");
    let raw = std::fs::read(&path)
        .map_err(|_| not_found_code(NO_SNAPSHOT_MSG, "snapshot_not_requested"))?;
    let manifest: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|_| not_found_code("artifact manifest is unreadable", "manifest_unreadable"))?;
    Ok(Json(manifest).into_response())
}

/// `GET /v1/sessions/{sid}/artifacts/{aid}` — one snapshotted file, exactly
/// the bytes that were hashed at collection time.
pub async fn get_artifact(
    _auth: AuthOk,
    AxPath((sid, aid)): AxPath<(String, String)>,
    Query(q): Query<WorkspaceQuery>,
) -> Result<Response, Response> {
    let ws = resolve_workspace(&q).map_err(|response| *response)?;
    if !safe_session_id(&sid) {
        return Err(bad_id());
    }
    let root = artifacts_root(&ws, &sid);
    let raw = std::fs::read(root.join("manifest.json"))
        .map_err(|_| not_found_code(NO_SNAPSHOT_MSG, "snapshot_not_requested"))?;
    let manifest: ArtifactManifest =
        serde_json::from_slice(&raw).map_err(|_| not_found("manifest unreadable"))?;
    let entry = manifest
        .artifacts
        .iter()
        .find(|a| a.id == aid || a.path == aid)
        .ok_or_else(|| not_found("no such artifact id"))?;
    // Serve from the SNAPSHOT — never the live workspace file.
    let file = root.join("files").join(&entry.path);
    let bytes = std::fs::read(&file).map_err(|_| not_found("artifact file missing"))?;
    Ok((
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/octet-stream".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"{}\"",
                    entry.path.rsplit('/').next().unwrap_or("artifact")
                ),
            ),
            (
                axum::http::HeaderName::from_static("x-sha256"),
                entry.sha256.clone(),
            ),
        ],
        bytes,
    )
        .into_response())
}

// ── inputs (orchestrator → workspace) ─────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct InputFile {
    /// Workspace-relative path. Must land under an allowed prefix.
    pub path: String,
    /// base64-encoded content.
    pub content_base64: String,
}

#[derive(Debug, Deserialize)]
pub struct InputsRequest {
    pub workspace_dir: Option<String>,
    pub files: Vec<InputFile>,
}

/// Allowed destination prefixes for `POST /v1/inputs`. Default `inputs/`
/// — safe-by-default; the agent reads its inputs from there. Operators
/// widen it with `THCLAWS_INPUTS_PREFIXES="inputs/,src/,docs/"` or open
/// the whole workspace (minus `.thclaws/` + `.git/`) with `*`.
fn allowed_prefixes() -> Vec<String> {
    match std::env::var("THCLAWS_INPUTS_PREFIXES") {
        Ok(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec!["inputs/".to_string()],
    }
}

fn path_allowed(rel: &str, prefixes: &[String]) -> bool {
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.contains("..")
        || rel.starts_with(".thclaws/")
        || rel == ".thclaws"
        || rel.starts_with(".git/")
        || rel == ".git"
    {
        return false;
    }
    prefixes
        .iter()
        .any(|p| p == "*" || rel.starts_with(p.as_str()))
}

/// `POST /v1/inputs` — place files into a workspace ahead of a dispatch.
/// The coder→reviewer handoff: orchestrator downloads worker A's
/// artifacts, POSTs them here to worker B, then `/agent/run`s B.
pub async fn post_inputs(
    _auth: AuthOk,
    Json(req): Json<InputsRequest>,
) -> Result<Response, Response> {
    let ws = resolve_workspace(&WorkspaceQuery {
        workspace_dir: req.workspace_dir.clone(),
    })
    .map_err(|response| *response)?;
    if req.files.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(OpenAiError::invalid_request("files[] is empty", "no_files")),
        )
            .into_response());
    }
    if req.files.len() > MAX_INPUT_FILES {
        return Err(limit(format!("more than {MAX_INPUT_FILES} files")));
    }
    let prefixes = allowed_prefixes();
    let mut written: Vec<serde_json::Value> = Vec::new();
    let mut total: usize = 0;
    for f in &req.files {
        if !path_allowed(&f.path, &prefixes) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(OpenAiError::invalid_request(
                    format!(
                        "path '{}' not under an allowed prefix ({}) — set THCLAWS_INPUTS_PREFIXES to widen",
                        f.path,
                        prefixes.join(", ")
                    ),
                    "path_not_allowed",
                )),
            )
                .into_response());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(f.content_base64.as_bytes())
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(OpenAiError::invalid_request(
                        format!("{}: bad base64: {e}", f.path),
                        "bad_base64",
                    )),
                )
                    .into_response()
            })?;
        total += bytes.len();
        if total > MAX_INPUT_TOTAL_BYTES {
            return Err(limit(format!(
                "total decoded size > {MAX_INPUT_TOTAL_BYTES} bytes"
            )));
        }
        let dest = ws.join(&f.path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let sha = hex(&Sha256::digest(&bytes));
        std::fs::write(&dest, &bytes).map_err(io_err)?;
        written.push(json!({ "path": f.path, "size": bytes.len(), "sha256": sha }));
    }
    Ok(Json(json!({
        "workspace_dir": ws.display().to_string(),
        "written": written,
    }))
    .into_response())
}

fn bad_id() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(OpenAiError::invalid_request(
            "invalid session id",
            "invalid_session_id",
        )),
    )
        .into_response()
}

/// Record a failed snapshot so the outcome is readable over HTTP rather than
/// only in the daemon's stderr (#191). Written on the error path only — runs
/// that never asked for artifacts stay free of any per-run write.
///
/// Best-effort by nature: the usual reason collection failed is an unwritable
/// workspace, in which case there is nowhere to record it either and the log
/// line stays the only trace.
pub(crate) fn record_snapshot_failure(
    workspace: &Path,
    session_id: &str,
    patterns: &[String],
    error: &str,
) -> std::io::Result<()> {
    let root = artifacts_root(workspace, session_id);
    std::fs::create_dir_all(&root)?;
    let manifest = ArtifactManifest {
        session_id: session_id.to_string(),
        collected_at: chrono::Utc::now().to_rfc3339(),
        patterns: patterns.to_vec(),
        artifacts: Vec::new(),
        skipped: Vec::new(),
        status: SnapshotStatus::Failed,
        error: Some(error.to_string()),
    };
    std::fs::write(
        root.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )
}

/// 404 carrying a machine-readable `code`, so a client can tell "this run
/// never asked for artifacts" from "the manifest is corrupt" without
/// string-matching the message (#191).
fn not_found_code(msg: &str, code: &'static str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(OpenAiError::invalid_request(msg.to_string(), code)),
    )
        .into_response()
}

const NO_SNAPSHOT_MSG: &str = "no artifact snapshot for this session — the run did not request one (`collect_files` was omitted or empty)";

fn not_found(msg: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(OpenAiError::invalid_request(msg.to_string(), "not_found")),
    )
        .into_response()
}

fn limit(msg: String) -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(OpenAiError::invalid_request(msg, "limit_exceeded")),
    )
        .into_response()
}

fn io_err(e: std::io::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(OpenAiError::server_error(format!("io: {e}"))),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_freezes_bytes_and_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("reports")).unwrap();
        std::fs::write(ws.join("reports/q3.pdf"), b"PDFBYTES").unwrap();
        std::fs::write(ws.join("notes.txt"), b"not collected").unwrap();

        let m = snapshot_artifacts(ws, "sess-test", &["reports/*.pdf".to_string()]).unwrap();
        assert_eq!(m.artifacts.len(), 1);
        assert_eq!(m.artifacts[0].path, "reports/q3.pdf");
        assert_eq!(m.artifacts[0].id, "a1");

        // Mutate the live file AFTER collection — the snapshot must not change.
        std::fs::write(ws.join("reports/q3.pdf"), b"TAMPERED").unwrap();
        let frozen = std::fs::read(
            artifacts_root(ws, "sess-test")
                .join("files")
                .join("reports/q3.pdf"),
        )
        .unwrap();
        assert_eq!(frozen, b"PDFBYTES");
        let expected_sha = hex(&Sha256::digest(b"PDFBYTES"));
        assert_eq!(m.artifacts[0].sha256, expected_sha);
    }

    #[test]
    fn snapshot_never_recurses_into_thclaws() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join(".thclaws/state/kms")).unwrap();
        std::fs::write(ws.join(".thclaws/state/kms/secret.md"), b"x").unwrap();
        let m = snapshot_artifacts(ws, "s", &["**/*.md".to_string()]).unwrap();
        assert!(m.artifacts.is_empty());
    }

    /// #191 — the four states a caller has to be able to tell apart.
    #[test]
    fn snapshot_states_are_distinguishable() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        // 1. Requested, matched nothing. A completed snapshot of nothing is
        //    NOT a failure — this already worked and must keep working.
        let m = snapshot_artifacts(ws, "s-empty", &["reports/*.pdf".to_string()]).unwrap();
        assert!(m.artifacts.is_empty());
        assert_eq!(m.status, SnapshotStatus::Completed);
        assert!(m.error.is_none());
        assert!(artifacts_root(ws, "s-empty").join("manifest.json").exists());

        // 2. Requested and matched. Same status, artifacts present.
        std::fs::create_dir_all(ws.join("reports")).unwrap();
        std::fs::write(ws.join("reports/q3.pdf"), b"PDF").unwrap();
        let m = snapshot_artifacts(ws, "s-ok", &["reports/*.pdf".to_string()]).unwrap();
        assert_eq!(m.status, SnapshotStatus::Completed);
        assert_eq!(m.artifacts.len(), 1);

        // 3. Never requested — no manifest is written at all, which is what
        //    makes the GET's `snapshot_not_requested` 404 meaningful. The
        //    point is that a run with no `collect_files` costs no write.
        assert!(!artifacts_root(ws, "s-never").join("manifest.json").exists());

        // 4. Requested and failed. The outcome is persisted rather than
        //    living only in the daemon's stderr.
        record_snapshot_failure(ws, "s-fail", &["out/*.bin".to_string()], "disk on fire").unwrap();
        let raw = std::fs::read(artifacts_root(ws, "s-fail").join("manifest.json")).unwrap();
        let m: ArtifactManifest = serde_json::from_slice(&raw).unwrap();
        assert_eq!(m.status, SnapshotStatus::Failed);
        assert_eq!(m.error.as_deref(), Some("disk on fire"));
        assert_eq!(m.patterns, vec!["out/*.bin".to_string()]);
        assert!(m.artifacts.is_empty());
    }

    /// The states above are only worth recording if a client can still tell
    /// them apart after the response is built. The manifest tests prove the
    /// outcome is *stored*; this one covers the layer #191 actually reported —
    /// a GET that answered 404 identically for "never asked" and "the manifest
    /// is corrupt", leaving the caller to read server logs.
    #[tokio::test]
    async fn get_manifest_keeps_the_states_apart_over_http() {
        // `validate_workspace_dir` reads THCLAWS_AGENT_WORKSPACE_ROOT.
        let _env = crate::kms::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        // The handler canonicalizes what it is given (/var → /private/var on
        // macOS); write where it will look, not where tempfile said.
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        let q = || {
            Query(WorkspaceQuery {
                workspace_dir: Some(ws.to_string_lossy().into_owned()),
            })
        };
        // Both arms are a Response — the status is what distinguishes them, so
        // flattening keeps the assertions on the wire shape rather than on
        // which side of the Result the handler chose.
        async fn wire(r: Result<Response, Response>) -> (StatusCode, serde_json::Value) {
            let r = match r {
                Ok(v) | Err(v) => v,
            };
            let status = r.status();
            let body = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, serde_json::from_slice(&body).unwrap())
        }

        // 1. Never requested — 404, but a code that says which 404 this is.
        let (status, body) = wire(get_manifest(AuthOk, AxPath("s-never".into()), q()).await).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "snapshot_not_requested");

        // 2. Corrupt manifest — same status, different code. Collapsing these
        //    two onto one code would restore the exact ambiguity #191 filed.
        let root = artifacts_root(&ws, "s-corrupt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("manifest.json"), b"{ not json").unwrap();
        let (status, body) =
            wire(get_manifest(AuthOk, AxPath("s-corrupt".into()), q()).await).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "manifest_unreadable");

        // 3. Requested and failed — 200. The failure is data the caller reads,
        //    not something inferred from a status code.
        record_snapshot_failure(&ws, "s-fail", &["out/*.bin".to_string()], "disk on fire").unwrap();
        let (status, body) = wire(get_manifest(AuthOk, AxPath("s-fail".into()), q()).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "failed");
        assert_eq!(body["error"], "disk on fire");

        // 4. Requested, matched nothing — also 200, and must not read as a
        //    failure: the patterns ran and the answer was "none".
        snapshot_artifacts(&ws, "s-empty", &["nope/*.pdf".to_string()]).unwrap();
        let (status, body) = wire(get_manifest(AuthOk, AxPath("s-empty".into()), q()).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "completed");
        assert!(body["artifacts"].as_array().unwrap().is_empty());
        assert!(body.get("error").is_none());
    }

    /// A manifest written before `status` existed could only have been a
    /// success, so it must read back as one — not as a failure.
    #[test]
    fn pre_status_manifests_read_as_completed() {
        let legacy = br#"{
            "session_id": "s-old",
            "collected_at": "2026-01-01T00:00:00Z",
            "patterns": ["*.pdf"],
            "artifacts": []
        }"#;
        let m: ArtifactManifest = serde_json::from_slice(legacy).unwrap();
        assert_eq!(m.status, SnapshotStatus::Completed);
        assert!(m.error.is_none());
    }

    /// `status` serializes as the documented lowercase string, and `error`
    /// stays absent on success rather than serializing as null.
    #[test]
    fn status_wire_shape_is_stable() {
        let m = ArtifactManifest {
            session_id: "s".into(),
            collected_at: "t".into(),
            patterns: vec![],
            artifacts: vec![],
            skipped: vec![],
            status: SnapshotStatus::Completed,
            error: None,
        };
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        assert_eq!(v["status"], "completed");
        assert!(v.get("error").is_none(), "no null error on success: {v}");

        let failed = ArtifactManifest {
            status: SnapshotStatus::Failed,
            error: Some("boom".into()),
            ..m
        };
        let v: serde_json::Value = serde_json::to_value(&failed).unwrap();
        assert_eq!(v["status"], "failed");
        assert_eq!(v["error"], "boom");
    }

    #[test]
    fn inputs_path_jail() {
        let p = vec!["inputs/".to_string()];
        assert!(path_allowed("inputs/a.txt", &p));
        assert!(!path_allowed("../etc/passwd", &p));
        assert!(!path_allowed("/abs/path", &p));
        assert!(!path_allowed(".thclaws/settings.json", &p));
        assert!(!path_allowed(".git/config", &p));
        assert!(!path_allowed("src/main.rs", &p));
        let star = vec!["*".to_string()];
        assert!(path_allowed("src/main.rs", &star));
        assert!(!path_allowed(".thclaws/settings.json", &star));
    }

    #[test]
    fn session_id_shape() {
        assert!(safe_session_id("sess-18c05f849ed5b1b8"));
        assert!(!safe_session_id("../../etc"));
        assert!(!safe_session_id(""));
        assert!(!safe_session_id("a/b"));
    }
}
