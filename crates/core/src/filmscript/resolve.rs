//! Resolution: bind `$refs` (including `$base#tag` variants/views) to
//! declarations, assign `@ImageN` slots, resolve the style cascade
//! (film ⊕ sequence ⊕ shot — inner wins), pick the generation mode,
//! and normalize directives into typed fields.
//!
//! Two determinism rules carry the design: slots follow *declaration
//! order of the base entities used in the shot* (the author never
//! writes an index), and one shot = one backdrop = one look per entity
//! (`E_MIXED_VARIANT` — base+variant or two views in one shot is a
//! contradiction, not a preference).

use super::ast::*;
use super::backend::BackendId;
use super::lexer::nfc;
use super::{is_thai_text, msg, CompileError};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reference,
    FirstFrame,
    TextOnly,
}

#[derive(Debug, Clone)]
pub struct ResolvedDialogue {
    pub speaker: String,
    pub text: String,
    pub trailing: Option<String>,
    pub lang: &'static str,
    pub tone_hint: Option<String>,
}

/// One base entity used by a shot, possibly through a variant/view.
#[derive(Debug, Clone)]
pub struct UsedRef {
    /// Index into the base-entity table (declaration order).
    pub entity: usize,
    pub tag: Option<String>,
    /// Variant image when `tag` is set, else the base image.
    pub image_path: String,
}

