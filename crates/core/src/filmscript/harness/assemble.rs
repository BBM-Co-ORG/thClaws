//! Assembly (dev-plan/52 Tier 4): clips → one film. Pure ffmpeg work —
//! the spec's hard boundary (§0/§2.8): optical transitions, music, SFX
//! and subtitles are deterministic post-production, never prompt work.
//!
//! v1 pipeline, per shot then once globally:
//! 1. normalize (24fps, 1280×720 pad-boxed, 44.1k stereo audio — every
//!    clip gets identical params so the concat demuxer can join them),
//! 2. continuation joins: frame-diff first — trim the duplicated
//!    boundary frame ONLY when it actually duplicates (T0: our run had
//!    no duplicate; blind-trim would eat a real frame),
//! 3. transition edges as fades (fade/dissolve/to_black render as
//!    fade-out[+fade-in]; true overlapped dissolve is a follow-up),
//! 4. concat → music bed (constant volume — sidechain ducking is
//!    deferred: under native sync the model places the speech, so duck
//!    spans would be guesses) + SFX at `shotStart + atSec`,
//! 5. SRT timestamped from FINAL clip durations, then rendered onto the film
//!    — soft-muxed as a toggle-able track by default, or burned into the
//!    picture on `subtitle_burn:` — plus manifest.json with every
//!    shot/task/credit/hash.

use super::super::phase1::{AssemblyPlan, SubtitleEntry};
use super::job::JobState;
use super::{atomic_write_json, ffprobe_duration_ms, job_dir, sha256_hex};
use crate::error::{Error, Result};
use serde_json::json;
use std::path::{Path, PathBuf};

const FPS: u32 = 24;

pub struct Assembled {
    pub mp4: PathBuf,
    pub srt: PathBuf,
    pub manifest: PathBuf,
}

fn run_ffmpeg(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-y", "-v", "error"])
        .args(args)
        .output()
        .map_err(|e| Error::Tool(format!("ffmpeg: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Tool(format!(
            "ffmpeg failed: {}",
            stderr.trim().lines().last().unwrap_or("(no stderr)")
        )));
    }
    Ok(())
}

/// Tiny grayscale thumbprint of one frame for the boundary-diff.
fn frame_gray(path: &Path, first: bool) -> Result<Vec<u8>> {
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    if !first {
        cmd.args(["-sseof", "-0.05"]);
    }
    cmd.arg("-i").arg(path);
    cmd.args([
        "-frames:v",
        "1",
        "-vf",
        "scale=32:18",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "gray",
        "-",
    ]);
    let out = cmd
        .output()
        .map_err(|e| Error::Tool(format!("ffmpeg frame: {e}")))?;
    if out.stdout.is_empty() {
        return Err(Error::Tool(format!(
            "no frame decoded from {}",
            path.display()
        )));
    }
    Ok(out.stdout)
}

/// True when the first frame of `cur` duplicates the last frame of
/// `prev` (mean abs pixel diff under a tight threshold).
fn boundary_duplicates(prev: &Path, cur: &Path) -> bool {
    let (Ok(a), Ok(b)) = (frame_gray(prev, false), frame_gray(cur, true)) else {
        return false;
    };
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    let diff: u64 = a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y) as u64).sum();
    (diff as f64 / a.len() as f64) < 3.0
}

