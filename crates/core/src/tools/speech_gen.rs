//! `TextToSpeech` — provider-abstracted speech synthesis (dev-plan/40).
//!
//! Mirrors `image_gen.rs`: resolves a model to a `SpeechProvider` via
//! `media::registry`, synthesises the audio, writes it to
//! `output/tts-<ts>-<sha8>.wav`, and returns the path (text-only — the
//! audio is an artifact for the user, not for the model to read back).
//! Gemini TTS runs over the same `generateContent` endpoint + gateway
//! `google` segment as images, so it gates and bills identically.

use crate::error::Result;
use crate::media::{registry, save_audio, SpeechRequest};
use crate::tools::{req_str, Tool};
use crate::types::ToolResultContent;
use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn opt(input: &Value, key: &str) -> String {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

const MODEL_DESC: &str = "Which speech model. Gemini: `flash` (default; \
gemini-3.1-flash-tts-preview). DashScope: `qwen3-tts-flash` (alias `qwen-tts`) or \
`qwen3-tts-instruct-flash` (honours `style`). Provider is inferred from the model. \
Default: flash (where the gateway serves only DashScope: qwen3-tts-flash).";
const VOICE_DESC: &str = "Prebuilt voice name. Gemini: e.g. Kore, Charon, Puck, Aoede, \
Fenrir (default Kore). Qwen3-TTS: Cherry, Serena, Chelsie, Jennifer, Katerina, Maia, \
Momo, Vivian, Sunny, Moon (female); Ethan, Ryan, Elias, Dylan, Kai, Nofish (male) — \
default Cherry.";

pub struct TextToSpeechTool;

#[async_trait]
impl Tool for TextToSpeechTool {
    fn name(&self) -> &'static str {
        "TextToSpeech"
    }
    fn description(&self) -> &'static str {
        "Synthesize speech from text. Provider-abstracted (Gemini TTS, model \
         `gemini-3.1-flash-tts-preview`; or DashScope Qwen3-TTS). Output is written to \
         `output/tts-<ts>-<sha8>.wav` (16-bit PCM WAV) and its path returned. \
         Requires `imageToolsEnabled: true` in `.thclaws/settings.json`, plus a \
         provider key in env (`GEMINI_API_KEY`/`GOOGLE_API_KEY`) — or the thClaws \
         Gateway key. Steer delivery with `style` (e.g. \"Say warmly\")."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The text to speak. Plain text; the model reads it verbatim."
                },
                "voice": { "type": "string", "description": VOICE_DESC },
                "style": {
                    "type": "string",
                    "description": "Optional natural-language delivery hint, e.g. \"Say warmly, like a friendly narrator\". Prepended as an instruction."
                },
                "language": {
                    "type": "string",
                    "description": "Language of the text, e.g. `th`, `en` (optional). Qwen3-TTS uses it to pick its language mode; Thai and other unlisted languages run in auto mode."
                },
                "model": { "type": "string", "description": MODEL_DESC },
                "provider": { "type": "string", "description": "Optional explicit provider (`gemini` | `dashscope`). Usually omit — inferred from `model`.", "enum": ["gemini", "dashscope"] }
            },
            "required": ["text"]
        })
    }
    fn requires_approval(&self, _input: &Value) -> bool {
        // Costs money + writes a file.
        true
    }
    async fn call(&self, input: Value) -> Result<String> {
        let result = self.call_multimodal(input).await?;
        Ok(result.to_text())
    }
    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        let text = req_str(&input, "text")?.to_string();
        let (provider, model) =
            registry::resolve_speech(&opt(&input, "provider"), &opt(&input, "model"))?;
        let style = opt(&input, "style");
        let req = SpeechRequest {
            model,
            text,
            voice: opt(&input, "voice"),
            language: opt(&input, "language"),
            style: if style.trim().is_empty() {
                None
            } else {
                Some(style)
            },
        };
        let out = provider.synthesize(&req).await?;
        let path = save_audio(&out.bytes, out.ext)?;
        let d = Sha256::digest(&out.bytes);
        Ok(ToolResultContent::Text(format!(
            "Wrote {} ({} bytes, sha256-4={:02x}{:02x}{:02x}{:02x})",
            path.display(),
            out.bytes.len(),
            d[0],
            d[1],
            d[2],
            d[3],
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_requires_text_and_names_tool() {
        let t = TextToSpeechTool;
        assert_eq!(t.name(), "TextToSpeech");
        let schema = t.input_schema();
        assert_eq!(schema["required"], json!(["text"]));
        assert!(t.requires_approval(&json!({"text": "hi"})));
    }
}
