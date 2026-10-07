//! Gemini image provider (dev-plan/40, Tier 1).
//!
//! Lifts the call logic that used to live inline in
//! `tools/gemini_image.rs` behind the `ImageProvider` trait. Calls
//! `generativelanguage.googleapis.com/v1beta/models/<model>:generateContent`
//! natively, or `<gateway>/google/...` when only the thClaws Gateway key
//! is present. Auth header is `x-goog-api-key` in both cases (the
//! gateway accepts it as the access-key carrier).

use crate::error::{Error, Result};
use crate::media::provider::{
    ImageModelInfo, ImageProvider, ImageRequest, ImageResult, SpeechProvider, SpeechRequest,
    SpeechResult,
};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com";

const MODELS: &[ImageModelInfo] = &[
    ImageModelInfo {
        id: "gemini-3.1-flash-image",
        // "" marks the cross-provider default (faster, cheaper).
        aliases: &["", "flash", "gemini-flash-image"],
        label: "Gemini 3.1 Flash Image",
    },
    ImageModelInfo {
        id: "gemini-3-pro-image",
        aliases: &["pro", "gemini-pro-image", "gemini-3.1-pro-image"],
        label: "Gemini 3 Pro Image",
    },
];

/// Build an informative error for the "HTTP 200 but no image" case —
/// the response shape that previously produced the opaque "missing
/// /candidates/0/content/parts". Surfaces the signals Gemini uses to
/// explain a missing image: a top-level `promptFeedback.blockReason`
/// (safety block on the prompt), the candidate `finishReason`
/// (SAFETY / RECITATION / etc.), any text the model returned instead of
/// an image, and finally a truncated raw body so unknown shapes are
/// still diagnosable.
fn no_image_error(v: &Value) -> String {
    let mut bits: Vec<String> = Vec::new();
    if let Some(reason) = v
        .pointer("/promptFeedback/blockReason")
        .and_then(|x| x.as_str())
    {
        bits.push(format!("promptFeedback.blockReason={reason}"));
    }
    if let Some(reason) = v
        .pointer("/candidates/0/finishReason")
        .and_then(|x| x.as_str())
    {
        bits.push(format!("finishReason={reason}"));
    }
    // The model sometimes replies with a text part explaining a refusal.
    if let Some(parts) = v
        .pointer("/candidates/0/content/parts")
        .and_then(|p| p.as_array())
    {
        for p in parts {
            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                let t = t.trim();
                if !t.is_empty() {
                    bits.push(format!("text={:?}", truncate(t, 200)));
                    break;
                }
            }
        }
    }
    let raw = v.to_string();
    let detail = if bits.is_empty() {
        format!("raw: {}", truncate(&raw, 500))
    } else {
        format!("{} | raw: {}", bits.join(", "), truncate(&raw, 300))
    };
    format!("gemini returned no image — {detail}")
}

/// Char-boundary-safe truncation — slicing a String at an arbitrary
/// byte index panics on a multi-byte boundary.
fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

pub struct GeminiImageProvider;

impl GeminiImageProvider {
    fn aspect(req: &ImageRequest) -> &'static str {
        match req.aspect_ratio.as_str() {
            "1:1" => "1:1",
            "3:4" => "3:4",
            "4:3" => "4:3",
            "9:16" => "9:16",
            _ => "16:9",
        }
    }
    fn size(req: &ImageRequest) -> &'static str {
        match req.size.as_str() {
            "512" => "512",
            "2K" => "2K",
            _ => "1K",
        }
    }
}

#[async_trait]
impl ImageProvider for GeminiImageProvider {
    fn id(&self) -> &'static str {
        "gemini"
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
        // Forward-compat: accept any future `gemini-*image` id verbatim.
        if raw.starts_with("gemini-") && raw.contains("image") {
            return Some(raw.to_string());
        }
        None
    }

    async fn generate(&self, req: &ImageRequest) -> Result<ImageResult> {
        let ep = crate::media::provider::resolve_endpoint(
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            GEMINI_BASE,
            "google",
        )?;
        let aspect = Self::aspect(req);
        let size = Self::size(req);

        // text→image is [text]; image→image is [image, …, text].
        let mut parts: Vec<Value> = Vec::new();
        for img in &req.input_images {
            parts.push(json!({
                "inlineData": { "mimeType": img.mime, "data": B64.encode(&img.bytes) }
            }));
        }
        parts.push(json!({ "text": req.prompt }));

        let body = json!({
            "contents": [{ "parts": parts }],
            "generationConfig": {
                "responseModalities": ["IMAGE"],
                "imageConfig": { "aspectRatio": aspect, "imageSize": size }
            }
        });
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            ep.base_url.trim_end_matches('/'),
            req.model
        );
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| Error::Tool(format!("http client: {e}")))?;
        let resp = crate::multi_tenant::attach_member(client.post(&url))
            .header("x-goog-api-key", &ep.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "gemini http {status}: {}",
                &body[..body.len().min(400)]
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("gemini response not json: {e}")))?;
        let parts = v
            .pointer("/candidates/0/content/parts")
            .and_then(|p| p.as_array())
            .ok_or_else(|| Error::Tool(no_image_error(&v)))?;
        for part in parts {
            if let Some(data_b64) = part.pointer("/inlineData/data").and_then(|v| v.as_str()) {
                let bytes = B64
                    .decode(data_b64)
                    .map_err(|e| Error::Tool(format!("base64 decode: {e}")))?;
                return Ok(ImageResult { bytes });
            }
        }
        Err(Error::Tool(no_image_error(&v)))
    }
}

