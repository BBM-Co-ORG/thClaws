//! Veo video provider (dev-plan/40, Tier 2).
//!
//! Lifts the proven wire shape from the short-video-generator /
//! movie-maker Python scripts (`video.py`, `animate.py`) into the
//! `VideoProvider` trait. Veo goes through `generativelanguage.
//! googleapis.com` (or `<gateway>/google` in cloud), same as Gemini
//! images, with `x-goog-api-key` auth.
//!
//! Flow:
//!   submit → `POST .../models/<model>:predictLongRunning` → operation name
//!   poll   → `GET  .../v1beta/<operation>` until `done:true`
//!   done   → walk the (several) response shapes for inline base64 or a
//!            downloadable URI; fetch the URI through the gateway when
//!            in cloud mode (the pod can't reach Google directly).
//!
//! Constraints baked in from the scripts: Veo 3.1 accepts only 9:16 /
//! 16:9 (other ratios are mapped), and `durationSeconds` MUST be a JSON
//! number — the string form caused a movie-maker regression.

use crate::error::{Error, Result};
use crate::media::provider::{
    ImageModelInfo, JobState, ProviderJobRef, VideoProvider, VideoRequest,
};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com";

const MODELS: &[ImageModelInfo] = &[
    ImageModelInfo {
        id: "veo-3.1-fast-generate-preview",
        // "" marks the default video model (cheaper/faster).
        aliases: &["", "veo", "veo-fast", "fast"],
        label: "Veo 3.1 Fast",
    },
    ImageModelInfo {
        id: "veo-3.1-generate-preview",
        aliases: &["veo-quality", "quality"],
        label: "Veo 3.1",
    },
    ImageModelInfo {
        id: "veo-3.1-lite-generate-preview",
        aliases: &["veo-lite", "lite"],
        label: "Veo 3.1 Lite",
    },
];

pub struct VeoVideoProvider;

impl VeoVideoProvider {
    /// Veo 3.1 only accepts 9:16 or 16:9. Square/portrait → vertical,
    /// landscape → horizontal (mirrors video.py's mapping).
    fn aspect(req: &VideoRequest) -> &'static str {
        match req.aspect_ratio.as_str() {
            "1:1" | "3:4" | "9:16" => "9:16",
            _ => "16:9",
        }
    }

    fn client(timeout_secs: u64) -> Result<Client> {
        Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| Error::Tool(format!("http client: {e}")))
    }

    /// Download a URI from an operation result, rewriting the host to
    /// the gateway when in cloud mode (the pod has no egress to Google).
    async fn fetch_bytes(base_url: &str, api_key: &str, uri: &str) -> Result<Vec<u8>> {
        let url =
            if base_url.ends_with("/google") && uri.contains("generativelanguage.googleapis.com") {
                let suffix = uri
                    .split_once("generativelanguage.googleapis.com")
                    .map(|(_, s)| s)
                    .unwrap_or("");
                format!("{base_url}{suffix}")
            } else {
                uri.to_string()
            };
        let client = Self::client(180)?;
        let resp = crate::multi_tenant::attach_member(client.get(&url))
            .header("x-goog-api-key", api_key)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("video download http: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Tool(format!(
                "video download http {}",
                resp.status()
            )));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| Error::Tool(format!("video download body: {e}")))
    }

    /// Walk the several response shapes Veo emits and return either the
    /// inline base64 bytes or a URI to fetch. Returns `Ok(Some(bytes))`,
    /// `Ok(None)` if a URI fetch is needed (handled by caller), via the
    /// `Either` encoded as a tagged tuple.
    fn find_inline_or_uri(response: &Value) -> Option<InlineOrUri> {
        // Shape A: predictions[] (Vertex-style)
        for pred in response
            .get("predictions")
            .and_then(|v| v.as_array())
            .unwrap_or(&vec![])
        {
            if let Some(b64) = pred
                .get("videoBytes")
                .or_else(|| pred.get("bytesBase64Encoded"))
                .and_then(|v| v.as_str())
            {
                return Some(InlineOrUri::Inline(b64.to_string()));
            }
            if let Some(uri) = pred
                .get("gcsUri")
                .or_else(|| pred.get("uri"))
                .or_else(|| pred.get("videoUri"))
                .and_then(|v| v.as_str())
            {
                return Some(InlineOrUri::Uri(uri.to_string()));
            }
        }
        // Shape B: generatedSamples[] and Shape C: generateVideoResponse.generatedSamples[]
        let sample_lists = [
            response.get("generatedSamples"),
            response
                .get("generateVideoResponse")
                .and_then(|g| g.get("generatedSamples")),
        ];
        for samples in sample_lists.into_iter().flatten() {
            for sample in samples.as_array().unwrap_or(&vec![]) {
                let video = sample.get("video").cloned().unwrap_or(Value::Null);
                if let Some(b64) = video.get("bytesBase64Encoded").and_then(|v| v.as_str()) {
                    return Some(InlineOrUri::Inline(b64.to_string()));
                }
                if let Some(uri) = video
                    .get("uri")
                    .or_else(|| video.get("gcsUri"))
                    .and_then(|v| v.as_str())
                {
                    return Some(InlineOrUri::Uri(uri.to_string()));
                }
            }
        }
        None
    }
}

