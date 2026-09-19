//! Line-oriented parser. The film level adds two block kinds around
//! Tier-1's shots — `film{}` (fields only) and `sequence{}` (fields +
//! shots) — so parsing is a small context stack instead of one flag.
//! Bare shots outside any sequence land in an implicit one, keeping
//! every Tier-1 flat script valid. Parse errors are collected, not
//! thrown — the LLM repair loop wants all of them.

use super::ast::*;
use super::lexer::*;
use super::{is_thai_text, msg, CompileError};

enum Ctx {
    Top,
    Film,
    Sequence,
    Shot,
}

pub(crate) fn parse(source: &str) -> (Program, Vec<CompileError>) {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let thai = is_thai_text(source);
    let mut errors = Vec::new();
    let mut header = FilmHeader::default();
    let mut decls: Vec<Declaration> = Vec::new();
    let mut sequences: Vec<Sequence> = Vec::new();
    let mut implicit: Option<Sequence> = None;
    let mut seq: Option<Sequence> = None;
    let mut shot: Option<Shot> = None;
    let mut ctx = Ctx::Top;

    for (idx, raw) in source.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let first = line.split_whitespace().next().unwrap_or("");

        match ctx {
            Ctx::Shot => {
                if line == "}" {
                    let done = shot.take().unwrap();
                    match seq.as_mut().or(implicit.as_mut()) {
                        Some(s) => s.shots.push(done),
                        None => unreachable!("shot always opens inside a sequence context"),
                    }
                    ctx = if seq.is_some() { Ctx::Sequence } else { Ctx::Top };
                } else {
                    shot.as_mut().unwrap().lines.push(parse_shot_line(
                        line,
                        line_no,
                        thai,
                        &mut errors,
                    ));
                }
            }
            Ctx::Film => {
                if line == "}" {
                    ctx = Ctx::Top;
                } else if let Some((key, value)) = split_key_value(line) {
                    parse_film_field(&mut header, key, value, line_no, thai, &mut errors);
                } else {
                    errors.push(CompileError::error(
                        "E_PARSE",
                        None,
                        msg(
                            thai,
                            &format!("บรรทัด {line_no}: ใน film block ต้องเป็น key: value"),
                            &format!("line {line_no}: film block lines must be key: value"),
                        ),
                    ));
                }
            }
            Ctx::Sequence => {
                if line == "}" {
                    sequences.push(seq.take().unwrap());
                    ctx = Ctx::Top;
                } else if first == "shot" {
                    match parse_shot_header(line, line_no, thai) {
                        Ok(s) => {
                            shot = Some(s);
                            ctx = Ctx::Shot;
                        }
                        Err(e) => errors.push(e),
                    }
                } else if let Some((key, value)) = split_key_value(line) {
                    parse_sequence_field(
                        seq.as_mut().unwrap(),
                        key,
                        value,
                        line_no,
                        thai,
                        &mut errors,
                    );
                } else {
                    errors.push(CompileError::error(
                        "E_PARSE",
                        None,
                        msg(
                            thai,
                            &format!("บรรทัด {line_no}: ใน sequence ต้องเป็น shot block หรือ key: value"),
                            &format!("line {line_no}: sequence lines must be shot blocks or key: value"),
                        ),
                    ));
                }
            }
            Ctx::Top => match first {
                "char" | "scene" | "prop" => match parse_entity_decl(first, line, line_no, thai) {
                    Ok(d) => push_decl(Declaration::Entity(d), &mut decls, thai, &mut errors),
                    Err(e) => errors.push(e),
                },
                "variant" | "view" => match parse_variant_decl(first, line, line_no, thai) {
                    Ok(d) => decls.push(d),
                    Err(e) => errors.push(e),
                },
                "music" | "sfx" => match parse_audio_decl(first, line, line_no, thai) {
                    Ok(d) => decls.push(d),
                    Err(e) => errors.push(e),
                },
                "film" => {
                    header.title = quoted_title(line);
                    if !line.trim_end().ends_with('{') {
                        errors.push(block_needs_brace(line_no, "film", thai));
                    } else {
                        ctx = Ctx::Film;
                    }
                }
                "sequence" => {
                    if let Some(s) = implicit.take() {
                        sequences.push(s);
                    }
                    if !line.trim_end().ends_with('{') {
                        errors.push(block_needs_brace(line_no, "sequence", thai));
                    } else {
                        seq = Some(Sequence {
                            title: quoted_title(line),
                            line: line_no,
                            ..Default::default()
                        });
                        ctx = Ctx::Sequence;
                    }
                }
                "shot" => match parse_shot_header(line, line_no, thai) {
                    Ok(s) => {
                        implicit.get_or_insert_with(|| Sequence { line: line_no, ..Default::default() });
                        shot = Some(s);
                        ctx = Ctx::Shot;
                    }
                    Err(e) => errors.push(e),
                },
                _ => errors.push(CompileError::error(
                    "E_PARSE",
                    None,
                    msg(
                        thai,
                        &format!("บรรทัด {line_no}: ไม่เข้าใจ '{line}' — นอก block ต้องเป็น declaration, film, sequence หรือ shot"),
                        &format!("line {line_no}: cannot parse '{line}' — expected a declaration, film, sequence or shot"),
                    ),
                )),
            },
        }
    }

    if let Some(s) = shot.take() {
        errors.push(CompileError::error(
            "E_PARSE",
            Some(&s.id),
            msg(
                thai,
                &format!("shot {}: ไม่มี `}}` ปิด block", s.id),
                &format!("shot {}: missing closing `}}`", s.id),
            ),
        ));
        if let Some(sq) = seq.as_mut().or(implicit.as_mut()) {
            sq.shots.push(s)
        }
    }
    if let Some(s) = seq.take() {
        errors.push(CompileError::error(
            "E_PARSE",
            None,
            msg(
                thai,
                "sequence ไม่มี `}` ปิด block",
                "sequence missing closing `}`",
            ),
        ));
        sequences.push(s);
    }
    if let Some(s) = implicit.take() {
        sequences.push(s);
    }

    (
        Program {
            header,
            declarations: decls,
            sequences,
            thai,
        },
        errors,
    )
}