// ── Gemini speech (text→speech) ──────────────────────────────────

const SPEECH_MODELS: &[ImageModelInfo] = &[ImageModelInfo {
    id: "gemini-3.1-flash-tts-preview",
    aliases: &["", "flash", "flash-tts", "gemini-flash-tts"],
    label: "Gemini 3.1 Flash TTS (preview)",
}];

pub struct GeminiSpeechProvider;

/// Wrap raw signed-16-bit little-endian mono PCM in a minimal WAV
/// container so the file plays anywhere. Gemini TTS returns bare PCM at a
/// rate advertised in the part's `mimeType` (`audio/L16;…;rate=24000`).
pub(crate) fn pcm16_to_wav(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let channels: u16 = 1;
    let bits: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * (bits as u32 / 8);
    let block_align = channels * (bits / 8);
    let data_len = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// Pull the sample rate out of a `mimeType` like `audio/L16;codec=pcm;rate=24000`.
/// Defaults to 24000 (Gemini TTS default) when absent/unparseable.
fn rate_from_mime(mime: &str) -> u32 {
    mime.split(';')
        .find_map(|p| p.trim().strip_prefix("rate="))
        .and_then(|r| r.trim().parse::<u32>().ok())
        .filter(|r| *r > 0)
        .unwrap_or(24000)
}

#[async_trait]
impl SpeechProvider for GeminiSpeechProvider {
    fn id(&self) -> &'static str {
        "gemini"
    }
    fn models(&self) -> &'static [ImageModelInfo] {
        SPEECH_MODELS
    }
    fn resolve_model(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        for m in SPEECH_MODELS {
            if raw == m.id || m.aliases.contains(&raw) {
                return Some(m.id.to_string());
            }
        }
        // Forward-compat: accept any future `gemini-*tts*` id verbatim.
        if raw.starts_with("gemini-") && raw.contains("tts") {
            return Some(raw.to_string());
        }
        None
    }

    async fn synthesize(&self, req: &SpeechRequest) -> Result<SpeechResult> {
        let ep = crate::media::provider::resolve_endpoint(
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            GEMINI_BASE,
            "google",
        )?;
        // Gemini steers delivery from a leading natural-language instruction.
        let text = match &req.style {
            Some(s) if !s.trim().is_empty() => {
                format!("{}: {}", s.trim().trim_end_matches(':'), req.text)
            }
            _ => req.text.clone(),
        };
        let voice = if req.voice.trim().is_empty() {
            "Kore"
        } else {
            req.voice.trim()
        };
        let body = json!({
            "contents": [{ "parts": [{ "text": text }] }],
            "generationConfig": {
                "responseModalities": ["AUDIO"],
                "speechConfig": {
                    "voiceConfig": { "prebuiltVoiceConfig": { "voiceName": voice } }
                }
            }
        });
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            ep.base_url.trim_end_matches('/'),
            req.model
        );
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| Error::Tool(format!("http client: {e}")))?;
        let resp = crate::multi_tenant::attach_member(client.post(&url))
            .header("x-goog-api-key", &ep.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "gemini tts http {status}: {}",
                &body[..body.len().min(400)]
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("gemini tts response not json: {e}")))?;
        let parts = v
            .pointer("/candidates/0/content/parts")
            .and_then(|p| p.as_array())
            .ok_or_else(|| Error::Tool(no_image_error(&v)))?;
        for part in parts {
            if let Some(data_b64) = part.pointer("/inlineData/data").and_then(|d| d.as_str()) {
                let pcm = B64
                    .decode(data_b64)
                    .map_err(|e| Error::Tool(format!("base64 decode: {e}")))?;
                let mime = part
                    .pointer("/inlineData/mimeType")
                    .and_then(|m| m.as_str())
                    .unwrap_or("audio/L16;rate=24000");
                let wav = pcm16_to_wav(&pcm, rate_from_mime(mime));
                return Ok(SpeechResult {
                    bytes: wav,
                    ext: "wav",
                });
            }
        }
        Err(Error::Tool(no_image_error(&v)))
    }
}

#[cfg(test)]
mod speech_tests {
    use super::*;

    #[test]
    fn wav_header_is_well_formed() {
        let pcm = vec![0u8, 1, 2, 3, 4, 5, 6, 7]; // 4 samples
        let wav = pcm16_to_wav(&pcm, 24000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]),
            36 + pcm.len() as u32
        );
        assert_eq!(
            u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]),
            pcm.len() as u32
        );
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            24000
        );
        assert_eq!(wav.len(), 44 + pcm.len());
    }

    #[test]
    fn rate_parsed_from_mime_or_defaults() {
        assert_eq!(rate_from_mime("audio/L16;codec=pcm;rate=16000"), 16000);
        assert_eq!(rate_from_mime("audio/L16;rate=24000"), 24000);
        assert_eq!(rate_from_mime("audio/pcm"), 24000);
    }
}
