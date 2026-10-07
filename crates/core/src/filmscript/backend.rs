//! Video-backend abstraction (dev-plan/52 multi-backend, part A).
//!
//! The compiler targets a capability *contract*, not one vendor. Each
//! backend declares [`VideoCaps`] (compile-time: validation + codegen
//! dispatch) and — in the harness — implements the runtime submit/poll
//! trait. This module is the pure, compile-side half: the backend id, its
//! capabilities, and the per-backend payload builder. Seedance's builder is
//! moved here verbatim from phase2 (golden tests freeze the JSON).
//!
//! Default backend = `Grok` (lab-proven: identity + stable background +
//! native Thai lip-sync in one cheap pass). See
//! `docs/video-pipeline/backends-matrix.md`.

use super::phase1::PartialShot;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendId {
    /// Lab-proven default: identity + stable background + native Thai
    /// lip-sync in one cheap pass (scene-first-frame → i2v).
    #[default]
    Grok,
    Ltx,
    Seedance,
    Veo,
    HappyHorse,
}

impl BackendId {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendId::Grok => "grok",
            BackendId::Ltx => "ltx",
            BackendId::Seedance => "seedance",
            BackendId::Veo => "veo",
            BackendId::HappyHorse => "happyhorse",
        }
    }

    /// The gateway segment family this backend's calls go through: Grok,
    /// Seedance and Veo are Kie jobs; Happy Horse is DashScope.
    fn media_id(self) -> &'static str {
        match self {
            BackendId::Grok | BackendId::Seedance | BackendId::Veo => "kie",
            BackendId::Ltx => "ltx",
            BackendId::HappyHorse => "happyhorse",
        }
    }

    /// Reachable on this install (always when not gateway-locked).
    pub fn reachable(self) -> bool {
        crate::media::registry::backend_reachable(self.media_id())
    }

    pub const ALL: [BackendId; 5] = [
        BackendId::Grok,
        BackendId::Ltx,
        BackendId::Seedance,
        BackendId::Veo,
        BackendId::HappyHorse,
    ];

    /// The default a film without `backend:` gets: Grok, or on a locked
    /// install the first backend its gateway serves.
    pub fn default_with(reach: impl Fn(BackendId) -> bool) -> Self {
        if reach(BackendId::default()) {
            return BackendId::default();
        }
        Self::ALL
            .into_iter()
            .find(|b| reach(*b))
            .unwrap_or_default()
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "grok" => BackendId::Grok,
            "ltx" => BackendId::Ltx,
            "seedance" => BackendId::Seedance,
            "veo" => BackendId::Veo,
            "happyhorse" | "happy_horse" => BackendId::HappyHorse,
            _ => return None,
        })
    }

    pub fn caps(self) -> VideoCaps {
        // Values from the live capability matrix (docs/video-pipeline).
        match self {
            BackendId::Grok => VideoCaps {
                id: self,
                native_audio: true,
                audio_ref: false,
                thai_native: true,
                max_image_refs: 7,
                identity: IdentityMode::I2vPortrait,
                continuation: ContinuationMode::ExtendOwnClip,
                voice_control: false,
                max_duration_s: 30.0,
            },
            BackendId::Ltx => VideoCaps {
                id: self,
                native_audio: true,
                audio_ref: true, // a2v
                thai_native: true,
                max_image_refs: 2, // first + last frame
                identity: IdentityMode::I2vPortrait,
                continuation: ContinuationMode::ExtendOwnClip,
                voice_control: true,
                max_duration_s: 20.0,
            },
            BackendId::Seedance => VideoCaps {
                id: self,
                native_audio: false,
                audio_ref: true,
                thai_native: false, // via our TTS
                max_image_refs: 9,
                identity: IdentityMode::MultiRef,
                continuation: ContinuationMode::RefVideo,
                voice_control: true,
                max_duration_s: 15.0,
            },
            BackendId::Veo => VideoCaps {
                id: self,
                native_audio: true,
                audio_ref: false,
                thai_native: true,
                max_image_refs: 7,
                identity: IdentityMode::ReferenceVideo,
                continuation: ContinuationMode::ExtendOwnClip,
                voice_control: false,
                max_duration_s: 8.0,
            },
            BackendId::HappyHorse => VideoCaps {
                id: self,
                native_audio: true,
                audio_ref: false,
                thai_native: true,
                max_image_refs: 9, // r2v (t2v/i2v only in engine today)
                identity: IdentityMode::MultiRef,
                continuation: ContinuationMode::None,
                voice_control: false,
                max_duration_s: 15.0,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityMode {
    /// Multiple `@ImageN` reference images (Seedance, Happy Horse r2v).
    MultiRef,
    /// A prior clip locks the subject (Veo REFERENCE_2_VIDEO).
    ReferenceVideo,
    /// Seed each shot from a per-character portrait (Grok/LTX i2v/a2v).
    I2vPortrait,
    /// No reliable identity lock (text-only).
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationMode {
    /// Feed any clip as a motion-preserving reference (Seedance).
    RefVideo,
    /// Extend the backend's own previous output (Grok/Veo/LTX).
    ExtendOwnClip,
    /// No continuation (Happy Horse).
    None,
}

/// Compile-time capability contract per backend — drives `validate.rs`
/// (D3) and codegen dispatch. Not all fields are consumed yet.
#[derive(Debug, Clone, Copy)]
pub struct VideoCaps {
    pub id: BackendId,
    pub native_audio: bool,
    pub audio_ref: bool,
    pub thai_native: bool,
    pub max_image_refs: u8,
    pub identity: IdentityMode,
    pub continuation: ContinuationMode,
    pub voice_control: bool,
    pub max_duration_s: f32,
}

/// Backend-native request builder. D1 has only Seedance (moved verbatim
/// from phase2); D4 adds Grok (default), D5 the rest. The dispatcher keeps
/// the compiler backend-neutral — phase2 resolves assets + validates, then
/// hands off to the selected backend here.
pub fn build_payload(
    backend: BackendId,
    shot: &PartialShot,
    prompt: &str,
    image_urls: &[String],
    frame_url: Option<&str>,
    video_url: Option<&str>,
    audio_url: Option<&str>,
) -> Value {
    match backend {
        BackendId::Grok => grok_payload(shot, prompt, image_urls, frame_url),
        BackendId::Seedance => {
            seedance_payload(shot, prompt, image_urls, frame_url, video_url, audio_url)
        }
        BackendId::Ltx => ltx_payload(shot, prompt, image_urls, frame_url, audio_url),
        BackendId::Veo => veo_payload(shot, prompt, image_urls, frame_url),
        BackendId::HappyHorse => happyhorse_payload(shot, prompt, image_urls, frame_url),
    }
}

/// LTX resolution keyword → native pixel string (16:9). LTX's floor is
/// 1080p (720p/480p aren't accepted); a2v is locked to 1080p, t2v/i2v go to
/// 4K.
fn ltx_resolution(res: &str) -> &'static str {
    match res {
        "1440p" => "2560x1440",
        "4k" => "3840x2160",
        _ => "1920x1080",
    }
}

/// LTX-2.3 via the native api.ltx.video schema (`*_uri` fields). a2v (pro)
/// when a voice track is bound; else i2v when a reference image is present;
/// else t2v. Playbook defaults: fps 25, guidance 3.5 (a2v-with-image → 2 so
/// the mouth animates), native audio on t2v/i2v.
fn ltx_payload(
    shot: &PartialShot,
    prompt: &str,
    image_urls: &[String],
    frame_url: Option<&str>,
    audio_url: Option<&str>,
) -> Value {
    let image = frame_url.or_else(|| image_urls.first().map(String::as_str));
    if let Some(audio) = audio_url {
        // audio-to-video (pro, 1080p-locked): the voice is the soundtrack.
        // `duration` is a gateway BILLING HINT only (a2v output length =
        // the audio length; LTX's a2v API takes no duration) — dispatch
        // strips it on the direct BYOK path, the gateway strips it before
        // forwarding on the metered path (dev-plan/53 Stage D).
        let mut input = json!({
            "model": "ltx-2-3-pro",
            "audio_uri": audio,
            "prompt": prompt,
            "resolution": "1920x1080",
            "duration": shot.resolved.duration,
            "guidance_scale": if image.is_some() { 2 } else { 3 },
        });
        if let Some(img) = image {
            input
                .as_object_mut()
                .unwrap()
                .insert("image_uri".into(), json!(img));
        }
        return input;
    }
    let mut input = json!({
        "model": "ltx-2-3-fast",
        "prompt": prompt,
        "duration": shot.resolved.duration,
        "resolution": ltx_resolution(&shot.resolved.resolution),
        "fps": 25,
        "generate_audio": shot.resolved.audio_on,
    });
    if let Some(img) = image {
        input
            .as_object_mut()
            .unwrap()
            .insert("image_uri".into(), json!(img));
    }
    input
}

/// Veo 3.1 via the Kie Veo endpoint. A reference image drives
/// `REFERENCE_2_VIDEO` (the identity-lock path); text-only omits it.
fn veo_payload(
    shot: &PartialShot,
    prompt: &str,
    image_urls: &[String],
    frame_url: Option<&str>,
) -> Value {
    let mut images: Vec<&str> = Vec::new();
    if let Some(f) = frame_url {
        images.push(f);
    }
    images.extend(image_urls.iter().map(String::as_str));
    let mut body = json!({
        "model": "veo3_fast",
        "prompt": prompt,
        "aspect_ratio": shot.resolved.aspect,
    });
    if !images.is_empty() {
        let obj = body.as_object_mut().unwrap();
        obj.insert("imageUrls".into(), json!(images));
        obj.insert("generationType".into(), json!("REFERENCE_2_VIDEO"));
    }
    body
}

/// Happy Horse 1.0 via DashScope (`ratio`, uppercase `720P`/`1080P`). i2v
/// feeds the reference as a `first_frame` media entry; else t2v.
fn happyhorse_payload(
    shot: &PartialShot,
    prompt: &str,
    image_urls: &[String],
    frame_url: Option<&str>,
) -> Value {
    let image = frame_url.or_else(|| image_urls.first().map(String::as_str));
    let resolution = if shot.resolved.resolution == "1080p" {
        "1080P"
    } else {
        "720P"
    };
    let parameters = json!({
        "resolution": resolution,
        "ratio": shot.resolved.aspect,
        "duration": shot.resolved.duration,
    });
    match image {
        Some(img) => json!({
            "model": "happyhorse-1.0-i2v",
            "input": { "prompt": prompt, "media": [{ "type": "first_frame", "url": img }] },
            "parameters": parameters,
        }),
        None => json!({
            "model": "happyhorse-1.0-t2v",
            "input": { "prompt": prompt },
            "parameters": parameters,
        }),
    }
}

/// Grok Imagine via the Kie jobs API. i2v when the shot carries a reference
/// image (identity — the winning scene-first-frame pattern), else t2v.
/// Native audio (no `generate_audio` flag, no audio-ref); a match-cut frame
/// seeds i2v like any other reference image.
fn grok_payload(
    shot: &PartialShot,
    prompt: &str,
    image_urls: &[String],
    frame_url: Option<&str>,
) -> Value {
    let mut images: Vec<&str> = Vec::new();
    if let Some(f) = frame_url {
        images.push(f);
    }
    images.extend(image_urls.iter().map(String::as_str));

    let mut input = json!({
        "prompt": prompt,
        "aspect_ratio": shot.resolved.aspect,
        "duration": shot.resolved.duration,
        "resolution": shot.resolved.resolution,
        "nsfw_checker": false,
    });
    let obj = input.as_object_mut().unwrap();
    let model = if images.is_empty() {
        "grok-imagine/text-to-video"
    } else {
        obj.insert("image_urls".into(), json!(images));
        "grok-imagine/image-to-video"
    };
    json!({ "model": model, "input": input })
}

/// Seedance 2.0 createTask body — verbatim extraction from phase2 (golden
/// tests freeze this JSON).
fn seedance_payload(
    shot: &PartialShot,
    prompt: &str,
    image_urls: &[String],
    frame_url: Option<&str>,
    video_url: Option<&str>,
    audio_url: Option<&str>,
) -> Value {
    let model = if shot.resolved.fast_model {
        "bytedance/seedance-2-fast"
    } else {
        "bytedance/seedance-2"
    };
    let mut input = json!({
        "prompt": prompt,
        "generate_audio": shot.resolved.audio_on,
        "resolution": shot.resolved.resolution,
        "aspect_ratio": shot.resolved.aspect,
        "duration": shot.resolved.duration,
        "nsfw_checker": true,
    });
    let obj = input.as_object_mut().unwrap();
    if let Some(f) = frame_url {
        obj.insert("first_frame_url".into(), json!(f));
    }
    if !image_urls.is_empty() {
        obj.insert("reference_image_urls".into(), json!(image_urls));
    }
    if let Some(v) = video_url {
        obj.insert("reference_video_urls".into(), json!([v]));
    }
    if let Some(a) = audio_url {
        obj.insert("reference_audio_urls".into(), json!([a]));
    }
    json!({ "model": model, "input": input })
}
