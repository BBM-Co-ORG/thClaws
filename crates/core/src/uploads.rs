//! File uploads from --serve mode (and, in a follow-up, the LINE
//! browser-chat surface).
//!
//! Two responsibilities: pick a non-colliding filename under the
//! workspace `_uploads/` dir, and synthesize a user-turn chat message
//! after one or more files land. The agent picks the synthetic message
//! up via the normal session input path, so project `AGENTS.md` /
//! `CLAUDE.md` instructions steer behavior (e.g. "when the user
//! uploads a PDF, summarize it into KMS").
//!
//! The 25 MB-per-file and 5-files-per-request caps live in
//! [`UPLOAD_MAX_BYTES`] and [`UPLOAD_MAX_FILES`] respectively. Both
//! are overridable via `settings.json: { "uploadMaxBytes": …,
//! "uploadMaxFiles": … }` resolved by the caller.
//!
//! ## Trust note
//!
//! Uploads originate from outside the desktop's local trust boundary
//! (a localhost browser, or via the relay from `chat.thclaws.ai`).
//! After save, the synthetic user turn enters the same input pipe as
//! a typed prompt — so the agent's existing approval gates govern any
//! mutating tool calls the model decides to make in response. There
//! is no additional gate at upload time.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub const UPLOAD_MAX_BYTES: u64 = 25 * 1024 * 1024;
pub const UPLOAD_MAX_FILES: usize = 5;
// Underscore prefix marks it as a purpose-specific workspace subfolder (user
// uploads / drag-drop), visible to the user and readable by the agent.
pub const UPLOADS_DIRNAME: &str = "_uploads";

/// Resolve a non-colliding destination path under `dir` for an upload
/// whose client-supplied filename is `filename`. Strategy:
///
/// 1. Sanitize the filename — strip any directory components and any
///    control / path-separator characters. Empty results fall back to
///    `"upload"`.
/// 2. Try `<dir>/<stem>.<ext>` first. If it exists, try
///    `<dir>/<stem>_1.<ext>`, `_2`, … until a free slot is found.
/// 3. Capped at 10_000 collision probes — defensive only, well above
///    any realistic upload-storm.
///
/// The returned path is **not** created on disk; the caller writes
/// bytes to it.
pub fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let sanitized = sanitize_filename(filename);
    let path = dir.join(&sanitized);
    if !path.exists() {
        return path;
    }
    let stem = Path::new(&sanitized)
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("upload");
    let ext = Path::new(&sanitized)
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or("");
    for n in 1..10_000 {
        let candidate = if ext.is_empty() {
            format!("{stem}_{n}")
        } else {
            format!("{stem}_{n}.{ext}")
        };
        let p = dir.join(&candidate);
        if !p.exists() {
            return p;
        }
    }
    // Astronomically unlikely; surface a clearly-broken filename so
    // the caller's write fails fast rather than silently overwriting.
    dir.join(format!("{stem}.collision-overflow"))
}

/// Sanitize a client-supplied filename for safe placement in the
/// uploads dir. Strips directory components (`a/b/../c.txt` →
/// `c.txt`), rejects control characters, and prevents reserved
/// names like `.` / `..` / empty from sneaking through.
fn sanitize_filename(raw: &str) -> String {
    // Take only the trailing component so `a/b/c.txt` and
    // `..\..\evil.txt` both collapse to `c.txt` / `evil.txt`.
    let trailing = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = trailing
        .chars()
        .filter(|c| !c.is_control() && *c != '\0')
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return "upload".to_string();
    }
    trimmed.to_string()
}

/// Ensure `<workspace>/_uploads/` exists and return the absolute path.
/// Idempotent; safe to call before every upload.
pub fn ensure_uploads_dir(workspace: &Path) -> std::io::Result<PathBuf> {
    let dir = workspace.join(UPLOADS_DIRNAME);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Ensure `<workspace>/<rel>/` exists and return the absolute path,
/// rejecting any `rel` that would escape the workspace. Used by the
/// upload endpoint's `?dir=` param so a shell can stage files into a
/// specific subfolder (e.g. `raw/`) instead of the default `_uploads/`.
/// `rel` must be a relative path with only normal components — no `..`,
/// no leading `/`, no drive/root — anything else returns an error
/// rather than writing outside the workspace.
pub fn ensure_target_dir(workspace: &Path, rel: &str) -> std::io::Result<PathBuf> {
    let rel = rel.trim().trim_matches('/');
    if rel.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty target dir",
        ));
    }
    let candidate = Path::new(rel);
    let unsafe_component = candidate
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)));
    if unsafe_component {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsafe upload dir: {rel}"),
        ));
    }
    let dir = workspace.join(candidate);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Render the user-turn text for a batch of uploaded files.
