//! Per-backend submit/poll/download (dev-plan/52 D5b). The compiler emits a
//! backend-native payload (`filmscript::backend`); this routes it to the
//! right API and writes the finished clip to `out`:
//!
//! - **Grok / Seedance** — Kie jobs (`createTask`/`recordInfo`), reused from
//!   [`KieClient`].
//! - **Veo** — Kie's Veo route (same key), [`KieClient::create_veo_task`].
//! - **LTX** — native `api.ltx.video` **sync** `/v1/{op}` returning raw MP4
//!   bytes (no poll); `LTX_BASE_URL` override points at a self-host (D6).
//! - **Happy Horse** — DashScope async video-synthesis + task poll.
//!
//! Each request mirrors the live-validated lab calls
//! (`dev-plan/52-ltx-lab`). Endpoint choice is derived from the payload
//! shape so the harness stays backend-agnostic beyond this file.

use super::kie::{first_mp4_url, KieClient, TaskResult};
use super::USER_AGENT;
use crate::error::{Error, Result};
use crate::filmscript::backend::BackendId;
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(15);
const POLL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Submit, poll if needed, and download the clip to `out`. `on_task_id` is
/// called with the provider task id right after submit so the caller can
/// persist it for resume (sync LTX passes a synthetic id). Returns the task
/// id + `TaskResult` (an empty `clip_url` means "no re-downloadable URL" —
/// the sync LTX case).
pub async fn generate_clip(
    backend: BackendId,
    payload: &Value,
    kie: &KieClient,
    out: &Path,
    cancel: &AtomicBool,
    mut on_task_id: impl FnMut(&str),
) -> Result<(String, TaskResult)> {
    match backend {
        BackendId::Grok | BackendId::Seedance => {
            let tid = kie.create_task(payload).await?;
            on_task_id(&tid);
            let r = kie.poll(&tid, cancel).await?;
            kie.download(&r.clip_url, out).await?;
            Ok((tid, r))
        }
        BackendId::Veo => {
            let tid = kie.create_veo_task(payload).await?;
            on_task_id(&tid);
            let r = kie.poll_veo(&tid, cancel).await?;
            kie.download(&r.clip_url, out).await?;
            Ok((tid, r))
        }
        BackendId::Ltx => {
            on_task_id("ltx-sync");
            ltx_generate(payload, out).await?;
            Ok((
                "ltx-sync".into(),
                TaskResult {
                    clip_url: String::new(),
                    credits: None,
                },
            ))
        }
        BackendId::HappyHorse => {
            let tid = dashscope_submit(payload).await?;
            on_task_id(&tid);
            let url = dashscope_poll(&tid, cancel).await?;
            kie.download(&url, out).await?;
            Ok((
                tid,
                TaskResult {
                    clip_url: url,
                    credits: None,
                },
            ))
        }
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .expect("reqwest client")
}

/// LTX op → native endpoint, chosen from the payload shape.
fn ltx_endpoint(payload: &Value) -> &'static str {
    if payload.get("audio_uri").is_some() {
        "audio-to-video"
    } else if payload.get("image_uri").is_some() {
        "image-to-video"
    } else {
        "text-to-video"
    }
}

fn ltx_base() -> String {
    std::env::var("LTX_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://api.ltx.video".to_string())
}

/// LTX sync `/v1/{op}` returns the MP4 bytes directly (no job/poll). A JSON
/// body instead of video means an API error. BYOK-or-gateway endpoint
/// (dev-plan/53 Stage D): the a2v payload carries a `duration` billing
/// hint for the gateway meter; LTX's own a2v API derives length from
/// the audio and takes no duration, so the hint is stripped here on
/// the direct path (the gateway strips it on the metered path).
async fn ltx_generate(payload: &Value, out: &Path) -> Result<()> {
    let ep = crate::media::provider::resolve_endpoint(&["LTX_API_KEY"], &ltx_base(), "ltx")?;
    let op = ltx_endpoint(payload);
    let stripped;
    let payload = if op == "audio-to-video" && !ep.via_gateway {
        let mut v = payload.clone();
        if let Some(obj) = v.as_object_mut() {
            obj.remove("duration");
        }
        stripped = v;
        &stripped
    } else {
        payload
    };
    let url = format!("{}/v1/{}", ep.base_url, op);
    let resp = crate::multi_tenant::attach_member(http().post(&url))
        .bearer_auth(&ep.api_key)
        .json(payload)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("ltx generate: {e}")))?;
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::Tool(format!("ltx generate body: {e}")))?;
    if !ct.contains("mp4") && !ct.contains("octet-stream") && bytes.len() < 10_000 {
        return Err(Error::Tool(format!(
            "ltx generate returned no video: {}",
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(250)
                .collect::<String>()
        )));
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, &bytes)?;
    Ok(())
}