#[derive(Debug, Clone)]
pub struct Beat {
    pub t1: u32,
    pub t2: u32,
    pub content: ShotLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionKind {
    Cut,
    Fade,
    Dissolve,
    ToBlack,
}

impl TransitionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransitionKind::Cut => "cut",
            TransitionKind::Fade => "fade",
            TransitionKind::Dissolve => "dissolve",
            TransitionKind::ToBlack => "to_black",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SfxCue {
    pub handle: String,
    pub at_sec: f32,
}

#[derive(Debug, Clone)]
pub struct ResolvedShot {
    pub id: String,
    pub shot_type: ShotType,
    pub sequence_index: usize,
    pub used: Vec<UsedRef>,
    pub action: Vec<ShotLine>,
    pub beats: Vec<Beat>,
    pub properties: BTreeMap<String, (String, usize)>,
    pub sfx: Vec<SfxCue>,
    pub ambient: Option<String>,
    pub dialogue: Option<ResolvedDialogue>,
    pub continue_from: Option<String>,
    pub match_cut: Option<String>,
    pub seed: Option<i64>,
    pub transition_out: (TransitionKind, f32),
    pub subtitle: bool,
    /// Cascade-resolved (film ⊕ sequence ⊕ shot).
    pub style: Option<String>,
    pub lighting: Option<String>,
    pub genre: Option<String>,
    pub fps_look: Option<String>,
    pub duration: u32,
    pub resolution: String,
    pub aspect: String,
    pub fast_model: bool,
    pub audio_on: bool,
    /// Selected video backend (shot `@backend:` ⊕ film default ⊕ Grok).
    pub backend: BackendId,
    /// `dialogue_sync: overlay` — synthesize a real TTS track and swap it in
    /// (post) instead of using the backend's native dialogue audio.
    pub dialogue_overlay: bool,
    /// `dialogue_sync: lipsync` — synthesize TTS and re-sync the mouth to it
    /// with a lip-sync model in post (exact voice + correct lips).
    pub dialogue_lipsync: bool,
}

/// Base-entity table entry (variants collapse onto their base).
#[derive(Debug, Clone)]
pub struct Entity {
    pub decl: EntityDecl,
    pub variants: BTreeMap<String, (String, bool)>,
}

#[derive(Debug, Clone, Default)]
pub struct AudioAssets {
    pub music: BTreeMap<String, String>,
    pub sfx: BTreeMap<String, String>,
}

pub(crate) struct Resolution {
    pub entities: Vec<Entity>,
    pub audio: AudioAssets,
    pub shots: Vec<ResolvedShot>,
}

pub(crate) fn resolve(program: &Program) -> (Resolution, Vec<CompileError>) {
    let thai = program.thai;
    let mut errors = Vec::new();

    match program.header.dialogue_sync.as_deref() {
        // native (default): every backend generates its own dialogue audio.
        // overlay: swap in a real TTS track in post (onset-aligned).
        // lipsync: TTS + a lip-sync model re-syncs the mouth in post (exact
        // voice AND correct lips, identity preserved). Both opt-ins need a
        // TTS asset; both are post-process.
        None | Some("native") | Some("overlay") | Some("lipsync") => {}
        Some("post") => errors.push(CompileError::error(
            "E_UNSUPPORTED_V1",
            None,
            msg(
                thai,
                "film header: dialogue_sync: post ยังไม่รองรับใน v1 — ใช้ native|overlay",
                "film header: dialogue_sync: post is not supported in v1 — use native|overlay",
            ),
        )),
        Some(other) => errors.push(CompileError::error(
            "E_BAD_VALUE",
            None,
            msg(
                thai,
                &format!("film header: dialogue_sync '{other}' ไม่ถูกต้อง — ใช้ native|overlay"),
                &format!("film header: invalid dialogue_sync '{other}' — use native|overlay"),
            ),
        )),
    }

    let mut entities: Vec<Entity> = Vec::new();
    let mut audio = AudioAssets::default();
    for d in &program.declarations {
        match d {
            Declaration::Entity(e) => entities.push(Entity {
                decl: e.clone(),
                variants: BTreeMap::new(),
            }),
            Declaration::Variant {
                base,
                tag,
                image_path,
                is_view,
                line,
            } => {
                let base_h = nfc(base);
                match entities.iter_mut().find(|en| en.decl.handle == base_h) {
                    Some(en) => {
                        if en
                            .variants
                            .insert(nfc(tag), (image_path.clone(), *is_view))
                            .is_some()
                        {
                            errors.push(CompileError::error(
                                "E_DUPLICATE_DECL",
                                None,
                                msg(
                                    thai,
                                    &format!("บรรทัด {line}: ${base_h}#{tag} ถูกประกาศซ้ำ"),
                                    &format!("line {line}: ${base_h}#{tag} declared twice"),
                                ),
                            ));
                        }
                    }
                    None => errors.push(CompileError::error(
                        "E_UNDECLARED_REF",
                        None,
                        msg(
                            thai,
                            &format!("บรรทัด {line}: variant/view ${base_h}#{tag} — ไม่มี ${base_h} เป็น base"),
                            &format!("line {line}: variant/view ${base_h}#{tag} — base ${base_h} is not declared"),
                        ),
                    )),
                }
            }
            Declaration::Audio {
                is_music,
                handle,
                path,
                line,
            } => {
                let map = if *is_music {
                    &mut audio.music
                } else {
                    &mut audio.sfx
                };
                if map.insert(handle.clone(), path.clone()).is_some() {
                    errors.push(CompileError::error(
                        "E_DUPLICATE_DECL",
                        None,
                        msg(
                            thai,
                            &format!("บรรทัด {line}: ${handle} ถูกประกาศซ้ำ"),
                            &format!("line {line}: ${handle} declared twice"),
                        ),
                    ));
                }
            }
        }
    }

    let mut shots: Vec<ResolvedShot> = Vec::new();
    for (seq_idx, seq) in program.sequences.iter().enumerate() {
        for shot in &seq.shots {
            if shots.iter().any(|s| s.id == shot.id) {
                errors.push(CompileError::error(
                    "E_DUPLICATE_SHOT",
                    Some(&shot.id),
                    msg(
                        thai,
                        &format!("shot {}: id ซ้ำ — ทุก shot ต้องมี id ไม่ซ้ำทั้งไฟล์ (DAG/ราคา/แผนตัดต่อ อ้างด้วย id)", shot.id),
                        &format!("shot {}: duplicate id — shot ids must be unique film-wide (the DAG, cost and assembly plan key on them)", shot.id),
                    ),
                ));
            }
            let r = resolve_shot(program, &entities, seq, seq_idx, shot, &mut errors);
            shots.push(r);
        }
    }

    (
        Resolution {
            entities,
            audio,
            shots,
        },
        errors,
    )
}

fn resolve_shot(
    program: &Program,
    entities: &[Entity],
    seq: &Sequence,
    seq_idx: usize,
    shot: &Shot,
    errors: &mut Vec<CompileError>,
) -> ResolvedShot {
    let thai = program.thai;
    let sid = shot.id.as_str();
    let mut used: Vec<UsedRef> = Vec::new();

    let mut bind = |raw: &str, errors: &mut Vec<CompileError>| {
        let full = nfc(raw);
        let (base, tag) = match full.split_once('#') {
            Some((b, t)) => (b.to_string(), Some(t.to_string())),
            None => (full.clone(), None),
        };
        let Some(idx) = entities.iter().position(|e| e.decl.handle == base) else {
            errors.push(CompileError::error(
                "E_UNDECLARED_REF",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: ${full} ไม่ได้ประกาศ — เพิ่ม declaration ก่อนใช้"),
                    &format!("shot {sid}: ${full} is not declared — add a declaration first"),
                ),
            ));
            return;
        };
        let image_path = match &tag {
            None => entities[idx].decl.image_path.clone(),
            Some(t) => match entities[idx].variants.get(t) {
                Some((p, _)) => p.clone(),
                None => {
                    errors.push(CompileError::error(
                        "E_UNKNOWN_VARIANT",
                        Some(sid),
                        msg(
                            thai,
                            &format!("shot {sid}: variant/view ${base}#{t} ไม่ได้ประกาศ"),
                            &format!("shot {sid}: variant/view ${base}#{t} is not declared"),
                        ),
                    ));
                    return;
                }
            },
        };
        match used.iter().find(|u| u.entity == idx) {
            Some(prev) if prev.tag != tag => errors.push(CompileError::error(
                "E_MIXED_VARIANT",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: ใช้ ${base} หลายเวอร์ชันในช็อตเดียวไม่ได้ (1 ช็อต = 1 มุม/1 ลุค) — เลือกอันเดียว"),
                    &format!("shot {sid}: ${base} appears in two looks/angles in one shot (one shot = one look) — pick one"),
                ),
            )),
            Some(_) => {}
            None => used.push(UsedRef { entity: idx, tag, image_path }),
        }
    };

    let mut action = Vec::new();
    let mut beats = Vec::new();
    let mut properties: BTreeMap<String, (String, usize)> = BTreeMap::new();
    let mut sfx = Vec::new();
    let mut dialogue: Option<ResolvedDialogue> = None;
    let mut directives: BTreeMap<String, (String, usize)> = BTreeMap::new();

    let handle_property = |key: &str,
                           value: &str,
                           line: usize,
                           properties: &mut BTreeMap<String, (String, usize)>,
                           sfx: &mut Vec<SfxCue>,
                           errors: &mut Vec<CompileError>| {
        if key == "sfx" {
            let mut toks = value.split_whitespace();
            let handle = toks.next().and_then(|t| t.strip_prefix('$')).map(nfc);
            let at = toks
                .skip_while(|t| *t != "at")
                .nth(1)
                .map(|t| t.trim_end_matches('s'))
                .and_then(|t| t.parse::<f32>().ok());
            match (handle, at) {
                (Some(h), Some(at_sec)) if at_sec.is_finite() && at_sec >= 0.0 => {
                    sfx.push(SfxCue { handle: h, at_sec })
                }
                _ => errors.push(CompileError::error(
                    "E_PARSE",
                    Some(sid),
                    msg(
                        thai,
                        &format!("shot {sid}: sfx ต้องเป็น `sfx: $ชื่อ at <วินาที>s`"),
                        &format!("shot {sid}: sfx must be `sfx: $name at <sec>s`"),
                    ),
                )),
            }
        } else if properties.contains_key(key) {
            errors.push(CompileError::error(
                "E_DUPLICATE_LINE",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: '{key}:' ซ้ำ — หนึ่ง slot ต่อช็อต"),
                    &format!("shot {sid}: duplicate '{key}:' — one slot per shot"),
                ),
            ));
        } else {
            properties.insert(key.to_string(), (value.to_string(), line));
        }
    };

    for line in &shot.lines {
        match line {
            ShotLine::Action { refs, .. } => {
                for r in refs {
                    bind(r, errors);
                }
                action.push(line.clone());
            }
            ShotLine::Property { key, value, line } => {
                // `sfx:` names an audio handle (E_SFX_UNDECLARED owns it),
                // not an entity ref.
                if key != "sfx" {
                    for r in super::lexer::scan_refs(value) {
                        bind(&r, errors);
                    }
                }
                handle_property(key, value, *line, &mut properties, &mut sfx, errors)
            }
            ShotLine::Directive { key, value, line } => {
                if directives.contains_key(key) {
                    errors.push(CompileError::error(
                        "E_DUPLICATE_LINE",
                        Some(sid),
                        msg(
                            thai,
                            &format!("shot {sid}: @{key}: ซ้ำ — หนึ่ง directive ต่อช็อต"),
                            &format!("shot {sid}: duplicate @{key}: — one directive per shot"),
                        ),
                    ));
                } else {
                    directives.insert(key.clone(), (value.clone(), *line));
                }
            }
            ShotLine::Dialogue {
                speaker,
                text,
                trailing,
                ..
            } => {
                bind(speaker, errors);
                if dialogue.is_some() {
                    errors.push(CompileError::error(
                        "E_MULTI_SPEAKER",
                        Some(sid),
                        msg(
                            thai,
                            &format!("shot {sid}: มีบทพูดมากกว่าหนึ่งบรรทัด — หนึ่งช็อตหนึ่งผู้พูด แยกช็อตแทน"),
                            &format!("shot {sid}: more than one say line — one speaker per shot; split the shot"),
                        ),
                    ));
                    continue;
                }
                let speaker_base = nfc(speaker)
                    .split('#')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                dialogue = Some(ResolvedDialogue {
                    speaker: speaker_base,
                    text: text.clone(),
                    trailing: trailing.clone(),
                    lang: if is_thai_text(text) { "th" } else { "en" },
                    tone_hint: None,
                });
            }
            ShotLine::Beat {
                t1, t2, content, ..
            } => {
                let ok = match &**content {
                    ShotLine::Action { text, refs, .. } => {
                        for r in refs {
                            bind(r, errors);
                        }
                        !text.trim().is_empty()
                    }
                    ShotLine::Property { key, value, .. } if key == "camera" => {
                        for r in super::lexer::scan_refs(value) {
                            bind(&r, errors);
                        }
                        true
                    }
                    _ => false,
                };
                if ok {
                    beats.push(Beat {
                        t1: *t1,
                        t2: *t2,
                        content: (**content).clone(),
                    });
                } else {
                    errors.push(CompileError::error(
                        "E_PARSE",
                        Some(sid),
                        msg(
                            thai,
                            &format!("shot {sid}: beat [{t1}-{t2}] รองรับเฉพาะ prose หรือ camera: — บทพูด/directive/slot อื่นอยู่ระดับช็อต"),
                            &format!("shot {sid}: a beat holds prose or camera: only — dialogue/directives/other slots live at shot level"),
                        ),
                    ));
                }
            }
        }
    }

    // Backdrop selection: shot `scene:` > sequence `scene:`; either may
    // name a view. Prose scene refs still bind normally. A `@match_cut`
    // shot skips the *inherited* backdrop — first-frame mode takes no
    // image refs and its backdrop IS the captured frame (an explicit
    // shot-level `scene:` still binds and surfaces E_MODE_CONFLICT).
    let is_match_cut = directives.contains_key("match_cut");
    let scene_sel = properties
        .get("scene")
        .map(|(v, _)| v.trim().trim_start_matches('$').to_string())
        .or_else(|| {
            if is_match_cut {
                None
            } else {
                seq.scene.clone()
            }
        });
    if let Some(sel) = scene_sel {
        bind(&sel, errors);
    }

    if let (Some(d), Some((tone, _))) = (dialogue.as_mut(), properties.get("voice_tone")) {
        d.tone_hint = Some(tone.clone());
    }

    used.sort_unstable_by_key(|u| u.entity);
    let shot_type = shot.shot_type.unwrap_or(if dialogue.is_some() {
        ShotType::Dialogue
    } else {
        ShotType::Action
    });

    let get = |k: &str| directives.get(k).map(|(v, _)| v.as_str());

    for (key, feature) in [("hold", "@hold"), ("dialogue_sync", "@dialogue_sync: post")] {
        let bad = match key {
            "hold" => get("hold").is_some(),
            _ => get("dialogue_sync") == Some("post"),
        };
        if bad {
            errors.push(CompileError::error(
                "E_UNSUPPORTED_V1",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: {feature} ยังไม่รองรับใน v1"),
                    &format!("shot {sid}: {feature} is not supported in v1"),
                ),
            ));
        }
    }
    if let Some(t) = get("takes").and_then(|v| v.parse::<u32>().ok()) {
        if t > 1 {
            errors.push(CompileError::error(
                "E_UNSUPPORTED_V1",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: @takes: {t} ยังไม่รองรับใน v1 — ใช้ re-roll รายช็อตแทน"),
                    &format!("shot {sid}: @takes: {t} is not supported in v1 — use per-shot re-roll instead"),
                ),
            ));
        }
    }

    let bad_value = |field: &str, got: &str, allowed: &str, errors: &mut Vec<CompileError>| {
        errors.push(CompileError::error(
            "E_BAD_VALUE",
            Some(sid),
            msg(
                thai,
                &format!("shot {sid}: {field} '{got}' ไม่ถูกต้อง — ใช้ {allowed}"),
                &format!("shot {sid}: invalid {field} '{got}' — use {allowed}"),
            ),
        ));
    };

    for (field, allowed) in [
        ("@audio", &["on", "off"][..]),
        ("@subtitle", &["on", "off"][..]),
        ("@model", &["standard", "fast"][..]),
        ("@dialogue_sync", &["native", "post"][..]),
    ] {
        if let Some(v) = get(field.trim_start_matches('@')) {
            if !allowed.contains(&v) {
                bad_value(field, v, &allowed.join("|"), errors);
            }
        }
    }
    for field in ["seed", "takes"] {
        if let Some(v) = get(field) {
            if v.parse::<i64>().is_err() {
                bad_value(&format!("@{field}"), v, "an integer", errors);
            }
        }
    }

    let transition_out = match get("transition") {
        None => (TransitionKind::Cut, 0.0),
        Some(v) => {
            let mut toks = v.split_whitespace();
            let kind = match toks.next() {
                Some("cut") => TransitionKind::Cut,
                Some("fade") => TransitionKind::Fade,
                Some("dissolve") => TransitionKind::Dissolve,
                Some("to_black") => TransitionKind::ToBlack,
                other => {
                    errors.push(CompileError::error(
                        "E_PARSE",
                        Some(sid),
                        msg(
                            thai,
                            &format!(
                                "shot {sid}: @transition '{}' ไม่รู้จัก (cut/fade/dissolve/to_black)",
                                other.unwrap_or("")
                            ),
                            &format!(
                                "shot {sid}: unknown @transition '{}' (cut/fade/dissolve/to_black)",
                                other.unwrap_or("")
                            ),
                        ),
                    ));
                    TransitionKind::Cut
                }
            };
            let default_sec = if kind == TransitionKind::Cut {
                0.0
            } else {
                0.5
            };
            let sec = match toks.next() {
                None => default_sec,
                Some(t) => match t.parse::<f32>() {
                    Ok(x) if x.is_finite() && (0.0..=10.0).contains(&x) => x,
                    _ => {
                        bad_value("@transition duration", t, "0–10 seconds", errors);
                        default_sec
                    }
                },
            };
            (kind, sec)
        }
    };

    let duration = match get("duration") {
        Some(v) => match v.parse::<u32>() {
            Ok(d) => d,
            Err(_) => {
                bad_value("@duration", v, "seconds 4-15", errors);
                5
            }
        },
        None if shot_type == ShotType::Insert => 4,
        None => 5,
    };
    let ambient = properties.get("ambient").map(|(v, _)| v.clone());
    let backend = resolve_backend(
        get("backend"),
        program.header.backend.as_deref(),
        sid,
        thai,
        errors,
    );
    // Audio on by default (every backend generates good ambient/scene sound
    // for free); `@audio: off` or a film/sequence `audio_default: off` silences
    // it. Seedance's only weak spot is Thai *dialogue* timbre — and that's
    // overlaid with the real TTS in the harness, so leaving generate_audio on
    // is still correct there (it drives the lip motion we overlay onto).
    let audio_on = match get("audio") {
        Some("on") => true,
        Some("off") => false,
        _ => seq
            .audio_default
            .or(program.header.audio_default)
            .unwrap_or(true),
    };
    let subtitle = match get("subtitle") {
        Some("off") => false,
        Some("on") => true,
        _ => program.header.subtitle_default.unwrap_or(true),
    };

    let shot_prop = |k: &str| properties.get(k).map(|(v, _)| v.clone());
    let style = shot_prop("style")
        .or(seq.style.clone())
        .or(program.header.style.clone());
    let lighting = shot_prop("lighting")
        .or(seq.lighting.clone())
        .or(program.header.lighting.clone());

    ResolvedShot {
        id: shot.id.clone(),
        shot_type,
        sequence_index: seq_idx,
        used,
        action,
        beats,
        properties,
        sfx,
        ambient,
        dialogue,
        continue_from: get("continue_from").map(str::to_string),
        match_cut: get("match_cut").map(str::to_string),
        seed: get("seed").and_then(|v| v.parse().ok()),
        transition_out,
        subtitle,
        style,
        lighting,
        genre: program.header.genre.clone(),
        fps_look: program.header.fps_look.clone(),
        duration,
        resolution: get("resolution")
            .map(str::to_string)
            .or(seq.resolution.clone())
            .or(program.header.resolution.clone())
            .unwrap_or_else(|| "720p".into()),
        aspect: get("aspect")
            .map(str::to_string)
            .or(seq.aspect.clone())
            .or(program.header.aspect.clone())
            .unwrap_or_else(|| "16:9".into()),
        fast_model: get("model") == Some("fast"),
        audio_on,
        backend,
        dialogue_overlay: program.header.dialogue_sync.as_deref() == Some("overlay"),
        dialogue_lipsync: program.header.dialogue_sync.as_deref() == Some("lipsync"),
    }
}

