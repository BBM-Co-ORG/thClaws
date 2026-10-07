//! DashScope (Alibaba Model Studio) speech — Qwen3-TTS. The native
//! `multimodal-generation/generation` endpoint returns a signed URL to a
//! 24 kHz mono 16-bit WAV (valid 24 h), downloaded here. Reached through
//! `resolve_dashscope_endpoint`, so a locked install (SIS) speaks through
//! its own Model Studio workspace and the gateway bills per character.
//!
//! `language_type` has no Thai value — `"Thai"` is rejected — but `Auto`
//! speaks Thai correctly (verified by an ASR round trip), so every language
//! outside the model's list maps to `Auto`. The instruct model rejects the
//! lowercase spellings the flash model tolerates, so values are sent
//! capitalised. Input is capped at 512 tokens; longer text is synthesised
//! in sentence chunks and the PCM concatenated.

use crate::error::{Error, Result};
use crate::media::provider::{ImageModelInfo, SpeechProvider, SpeechRequest, SpeechResult};
use crate::media::providers::gemini::pcm16_to_wav;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const DASHSCOPE_BASE: &str = "https://dashscope-intl.aliyuncs.com";
const GEN_PATH: &str = "/api/v1/services/aigc/multimodal-generation/generation";
pub const DEFAULT_MODEL: &str = "qwen3-tts-flash";
const INSTRUCT_MODEL: &str = "qwen3-tts-instruct-flash";
pub const DEFAULT_VOICE: &str = "Cherry";
const SAMPLE_RATE: u32 = 24000;
/// Characters per request — well under the 512-token cap even for Thai.
const CHUNK_CHARS: usize = 300;

/// System voices verified live on Model Studio (Singapore), 2026-09-29.
pub const VOICES: &[&str] = &[
    "Cherry", "Serena", "Chelsie", "Jennifer", "Katerina", "Maia", "Momo", "Vivian", "Sunny",
    "Moon", "Ethan", "Ryan", "Elias", "Dylan", "Kai", "Nofish",
];

const MODELS: &[ImageModelInfo] = &[
    ImageModelInfo {
        id: DEFAULT_MODEL,
        aliases: &["qwen-tts", "qwen3-tts", DEFAULT_MODEL],
        label: "Qwen3-TTS Flash",
    },
    ImageModelInfo {
        id: INSTRUCT_MODEL,
        aliases: &["qwen-tts-instruct", INSTRUCT_MODEL],
        label: "Qwen3-TTS Instruct Flash (delivery via style)",
    },
];

/// The model's `language_type` for a language code or name; `Auto` for
/// everything it doesn't list (Thai included).
pub fn language_type(lang: &str) -> &'static str {
    match lang.trim().to_ascii_lowercase().as_str() {
        "zh" | "zh-cn" | "chinese" => "Chinese",
        "en" | "english" => "English",
        "de" | "german" => "German",
        "it" | "italian" => "Italian",
        "pt" | "portuguese" => "Portuguese",
        "es" | "spanish" => "Spanish",
        "ja" | "japanese" => "Japanese",
        "ko" | "korean" => "Korean",
        "fr" | "french" => "French",
        "ru" | "russian" => "Russian",
        _ => "Auto",
    }
}

/// Split at sentence ends (or spaces, Thai's phrase break) into pieces of at
/// most `max` chars; a single overlong run is hard-cut.
fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for piece in text.split_inclusive(['.', '!', '?', '\n', ' ', '。']) {
        if cur.chars().count() + piece.chars().count() > max && !cur.trim().is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if piece.chars().count() > max {
            let cs: Vec<char> = piece.chars().collect();
            for c in cs.chunks(max) {
                out.push(c.iter().collect());
            }
        } else {
            cur.push_str(piece);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out.into_iter().filter(|c| !c.trim().is_empty()).collect()
}

/// The `data` chunk of a PCM WAV.
fn wav_pcm(wav: &[u8]) -> Result<&[u8]> {
    if wav.len() < 12 || &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err(Error::Tool(
            "dashscope tts: response is not a WAV file".into(),
        ));
    }
    let mut i = 12;
    while i + 8 <= wav.len() {
        let len = u32::from_le_bytes([wav[i + 4], wav[i + 5], wav[i + 6], wav[i + 7]]) as usize;
        if &wav[i..i + 4] == b"data" {
            return Ok(&wav[i + 8..(i + 8 + len).min(wav.len())]);
        }
        i += 8 + len + (len & 1);
    }
    Err(Error::Tool("dashscope tts: WAV has no data chunk".into()))
}

/// DashScope hands back the WAV as a plain-http OSS URL, and the same signed
/// URL serves over https. Networks that shut port 80 (SIS) time out on http.
fn https_url(url: &str) -> String {
    match url.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    }
}

fn client(secs: u64) -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(secs))
        .build()
        .map_err(|e| Error::Tool(format!("http client: {e}")))
}