/// Render captions onto `<out>/final.mp4`. Default: **soft-mux** the SRT as a
/// toggle-able `mov_text` track (no re-encode, no font dependency, universal).
/// `burn`: hardcode styled captions into the pixels (Thai social delivery) —
/// re-encodes with a Thai-capable font via fontconfig, and falls back to the
/// soft-mux on any failure (missing font, etc.). No-op when there are no
/// caption entries. Runs from `out_dir` so filenames stay bare — the
/// `subtitles` filter's path escaping is notoriously brittle otherwise.
/// Returns the mode actually applied.
fn render_subtitles(out_dir: &Path, burn: bool, have_entries: bool) -> Result<&'static str> {
    if !have_entries {
        return Ok("none");
    }
    let ffmpeg_in = |args: &[&str]| -> Result<()> {
        let out = std::process::Command::new("ffmpeg")
            .args(["-y", "-v", "error"])
            .args(args)
            .current_dir(out_dir)
            .output()
            .map_err(|e| Error::Tool(format!("ffmpeg: {e}")))?;
        if !out.status.success() {
            return Err(Error::Tool(format!(
                "ffmpeg subtitles: {}",
                String::from_utf8_lossy(&out.stderr)
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("")
            )));
        }
        Ok(())
    };
    let tmp = "final.subbed.mp4";
    if burn {
        let style = "FontName=Sarabun,FontSize=20,PrimaryColour=&H00FFFFFF,\
                     OutlineColour=&H00000000,BorderStyle=1,Outline=2,Shadow=1,MarginV=36";
        let vf = format!("subtitles=filename='final.srt':force_style='{style}'");
        if ffmpeg_in(&[
            "-i",
            "final.mp4",
            "-vf",
            &vf,
            "-c:a",
            "copy",
            "-c:v",
            "libx264",
            "-crf",
            "18",
            "-preset",
            "fast",
            tmp,
        ])
        .is_ok()
        {
            std::fs::rename(out_dir.join(tmp), out_dir.join("final.mp4"))?;
            return Ok("burn");
        }
        // fall through to the always-reliable soft-mux
    }
    ffmpeg_in(&[
        "-i",
        "final.mp4",
        "-i",
        "final.srt",
        "-map",
        "0",
        "-map",
        "1",
        "-c",
        "copy",
        "-c:s",
        "mov_text",
        "-metadata:s:s:0",
        "language=tha",
        "-disposition:s:0",
        "default",
        tmp,
    ])?;
    std::fs::rename(out_dir.join(tmp), out_dir.join("final.mp4"))?;
    Ok(if burn {
        "embed (burn font unavailable)"
    } else {
        "embed"
    })
}

fn srt_ts(ms: u64) -> String {
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000
    )
}

fn write_srt(path: &Path, entries: &[(u64, u64, &SubtitleEntry)]) -> Result<()> {
    let mut out = String::new();
    for (i, (start, end, e)) in entries.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            srt_ts(*start),
            srt_ts(*end),
            e.text
        ));
    }
    std::fs::write(path, out)?;
    Ok(())
}

