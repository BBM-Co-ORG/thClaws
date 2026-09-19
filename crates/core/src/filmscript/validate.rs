//! Validation: the three-state field model's third state. Every rule
//! answers one question — *can Seedance fill this itself?* — and
//! errors only when it cannot (spec §0). Rules and provenance live in
//! the spec (§3.4 + Appendix A); messages tell the authoring LLM the
//! exact fix, in the script's language.

use super::ast::*;
use super::backend::ContinuationMode;
use super::resolve::{Resolution, ResolvedShot};
use super::{msg, CompileError};
use unicode_segmentation::UnicodeSegmentation;

/// Camera-move tokens that count as a *primary* move — ≥2 in one
/// shot/beat jitters (Appendix A; per-beat since v2 §2.7). Matched
/// case-insensitively on word boundaries.
const MOVE_TOKENS: &[&str] = &[
    "push-in",
    "pushing",
    "push",
    "pull-back",
    "pull-out",
    "pulling",
    "pull",
    "panning",
    "pan",
    "tilting",
    "tilt",
    "tracking",
    "track",
    "dollying",
    "dolly",
    "zooming",
    "zoom",
    "craning",
    "crane",
    "orbiting",
    "orbit",
    "arc",
];

const PERFORMANCE_KEYS: &[&str] = &["expression", "gesture", "voice_tone"];

pub(crate) fn validate(program: &Program, res: &Resolution, errors: &mut Vec<CompileError>) {
    let thai = program.thai;

    for e in &res.entities {
        if let Some(VoiceSpec::CloneSample(p)) = &e.decl.voice {
            errors.push(CompileError::error(
                "E_UNSUPPORTED_V1",
                None,
                msg(
                    thai,
                    &format!("${}: voice:@{p} (voice cloning) ยังไม่รองรับใน v1 — ใช้ voice id จาก voices.json", e.decl.handle),
                    &format!("${}: voice:@{p} (voice cloning) is not supported in v1 — use a voices.json id", e.decl.handle),
                ),
            ));
        }
    }

    for e in &res.entities {
        if let Some(t) = &e.decl.time {
            if !["dawn", "day", "golden_hour", "dusk", "night"].contains(&t.as_str()) {
                errors.push(CompileError::error(
                    "E_BAD_VALUE",
                    None,
                    msg(
                        thai,
                        &format!(
                            "${}: time '{t}' ไม่ถูกต้อง — ใช้ dawn|day|golden_hour|dusk|night",
                            e.decl.handle
                        ),
                        &format!(
                            "${}: invalid time '{t}' — use dawn|day|golden_hour|dusk|night",
                            e.decl.handle
                        ),
                    ),
                ));
            }
        }
    }

    for (seq_idx, seq) in program.sequences.iter().enumerate() {
        if let Some(cue) = &seq.music {
            if !res.audio.music.contains_key(&cue.handle) {
                errors.push(CompileError::error(
                    "E_UNDECLARED_REF",
                    None,
                    msg(
                        thai,
                        &format!(
                            "sequence '{}': ${} ไม่ได้ประกาศเป็น music",
                            seq_title(seq),
                            cue.handle
                        ),
                        &format!(
                            "sequence '{}': ${} is not declared as music",
                            seq_title(seq),
                            cue.handle
                        ),
                    ),
                ));
            }
        }
        if let Some(scene) = &seq.scene {
            let base = scene.split('#').next().unwrap_or(scene);
            let is_new = !program.sequences[..seq_idx].iter().any(|prev| {
                prev.scene
                    .as_deref()
                    .map(|s| s.split('#').next().unwrap_or(s))
                    == Some(base)
            });
            let first_establishes = seq
                .shots
                .first()
                .map(|s| s.shot_type == Some(ShotType::Establishing))
                .unwrap_or(false);
            if is_new && !first_establishes {
                errors.push(CompileError::warning(
                    "W_SEQ_NO_ESTABLISH",
                    None,
                    msg(
                        thai,
                        &format!("sequence '{}': เปิด scene ใหม่โดยไม่มี establishing — ผู้ชมอาจหลงพื้นที่ (ปิด warning ได้ถ้าตั้งใจ)", seq_title(seq)),
                        &format!("sequence '{}': opens a new scene without an establishing shot — viewers may lose the space", seq_title(seq)),
                    ),
                ));
            }
        }
    }

    for shot in &res.shots {
        validate_shot(program, res, shot, errors);
    }
}