fn push_decl(
    d: Declaration,
    decls: &mut Vec<Declaration>,
    thai: bool,
    errors: &mut Vec<CompileError>,
) {
    let Declaration::Entity(e) = &d else {
        unreachable!()
    };
    let dup = decls
        .iter()
        .any(|x| matches!(x, Declaration::Entity(y) if y.handle == e.handle));
    if dup {
        errors.push(CompileError::error(
            "E_DUPLICATE_DECL",
            None,
            msg(
                thai,
                &format!("บรรทัด {}: ${} ถูกประกาศซ้ำ", e.line, e.handle),
                &format!("line {}: ${} declared twice", e.line, e.handle),
            ),
        ));
    } else {
        decls.push(d);
    }
}

fn block_needs_brace(line_no: usize, what: &str, thai: bool) -> CompileError {
    CompileError::error(
        "E_PARSE",
        None,
        msg(
            thai,
            &format!("บรรทัด {line_no}: {what} header ต้องจบด้วย '{{'"),
            &format!("line {line_no}: {what} header must end with '{{'"),
        ),
    )
}

fn quoted_title(line: &str) -> Option<String> {
    let start = line.find('"')?;
    let rest = &line[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn parse_film_field(
    header: &mut FilmHeader,
    key: &str,
    value: &str,
    line_no: usize,
    thai: bool,
    errors: &mut Vec<CompileError>,
) {
    let v = value.to_string();
    match key {
        "aspect" => header.aspect = Some(v),
        "resolution" => header.resolution = Some(v),
        "style" => header.style = Some(v),
        "lighting" => header.lighting = Some(v),
        "genre" => header.genre = Some(v),
        "fps_look" => header.fps_look = Some(v),
        "audio_default" => header.audio_default = Some(value == "on" || value == "true"),
        "subtitle" | "subtitle_default" => {
            header.subtitle_default = Some(value == "on" || value == "true")
        }
        "subtitle_burn" => header.subtitle_burn = Some(value == "on" || value == "true"),
        "dialogue_sync" => header.dialogue_sync = Some(v),
        "backend" => header.backend = Some(v),
        _ => errors.push(CompileError::error(
            "E_PARSE",
            None,
            msg(
                thai,
                &format!("บรรทัด {line_no}: film field '{key}:' ไม่รู้จัก"),
                &format!("line {line_no}: unknown film field '{key}:'"),
            ),
        )),
    }
}

fn parse_sequence_field(
    seq: &mut Sequence,
    key: &str,
    value: &str,
    line_no: usize,
    thai: bool,
    errors: &mut Vec<CompileError>,
) {
    match key {
        "scene" => match value.strip_prefix('$') {
            Some(h) => seq.scene = Some(nfc(h)),
            None => errors.push(CompileError::error(
                "E_PARSE",
                None,
                msg(
                    thai,
                    &format!("บรรทัด {line_no}: sequence scene: ต้องเป็น $handle"),
                    &format!("line {line_no}: sequence scene: takes a $handle"),
                ),
            )),
        },
        "style" => seq.style = Some(value.to_string()),
        "lighting" => seq.lighting = Some(value.to_string()),
        "aspect" => seq.aspect = Some(value.to_string()),
        "resolution" => seq.resolution = Some(value.to_string()),
        "audio_default" => seq.audio_default = Some(value == "on" || value == "true"),
        "music" => {
            let mut toks = value.split_whitespace();
            let handle = toks.next().and_then(|t| t.strip_prefix('$'));
            match handle {
                Some(h) => {
                    let volume = match toks.find_map(|t| t.strip_prefix("volume:")) {
                        None => 1.0,
                        Some(v) => match v.parse::<f32>() {
                            Ok(x) if x.is_finite() && (0.0..=2.0).contains(&x) => x,
                            _ => {
                                errors.push(CompileError::error(
                                    "E_BAD_VALUE",
                                    None,
                                    msg(
                                        thai,
                                        &format!("บรรทัด {line_no}: volume '{v}' ไม่ถูกต้อง — ใช้ 0.0–2.0"),
                                        &format!("line {line_no}: invalid volume '{v}' — use 0.0–2.0"),
                                    ),
                                ));
                                1.0
                            }
                        },
                    };
                    seq.music = Some(MusicCue {
                        handle: nfc(h),
                        volume,
                    });
                }
                None => errors.push(CompileError::error(
                    "E_PARSE",
                    None,
                    msg(
                        thai,
                        &format!("บรรทัด {line_no}: music: ต้องเป็น $handle [volume:x]"),
                        &format!("line {line_no}: music: takes $handle [volume:x]"),
                    ),
                )),
            }
        }
        _ => errors.push(CompileError::error(
            "E_PARSE",
            None,
            msg(
                thai,
                &format!(
                    "บรรทัด {line_no}: sequence field '{key}:' ไม่รู้จัก (scene/style/lighting/aspect/resolution/audio_default/music)"
                ),
                &format!(
                    "line {line_no}: unknown sequence field '{key}:' (scene/style/lighting/aspect/resolution/audio_default/music)"
                ),
            ),
        )),
    }
}

fn parse_entity_decl(
    kind_tok: &str,
    line: &str,
    line_no: usize,
    thai: bool,
) -> Result<EntityDecl, CompileError> {
    let kind = match kind_tok {
        "char" => EntityKind::Char,
        "scene" => EntityKind::Scene,
        _ => EntityKind::Prop,
    };
    let bad = |th: String, en: String| CompileError::error("E_PARSE", None, msg(thai, &th, &en));

    let (lhs, rhs) = line.split_once('=').ok_or_else(|| {
        bad(
            format!("บรรทัด {line_no}: declaration ต้องมี '=' (เช่น char $ชื่อ = @./รูป.png)"),
            format!("line {line_no}: declaration needs '=' (e.g. char $name = @./img.png)"),
        )
    })?;

    let mut lhs_toks = decl_tokens(lhs.trim());
    lhs_toks.remove(0);
    let handle_tok = lhs_toks.first().ok_or_else(|| {
        bad(
            format!("บรรทัด {line_no}: ไม่มี $handle"),
            format!("line {line_no}: missing $handle"),
        )
    })?;
    let handle = handle_tok.strip_prefix('$').map(nfc).ok_or_else(|| {
        bad(
            format!("บรรทัด {line_no}: handle ต้องขึ้นต้นด้วย $ (ได้ '{handle_tok}')"),
            format!("line {line_no}: handle must start with $ (got '{handle_tok}')"),
        )
    })?;
    if handle.contains('#') {
        return Err(bad(
            format!("บรรทัด {line_no}: ${handle} — ประกาศ variant/view ด้วยคีย์เวิร์ด variant/view ไม่ใช่ {kind_tok}"),
            format!("line {line_no}: ${handle} — declare variants/views with the variant/view keywords, not {kind_tok}"),
        ));
    }
    let alias = lhs_toks.get(1).map(|t| unquote(t).to_string());

    let rhs_toks = decl_tokens(rhs.trim());
    let path_tok = rhs_toks.first().ok_or_else(|| {
        bad(
            format!("บรรทัด {line_no}: ไม่มี @path หลัง '='"),
            format!("line {line_no}: missing @path after '='"),
        )
    })?;
    let image_path = path_tok
        .strip_prefix('@')
        .map(str::to_string)
        .ok_or_else(|| {
            bad(
                format!("บรรทัด {line_no}: ไฟล์ asset ต้องเขียนเป็น @./path (ได้ '{path_tok}')"),
                format!("line {line_no}: asset file must be @./path (got '{path_tok}')"),
            )
        })?;

    let mut voice = None;
    let mut desc = None;
    let mut time = None;
    for tok in &rhs_toks[1..] {
        let Some((k, v)) = tok.split_once(':') else {
            return Err(bad(
                format!("บรรทัด {line_no}: ไม่เข้าใจ '{tok}' — property ต้องเป็น key:value"),
                format!("line {line_no}: cannot parse '{tok}' — properties are key:value"),
            ));
        };
        match k {
            "voice" => {
                voice = Some(match v.strip_prefix('@') {
                    Some(p) => VoiceSpec::CloneSample(p.to_string()),
                    None => VoiceSpec::Registry(v.to_string()),
                })
            }
            "desc" => desc = Some(unquote(v).to_string()),
            "time" => time = Some(v.to_string()),
            _ => {
                return Err(bad(
                    format!("บรรทัด {line_no}: property '{k}:' ไม่รู้จัก (รองรับ voice/desc/time)"),
                    format!("line {line_no}: unknown property '{k}:' (supported: voice/desc/time)"),
                ))
            }
        }
    }

    Ok(EntityDecl {
        kind,
        handle,
        alias,
        image_path,
        voice,
        desc,
        time,
        line: line_no,
    })
}

fn parse_variant_decl(
    kw: &str,
    line: &str,
    line_no: usize,
    thai: bool,
) -> Result<Declaration, CompileError> {
    let bad = |th: String, en: String| CompileError::error("E_PARSE", None, msg(thai, &th, &en));
    let (lhs, rhs) = line.split_once('=').ok_or_else(|| {
        bad(
            format!("บรรทัด {line_no}: {kw} ต้องมี '=' (เช่น {kw} $base#tag = @./img.png)"),
            format!("line {line_no}: {kw} needs '=' (e.g. {kw} $base#tag = @./img.png)"),
        )
    })?;
    let handle_tok = lhs.split_whitespace().nth(1).unwrap_or("");
    let full = handle_tok.strip_prefix('$').map(nfc).ok_or_else(|| {
        bad(
            format!("บรรทัด {line_no}: {kw} ต้องเป็น $base#tag"),
            format!("line {line_no}: {kw} takes $base#tag"),
        )
    })?;
    let (base, tag) = full.split_once('#').ok_or_else(|| {
        bad(
            format!("บรรทัด {line_no}: {kw} ${full} ไม่มี #tag"),
            format!("line {line_no}: {kw} ${full} lacks a #tag"),
        )
    })?;
    let mut rhs_toks = rhs.split_whitespace();
    let path_tok = rhs_toks.next().unwrap_or("");
    let image_path = path_tok
        .strip_prefix('@')
        .map(str::to_string)
        .ok_or_else(|| {
            bad(
                format!("บรรทัด {line_no}: ไฟล์ asset ต้องเขียนเป็น @./path"),
                format!("line {line_no}: asset file must be @./path"),
            )
        })?;
    if let Some(extra) = rhs_toks.next() {
        return Err(bad(
            format!("บรรทัด {line_no}: {kw} ไม่รับ property เพิ่ม (เจอ '{extra}') — สืบทอดจาก base ทั้งหมด"),
            format!("line {line_no}: {kw} takes no extra properties (got '{extra}') — everything inherits from the base"),
        ));
    }
    Ok(Declaration::Variant {
        base: base.to_string(),
        tag: tag.to_string(),
        image_path,
        is_view: kw == "view",
        line: line_no,
    })
}

fn parse_audio_decl(
    kw: &str,
    line: &str,
    line_no: usize,
    thai: bool,
) -> Result<Declaration, CompileError> {
    let bad = |th: String, en: String| CompileError::error("E_PARSE", None, msg(thai, &th, &en));
    let (lhs, rhs) = line.split_once('=').ok_or_else(|| {
        bad(
            format!("บรรทัด {line_no}: {kw} ต้องมี '=' (เช่น {kw} $ชื่อ = @./file.mp3)"),
            format!("line {line_no}: {kw} needs '=' (e.g. {kw} $name = @./file.mp3)"),
        )
    })?;
    let handle_tok = lhs.split_whitespace().nth(1).unwrap_or("");
    let handle = handle_tok.strip_prefix('$').map(nfc).ok_or_else(|| {
        bad(
            format!("บรรทัด {line_no}: {kw} ต้องมี $handle"),
            format!("line {line_no}: {kw} needs a $handle"),
        )
    })?;
    let mut rhs_toks = rhs.split_whitespace();
    let path_tok = rhs_toks.next().unwrap_or("");
    let path = path_tok
        .strip_prefix('@')
        .map(str::to_string)
        .ok_or_else(|| {
            bad(
                format!("บรรทัด {line_no}: ไฟล์เสียงต้องเขียนเป็น @./path"),
                format!("line {line_no}: audio file must be @./path"),
            )
        })?;
    if let Some(extra) = rhs_toks.next() {
        return Err(bad(
            format!("บรรทัด {line_no}: {kw} ไม่รับ property เพิ่ม (เจอ '{extra}')"),
            format!("line {line_no}: {kw} takes no extra properties (got '{extra}')"),
        ));
    }
    Ok(Declaration::Audio {
        is_music: kw == "music",
        handle,
        path,
        line: line_no,
    })
}

fn parse_shot_header(line: &str, line_no: usize, thai: bool) -> Result<Shot, CompileError> {
    let body = line.strip_suffix('{').map(str::trim).unwrap_or(line);
    let mut toks = body.split_whitespace();
    toks.next();
    let id = toks.next().map(str::to_string).ok_or_else(|| {
        CompileError::error(
            "E_PARSE",
            None,
            msg(
                thai,
                &format!("บรรทัด {line_no}: shot ต้องมี id (เช่น shot 1 (dialogue) {{)"),
                &format!("line {line_no}: shot needs an id (e.g. shot 1 (dialogue) {{)"),
            ),
        )
    })?;
    let shot_type = match toks.next() {
        None => None,
        Some(t) => Some(match t.trim_matches(&['(', ')'][..]) {
            "dialogue" => ShotType::Dialogue,
            "action" => ShotType::Action,
            "establishing" => ShotType::Establishing,
            "insert" => ShotType::Insert,
            other => {
                return Err(CompileError::error(
                    "E_PARSE",
                    Some(&id),
                    msg(
                        thai,
                        &format!("shot {id}: ชนิด '({other})' ไม่รู้จัก — ใช้ dialogue|action|establishing|insert"),
                        &format!("shot {id}: unknown type '({other})' — use dialogue|action|establishing|insert"),
                    ),
                ))
            }
        }),
    };
    if !line.trim_end().ends_with('{') {
        return Err(block_needs_brace(line_no, "shot", thai));
    }
    Ok(Shot {
        id,
        shot_type,
        lines: Vec::new(),
        line: line_no,
    })
}

/// `[t]` or `[mm:ss]` → seconds.
fn parse_ts(s: &str) -> Option<u32> {
    match s.split_once(':') {
        Some((m, sec)) => m
            .parse::<u32>()
            .ok()?
            .checked_mul(60)?
            .checked_add(sec.parse::<u32>().ok()?),
        None => s.parse().ok(),
    }
}

fn parse_shot_line(
    line: &str,
    line_no: usize,
    thai: bool,
    errors: &mut Vec<CompileError>,
) -> ShotLine {
    if let Some(rest) = line.strip_prefix('[') {
        if let Some((range, content)) = rest.split_once(']') {
            if let Some((a, b)) = range.split_once('-') {
                if let (Some(t1), Some(t2)) = (parse_ts(a.trim()), parse_ts(b.trim())) {
                    let inner = parse_shot_line(content.trim(), line_no, thai, errors);
                    return ShotLine::Beat {
                        t1,
                        t2,
                        content: Box::new(inner),
                        line: line_no,
                    };
                }
            }
            errors.push(CompileError::error(
                "E_PARSE",
                None,
                msg(
                    thai,
                    &format!("บรรทัด {line_no}: beat ต้องเป็น [t1-t2] (วินาที หรือ mm:ss)"),
                    &format!("line {line_no}: beats are [t1-t2] (seconds or mm:ss)"),
                ),
            ));
        }
    }

    if let Some(rest) = line.strip_prefix('@') {
        if let Some((key, value)) = split_key_value(rest) {
            if !DIRECTIVE_KEYS.contains(&key) {
                errors.push(CompileError::error(
                    "E_UNKNOWN_DIRECTIVE",
                    None,
                    msg(
                        thai,
                        &format!(
                            "บรรทัด {line_no}: @{key}: ไม่รู้จัก — รองรับ {}",
                            DIRECTIVE_KEYS.join("/")
                        ),
                        &format!(
                            "line {line_no}: unknown @{key}: — supported: {}",
                            DIRECTIVE_KEYS.join("/")
                        ),
                    ),
                ));
            }
            return ShotLine::Directive {
                key: key.to_string(),
                value: value.to_string(),
                line: line_no,
            };
        }
    }

    if let Some(after_sigil) = line.strip_prefix('$') {
        let (speaker, rest) = scan_handle(after_sigil);
        let rest_trim = rest.trim_start();
        if let Some(after_say) = rest_trim.strip_prefix("say") {
            // Word boundary: `$a says nothing` is prose, not dialogue.
            let boundary_ok = after_say.is_empty()
                || after_say.starts_with(char::is_whitespace)
                || after_say.starts_with('"');
            if boundary_ok {
                let after_say = after_say.trim_start();
                if let Some(q) = after_say.strip_prefix('"') {
                    let (text, trailing) = match q.find('"') {
                        Some(end) => (q[..end].to_string(), q[end + 1..].trim()),
                        None => {
                            errors.push(CompileError::error(
                                "E_PARSE",
                                None,
                                msg(
                                    thai,
                                    &format!("บรรทัด {line_no}: บทพูดไม่มีเครื่องหมายคำพูดปิด"),
                                    &format!(
                                        "line {line_no}: dialogue is missing its closing quote"
                                    ),
                                ),
                            ));
                            (q.to_string(), "")
                        }
                    };
                    return ShotLine::Dialogue {
                        speaker,
                        text,
                        trailing: (!trailing.is_empty()).then(|| trailing.to_string()),
                        line: line_no,
                    };
                }
                return ShotLine::Dialogue {
                    speaker,
                    text: String::new(),
                    trailing: None,
                    line: line_no,
                };
            }
        }
    }

    if let Some((key, value)) = split_key_value(line) {
        if PROPERTY_KEYS.contains(&key) {
            return ShotLine::Property {
                key: key.to_string(),
                value: value.to_string(),
                line: line_no,
            };
        }
    }

    ShotLine::Action {
        text: line.to_string(),
        refs: scan_refs(line),
        line: line_no,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILM: &str = r#"
film "เช้าวันจันทร์" {
    aspect: 16:9
    style: naturalistic Thai drama, warm muted palette
}

char  $แอน   = @./anny.png  voice:th-female-warm  desc:"หญิงสาวผมยาว"
scene $ห้อง   = @./room.png  time:day
view  $ห้อง#หน้าต่าง = @./room_window.png
music $เพลงเศร้า = @./sad_theme.mp3

sequence "บทสนทนา" {
    scene: $ห้อง
    music: $เพลงเศร้า volume:0.5

    shot 1 (dialogue) {
        $แอน นั่งที่โต๊ะ
        [00:00-00:03] wide static shot
        [00:03-00:06] camera: push-in to medium
        $แอน say "คุณหายไปไหนมาตั้งแต่เช้า"
        @duration: 6
        @transition: to_black 1.0
    }
}
"#;

    #[test]
    fn parses_film_level_constructs() {
        let (prog, errors) = parse(FILM);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(prog.header.title.as_deref(), Some("เช้าวันจันทร์"));
        assert_eq!(prog.header.aspect.as_deref(), Some("16:9"));
        assert_eq!(prog.declarations.len(), 4);
        assert!(matches!(
            &prog.declarations[2],
            Declaration::Variant { base, tag, is_view: true, .. }
                if base == "ห้อง" && tag == "หน้าต่าง"
        ));
        assert!(matches!(
            &prog.declarations[3],
            Declaration::Audio { is_music: true, handle, .. } if handle == "เพลงเศร้า"
        ));
        let seq = &prog.sequences[0];
        assert_eq!(seq.title.as_deref(), Some("บทสนทนา"));
        assert_eq!(seq.scene.as_deref(), Some("ห้อง"));
        assert_eq!(seq.music.as_ref().unwrap().volume, 0.5);
        let shot = &seq.shots[0];
        let beats: Vec<_> = shot
            .lines
            .iter()
            .filter_map(|l| match l {
                ShotLine::Beat { t1, t2, .. } => Some((*t1, *t2)),
                _ => None,
            })
            .collect();
        assert_eq!(beats, vec![(0, 3), (3, 6)]);
        assert!(matches!(
            &shot.lines[2],
            ShotLine::Beat { content, .. } if matches!(&**content, ShotLine::Property { key, .. } if key == "camera")
        ));
    }

    #[test]
    fn bare_shots_get_implicit_sequence() {
        let (prog, errors) = parse("shot 1 {\nsome action\n}\nshot 2 {\nmore\n}\n");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(prog.sequences.len(), 1);
        assert_eq!(prog.sequences[0].shots.len(), 2);
        assert!(prog.sequences[0].title.is_none());
    }

    #[test]
    fn unknown_directive_flagged_but_takes_parses() {
        let (prog, errors) = parse("shot 1 {\n@takes: 3\n@frobnicate: x\n}\n");
        assert_eq!(
            errors
                .iter()
                .filter(|e| e.code == "E_UNKNOWN_DIRECTIVE")
                .count(),
            1
        );
        assert_eq!(prog.sequences[0].shots[0].lines.len(), 2);
    }

    #[test]
    fn variant_via_char_keyword_is_guided() {
        let (_, errors) = parse("char $แอน#เปียก = @./x.png\n");
        assert!(errors
            .iter()
            .any(|e| e.code == "E_PARSE" && e.message.contains("variant")));
    }

    #[test]
    fn mmss_beats_parse() {
        let (prog, errors) = parse("shot 1 {\n[01:02-01:05] hold on her face\n}\n");
        assert!(errors.is_empty(), "{errors:?}");
        assert!(matches!(
            &prog.sequences[0].shots[0].lines[0],
            ShotLine::Beat { t1: 62, t2: 65, .. }
        ));
    }

    #[test]
    fn bom_is_stripped() {
        let (prog, errors) = parse("\u{feff}shot 1 {\nx\n}\n");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(prog.sequences[0].shots.len(), 1);
    }

    #[test]
    fn say_requires_word_boundary() {
        let (prog, errors) =
            parse("char $a = @./a.png\nshot 1 {\n$a says nothing and walks away\n}\n");
        assert!(errors.is_empty(), "{errors:?}");
        assert!(matches!(
            &prog.sequences[0].shots[0].lines[0],
            ShotLine::Action { .. }
        ));
    }

    #[test]
    fn unterminated_say_quote_errors() {
        let (_, errors) = parse("char $a = @./a.png\nshot 1 {\n$a say \"no close\n}\n");
        assert!(errors.iter().any(|e| e.code == "E_PARSE"), "{errors:?}");
    }

    #[test]
    fn decl_trailing_tokens_rejected() {
        let (_, e1) = parse("scene $s = @./s.png\nview $s#a = @./a.png time:dusk\n");
        assert!(e1.iter().any(|e| e.code == "E_PARSE"), "{e1:?}");
        let (_, e2) = parse("music $m = @./m.mp3 volume:0.5\n");
        assert!(e2.iter().any(|e| e.code == "E_PARSE"), "{e2:?}");
    }

    #[test]
    fn prose_with_colon_stays_prose() {
        let (prog, errors) = parse("shot 1 {\nnote: this is prose not a slot\n}\n");
        assert!(errors.is_empty(), "{errors:?}");
        assert!(matches!(
            &prog.sequences[0].shots[0].lines[0],
            ShotLine::Action { .. }
        ));
    }
}