/// Assemble the film for a completed job. `state` supplies per-shot
/// clips + bookkeeping; `plan` supplies order/transitions/music/sfx/
/// subtitles. Returns the three artifacts under `<job>/out/`.
pub fn assemble(job_id: &str, plan: &AssemblyPlan, state: &JobState) -> Result<Assembled> {
    let dir = job_dir(job_id);
    let out_dir = dir.join("out");
    let work = out_dir.join("assembly");
    std::fs::create_dir_all(&work)?;

    let clip_of = |shot_id: &str| -> Result<PathBuf> {
        state
            .shots
            .iter()
            .find(|s| s.id == shot_id)
            .and_then(|s| s.clip.as_ref())
            .map(|rel| dir.join(rel))
            .ok_or_else(|| Error::Tool(format!("shot {shot_id} has no clip to assemble")))
    };

    // 1+2+3: normalize each clip, trimming duplicated boundary frames
    // and rendering transition edges as fades.
    let mut normalized: Vec<(String, PathBuf, u64)> = Vec::new();
    for (idx, shot_id) in plan.order.iter().enumerate() {
        let src = clip_of(shot_id)?;
        let dst = work.join(format!("{idx:03}-{shot_id}.mp4"));
        let src_ms = ffprobe_duration_ms(&src)?;

        let mut vf = vec![format!(
            "scale=1280:720:force_original_aspect_ratio=decrease,pad=1280:720:(ow-iw)/2:(oh-ih)/2,fps={FPS}"
        )];
        let mut af: Vec<String> = vec!["aresample=44100".into()];
        let mut trim_head = false;

        if idx > 0 {
            let prev = clip_of(&plan.order[idx - 1])?;
            let continuation = state
                .shots
                .iter()
                .any(|s| s.id == *shot_id && s.state == "done")
                && boundary_duplicates(&prev, &src);
            if continuation {
                trim_head = true;
            }
            // Fade-in when the PREVIOUS shot fades/dissolves into us.
            if let Some(t) = plan
                .transitions
                .iter()
                .find(|t| t.after_shot == plan.order[idx - 1])
            {
                if t.transition != "to_black" {
                    vf.push(format!("fade=t=in:st=0:d={}", t.sec));
                    af.push(format!("afade=t=in:st=0:d={}", t.sec));
                }
            }
        }
        if trim_head {
            vf.insert(0, "trim=start_frame=1,setpts=PTS-STARTPTS".to_string());
        }
        if let Some(t) = plan.transitions.iter().find(|t| t.after_shot == *shot_id) {
            let start = (src_ms as f64 / 1000.0 - t.sec as f64).max(0.0);
            vf.push(format!("fade=t=out:st={start:.3}:d={}", t.sec));
            af.push(format!("afade=t=out:st={start:.3}:d={}", t.sec));
        }

        let vf_s = vf.join(",");
        let af_s = af.join(",");
        let src_s = src.to_string_lossy().to_string();
        let dst_s = dst.to_string_lossy().to_string();
        // Clips may carry no audio track; synthesize silence so every
        // normalized clip has identical stream layout for concat.
        run_ffmpeg(&[
            "-i",
            &src_s,
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=stereo",
            "-filter_complex",
            &format!("[0:v]{vf_s}[v];[0:a][1:a]amix=inputs=2:duration=first,{af_s}[a]"),
            "-map",
            "[v]",
            "-map",
            "[a]",
            "-c:v",
            "libx264",
            "-preset",
            "fast",
            "-crf",
            "18",
            "-c:a",
            "aac",
            "-shortest",
            &dst_s,
        ])
        .or_else(|_| {
            // No source audio stream at all → video + silence.
            run_ffmpeg(&[
                "-i",
                &src_s,
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=44100:cl=stereo",
                "-filter_complex",
                &format!("[0:v]{vf_s}[v];[1:a]{af_s}[a]"),
                "-map",
                "[v]",
                "-map",
                "[a]",
                "-c:v",
                "libx264",
                "-preset",
                "fast",
                "-crf",
                "18",
                "-c:a",
                "aac",
                "-shortest",
                &dst_s,
            ])
        })?;
        let ms = ffprobe_duration_ms(&dst)?;
        normalized.push((shot_id.clone(), dst, ms));
    }

    // 4a: concat (identical params → demuxer, no re-encode).
    let list = work.join("concat.txt");
    let mut listing = String::new();
    for (_, p, _) in &normalized {
        listing.push_str(&format!(
            "file '{}'\n",
            p.file_name().unwrap().to_string_lossy()
        ));
    }
    std::fs::write(&list, listing)?;
    let joined = work.join("joined.mp4");
    run_ffmpeg(&[
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        &list.to_string_lossy(),
        "-c",
        "copy",
        &joined.to_string_lossy(),
    ])?;

    // Shot start offsets in the FINAL timeline.
    let mut offsets: Vec<(String, u64, u64)> = Vec::new();
    let mut cursor = 0u64;
    for (id, _, ms) in &normalized {
        offsets.push((id.clone(), cursor, *ms));
        cursor += ms;
    }

    // 4b: music bed + SFX in one mix pass.
    let final_mp4 = out_dir.join("final.mp4");
    let mut audio_inputs: Vec<(String, String)> = Vec::new(); // (path, filter)
    for span in &plan.music {
        let from = offsets.iter().find(|(id, ..)| id == &span.from_shot);
        let to = offsets.iter().find(|(id, ..)| id == &span.to_shot);
        if let (Some((_, start, _)), Some((_, to_start, to_ms))) = (from, to) {
            let span_ms = to_start + to_ms - start;
            audio_inputs.push((
                span.path.clone(),
                format!(
                    "volume={},atrim=duration={:.3},adelay={start}|{start}",
                    span.volume,
                    span_ms as f64 / 1000.0
                ),
            ));
        }
    }
    for sfx in &plan.sfx {
        if let Some((_, start, _)) = offsets.iter().find(|(id, ..)| id == &sfx.shot_id) {
            let at = start + (sfx.at_sec.max(0.0) * 1000.0) as u64;
            audio_inputs.push((sfx.path.clone(), format!("adelay={at}|{at}")));
        }
    }

    if audio_inputs.is_empty() {
        std::fs::copy(&joined, &final_mp4)?;
    } else {
        let mut args: Vec<String> = vec!["-i".into(), joined.to_string_lossy().to_string()];
        for (p, _) in &audio_inputs {
            args.push("-i".into());
            args.push(p.clone());
        }
        let mut fc = String::new();
        let mut labels = vec!["[0:a]".to_string()];
        for (i, (_, filter)) in audio_inputs.iter().enumerate() {
            fc.push_str(&format!("[{}:a]{filter}[m{i}];", i + 1));
            labels.push(format!("[m{i}]"));
        }
        fc.push_str(&format!(
            "{}amix=inputs={}:duration=first:normalize=0[a]",
            labels.join(""),
            labels.len()
        ));
        args.extend([
            "-filter_complex".into(),
            fc,
            "-map".into(),
            "0:v".into(),
            "-map".into(),
            "[a]".into(),
            "-c:v".into(),
            "copy".into(),
            "-c:a".into(),
            "aac".into(),
            final_mp4.to_string_lossy().to_string(),
        ]);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_ffmpeg(&arg_refs)?;
    }

    // 5: SRT from final durations + manifest.
    let srt_path = out_dir.join("final.srt");
    let entries: Vec<(u64, u64, &SubtitleEntry)> = plan
        .subtitles
        .iter()
        .filter_map(|e| {
            offsets
                .iter()
                .find(|(id, ..)| id == &e.shot_id)
                .map(|(_, start, ms)| (*start + 200, start + ms - 100, e))
        })
        .collect();
    write_srt(&srt_path, &entries)?;

    // Render captions onto the film (soft-mux track by default, burned when
    // the film asks). The .srt sidecar above stays regardless.
    let sub_mode = render_subtitles(&out_dir, plan.subtitle_burn, !entries.is_empty())?;

    let manifest_path = out_dir.join("manifest.json");
    let shots: Vec<serde_json::Value> = plan
        .order
        .iter()
        .filter_map(|id| state.shots.iter().find(|s| &s.id == id))
        .map(|s| {
            let hash = s
                .clip
                .as_ref()
                .and_then(|rel| std::fs::read(dir.join(rel)).ok())
                .map(|b| sha256_hex(&b));
            json!({
                "id": s.id, "task_id": s.task_id, "clip": s.clip,
                "credits": s.credits, "sha256": hash,
            })
        })
        .collect();
    atomic_write_json(
        &manifest_path,
        &json!({
            "job_id": job_id,
            "spent_credits": state.spent_credits,
            "estimate_usd": state.estimate_usd,
            "shots": shots,
            "final_duration_ms": cursor,
            "subtitles": { "count": entries.len(), "mode": sub_mode },
            "warnings": state.warnings,
        }),
    )?;

    Ok(Assembled {
        mp4: final_mp4,
        srt: srt_path,
        manifest: manifest_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_timestamp_format() {
        assert_eq!(srt_ts(0), "00:00:00,000");
        assert_eq!(srt_ts(61_234), "00:01:01,234");
        assert_eq!(srt_ts(3_723_045), "01:02:03,045");
    }
}