enum InlineOrUri {
    Inline(String),
    Uri(String),
}

#[async_trait]
impl VideoProvider for VeoVideoProvider {
    fn id(&self) -> &'static str {
        "veo"
    }
    fn models(&self) -> &'static [ImageModelInfo] {
        MODELS
    }
    fn resolve_model(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        for m in MODELS {
            if raw == m.id || m.aliases.contains(&raw) {
                return Some(m.id.to_string());
            }
        }
        // Forward-compat: accept any future `veo-*` id verbatim.
        if raw.starts_with("veo-") {
            return Some(raw.to_string());
        }
        None
    }

    async fn submit(&self, req: &VideoRequest) -> Result<ProviderJobRef> {
        let ep = crate::media::provider::resolve_endpoint(
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            GEMINI_BASE,
            "google",
        )?;
        let aspect = Self::aspect(req);

        let mut instance = json!({ "prompt": req.prompt });
        if let Some(img) = &req.init_image {
            instance["image"] = json!({
                "bytesBase64Encoded": B64.encode(&img.bytes),
                "mimeType": img.mime,
            });
        }
        let body = json!({
            "instances": [instance],
            "parameters": {
                "aspectRatio": aspect,
                // MUST be a JSON number, not a string (movie-maker regression).
                "durationSeconds": req.duration_seconds,
                "personGeneration": "allow_all",
            }
        });
        let url = format!(
            "{}/v1beta/models/{}:predictLongRunning",
            ep.base_url.trim_end_matches('/'),
            req.model
        );
        let client = Self::client(60)?;
        let resp = crate::multi_tenant::attach_member(client.post(&url))
            .header("x-goog-api-key", &ep.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("veo submit http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "veo submit http {status}: {}",
                &b[..b.len().min(400)]
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("veo submit response not json: {e}")))?;
        let name = v
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| Error::Tool("veo submit response missing 'name'".into()))?;
        Ok(ProviderJobRef {
            op: name.to_string(),
        })
    }

    async fn poll(&self, job: &ProviderJobRef) -> Result<JobState> {
        let ep = crate::media::provider::resolve_endpoint(
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            GEMINI_BASE,
            "google",
        )?;
        let url = format!("{}/v1beta/{}", ep.base_url.trim_end_matches('/'), job.op);
        let client = Self::client(30)?;
        let resp = crate::multi_tenant::attach_member(client.get(&url))
            .header("x-goog-api-key", &ep.api_key)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("veo poll http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "veo poll http {status}: {}",
                &b[..b.len().min(400)]
            )));
        }
        let op: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("veo poll response not json: {e}")))?;

        if !op.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
            return Ok(JobState::Running { pct: None });
        }
        if let Some(err) = op.get("error") {
            return Ok(JobState::Failed {
                msg: format!(
                    "veo op error: {}",
                    &err.to_string()[..err.to_string().len().min(300)]
                ),
            });
        }
        let response = op.get("response").cloned().unwrap_or(Value::Null);
        match Self::find_inline_or_uri(&response) {
            Some(InlineOrUri::Inline(b64)) => {
                let bytes = B64
                    .decode(b64)
                    .map_err(|e| Error::Tool(format!("video base64 decode: {e}")))?;
                Ok(JobState::Done { bytes })
            }
            Some(InlineOrUri::Uri(uri)) => {
                let bytes = Self::fetch_bytes(&ep.base_url, &ep.api_key, &uri).await?;
                Ok(JobState::Done { bytes })
            }
            None => Ok(JobState::Failed {
                msg: format!(
                    "veo done but no video in response: {}",
                    &response.to_string()[..response.to_string().len().min(300)]
                ),
            }),
        }
    }
}