async fn synthesize_one(
    model: &str,
    text: &str,
    voice: &str,
    language: &str,
    instructions: Option<&str>,
) -> Result<Vec<u8>> {
    let ep = crate::media::provider::resolve_dashscope_endpoint(DASHSCOPE_BASE)?;
    let mut input = json!({ "text": text, "voice": voice, "language_type": language });
    if let Some(i) = instructions.filter(|i| !i.trim().is_empty() && model == INSTRUCT_MODEL) {
        input["instructions"] = json!(i);
    }
    let url = format!("{}{}", ep.base_url.trim_end_matches('/'), GEN_PATH);
    let resp = crate::multi_tenant::attach_member(client(120)?.post(&url))
        .bearer_auth(&ep.api_key)
        .json(&json!({ "model": model, "input": input }))
        .send()
        .await
        .map_err(|e| Error::Tool(format!("dashscope tts http: {e}")))?;
    let status = resp.status();
    let v: Value = resp
        .json()
        .await
        .map_err(|e| Error::Tool(format!("dashscope tts not json: {e}")))?;
    if !status.is_success() {
        let msg = v.get("message").and_then(Value::as_str).unwrap_or("");
        return Err(Error::Tool(format!("dashscope tts http {status}: {msg}")));
    }
    let audio = v.pointer("/output/audio").cloned().unwrap_or(Value::Null);
    if let Some(u) = audio
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
    {
        return client(120)?
            .get(https_url(u))
            .send()
            .await
            .map_err(|e| Error::Tool(format!("dashscope tts download: {e}")))?
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| Error::Tool(format!("dashscope tts body: {e}")));
    }
    if let Some(d) = audio
        .get("data")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
    {
        return B64
            .decode(d)
            .map_err(|e| Error::Tool(format!("dashscope tts audio.data: {e}")));
    }
    Err(Error::Tool(
        "dashscope tts: response carried no audio".into(),
    ))
}

/// Synthesise `text` to one WAV. `language` is a code or name ("th", "en",
/// "Thai"…; empty = auto); `instructions` steers delivery on the instruct model.
pub async fn synthesize_wav(
    model: &str,
    text: &str,
    voice: &str,
    language: &str,
    instructions: Option<&str>,
) -> Result<Vec<u8>> {
    let voice = if voice.trim().is_empty() {
        DEFAULT_VOICE
    } else {
        voice.trim()
    };
    let lt = language_type(language);
    let parts = chunks(text, CHUNK_CHARS);
    if parts.len() <= 1 {
        return synthesize_one(model, text, voice, lt, instructions).await;
    }
    let mut pcm = Vec::new();
    for p in &parts {
        let wav = synthesize_one(model, p, voice, lt, instructions).await?;
        pcm.extend_from_slice(wav_pcm(&wav)?);
    }
    Ok(pcm16_to_wav(&pcm, SAMPLE_RATE))
}

pub struct DashScopeSpeechProvider;

#[async_trait]
impl SpeechProvider for DashScopeSpeechProvider {
    fn id(&self) -> &'static str {
        "dashscope"
    }
    fn models(&self) -> &'static [ImageModelInfo] {
        MODELS
    }
    async fn synthesize(&self, req: &SpeechRequest) -> Result<SpeechResult> {
        let bytes = synthesize_wav(
            &req.model,
            &req.text,
            &req.voice,
            &req.language,
            req.style.as_deref(),
        )
        .await?;
        Ok(SpeechResult { bytes, ext: "wav" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_downloads_over_https() {
        assert_eq!(
            https_url("http://dashscope-result-sgp.oss-ap-southeast-1.aliyuncs.com/a.wav?x=1"),
            "https://dashscope-result-sgp.oss-ap-southeast-1.aliyuncs.com/a.wav?x=1"
        );
        assert_eq!(https_url("https://h/a.wav"), "https://h/a.wav");
    }

    #[test]
    fn thai_and_unlisted_languages_speak_auto() {
        assert_eq!(language_type("th"), "Auto");
        assert_eq!(language_type("Thai"), "Auto");
        assert_eq!(language_type(""), "Auto");
        assert_eq!(language_type("en"), "English");
        assert_eq!(language_type("Japanese"), "Japanese");
    }

    #[test]
    fn long_text_splits_under_the_cap() {
        let text = "ประโยคหนึ่ง ".repeat(80);
        let cs = chunks(&text, CHUNK_CHARS);
        assert!(cs.len() > 1);
        assert!(cs.iter().all(|c| c.chars().count() <= CHUNK_CHARS));
        assert_eq!(cs.concat(), text);
        assert_eq!(chunks("short", CHUNK_CHARS), vec!["short".to_string()]);
    }

    #[test]
    fn concatenated_pcm_round_trips_through_wav() {
        let a = pcm16_to_wav(&[1, 0, 2, 0], SAMPLE_RATE);
        assert_eq!(wav_pcm(&a).unwrap(), &[1, 0, 2, 0]);
        assert!(wav_pcm(b"not a wav").is_err());
    }

    // Live: `DASHSCOPE_API_KEY=… cargo test dashscope_tts_live -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn dashscope_tts_live() {
        let text = "ทดสอบเสียงภาษาไทยจากระบบของเรา ".repeat(12);
        assert!(chunks(&text, CHUNK_CHARS).len() > 1);
        let wav = synthesize_wav(DEFAULT_MODEL, &text, "", "th", None)
            .await
            .expect("tts");
        let pcm = wav_pcm(&wav).expect("wav");
        assert!(
            pcm.len() > SAMPLE_RATE as usize * 2 * 5,
            "at least 5 s of audio"
        );
    }
}