const DASHSCOPE_BASE: &str = "https://dashscope-intl.aliyuncs.com";
const DASHSCOPE_PATH: &str = "/api/v1/services/aigc/video-generation/video-synthesis";

async fn dashscope_submit(payload: &Value) -> Result<String> {
    let ep = crate::media::provider::resolve_dashscope_endpoint(DASHSCOPE_BASE)?;
    let resp: Value =
        crate::multi_tenant::attach_member(http().post(format!("{}{DASHSCOPE_PATH}", ep.base_url)))
            .bearer_auth(&ep.api_key)
            .header("X-DashScope-Async", "enable")
            .json(payload)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("dashscope submit: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Tool(format!("dashscope submit response: {e}")))?;
    resp.pointer("/output/task_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            Error::Tool(format!(
                "dashscope submit failed: {}",
                resp.to_string().chars().take(250).collect::<String>()
            ))
        })
}

async fn dashscope_poll(task_id: &str, cancel: &AtomicBool) -> Result<String> {
    let ep = crate::media::provider::resolve_dashscope_endpoint(DASHSCOPE_BASE)?;
    let started = std::time::Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Tool("job cancelled".into()));
        }
        if started.elapsed() > POLL_TIMEOUT {
            return Err(Error::Tool(format!(
                "dashscope task {task_id} still not terminal after {}s",
                POLL_TIMEOUT.as_secs()
            )));
        }
        let resp: Value = crate::multi_tenant::attach_member(
            http().get(format!("{}/api/v1/tasks/{task_id}", ep.base_url)),
        )
        .bearer_auth(&ep.api_key)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("dashscope poll: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Tool(format!("dashscope poll response: {e}")))?;
        match resp.pointer("/output/task_status").and_then(Value::as_str) {
            Some("SUCCEEDED") => {
                return resp
                    .pointer("/output/video_url")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| first_mp4_url(&resp))
                    .ok_or_else(|| {
                        Error::Tool(format!("dashscope {task_id}: SUCCEEDED but no video_url"))
                    });
            }
            Some("FAILED") | Some("CANCELED") | Some("UNKNOWN") => {
                return Err(Error::Tool(format!(
                    "dashscope task {task_id} failed: {}",
                    resp.pointer("/output/message")
                        .and_then(Value::as_str)
                        .unwrap_or("(no message)")
                )));
            }
            _ => tokio::time::sleep(POLL_INTERVAL).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ltx_endpoint_from_payload_shape() {
        assert_eq!(
            ltx_endpoint(&json!({"model":"ltx-2-3-pro","audio_uri":"x","image_uri":"y"})),
            "audio-to-video"
        );
        assert_eq!(
            ltx_endpoint(&json!({"model":"ltx-2-3-fast","image_uri":"y"})),
            "image-to-video"
        );
        assert_eq!(
            ltx_endpoint(&json!({"model":"ltx-2-3-fast","prompt":"z"})),
            "text-to-video"
        );
    }

    #[test]
    fn first_mp4_url_scans_nested() {
        let v = json!({"data":{"response":{"resultUrls":["https://x/clip.mp4"]}}});
        assert_eq!(first_mp4_url(&v).as_deref(), Some("https://x/clip.mp4"));
        assert!(first_mp4_url(&json!({"data":{"successFlag":0}})).is_none());
    }

    // Live smokes (each costs real money + needs the backend's key): compile
    // a text-only shot for one backend through the real pipeline and generate
    // a clip. Run individually, e.g.:
    //   KIE_API_KEY=… cargo test --lib grok_live_smoke -- --ignored --nocapture
    //   LTX_API_KEY=… cargo test --lib ltx_live_smoke  -- --ignored --nocapture
    async fn smoke(backend: BackendId) {
        use crate::filmscript::{compile_phase1, compile_phase2};
        let script = format!(
            "film \"t\" {{\nbackend: {}\n}}\nshot 1 {{\nwaves crashing on an empty beach at sunset, cinematic, dynamic camera\n@duration: 6\n}}\n",
            backend.as_str()
        );
        let p1 = compile_phase1(&script);
        assert!(!p1.has_errors(), "{:?}", p1.errors);
        let p2 = compile_phase2(&p1, &[]);
        let payload = &p2.payloads[0];
        assert_eq!(payload.backend, backend);
        println!("[{}] payload: {}", backend.as_str(), payload.payload);

        let out = std::env::temp_dir().join(format!("{}_smoke.mp4", backend.as_str()));
        let _ = std::fs::remove_file(&out);
        // KieClient is used by the Kie backends + as a plain URL downloader
        // (Happy Horse); LTX/DashScope read their own keys inside dispatch.
        let kie = KieClient::new(std::env::var("KIE_API_KEY").unwrap_or_default());
        let cancel = AtomicBool::new(false);
        let (tid, res) = generate_clip(backend, &payload.payload, &kie, &out, &cancel, |t| {
            println!("[{}] task {t}", backend.as_str())
        })
        .await
        .expect("generate");
        let sz = std::fs::metadata(&out).expect("clip file").len();
        println!(
            "[{}] task={tid} credits={:?} bytes={sz} → {}",
            backend.as_str(),
            res.credits,
            out.display()
        );
        assert!(sz > 50_000, "clip too small: {sz} bytes");
    }

    // Engine codegen for the reference reel across all backends → JSON the
    // Python orchestrator fires. Uses mock asset URLs (MOCK://<id>) that the
    // orchestrator swaps for the real uploaded URLs.
    //   cargo test --lib reel_dump -- --ignored --nocapture
    #[test]
    #[ignore]
    fn reel_dump() {
        use crate::filmscript::{compile_phase1, compile_phase2, AssetRequest, ResolvedAsset};
        use serde_json::{json, Value};
        let dir = "/Volumes/Data01/agentic-workspace/dev-plan/52-ltx-lab/reel";
        let base = std::fs::read_to_string(format!("{dir}/three.film")).expect("read");

        // asset_id → path (mock URLs keyed on id) from a default compile.
        let p0 = compile_phase1(&base);
        assert!(!p0.has_errors(), "{:?}", p0.errors);
        let mut asset_map = serde_json::Map::new();
        let mock: Vec<ResolvedAsset> = p0
            .asset_requests
            .iter()
            .filter_map(|r| match r {
                AssetRequest::File { id, path, .. } => {
                    asset_map.insert(id.clone(), json!(path));
                    Some(ResolvedAsset {
                        id: id.clone(),
                        url: format!("MOCK://{id}"),
                        duration_ms: None,
                    })
                }
                _ => None,
            })
            .collect();

        let mut out = serde_json::Map::new();
        out.insert("assets".into(), Value::Object(asset_map));
        // dialogue lines for TTS overlay (shot_id, speaker, text)
        let dlg: Vec<Value> = p0
            .shots
            .iter()
            .filter_map(|s| {
                s.resolved
                    .dialogue
                    .as_ref()
                    .map(|d| json!({"shot": s.id(), "speaker": d.speaker, "text": d.text}))
            })
            .collect();
        out.insert("dialogue".into(), json!(dlg));

        for backend in ["grok", "seedance", "ltx", "veo", "happyhorse"] {
            let src = base.replacen(
                "aspect: 16:9",
                &format!("aspect: 16:9\n    backend: {backend}"),
                1,
            );
            let p1 = compile_phase1(&src);
            // Tolerant: capture compile limits (e.g. LTX's 2-ref cap on the
            // 3-character shots) instead of crashing.
            let errs: Vec<Value> = p1
                .errors
                .iter()
                .filter(|e| e.severity == crate::filmscript::Severity::Error)
                .map(|e| json!({"shot": e.shot, "code": e.code, "msg": e.message}))
                .collect();
            let p2 = compile_phase2(&p1, &mock);
            let shots: Vec<Value> = p2
                .payloads
                .iter()
                .map(|p| json!({"shot": p.shot_id, "payload": p.payload}))
                .collect();
            out.insert(backend.into(), json!({"shots": shots, "errors": errs}));
            println!(
                "[{backend}] {} payloads, {} errors",
                p2.payloads.len(),
                errs.len()
            );
        }
        std::fs::write(
            format!("{dir}/payloads.json"),
            serde_json::to_string_pretty(&Value::Object(out)).unwrap(),
        )
        .unwrap();
        println!("wrote {dir}/payloads.json");
    }

    #[tokio::test]
    #[ignore]
    async fn grok_live_smoke() {
        smoke(BackendId::Grok).await
    }

    #[tokio::test]
    #[ignore]
    async fn seedance_live_smoke() {
        smoke(BackendId::Seedance).await
    }

    #[tokio::test]
    #[ignore]
    async fn ltx_live_smoke() {
        smoke(BackendId::Ltx).await
    }

    #[tokio::test]
    #[ignore]
    async fn veo_live_smoke() {
        smoke(BackendId::Veo).await
    }

    #[tokio::test]
    #[ignore]
    async fn happyhorse_live_smoke() {
        smoke(BackendId::HappyHorse).await
    }
}