/// Cascade: shot `@backend:` ⊕ film `backend:` ⊕ Grok. An unrecognized name
/// is `E_UNKNOWN_BACKEND` (and falls back to the default so the rest of the
/// shot still resolves).
fn resolve_backend(
    shot: Option<&str>,
    film: Option<&str>,
    sid: &str,
    thai: bool,
    errors: &mut Vec<CompileError>,
) -> BackendId {
    resolve_backend_with(shot, film, sid, thai, errors, BackendId::reachable)
}

/// [`resolve_backend`] against an explicit reachability test. On a
/// gateway-locked install the default is the first reachable backend and
/// an unreachable one is `E_BACKEND_UNAVAILABLE`.
fn resolve_backend_with(
    shot: Option<&str>,
    film: Option<&str>,
    sid: &str,
    thai: bool,
    errors: &mut Vec<CompileError>,
    reach: impl Fn(BackendId) -> bool,
) -> BackendId {
    let default = BackendId::default_with(&reach);
    let raw = match shot.or(film) {
        None => return default,
        Some(r) => r,
    };
    match BackendId::parse(raw) {
        Some(b) if !reach(b) => {
            let avail: Vec<&str> = BackendId::ALL
                .into_iter()
                .filter(|b| reach(*b))
                .map(BackendId::as_str)
                .collect();
            let avail = if avail.is_empty() {
                "-".to_string()
            } else {
                avail.join(" | ")
            };
            errors.push(CompileError::error(
                "E_BACKEND_UNAVAILABLE",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: @backend '{raw}' ใช้ไม่ได้บนระบบนี้ (ใช้ได้: {avail})"),
                    &format!("shot {sid}: @backend '{raw}' is not available on this deployment (available: {avail})"),
                ),
            ));
            default
        }
        Some(b) => b,
        None => {
            errors.push(CompileError::error(
                "E_UNKNOWN_BACKEND",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: @backend '{raw}' ไม่รู้จัก (grok | ltx | seedance | veo | happyhorse)"),
                    &format!("shot {sid}: unknown @backend '{raw}' (grok | ltx | seedance | veo | happyhorse)"),
                ),
            ));
            default
        }
    }
}