fn seq_title(seq: &Sequence) -> &str {
    seq.title.as_deref().unwrap_or("(unnamed)")
}

fn validate_shot(
    program: &Program,
    res: &Resolution,
    shot: &ResolvedShot,
    errors: &mut Vec<CompileError>,
) {
    let thai = program.thai;
    let sid = shot.id.as_str();

    // Backend capability contract (dev-plan/52 D3): reject shots the
    // selected backend can't fulfil. Kept to checks that are correct for the
    // default (Grok) so existing scripts are unaffected; voice-control/
    // dialogue-routing checks land with D4.
    let caps = shot.backend.caps();
    if shot.used.len() > caps.max_image_refs as usize {
        errors.push(CompileError::error(
            "E_TOO_MANY_REFS",
            Some(sid),
            msg(
                thai,
                &format!(
                    "shot {sid}: ใช้ ref ภาพ {} รูป แต่ backend '{}' รับได้สูงสุด {}",
                    shot.used.len(),
                    caps.id.as_str(),
                    caps.max_image_refs
                ),
                &format!(
                    "shot {sid}: {} image refs but backend '{}' allows at most {}",
                    shot.used.len(),
                    caps.id.as_str(),
                    caps.max_image_refs
                ),
            ),
        ));
    }
    if shot.continue_from.is_some() && caps.continuation == ContinuationMode::None {
        errors.push(CompileError::error(
            "E_NO_CONTINUATION",
            Some(sid),
            msg(
                thai,
                &format!(
                    "shot {sid}: @continue_from แต่ backend '{}' ต่อคลิปไม่ได้",
                    caps.id.as_str()
                ),
                &format!(
                    "shot {sid}: @continue_from but backend '{}' has no continuation",
                    caps.id.as_str()
                ),
            ),
        ));
    }

    if let Some(d) = &shot.dialogue {
        if d.text.trim().is_empty() {
            errors.push(CompileError::error(
                "E_DIALOGUE_NO_TEXT",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: ${} say แต่ข้อความว่าง", d.speaker),
                    &format!("shot {sid}: ${} says an empty line", d.speaker),
                ),
            ));
        }
        if let Some(e) = res.entities.iter().find(|e| e.decl.handle == d.speaker) {
            if e.decl.voice.is_none() {
                errors.push(CompileError::error(
                    "E_TTS_NO_VOICE",
                    Some(sid),
                    msg(
                        thai,
                        &format!("shot {sid}: ${} มีบทพูดแต่ไม่มี voice — ทุกตัวละครที่พูดต้องผูก voice (บทพูดทุกภาษาสังเคราะห์ผ่าน TTS)", d.speaker),
                        &format!("shot {sid}: ${} has dialogue but no voice binding — every speaking character needs voice: (all dialogue is TTS-synthesized)", d.speaker),
                    ),
                ));
            }
        }
        let too_long = if d.lang == "th" {
            d.text.graphemes(true).count() > 60
        } else {
            d.text.unicode_words().count() > 10
        };
        if too_long {
            errors.push(CompileError::warning(
                "W_LONG_LINE",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: บทพูดยาว — เกิน ~8 วินาทีปากจะเบลอ แบ่งเป็นหลายช็อตดีกว่า"),
                    &format!("shot {sid}: long dialogue line — lip-sync degrades past ~8s; split the shot"),
                ),
            ));
        }
        if matches!(shot.shot_type, ShotType::Establishing | ShotType::Insert) {
            errors.push(CompileError::error(
                "E_SLOT_NOT_ALLOWED",
                Some(sid),
                msg(
                    thai,
                    &format!(
                        "shot {sid}: ช็อตชนิด {:?} มีบทพูดไม่ได้ — ใช้ dialogue/action",
                        shot.shot_type
                    ),
                    &format!(
                        "shot {sid}: a {:?} shot cannot carry dialogue — use dialogue/action",
                        shot.shot_type
                    ),
                ),
            ));
        }
        // In-clip environmental sound (ambient rendered into the Seedance
        // prompt) competes with the dialogue audio-ref AND corrupts the
        // audio-overlay onset detection (silencedetect trips on the
        // explosion/rain, not the speech). Keep dialogue shots acoustically
        // clean; put loud ambience in separate non-dialogue shots (add SFX
        // at the assembly layer instead, which never touches the clip).
        if shot.ambient.is_some() {
            errors.push(CompileError::warning(
                "W_DIALOGUE_AMBIENT",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: มีทั้งบทพูดและ ambient ในช็อตเดียว — เสียงสภาพแวดล้อมในคลิปจะกวนการ sync เสียงพูด แยกเสียงดัง (ระเบิด/ฝน) ไปช็อตที่ไม่มีบทพูด หรือใส่เป็น sfx ตอน assembly"),
                    &format!("shot {sid}: dialogue + ambient in one shot — in-clip environmental sound disrupts speech sync; move loud ambience (explosion/rain) to a non-dialogue shot, or add it as an assembly-layer sfx"),
                ),
            ));
        }
    }

    for key in PERFORMANCE_KEYS {
        if shot.properties.contains_key(*key) {
            let allowed = match shot.shot_type {
                ShotType::Dialogue => true,
                ShotType::Action => *key != "voice_tone",
                ShotType::Establishing | ShotType::Insert => false,
            };
            if !allowed {
                errors.push(CompileError::error(
                    "E_SLOT_NOT_ALLOWED",
                    Some(sid),
                    msg(
                        thai,
                        &format!("shot {sid}: '{key}:' ใช้กับช็อตชนิด {:?} ไม่ได้ (performance slots ปิด)", shot.shot_type),
                        &format!("shot {sid}: '{key}:' is not allowed on a {:?} shot (performance slots are closed)", shot.shot_type),
                    ),
                ));
            }
        }
    }

    if let Some((camera, _)) = shot.properties.get("camera") {
        check_camera(camera, sid, thai, errors);
    }
    for beat in &shot.beats {
        if let ShotLine::Property { key, value, .. } = &beat.content {
            if key == "camera" {
                check_camera(value, sid, thai, errors);
            }
        }
    }

    if !shot.beats.is_empty() {
        let mut sorted: Vec<(u32, u32)> = shot.beats.iter().map(|b| (b.t1, b.t2)).collect();
        sorted.sort_unstable();
        let mut prev_end: Option<u32> = None;
        for (t1, t2) in &sorted {
            let broken =
                t1 >= t2 || *t2 > shot.duration || prev_end.map(|p| p != *t1).unwrap_or(false);
            if broken {
                errors.push(CompileError::error(
                    "E_BEAT_OVERLAP",
                    Some(sid),
                    msg(
                        thai,
                        &format!("shot {sid}: beat {t1}-{t2} ทับ/มีช่องว่าง/เกิน duration {}s — beats ต้องต่อเนื่องกัน", shot.duration),
                        &format!("shot {sid}: beat {t1}-{t2} overlaps/gaps/exceeds the {}s duration — beats must be contiguous", shot.duration),
                    ),
                ));
                break;
            }
            prev_end = Some(*t2);
        }
        if shot.beats.len() > 3 {
            errors.push(CompileError::warning(
                "W_BEATS_MANY",
                Some(sid),
                msg(
                    thai,
                    &format!(
                        "shot {sid}: {} beats — เกิน 3 มักทำ pacing เละ",
                        shot.beats.len()
                    ),
                    &format!(
                        "shot {sid}: {} beats — more than 3 usually wrecks pacing",
                        shot.beats.len()
                    ),
                ),
            ));
        }
    }

    if shot.match_cut.is_some()
        && (!shot.used.is_empty() || shot.dialogue.is_some() || shot.continue_from.is_some())
    {
        errors.push(CompileError::error(
                "E_MODE_CONFLICT",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: match_cut ใช้ first-frame mode ซึ่งใส่ ref รูป/เสียง/วิดีโอไม่ได้ — ใช้ @continue_from หรือเอา ref/บทพูดออก"),
                    &format!("shot {sid}: match_cut uses first-frame mode, which excludes image/audio/video refs — use @continue_from or drop the refs/dialogue"),
                ),
            ));
    }

    for (target, dir) in [
        (&shot.continue_from, "@continue_from"),
        (&shot.match_cut, "@match_cut"),
    ] {
        if let Some(target) = target {
            let known_earlier = res
                .shots
                .iter()
                .take_while(|s| s.id != shot.id)
                .any(|s| &s.id == target);
            if !known_earlier {
                errors.push(CompileError::error(
                    "E_CONTINUE_UNKNOWN",
                    Some(sid),
                    msg(
                        thai,
                        &format!("shot {sid}: {dir}: {target} — ไม่มี shot นี้ก่อนหน้า"),
                        &format!("shot {sid}: {dir}: {target} — no such earlier shot"),
                    ),
                ));
            }
        }
    }

    for cue in &shot.sfx {
        if !res.audio.sfx.contains_key(&cue.handle) {
            errors.push(CompileError::error(
                "E_SFX_UNDECLARED",
                Some(sid),
                msg(
                    thai,
                    &format!("shot {sid}: ${} ไม่ได้ประกาศเป็น sfx", cue.handle),
                    &format!("shot {sid}: ${} is not declared as sfx", cue.handle),
                ),
            ));
        }
    }

    const RESOLUTIONS: &[&str] = &["480p", "720p", "1080p", "4k"];
    if !RESOLUTIONS.contains(&shot.resolution.as_str()) {
        errors.push(CompileError::error(
            "E_BAD_VALUE",
            Some(sid),
            msg(
                thai,
                &format!(
                    "shot {sid}: resolution '{}' ไม่ถูกต้อง — ใช้ 480p|720p|1080p|4k",
                    shot.resolution
                ),
                &format!(
                    "shot {sid}: invalid resolution '{}' — use 480p|720p|1080p|4k",
                    shot.resolution
                ),
            ),
        ));
    } else if shot.fast_model && !["480p", "720p"].contains(&shot.resolution.as_str()) {
        errors.push(CompileError::error(
            "E_BAD_VALUE",
            Some(sid),
            msg(
                thai,
                &format!(
                    "shot {sid}: @model: fast มีเฉพาะ 480p/720p (ได้ {})",
                    shot.resolution
                ),
                &format!(
                    "shot {sid}: @model: fast only offers 480p/720p (got {})",
                    shot.resolution
                ),
            ),
        ));
    }

    if !(4..=15).contains(&shot.duration) {
        errors.push(CompileError::error(
            "E_DURATION_RANGE",
            Some(sid),
            msg(
                thai,
                &format!("shot {sid}: duration {}s อยู่นอกช่วง 4–15", shot.duration),
                &format!("shot {sid}: duration {}s is outside 4–15", shot.duration),
            ),
        ));
    }

    let char_count = shot
        .used
        .iter()
        .filter(|u| res.entities[u.entity].decl.kind == EntityKind::Char)
        .count();
    if char_count > 5 {
        errors.push(CompileError::warning(
            "W_TOO_MANY_CHARS",
            Some(sid),
            msg(
                thai,
                &format!("shot {sid}: ตัวละคร {char_count} ตัว — เกิน 5 identity จะเริ่มเพี้ยน"),
                &format!("shot {sid}: {char_count} characters — identity degrades past 5"),
            ),
        ));
    }
    if shot.used.len() > 9 {
        errors.push(CompileError::warning(
            "W_TOO_MANY_IMAGES",
            Some(sid),
            msg(
                thai,
                &format!(
                    "shot {sid}: ใช้รูปอ้างอิง {} รูป — Seedance รับสูงสุด 9",
                    shot.used.len()
                ),
                &format!(
                    "shot {sid}: {} image refs — Seedance caps at 9",
                    shot.used.len()
                ),
            ),
        ));
    }
}