///
/// Shape:
///
/// ```text
/// [Uploaded 2 files via serve:
///   - _uploads/photo_3.jpg (image/jpeg, 1.2 MB)
///   - _uploads/notes.pdf (application/pdf, 240 KB)
/// ]
/// ```
///
/// `relative_paths` should already be expressed relative to the
/// workspace root (e.g. `_uploads/photo_3.jpg`) so the agent can
/// pass them straight to `Read` / `PdfRead` / `XlsxRead` without
/// translation.
pub fn render_upload_message(origin: &str, files: &[UploadedFile]) -> String {
    render_upload_message_with_hint(origin, files, None)
}

/// Variant of [`render_upload_message`] that threads through an
/// optional one-line `local_hint` produced by a browser-side
/// classifier (today: Gemma 4 E2B running in the chat SPA's worker
/// per dev-plan/15 Phase 1; tomorrow possibly a desktop-side
/// classifier).
///
/// The hint renders as an extra line in the bracketed block:
/// ```text
/// [Uploaded 1 file via line:
///   - _uploads/photo_3.jpg (image/jpeg, 1.2 MB)
///   Local classification: receipt, text-heavy, restaurant bill
///   Read the file and respond.]
/// ```
///
/// **Directive trailer.** The message ends with an explicit instruction
/// to read the file. Without it, the model often interprets the
/// informational header as "user mentioned an upload but didn't ask
/// anything" and replies with "what would you like me to do with it?"
/// — a frustrating UX for someone who just dropped a photo into the
/// chat (reported May 2026, fixed by adding this line). Project-level
/// `AGENT.md` / `CLAUDE.md` can override the default behaviour.
///
/// The hint and the directive are both treated by the agent as
/// **untrusted user-supplied text** — same authority as anything else
/// in the synthetic message. The hint is a head-start, not a
/// replacement for reading the file.
pub fn render_upload_message_with_hint(
    origin: &str,
    files: &[UploadedFile],
    local_hint: Option<&str>,
) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "[Uploaded {} file{} via {origin}:\n",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    );
    for f in files {
        out.push_str(&format!(
            "  - {} ({}, {})\n",
            f.relative_path,
            f.media_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
            format_bytes(f.size_bytes),
        ));
    }
    if let Some(hint) = local_hint.map(str::trim).filter(|h| !h.is_empty()) {
        out.push_str(&format!("  Local classification: {hint}\n"));
    }
    // Directive trailer — see doc-comment for the why. Phrasing is
    // singular/plural-aware so a single-file message reads naturally
    // and a multi-file batch doesn't get an awkward "Read it".
    let directive = if files.len() == 1 {
        "Read the file and respond."
    } else {
        "Read the files and respond."
    };
    out.push_str(&format!("  {directive}\n"));
    out.push(']');
    out
}

/// One saved upload — what the caller passes to
/// [`render_upload_message`].
#[derive(Debug, Clone)]
pub struct UploadedFile {
    pub relative_path: String,
    pub media_type: Option<String>,
    pub size_bytes: u64,
}

fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.0} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn ensure_target_dir_accepts_safe_subdir_rejects_escape() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        // Safe relative subdir → created under the workspace.
        let raw = ensure_target_dir(ws, "raw").unwrap();
        assert_eq!(raw, ws.join("raw"));
        assert!(raw.is_dir());
        // Nested safe path also fine.
        assert!(ensure_target_dir(ws, "a/b/c").unwrap().starts_with(ws));
        // Leading slash trimmed, still safe.
        assert_eq!(ensure_target_dir(ws, "/raw/").unwrap(), ws.join("raw"));
        // Escapes + absolute + empty are rejected.
        for bad in ["../escape", "raw/../../etc", "..", "  "] {
            assert!(ensure_target_dir(ws, bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn unique_path_returns_original_when_free() {
        let td = tempdir().unwrap();
        let p = unique_path(td.path(), "photo.jpg");
        assert_eq!(p, td.path().join("photo.jpg"));
    }

    #[test]
    fn unique_path_appends_underscore_n_on_collision() {
        let td = tempdir().unwrap();
        std::fs::write(td.path().join("photo.jpg"), b"a").unwrap();
        let p = unique_path(td.path(), "photo.jpg");
        assert_eq!(p, td.path().join("photo_1.jpg"));

        std::fs::write(td.path().join("photo_1.jpg"), b"b").unwrap();
        let p = unique_path(td.path(), "photo.jpg");
        assert_eq!(p, td.path().join("photo_2.jpg"));
    }

    #[test]
    fn unique_path_handles_extensionless_files() {
        let td = tempdir().unwrap();
        std::fs::write(td.path().join("README"), b"x").unwrap();
        let p = unique_path(td.path(), "README");
        assert_eq!(p, td.path().join("README_1"));
    }

    #[test]
    fn unique_path_strips_path_components() {
        let td = tempdir().unwrap();
        let p = unique_path(td.path(), "../../etc/passwd");
        assert_eq!(p, td.path().join("passwd"));

        let p = unique_path(td.path(), "a/b/c.txt");
        assert_eq!(p, td.path().join("c.txt"));
    }

    #[test]
    fn unique_path_strips_backslash_components_for_windows_uploads() {
        let td = tempdir().unwrap();
        let p = unique_path(td.path(), "..\\..\\evil.txt");
        assert_eq!(p, td.path().join("evil.txt"));
    }

    #[test]
    fn sanitize_filename_rejects_empty_and_dots() {
        assert_eq!(sanitize_filename(""), "upload");
        assert_eq!(sanitize_filename("."), "upload");
        assert_eq!(sanitize_filename(".."), "upload");
        assert_eq!(sanitize_filename("   "), "upload");
    }

    #[test]
    fn sanitize_filename_strips_control_characters() {
        let s = sanitize_filename("hello\x00world\n.txt");
        assert_eq!(s, "helloworld.txt");
    }

    #[test]
    fn render_upload_message_single_file() {
        let msg = render_upload_message(
            "serve",
            &[UploadedFile {
                relative_path: "uploads/photo.jpg".into(),
                media_type: Some("image/jpeg".into()),
                size_bytes: 1_300_000,
            }],
        );
        assert!(msg.starts_with("[Uploaded 1 file via serve:"));
        assert!(msg.contains("uploads/photo.jpg"));
        assert!(msg.contains("image/jpeg"));
        assert!(msg.contains("1.2 MB"));
        assert!(msg.ends_with(']'));
    }

    #[test]
    fn render_upload_message_pluralizes() {
        let msg = render_upload_message(
            "serve",
            &[
                UploadedFile {
                    relative_path: "uploads/a.txt".into(),
                    media_type: Some("text/plain".into()),
                    size_bytes: 500,
                },
                UploadedFile {
                    relative_path: "uploads/b.txt".into(),
                    media_type: None,
                    size_bytes: 2048,
                },
            ],
        );
        assert!(msg.starts_with("[Uploaded 2 files via serve:"));
        assert!(msg.contains("application/octet-stream"));
        assert!(msg.contains("500 B"));
        assert!(msg.contains("2 KB"));
    }

    #[test]
    fn render_upload_message_empty_files_returns_empty_string() {
        assert_eq!(render_upload_message("serve", &[]), "");
    }

    #[test]
    fn render_upload_message_with_hint_appends_classification_line() {
        let msg = render_upload_message_with_hint(
            "line",
            &[UploadedFile {
                relative_path: "uploads/photo.jpg".into(),
                media_type: Some("image/jpeg".into()),
                size_bytes: 1_300_000,
            }],
            Some("receipt, text-heavy, restaurant bill"),
        );
        assert!(msg.contains("uploads/photo.jpg"));
        assert!(msg.contains("Local classification: receipt, text-heavy, restaurant bill"));
        assert!(msg.ends_with(']'));
    }

    #[test]
    fn render_upload_message_appends_directive_singular() {
        let msg = render_upload_message(
            "line",
            &[UploadedFile {
                relative_path: "uploads/x.jpg".into(),
                media_type: Some("image/jpeg".into()),
                size_bytes: 7000,
            }],
        );
        assert!(
            msg.contains("Read the file and respond."),
            "missing singular directive: {msg:?}"
        );
        assert!(!msg.contains("Read the files"));
    }

    #[test]
    fn render_upload_message_appends_directive_plural() {
        let msg = render_upload_message(
            "serve",
            &[
                UploadedFile {
                    relative_path: "uploads/a.txt".into(),
                    media_type: None,
                    size_bytes: 100,
                },
                UploadedFile {
                    relative_path: "uploads/b.txt".into(),
                    media_type: None,
                    size_bytes: 200,
                },
            ],
        );
        assert!(
            msg.contains("Read the files and respond."),
            "missing plural directive: {msg:?}"
        );
    }

    #[test]
    fn render_upload_message_with_hint_skips_empty_hint() {
        let msg_none = render_upload_message_with_hint(
            "line",
            &[UploadedFile {
                relative_path: "uploads/a.bin".into(),
                media_type: None,
                size_bytes: 10,
            }],
            None,
        );
        let msg_blank = render_upload_message_with_hint(
            "line",
            &[UploadedFile {
                relative_path: "uploads/a.bin".into(),
                media_type: None,
                size_bytes: 10,
            }],
            Some("   "),
        );
        assert!(!msg_none.contains("Local classification"));
        assert!(!msg_blank.contains("Local classification"));
    }

    #[test]
    fn ensure_uploads_dir_creates_and_is_idempotent() {
        let td = tempdir().unwrap();
        let p1 = ensure_uploads_dir(td.path()).unwrap();
        assert!(p1.is_dir());
        assert_eq!(p1, td.path().join(UPLOADS_DIRNAME));
        let p2 = ensure_uploads_dir(td.path()).unwrap();
        assert_eq!(p1, p2);
    }
}