pub(crate) fn mode_of(shot: &ResolvedShot) -> Mode {
    if shot.match_cut.is_some() {
        Mode::FirstFrame
    } else if shot.continue_from.is_some() || !shot.used.is_empty() || shot.dialogue.is_some() {
        Mode::Reference
    } else {
        Mode::TextOnly
    }
}

#[cfg(test)]
mod tests {
    use super::super::parser::parse;
    use super::*;

    fn resolve_src(src: &str) -> (Resolution, Vec<CompileError>) {
        let (prog, perr) = parse(src);
        assert!(perr.is_empty(), "{perr:?}");
        resolve(&prog)
    }

    #[test]
    fn locked_backend_default_and_unavailable_error() {
        let only_hh = |b: BackendId| b == BackendId::HappyHorse;
        let mut errs = Vec::new();
        assert_eq!(
            resolve_backend_with(None, None, "s1", false, &mut errs, only_hh),
            BackendId::HappyHorse
        );
        assert_eq!(
            resolve_backend_with(Some("grok"), None, "s1", false, &mut errs, only_hh),
            BackendId::HappyHorse
        );
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, "E_BACKEND_UNAVAILABLE");
        assert!(
            errs[0].message.contains("happyhorse"),
            "{}",
            errs[0].message
        );
        let mut none = Vec::new();
        assert_eq!(
            resolve_backend_with(None, Some("ltx"), "s1", false, &mut none, |_| true),
            BackendId::Ltx
        );
        assert!(none.is_empty());
    }

    #[test]
    fn image_index_follows_declaration_order() {
        let (r, errors) = resolve_src(
            "char $a = @./a.png\nchar $b = @./b.png\nscene $s = @./s.png\n\
             shot 1 {\n$b ยืนข้าง $s แล้ว $a เดินเข้า\n}\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
        let used: Vec<usize> = r.shots[0].used.iter().map(|u| u.entity).collect();
        assert_eq!(used, vec![0, 1, 2]);
    }

    #[test]
    fn view_resolves_to_variant_image_at_base_slot() {
        let (r, errors) = resolve_src(
            "scene $ห้อง = @./room.png time:day\nview $ห้อง#หน้าต่าง = @./room_window.png\n\
             shot 1 {\nscene: $ห้อง#หน้าต่าง\nsomeone sits\n}\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
        let u = &r.shots[0].used[0];
        assert_eq!(u.tag.as_deref(), Some("หน้าต่าง"));
        assert_eq!(u.image_path, "./room_window.png");
    }

    #[test]
    fn mixed_variant_rejected() {
        let (_, errors) = resolve_src(
            "scene $ห้อง = @./room.png\nview $ห้อง#a = @./a.png\nview $ห้อง#b = @./b.png\n\
             shot 1 {\n$ห้อง#a และ $ห้อง#b\n}\n",
        );
        assert!(
            errors.iter().any(|e| e.code == "E_MIXED_VARIANT"),
            "{errors:?}"
        );
    }

    #[test]
    fn unknown_variant_rejected() {
        let (_, errors) = resolve_src("scene $s = @./s.png\nshot 1 {\nscene: $s#ghost\nx\n}\n");
        assert!(
            errors.iter().any(|e| e.code == "E_UNKNOWN_VARIANT"),
            "{errors:?}"
        );
    }

    #[test]
    fn sequence_scene_attaches_when_shot_silent() {
        let (r, errors) = resolve_src(
            "scene $s = @./s.png\nsequence \"x\" {\nscene: $s\nshot 1 {\nsomeone walks\n}\n}\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(r.shots[0].used.len(), 1);
    }

    #[test]
    fn style_cascade_inner_wins() {
        let src = "film \"t\" {\nstyle: film-style\n}\n\
                   sequence \"s\" {\nstyle: seq-style\nshot 1 {\nx\n}\nshot 2 {\nstyle: shot-style\ny\n}\n}\n\
                   shot 3 {\nz\n}\n";
        let (r, errors) = resolve_src(src);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(r.shots[0].style.as_deref(), Some("seq-style"));
        assert_eq!(r.shots[1].style.as_deref(), Some("shot-style"));
        assert_eq!(r.shots[2].style.as_deref(), Some("film-style"));
    }

    #[test]
    fn deferred_directives_error_clearly() {
        let (_, errors) =
            resolve_src("shot 1 {\n@takes: 3\n@hold: 2\n@dialogue_sync: post\nx\n}\n");
        assert_eq!(
            errors
                .iter()
                .filter(|e| e.code == "E_UNSUPPORTED_V1")
                .count(),
            3,
            "{errors:?}"
        );
    }

    #[test]
    fn match_cut_is_first_frame_mode() {
        let (r, errors) = resolve_src("shot 1 {\nx\n}\nshot 2 {\n@match_cut: 1\ny\n}\n");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(mode_of(&r.shots[1]), Mode::FirstFrame);
    }

    #[test]
    fn backend_cascade_defaults_and_overrides() {
        let (r, e) = resolve_src("shot 1 {\nx\n}\n");
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(
            r.shots[0].backend,
            BackendId::Grok,
            "bare script defaults to Grok"
        );

        let (r, e) = resolve_src(
            "film \"f\" {\nbackend: ltx\n}\nshot 1 {\nx\n}\nshot 2 {\n@backend: seedance\ny\n}\n",
        );
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(r.shots[0].backend, BackendId::Ltx, "film default");
        assert_eq!(r.shots[1].backend, BackendId::Seedance, "shot override");
    }

    #[test]
    fn ambient_turns_audio_on() {
        let (r, _) = resolve_src("shot 1 {\nambient: rain on the roof\nx\n}\n");
        assert!(r.shots[0].audio_on);
        assert_eq!(r.shots[0].ambient.as_deref(), Some("rain on the roof"));
    }

    #[test]
    fn sfx_cue_parses_and_transition_defaults() {
        let (r, errors) = resolve_src(
            "sfx $door = @./door.wav\nshot 1 {\nsfx: $door at 3s\n@transition: to_black 1.0\nx\n}\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(r.shots[0].sfx[0].handle, "door");
        assert_eq!(r.shots[0].sfx[0].at_sec, 3.0);
        assert_eq!(r.shots[0].transition_out, (TransitionKind::ToBlack, 1.0));
    }
}