fn check_camera(camera: &str, sid: &str, thai: bool, errors: &mut Vec<CompileError>) {
    let lower = camera.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .collect();
    if words.contains(&"fast") {
        errors.push(CompileError::error(
            "E_FAST_TOKEN",
            Some(sid),
            msg(
                thai,
                &format!("shot {sid}: คำว่า 'fast' ใน camera ทำคุณภาพพัง — บอกจังหวะด้วยคำอื่น (เช่น brisk, energetic)"),
                &format!("shot {sid}: the token 'fast' in camera degrades quality — use explicit pacing words instead"),
            ),
        ));
    }
    let mut moves: Vec<&str> = Vec::new();
    for t in MOVE_TOKENS {
        if words.contains(t) && !moves.iter().any(|m| m.starts_with(t) || t.starts_with(*m)) {
            moves.push(t);
        }
    }
    if moves.len() > 1 {
        errors.push(CompileError::error(
            "E_TWO_CAMERA_MOVES",
            Some(sid),
            msg(
                thai,
                &format!(
                    "shot {sid}: camera มี move มากกว่าหนึ่ง ({}) — หนึ่งช็อต/หนึ่ง beat ต่อหนึ่ง primary move",
                    moves.join(", ")
                ),
                &format!(
                    "shot {sid}: camera has multiple moves ({}) — one primary move per shot/beat",
                    moves.join(", ")
                ),
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::super::compile_phase1;

    fn codes(src: &str) -> Vec<&'static str> {
        compile_phase1(src).errors.iter().map(|e| e.code).collect()
    }

    #[test]
    fn tts_no_voice_is_universal() {
        let c = codes("char $a = @./a.png desc:\"x\"\nshot 1 {\n$a say \"hello world\"\n}\n");
        assert!(c.contains(&"E_TTS_NO_VOICE"), "{c:?}");
    }

    #[test]
    fn two_camera_moves_rejected_per_beat_too() {
        assert!(codes("shot 1 {\ncamera: pan then push-in\n}\n").contains(&"E_TWO_CAMERA_MOVES"));
        let c = codes("shot 1 {\n[0-2] camera: pan and zoom\n[2-5] x\n}\n");
        assert!(c.contains(&"E_TWO_CAMERA_MOVES"), "{c:?}");
    }

    #[test]
    fn beat_rules() {
        let gap = codes("shot 1 {\n[0-2] a\n[3-5] b\n}\n");
        assert!(gap.contains(&"E_BEAT_OVERLAP"), "{gap:?}");
        let over = codes("shot 1 {\n[0-3] a\n[3-9] b\n}\n");
        assert!(over.contains(&"E_BEAT_OVERLAP"), "{over:?}");
        let ok = codes("shot 1 {\n[0-3] a\n[3-5] b\n}\n");
        assert!(!ok.contains(&"E_BEAT_OVERLAP"), "{ok:?}");
        let many = codes("shot 1 {\n@duration: 8\n[0-2] a\n[2-4] b\n[4-6] c\n[6-8] d\n}\n");
        assert!(many.contains(&"W_BEATS_MANY"), "{many:?}");
    }

    #[test]
    fn match_cut_conflicts() {
        let c = codes(
            "char $a = @./a.png voice:v\nshot 1 {\nx\n}\nshot 2 {\n@match_cut: 1\n$a walks\n}\n",
        );
        assert!(c.contains(&"E_MODE_CONFLICT"), "{c:?}");
        let ok = codes("shot 1 {\nx\n}\nshot 2 {\n@match_cut: 1\nwide empty room\n}\n");
        assert!(!ok.contains(&"E_MODE_CONFLICT"), "{ok:?}");
    }

    #[test]
    fn sfx_and_music_must_be_declared() {
        let c = codes("shot 1 {\nsfx: $door at 2s\nx\n}\n");
        assert!(c.contains(&"E_SFX_UNDECLARED"), "{c:?}");
        let m = codes("sequence \"s\" {\nmusic: $theme volume:0.5\nshot 1 {\nx\n}\n}\n");
        assert!(m.contains(&"E_UNDECLARED_REF"), "{m:?}");
    }

    #[test]
    fn new_scene_without_establishing_warns() {
        let c = codes(
            "scene $s = @./s.png\nsequence \"a\" {\nscene: $s\nshot 1 (dialogue) {\nchar talk\n}\n}\n",
        );
        assert!(c.contains(&"W_SEQ_NO_ESTABLISH"), "{c:?}");
        let ok = codes(
            "scene $s = @./s.png\nsequence \"a\" {\nscene: $s\nshot 0 (establishing) {\nthe room\n}\n}\n",
        );
        assert!(!ok.contains(&"W_SEQ_NO_ESTABLISH"), "{ok:?}");
    }

    #[test]
    fn continue_from_must_reference_earlier_shot() {
        let fwd = codes("shot 1 {\n@continue_from: 2\nx\n}\nshot 2 {\ny\n}\n");
        assert!(fwd.contains(&"E_CONTINUE_UNKNOWN"), "{fwd:?}");
        let ok = codes("shot 1 {\nx\n}\nshot 2 {\n@continue_from: 1\ny\n}\n");
        assert!(!ok.contains(&"E_CONTINUE_UNKNOWN"), "{ok:?}");
    }

    #[test]
    fn duration_range_enforced() {
        assert!(codes("shot 1 {\n@duration: 3\nx\n}\n").contains(&"E_DURATION_RANGE"));
        assert!(codes("shot 1 {\n@duration: 16\nx\n}\n").contains(&"E_DURATION_RANGE"));
    }

    #[test]
    fn insert_rejects_performance_and_dialogue() {
        let c = codes(
            "char $a = @./a.png voice:v\nshot 1 (insert) {\nexpression: sad\n$a say \"x\"\n}\n",
        );
        assert_eq!(
            c.iter().filter(|c| **c == "E_SLOT_NOT_ALLOWED").count(),
            2,
            "{c:?}"
        );
    }

    #[test]
    fn variant_speaker_keeps_voice_contract() {
        // audit H2: $a#wet say must hit the SAME voice/TTS path as $a
        let missing = codes(
            "char $a = @./a.png desc:\"x\"\nvariant $a#wet = @./w.png\nshot 1 {\n$a#wet say \"hi\"\n}\n",
        );
        assert!(missing.contains(&"E_TTS_NO_VOICE"), "{missing:?}");
        let ok = codes(
            "char $a = @./a.png voice:v desc:\"x\"\nvariant $a#wet = @./w.png\nshot 1 {\n$a#wet say \"hi\"\n}\n",
        );
        assert!(!ok.contains(&"E_TTS_NO_VOICE"), "{ok:?}");
    }

    #[test]
    fn beat_content_restricted() {
        let c = codes(
            "char $a = @./a.png voice:v\nshot 1 {\n[0-3] $a say \"hi\"\n[3-5] @duration: 9\n}\n",
        );
        assert_eq!(c.iter().filter(|c| **c == "E_PARSE").count(), 2, "{c:?}");
        let empty = codes("shot 1 {\n[0-3]\n}\n");
        assert!(empty.contains(&"E_PARSE"), "{empty:?}");
    }

    #[test]
    fn duplicate_shot_ids_rejected_and_continue_still_works() {
        let c = codes("shot 1 {\nx\n}\nshot 1 {\ny\n}\n");
        assert!(c.contains(&"E_DUPLICATE_SHOT"), "{c:?}");
        let c2 = codes("shot 2 {\nx\n}\nshot 1 {\n@continue_from: 2\ny\n}\n");
        assert!(!c2.contains(&"E_CONTINUE_UNKNOWN"), "{c2:?}");
    }

    #[test]
    fn bad_values_diagnosed() {
        assert!(codes("shot 1 {\n@resolution: 1080P\nx\n}\n").contains(&"E_BAD_VALUE"));
        assert!(codes("shot 1 {\n@model: fast\n@resolution: 4k\nx\n}\n").contains(&"E_BAD_VALUE"));
        assert!(codes("shot 1 {\n@duration: abc\nx\n}\n").contains(&"E_BAD_VALUE"));
        assert!(codes("shot 1 {\n@transition: dissolve NaN\nx\n}\n").contains(&"E_BAD_VALUE"));
        assert!(codes("shot 1 {\n@audio: loud\nx\n}\n").contains(&"E_BAD_VALUE"));
        assert!(
            codes("scene $s = @./s.png time:noonish\nshot 1 {\n$s\n}\n").contains(&"E_BAD_VALUE")
        );
        assert!(
            codes("film \"t\" {\ndialogue_sync: post\n}\nshot 1 {\nx\n}\n")
                .contains(&"E_UNSUPPORTED_V1")
        );
    }

    #[test]
    fn duplicate_lines_rejected_not_last_win() {
        let c = codes("shot 1 {\ncamera: pan and zoom\ncamera: static\n}\n");
        assert!(c.contains(&"E_DUPLICATE_LINE"), "{c:?}");
        assert!(c.contains(&"E_TWO_CAMERA_MOVES"), "{c:?}");
        let d = codes("shot 1 {\n@duration: 5\n@duration: 9\nx\n}\n");
        assert!(d.contains(&"E_DUPLICATE_LINE"), "{d:?}");
    }

    #[test]
    fn match_cut_ignores_inherited_scene() {
        let c = codes(
            "scene $s = @./s.png\nsequence \"q\" {\nscene: $s\nshot 0 (establishing) {\nthe room\n}\nshot 2 {\n@match_cut: 0\nempty room\n}\n}\n",
        );
        assert!(!c.contains(&"E_MODE_CONFLICT"), "{c:?}");
    }

    #[test]
    fn adversarial_inputs_do_not_panic() {
        let _ = codes("shot 1 {\n@duration: 4294967295\nx\n}\nshot 2 {\n@continue_from: 1\ny\n}\n");
        let _ = codes("shot 1 {\n[100000000:00-100000000:01] x\n}\n");
        let _ = codes("\u{feff}shot 1 {\nx\n}\n");
    }

    #[test]
    fn beats_may_start_after_zero() {
        let c = codes("shot 1 {\n[2-5] she turns\nx\n}\n");
        assert!(!c.contains(&"E_BEAT_OVERLAP"), "{c:?}");
    }

    #[test]
    fn inflected_camera_moves_detected() {
        let c = codes("shot 1 {\ncamera: panning left, zooming in\n}\n");
        assert!(c.contains(&"E_TWO_CAMERA_MOVES"), "{c:?}");
    }

    #[test]
    fn dialogue_plus_ambient_warns() {
        let c = codes("char $a = @./a.png voice:v desc:\"x\"\nshot 1 (dialogue) {\nambient: rain on the roof\n$a say \"hello\"\n}\n");
        assert!(c.contains(&"W_DIALOGUE_AMBIENT"), "{c:?}");
        // ambient alone (no dialogue) is fine
        let ok = codes("shot 1 {\nambient: distant thunder\nlightning over the sea\n}\n");
        assert!(!ok.contains(&"W_DIALOGUE_AMBIENT"), "{ok:?}");
    }

    #[test]
    fn unknown_backend_rejected() {
        let c = codes("shot 1 {\n@backend: bogus\nan empty room\n}\n");
        assert!(c.contains(&"E_UNKNOWN_BACKEND"), "{c:?}");
    }

    #[test]
    fn too_many_refs_for_backend() {
        // LTX allows 2 image refs; a 3-ref shot must error, but Grok (7) is fine.
        let src = "char $a = @./a.png desc:\"a\"\n\
                   char $b = @./b.png desc:\"b\"\n\
                   char $c = @./c.png desc:\"c\"\n\
                   shot 1 {\n@backend: BK\n$a with $b and $c stand together\n}\n";
        let c = codes(&src.replace("BK", "ltx"));
        assert!(c.contains(&"E_TOO_MANY_REFS"), "{c:?}");
        let ok = codes(&src.replace("BK", "grok"));
        assert!(!ok.contains(&"E_TOO_MANY_REFS"), "{ok:?}");
    }

    #[test]
    fn continuation_unsupported_on_happyhorse() {
        let src = "shot 1 {\na wide empty street\n}\n\
                   shot 2 {\n@backend: BK\n@continue_from: 1\nthe same street, closer\n}\n";
        let c = codes(&src.replace("BK", "happyhorse"));
        assert!(c.contains(&"E_NO_CONTINUATION"), "{c:?}");
        let ok = codes(&src.replace("BK", "grok"));
        assert!(!ok.contains(&"E_NO_CONTINUATION"), "{ok:?}");
    }

    #[test]
    fn clone_voice_deferred() {
        let c = codes("char $a = @./a.png voice:@./sample.mp3\nshot 1 {\n$a walks\n}\n");
        assert!(c.contains(&"E_UNSUPPORTED_V1"), "{c:?}");
    }
}
