//! `PdfRead` — extract text from a PDF by shelling out to `pdftotext`
//! (poppler-utils). poppler does the heavy lifting (Thai shaping, ligature
//! decomposition, layout-aware extraction); we just wrap it with sandbox
//! checks, page-range parsing, and a clear missing-binary error.
//!
//! When the PDF is a scanned image (no embedded text layer), pdftotext
//! returns empty / mostly-empty output. In that case the multimodal
//! entry point falls through to `pdftoppm` which renders each requested
//! page as a PNG and returns them as image blocks so the model sees the
//! pages visually. Both binaries ship together in poppler-utils, so the
//! fallback adds no new install requirement.
//!
//! Thai gets two extra layers. `-layout` sprinkles spurious spaces inside
//! Thai words (the script has no word boundaries, so every glyph gap reads
//! as a space); `normalize_thai_spacing` repairs that with script-level
//! rules — no per-document word lists. And when a PDF's font carries a
//! broken `ToUnicode` map, pdftotext emits genuinely wrong characters that
//! no post-processing can recover; `thai_looks_garbled` detects the heavy
//! cluster fragmentation that comes with it and routes those pages to the
//! vision path, which reads the rendered glyphs instead of the bad map.
//!
//! Why shell-out instead of a pure-Rust pdf crate: extraction quality
//! across real-world PDFs (tagged structure, form fields, embedded fonts
//! with non-standard cmaps) is dominated by poppler's twenty-plus years
//! of corner-case handling. The Rust crates that exist are good for
//! valid PDFs but break on the long tail. Measured against ground truth
//! (`docs/pdf-extraction-bench.md`): poppler 98.3% / 97.8% Thai 5-gram
//! recall / precision, PDFium 89.9% / 80.5% — and PDFium's damage is a
//! duplicated Thai cluster every ~28 characters that `thai_looks_garbled`
//! does NOT catch, so it would degrade Thai silently.
//!
//! The cost of that choice is that poppler has to be installed, which a
//! desktop user has not done and a Windows user has no instructions for.
//! So when `pdftotext` is missing, extraction falls back to the public
//! `pdf.thclaws.cloud` service — no account, no key (dev-plan/66). The
//! file leaves the machine, so that path is approval-gated, names the
//! destination in the prompt, marks its output, and is switched off by
//! `"pdfCloudFallback": false` or `THCLAWS_PDF_CLOUD=0`. It is a stopgap
//! for the install problem, not a change of extractor: when poppler is
//! present nothing here reaches the network.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const EXTRACT_TIMEOUT: Duration = Duration::from_secs(60);

/// Vision-OCR fallback constants. Tuned conservatively — the goal is
/// "make scanned PDFs work at all", not "perfectly handle 100-page
/// scans". Users with bigger documents can paginate via the `pages`
/// parameter.
mod fallback {
    use super::Duration;
    /// If pdftotext returns less than this many non-whitespace chars
    /// per requested page on average, treat the PDF as scanned and
    /// fall through to vision OCR. 50 chars ≈ a one-line title;
    /// anything thinner is almost certainly a scanned image.
    pub const MIN_CHARS_PER_PAGE: usize = 50;
    /// Hard cap on pages to render. Twenty 150-DPI A4 PNGs at typical
    /// content density land around 8-15 MB total before base64 — fits
    /// comfortably under most providers' per-request limits.
    pub const MAX_PAGES_TO_RENDER: u32 = 20;
    /// Render resolution. 150 DPI is the sweet spot for OCR quality
    /// vs file size; below that fine print drops out of the model's
    /// recognition; above that the size grows quadratically with
    /// minimal accuracy gain.
    pub const RENDER_DPI: u32 = 150;
    /// Per-page byte cap. Anthropic's documented per-image limit is
    /// 5 MB; matches the Read tool's MAX_IMAGE_BYTES.
    pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
    /// Render-step timeout. pdftoppm at 150 DPI on a 20-page PDF is
    /// usually ~5s on a modern machine, but spinning rust + complex
    /// fonts can push past 30s.
    pub const RENDER_TIMEOUT: Duration = Duration::from_secs(120);

    /// Thai garble detection. A correctly-extracted Thai text layer
    /// orphans essentially no combining marks behind a space; a PDF whose
    /// font has a broken `ToUnicode` map makes pdftotext fragment clusters
    /// and leaves many. Above BOTH thresholds we treat the text layer as
    /// untrustworthy and prefer the vision path (which reads the rendered
    /// glyphs, sidestepping the bad mapping). `MIN_THAI…` keeps short
    /// snippets from tripping the ratio; AND-ing the two avoids firing on
    /// a long clean doc with a stray fragment.
    pub const MIN_THAI_FOR_GARBLE_CHECK: usize = 40;
    pub const GARBLE_ORPHAN_MARKS: usize = 6;
    pub const GARBLE_ORPHAN_RATIO: f32 = 0.04;

    /// Below this many Thai characters there is not enough text to judge how
    /// the extractor is treating the script.
    pub const MIN_THAI_FOR_MODE_CHOICE: usize = 200;
    /// "Mark, space, consonant" per 1000 Thai characters, above which
    /// `-layout` is breaking words rather than preserving columns. Measured
    /// over 24 Thai PDFs: the documents above it gain 1.33x-3.70x average Thai
    /// run length from `-raw`, the ones at 2.6 and below gain exactly 1.00x,
    /// and budget tables sit at 0.0-0.8 so their columns are never traded away.
    pub const SHRED_PER_1K_RETRY: f32 = 10.0;
    /// Mean length of a contiguous Thai run, below which the text layer holds
    /// no words at all — every character stands alone. One PDF in that corpus
    /// reads 1.0 in both modes, and no spacing rule can help it; it belongs on
    /// the vision path.
    pub const MIN_THAI_RUN: f32 = 2.0;
}

pub struct PdfReadTool;

#[async_trait]
impl Tool for PdfReadTool {
    fn name(&self) -> &'static str {
        "PdfRead"
    }

    fn description(&self) -> &'static str {
        "Extract text from a PDF file. Uses `pdftotext` from poppler-utils. \
         Optional `pages` parameter accepts \"all\" (default), \"3\" \
         (single page), or \"1-5\" (inclusive range). Returns extracted \
         text. **Scanned / image-based PDFs** (no embedded text layer) \
         fall through to a vision-OCR path that renders each requested \
         page as PNG via `pdftoppm` so the model sees the pages directly \
         — no separate OCR step needed. poppler-utils gives the best results \
         and keeps the file local (`brew install poppler` on macOS, \
         `apt install poppler-utils` on Debian/Ubuntu); without it, text \
         extraction falls back to a public thClaws service that needs no \
         account — the user approves that upload, and the scanned-PDF vision \
         path is unavailable until poppler is installed."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":  {"type": "string", "description": "PDF file path."},
                "pages": {"type": "string", "description": "Page range: \"all\", \"N\", or \"M-N\". Default: all."},
                "vision": {"type": "boolean", "description": "Force the vision-OCR path (render pages to images for the model to read) instead of text extraction. Use when the text layer is garbled — e.g. a PDF whose font has a broken Thai ToUnicode map that swaps า/ำ. Default false."}
            },
            "required": ["path"]
        })
    }

    /// Reading a local file needs no approval — but with no poppler the file
    /// is about to be uploaded to the public extraction service, and that
    /// does (dev-plan/66). Nothing else about the call changes, so the gate
    /// tracks exactly the condition that sends bytes off the machine.
    fn requires_approval(&self, _input: &Value) -> bool {
        poppler_missing() && cloud_endpoint().is_some()
    }

    fn approval_summary(&self, input: &Value) -> Option<String> {
        if !self.requires_approval(input) {
            return None;
        }
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("?");
        let url = cloud_endpoint()?;
        Some(format!(
            "no poppler on this machine — UPLOADS {path} to {url} to extract its text"
        ))
    }

    async fn call(&self, input: Value) -> Result<String> {
        let validated = crate::sandbox::Sandbox::check(req_str(&input, "path")?)?;
        let pages_spec = input.get("pages").and_then(|v| v.as_str()).unwrap_or("all");
        let (first, last) = parse_page_range(pages_spec)?;
        extract_text(&validated, first, last).await
    }

    /// Multimodal entry: text-first, vision-OCR fallback for scanned PDFs.
    /// When the model invokes PdfRead via the agent loop (not a direct
    /// `call`), this path runs. If pdftotext returns enough text for
    /// the requested pages it returns a Text block as before. If the
    /// text is empty / sparse (typical scanned PDF) it renders each
    /// page to PNG and returns an Image block per page plus a summary
    /// Text block so the model has both the visual and a textual
    /// handle on what it saw.
    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        let validated = crate::sandbox::Sandbox::check(req_str(&input, "path")?)?;
        let pages_spec = input.get("pages").and_then(|v| v.as_str()).unwrap_or("all");
        let (first, last) = parse_page_range(pages_spec)?;

        // Caller forced vision (text layer known-garbled, e.g. broken Thai
        // ToUnicode): skip extraction entirely and read the rendered glyphs.
        if input
            .get("vision")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return render_pages_as_image_blocks(&validated, first, last)
                .await
                .map(ToolResultContent::Blocks);
        }

        let raw = extract_text_raw(&validated, first, last, CloudFallback::Allowed).await?;

        // Take the text layer when it's both present (not scanned) and
        // trustworthy (not a garbled Thai font). "Looks scanned" splits on
        // form-feed and averages chars per page; "looks garbled" counts
        // Thai combining marks orphaned behind spaces. Either one routes to
        // vision-OCR, which reads the rendered glyphs directly.
        if !text_is_too_sparse(&raw) && !thai_looks_garbled(&raw) {
            let mut text = normalize_thai_spacing(&raw);
            if poppler_missing() {
                text.push_str(CLOUD_NOTE);
            }
            return Ok(ToolResultContent::Text(text));
        }

        // The text layer is unusable — but rendering needs `pdftoppm`, which
        // is the same package as `pdftotext`. With no poppler at all there is
        // no vision path to fall through TO (the service returns text only),
        // so say that rather than failing on a missing binary the user was
        // never told about.
        if poppler_missing() {
            return Ok(ToolResultContent::Text(format!(
                "{}\n\n[this PDF's text layer is empty or unreliable — it is probably a \
                 scan. Reading it needs the vision path, which renders pages with \
                 `pdftoppm`: install poppler-utils (`brew install poppler`, \
                 `apt install poppler-utils`, `winget install oschwartz10612.Poppler`). \
                 The no-install extraction service returns text only.]",
                normalize_thai_spacing(&raw)
            )));
        }

        // Fall through to vision-OCR (scanned, or a text layer too garbled
        // to trust).
        render_pages_as_image_blocks(&validated, first, last)
            .await
            .map(|blocks| ToolResultContent::Blocks(blocks))
    }
}

/// True when the extracted text is so thin it almost certainly came
/// from a PDF without an embedded text layer (scanned image). Splits
/// on form-feed (pdftotext's page boundary marker) and checks the
/// average non-whitespace char count per page against
/// `fallback::MIN_CHARS_PER_PAGE`. A single-page PDF with a one-line
/// title (~30 chars) trips the threshold; that's intentional — vision
/// OCR adds little overhead for a single page and meaningfully
/// improves quality on covers / title slides / posters.
fn text_is_too_sparse(text: &str) -> bool {
    let pages: Vec<&str> = text.split('\u{000C}').collect();
    let page_count = pages.len().max(1);
    let total_meaningful: usize = text.chars().filter(|c| !c.is_whitespace()).count();
    let avg = total_meaningful / page_count;
    avg < fallback::MIN_CHARS_PER_PAGE
}

/// True for Thai vowels/tone marks that must attach to a preceding base
/// character — they can never legitimately start a cluster, so a space
/// in front of one is always a pdftotext fragmentation artifact.
/// Covers U+0E30–U+0E3A (sara/phinthu) and U+0E47–U+0E4E (tone marks +
/// thanthakhat + nikhahit + yamakkan).
fn is_thai_trailing_mark(c: char) -> bool {
    matches!(c, '\u{0E30}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}')
}

/// "Mark, space, consonant" per 1000 Thai characters — the shape `-layout`
/// leaves when it pads a glyph gap inside a Thai word.
fn shred_per_1k(text: &str) -> f32 {
    let chars: Vec<char> = text.chars().collect();
    let thai = chars.iter().filter(|c| is_thai(**c)).count();
    if thai == 0 {
        return 0.0;
    }
    let breaks = chars
        .windows(3)
        .filter(|w| w[1] == ' ' && is_thai_trailing_mark(w[0]) && is_thai_consonant(w[2]))
        .count();
    1000.0 * breaks as f32 / thai as f32
}

/// Mean length of a contiguous run of Thai characters. Thai writes without
/// spaces between words, so a healthy run is many characters long; extraction
/// damage is what cuts it down. No dictionary needed, which matters — the
/// bundled word list is a 229-word placeholder.
fn mean_thai_run(text: &str) -> f32 {
    let (mut runs, mut total, mut cur) = (0usize, 0usize, 0usize);
    for c in text.chars() {
        if is_thai(c) {
            cur += 1;
        } else if cur > 0 {
            runs += 1;
            total += cur;
            cur = 0;
        }
    }
    if cur > 0 {
        runs += 1;
        total += cur;
    }
    if runs == 0 {
        0.0
    } else {
        total as f32 / runs as f32
    }
}

/// Is `-layout` breaking this Thai text apart rather than laying it out?
fn thai_is_shredded(text: &str) -> bool {
    let thai = text.chars().filter(|c| is_thai(*c)).count();
    thai >= fallback::MIN_THAI_FOR_MODE_CHOICE && shred_per_1k(text) > fallback::SHRED_PER_1K_RETRY
}

/// Thai consonants — the class a stray `-layout` space lands in front of.
fn is_thai_consonant(c: char) -> bool {
    ('\u{0E01}'..='\u{0E2E}').contains(&c)
}

/// Any character in the Thai block.
fn is_thai(c: char) -> bool {
    ('\u{0E01}'..='\u{0E5B}').contains(&c)
}

/// Repair Thai clusters that `pdftotext -layout` fragmented by orphaning a
/// combining mark behind a space (e.g. "ผู ้" → "ผู้", "ก ำหนด" → "กำหนด").
/// A Thai vowel/tone mark can never legitimately follow a space — it must
/// attach to the preceding base — so this only ever undoes fragmentation;
/// it can NEVER merge two real words. That safety is deliberate: the
/// normalizer also runs on clean Thai that uses real phrase spaces, and
/// many Thai words end in a vowel, so a rule that also stripped
/// "mark + space + consonant" would wrongly glue "เวลา ทำงาน" into one
/// token. The harder consonant-after-space fragmentation is
/// indistinguishable from a real word break without a segmentation
/// dictionary, so heavily-fragmented documents route to the vision path
/// via `thai_looks_garbled` instead of being force-joined here. This is a
/// script-level rule, not a vocabulary list, so it generalizes to any Thai
/// document; non-Thai text is untouched (the pattern needs Thai on the
/// left and a Thai mark on the right).
pub(crate) fn normalize_thai_spacing(text: &str) -> String {
    use regex::Regex;
    let drop_before_mark =
        Regex::new(r"([\u{0E01}-\u{0E4E}]) +([\u{0E30}-\u{0E3A}\u{0E47}-\u{0E4E}])").unwrap();
    let mut s = text.to_string();
    // Two passes close stacked clusters like "ษ ั ้" that one
    // left-to-right replace_all can't fully collapse.
    for _ in 0..2 {
        s = drop_before_mark.replace_all(&s, "$1$2").into_owned();
    }
    repair_broken_saraam(&s)
}

/// Repair a known broken-`ToUnicode` corruption: some PDF fonts map sara-am
/// (ำ, U+0E33) to bare sara-aa (า, U+0E32), turning e.g. "ทำงาน" into the
/// non-word "ทางาน". Unlike the spacing rule this is a guess (า is a real,
/// extremely common letter), so the table is deliberately limited to whole
/// forms that are NOT valid Thai in their า spelling — real า words (ด้าน,
/// หน้า, ภาพ, เป้า, ทางการ, ประจาน) are never listed, so the substring replace
/// can't damage them. Substring-based, not token-based, because Thai writes
/// without inter-word spaces (the corrupted form appears glued inside a run,
/// e.g. "การทางานของ"). Only covers the high-frequency words; broken-cmap PDFs
/// outside this set still need the vision-OCR path for perfect ำ.
const SARAAM_REPAIRS: &[(&str, &str)] = &[
    ("ทางาน", "ทำงาน"),
    ("ทาความ", "ทำความ"),
    ("จากัด", "จำกัด"),
    ("จานวน", "จำนวน"),
    ("จาเป็น", "จำเป็น"),
    ("จาพวก", "จำพวก"),
    ("กาหนด", "กำหนด"),
    ("กาลัง", "กำลัง"),
    ("กาไร", "กำไร"),
    ("กากับ", "กำกับ"),
    ("สาคัญ", "สำคัญ"),
    ("สาเร็จ", "สำเร็จ"),
    ("สาหรับ", "สำหรับ"),
    ("สานัก", "สำนัก"),
    ("สารวจ", "สำรวจ"),
    ("สาเนา", "สำเนา"),
    ("ดาเนิน", "ดำเนิน"),
    ("ดารง", "ดำรง"),
    ("คานวณ", "คำนวณ"),
    ("คาสั่ง", "คำสั่ง"),
    ("ชาระ", "ชำระ"),
    ("นาเสนอ", "นำเสนอ"),
    ("ลาดับ", "ลำดับ"),
    ("ตาแหน่ง", "ตำแหน่ง"),
    ("อานวย", "อำนวย"),
    ("บารุง", "บำรุง"),
];

fn repair_broken_saraam(text: &str) -> String {
    let mut s = text.to_string();
    for (bad, good) in SARAAM_REPAIRS {
        if s.contains(bad) {
            s = s.replace(bad, good);
        }
    }
    s
}

/// True when extracted Thai shows heavy cluster fragmentation — many
/// combining marks orphaned behind a space, which a correct text layer
/// never produces. It signals a broken font / `ToUnicode` map whose
/// character mapping (not just spacing) can't be trusted, so the caller
/// should prefer the vision path over the text. Computed on the *raw*
/// pdftotext output, before `normalize_thai_spacing` hides the evidence.
fn thai_looks_garbled(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let thai_total = chars.iter().filter(|c| is_thai(**c)).count();
    if thai_total < fallback::MIN_THAI_FOR_GARBLE_CHECK {
        return false;
    }
    let orphan_marks = chars
        .windows(3)
        .filter(|w| w[1] == ' ' && is_thai(w[0]) && is_thai_trailing_mark(w[2]))
        .count();
    if orphan_marks >= fallback::GARBLE_ORPHAN_MARKS
        && (orphan_marks as f32) / (thai_total as f32) > fallback::GARBLE_ORPHAN_RATIO
    {
        return true;
    }
    // Second signal: a text layer that holds no words. Thai runs together, so
    // a mean run near one character means every character came out isolated —
    // no spacing rule can reassemble that, and one PDF in the survey corpus
    // reads 1.0 in BOTH extraction modes while orphan marks stay at zero, so
    // the check above never saw it.
    mean_thai_run(text) < fallback::MIN_THAI_RUN
}

/// Text-first extraction with Thai post-processing applied. Used by the
/// direct `call` path. The multimodal path calls `extract_text_raw`
/// itself so its garble check can see the unrepaired output, then
/// normalizes only when it keeps the text.
/// Extracted text with the Thai marks put back together.
///
/// `pub(crate)` so callers outside the tool can ask for a whole
/// document with `(path, None, None)`. `kms::ingest_pdf` grew its own
/// `pdftotext` call instead and did not carry the normalisation with
/// it, which is how an ingested Thai paper kept the vowel/tone
/// fragmentation `-layout` introduces while the same PDF read through
/// this tool came out clean.
pub(crate) async fn extract_text(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
) -> Result<String> {
    extract_text_inner(validated, first, last, CloudFallback::Allowed).await
}

/// Same, but the public service is never used — for callers that cannot ask
/// the user first. The Files tab's one-click "Convert to markdown" runs as an
/// IPC arm, which does not pass through `Tool::requires_approval`, so it must
/// not be able to put a file on the network (dev-plan/66).
pub(crate) async fn extract_text_local(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
) -> Result<String> {
    extract_text_inner(validated, first, last, CloudFallback::Refused).await
}

async fn extract_text_inner(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    cloud: CloudFallback,
) -> Result<String> {
    let raw = extract_text_raw(validated, first, last, cloud).await?;
    let mut out = normalize_thai_spacing(&raw);
    if poppler_missing() {
        out.push_str(CLOUD_NOTE);
    }
    Ok(out)
}

// ── No-poppler fallback: the public extraction service (dev-plan/66) ──

/// The public, keyless endpoint. Overridable, so an enterprise can run
/// `thclaws-cloud/pdf-text/` inside its own network and keep the files there.
const DEFAULT_PDF_TEXT_API: &str = "https://pdf.thclaws.cloud/extract";

/// Matches the service's own body cap: refuse locally rather than upload
/// 30 MB to be told no.
const CLOUD_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// Appended to text the service produced. The approval prompt is the consent;
/// this is so the result itself says where it came from — and how to stop it.
const CLOUD_NOTE: &str = "\n\n[extracted by pdf.thclaws.cloud: poppler is not installed \
     on this machine, so this file was uploaded to the public thClaws extraction service. \
     Install poppler-utils to keep PDFs local, or set \"pdfCloudFallback\": false to \
     disable the fallback.]";

/// `Some(url)` when the fallback may be used, `None` when it is switched off.
fn cloud_endpoint() -> Option<String> {
    let enabled = crate::config::AppConfig::load()
        .map(|c| c.pdf_cloud_fallback)
        .unwrap_or(true);
    if !enabled {
        return None;
    }
    Some(
        std::env::var("THCLAWS_PDF_TEXT_API")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_PDF_TEXT_API.to_string()),
    )
}

/// Is poppler absent? Then any text that came back came from the service, and
/// neither the vision path nor a page render is available at all. Checked at
/// the call sites instead of threading a "came from the cloud" flag through
/// every signature.
fn poppler_missing() -> bool {
    !crate::config::command_on_path("pdftotext")
}

/// What to do when `pdftotext` is not installed: use the public service, or
/// name every way to install it. Split out from `extract_text_raw` so both
/// branches are reachable in a test without emptying `PATH` — doing that in a
/// parallel test run breaks the sibling test that spawns the real binary.
/// Whether a caller is in a position to send the file off the machine: the
/// tool path is approval-gated and may, a UI-initiated IPC arm is not and
/// may not.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum CloudFallback {
    Allowed,
    Refused,
}

async fn without_poppler(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    cloud: CloudFallback,
) -> Result<String> {
    if cloud == CloudFallback::Refused {
        return Err(Error::Tool(
            "pdftotext not found — install poppler-utils: `brew install poppler` (macOS), `apt install poppler-utils` (Debian/Ubuntu), `winget install oschwartz10612.Poppler` or `scoop install poppler` (Windows). Or ask the agent to read the PDF — that path can extract it through the public thClaws service, with your approval and no account"
                .into(),
        ));
    }
    match cloud_endpoint() {
        Some(url) => extract_via_cloud(&url, validated, first, last).await,
        None => Err(Error::Tool(
            "pdftotext not found — install poppler-utils (`brew install poppler` on \
             macOS, `apt install poppler-utils` on Debian/Ubuntu, `winget install \
             oschwartz10612.Poppler` or `scoop install poppler` on Windows). The \
             no-install fallback to pdf.thclaws.cloud is switched off here \
             (`pdfCloudFallback` / THCLAWS_PDF_CLOUD)"
                .into(),
        )),
    }
}

/// POST the file to the extraction service and return its text.
async fn extract_via_cloud(
    url: &str,
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
) -> Result<String> {
    let size = tokio::fs::metadata(validated)
        .await
        .map_err(|e| Error::Tool(format!("read {}: {e}", validated.display())))?
        .len();
    if size > CLOUD_MAX_BYTES {
        return Err(Error::Tool(format!(
            "{} is {:.1} MB; the no-install extraction service caps uploads at 25 MB. \
             Install poppler-utils to read it locally with no limit.",
            validated.display(),
            size as f64 / (1024.0 * 1024.0)
        )));
    }
    let bytes = tokio::fs::read(validated)
        .await
        .map_err(|e| Error::Tool(format!("read {}: {e}", validated.display())))?;
    let name = validated
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "input.pdf".into());

    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(name)
        .mime_str("application/pdf")
        .map_err(|e| Error::Tool(format!("multipart: {e}")))?;
    let mut form = reqwest::multipart::Form::new().part("file", part);
    if let Some(f) = first {
        form = form.text("first", f.to_string());
    }
    if let Some(l) = last {
        form = form.text("last", l.to_string());
    }

    // Generous: the service's own budget is 30 s and the upload is on top.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| Error::Tool(format!("http client: {e}")))?;
    let resp = client.post(url).multipart(form).send().await.map_err(|e| {
        Error::Tool(format!(
            "no poppler locally and the extraction service at {url} is unreachable: {e}. \
                 Install poppler-utils to read PDFs offline."
        ))
    })?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Tool(format!("read response: {e}")))?;
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| body.chars().take(300).collect());
        return Err(Error::Tool(format!("pdf extraction service: {detail}")));
    }
    Ok(body)
}

/// Run pdftotext and return the raw extracted text. Shared between
/// `extract_text` and the multimodal entry's text-first path.
async fn extract_text_raw(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    cloud: CloudFallback,
) -> Result<String> {
    let laid_out = run_pdftotext(validated, first, last, "-layout", cloud).await?;
    // `-layout` reproduces the page by padding with spaces, and it puts one at
    // every glyph gap. Thai does not space its words, so those land INSIDE
    // them: `บริษัท` comes out as `บริ ษ ทั`. Measured over 24 Thai PDFs, the
    // density of "mark, space, consonant" separates the shredded documents
    // from the intact ones cleanly, and re-reading a shredded one with `-raw`
    // lengthens the average Thai run by 1.3x-3.7x while changing the Thai
    // character count by 0.00% (docs/pdf-thai-extraction-modes.md).
    if !thai_is_shredded(&laid_out) {
        return Ok(laid_out);
    }
    match run_pdftotext(validated, first, last, "-raw", cloud).await {
        Ok(raw) => {
            eprintln!(
                "\x1b[2m[pdf] -layout shredded this Thai text ({:.0} breaks per 1k) — re-read with -raw\x1b[0m",
                shred_per_1k(&laid_out)
            );
            Ok(raw)
        }
        // The second read is an improvement, never a requirement.
        Err(_) => Ok(laid_out),
    }
}

/// One `pdftotext` run in the given mode.
async fn run_pdftotext(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    mode: &str,
    cloud: CloudFallback,
) -> Result<String> {
    let mut cmd = Command::new("pdftotext");
    cmd.arg(mode);
    if let Some(f) = first {
        cmd.arg("-f").arg(f.to_string());
    }
    if let Some(l) = last {
        cmd.arg("-l").arg(l.to_string());
    }
    cmd.arg(validated.as_os_str()).arg("-"); // stdout
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let spawned = cmd.spawn();
    let mut child = match spawned {
        Ok(c) => c,
        // No poppler: borrow one from the public service, or say exactly what
        // to install — never both, and never silently.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return without_poppler(validated, first, last, cloud).await;
        }
        Err(e) => return Err(Error::Tool(format!("spawn pdftotext: {e}"))),
    };

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();

    let run = async {
        let stdout_fut = stdout.read_to_end(&mut out_buf);
        let stderr_fut = stderr.read_to_end(&mut err_buf);
        let (a, b) = tokio::join!(stdout_fut, stderr_fut);
        a.map_err(|e| Error::Tool(format!("read stdout: {e}")))?;
        b.map_err(|e| Error::Tool(format!("read stderr: {e}")))?;
        let status = child
            .wait()
            .await
            .map_err(|e| Error::Tool(format!("wait pdftotext: {e}")))?;
        Ok::<_, Error>(status)
    };

    let status = match timeout(EXTRACT_TIMEOUT, run).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(Error::Tool(format!(
                "pdftotext timed out after {}s",
                EXTRACT_TIMEOUT.as_secs()
            )));
        }
    };

    if !status.success() {
        let stderr_str = String::from_utf8_lossy(&err_buf);
        return Err(Error::Tool(format!(
            "pdftotext failed (exit {}): {}",
            status.code().unwrap_or(-1),
            stderr_str.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&out_buf).to_string())
}

/// Render the requested page range to PNG via `pdftoppm` and wrap each
/// page as a `ToolResultBlock::Image`. Caps at `MAX_PAGES_TO_RENDER`
/// pages — anything beyond that returns the rendered prefix plus a
/// trailing `Text` block telling the user how to fetch the rest via a
/// narrower `pages` argument.
async fn render_pages_as_image_blocks(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
) -> Result<Vec<ToolResultBlock>> {
    // pdftoppm needs a concrete page range. "all" → 1..=∞ is fine in
    // CLI semantics but we want to enforce our own MAX_PAGES_TO_RENDER
    // cap, so when last is None we substitute first + cap and tell
    // the user about the truncation in the trailing text block.
    let render_first = first.unwrap_or(1);
    let (render_last, truncated) = match last {
        Some(l) if l >= render_first => {
            let span = l - render_first + 1;
            if span > fallback::MAX_PAGES_TO_RENDER {
                (
                    render_first + fallback::MAX_PAGES_TO_RENDER - 1,
                    Some((span, fallback::MAX_PAGES_TO_RENDER)),
                )
            } else {
                (l, None)
            }
        }
        _ => {
            // first set, last unbounded ("3" already collapsed to
            // (3,3) by parse_page_range so this only triggers on the
            // "all" path where both are None — still substitute a
            // capped end and let pdftoppm clip naturally if the PDF
            // has fewer pages).
            (render_first + fallback::MAX_PAGES_TO_RENDER - 1, None)
        }
    };

    let tmp = tempfile::tempdir().map_err(|e| Error::Tool(format!("tempdir: {e}")))?;
    let prefix = tmp.path().join("page");
    let prefix_str = prefix.to_string_lossy().into_owned();

    let mut cmd = Command::new("pdftoppm");
    cmd.arg("-png")
        .arg("-r")
        .arg(fallback::RENDER_DPI.to_string())
        .arg("-f")
        .arg(render_first.to_string())
        .arg("-l")
        .arg(render_last.to_string())
        .arg(validated.as_os_str())
        .arg(&prefix_str);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Tool(
                "pdftoppm not found — install poppler-utils for the vision-OCR \
                 fallback (`brew install poppler` on macOS, `apt install \
                 poppler-utils` on Debian/Ubuntu)"
                    .into(),
            )
        } else {
            Error::Tool(format!("spawn pdftoppm: {e}"))
        }
    })?;

    let mut stderr = child.stderr.take().unwrap();
    let mut err_buf = Vec::new();

    let run = async {
        stderr
            .read_to_end(&mut err_buf)
            .await
            .map_err(|e| Error::Tool(format!("read stderr: {e}")))?;
        let status = child
            .wait()
            .await
            .map_err(|e| Error::Tool(format!("wait pdftoppm: {e}")))?;
        Ok::<_, Error>(status)
    };

    let status = match timeout(fallback::RENDER_TIMEOUT, run).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(Error::Tool(format!(
                "pdftoppm timed out after {}s",
                fallback::RENDER_TIMEOUT.as_secs()
            )));
        }
    };
    if !status.success() {
        let stderr_str = String::from_utf8_lossy(&err_buf);
        return Err(Error::Tool(format!(
            "pdftoppm failed (exit {}): {}",
            status.code().unwrap_or(-1),
            stderr_str.trim()
        )));
    }

    // pdftoppm's filename pattern is `<prefix>-<N>.png` where N is
    // 1-indexed and zero-padded to the digits needed for the LAST
    // page. e.g. 100 pages → `page-001.png`; 9 pages → `page-1.png`.
    // Walking the directory and sorting by name gives us pages in
    // the right order regardless of the padding width.
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(tmp.path())
        .map_err(|e| Error::Tool(format!("read tmp dir: {e}")))?
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("png"))
        .collect();
    entries.sort();

    let mut blocks: Vec<ToolResultBlock> = Vec::with_capacity(entries.len() + 1);
    let mut total_bytes: usize = 0;
    for (idx, path) in entries.iter().enumerate() {
        let bytes = std::fs::read(path).map_err(|e| Error::Tool(format!("read png: {e}")))?;
        if bytes.len() > fallback::MAX_IMAGE_BYTES {
            blocks.push(ToolResultBlock::Text {
                text: format!(
                    "(page {} skipped — rendered PNG is {} bytes, over the {}-byte cap; \
                     try a narrower `pages` range or downscale via the source PDF.)",
                    render_first + idx as u32,
                    bytes.len(),
                    fallback::MAX_IMAGE_BYTES
                ),
            });
            continue;
        }
        total_bytes += bytes.len();
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        blocks.push(ToolResultBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data,
            },
        });
    }

    let mut summary = format!(
        "PDF appears to be scanned / image-based (no extractable text layer). \
         Rendered {} page(s) at {} DPI for vision OCR — total {} KB before \
         base64.",
        blocks
            .iter()
            .filter(|b| matches!(b, ToolResultBlock::Image { .. }))
            .count(),
        fallback::RENDER_DPI,
        (total_bytes + 512) / 1024
    );
    if let Some((requested, capped)) = truncated {
        summary.push_str(&format!(
            " Truncated: requested {} pages but only the first {} were rendered \
             (cap: MAX_PAGES_TO_RENDER). Re-invoke with a narrower `pages` range \
             to see the rest.",
            requested, capped
        ));
    }
    blocks.push(ToolResultBlock::Text { text: summary });

    Ok(blocks)
}

/// Parse a `pages` string into (first, last) page numbers (1-indexed,
/// inclusive). `None` for either side means "no bound". Examples:
/// - `"all"` → (None, None)
/// - `"3"` → (Some(3), Some(3))
/// - `"1-5"` → (Some(1), Some(5))
fn parse_page_range(spec: &str) -> Result<(Option<u32>, Option<u32>)> {
    let s = spec.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("all") {
        return Ok((None, None));
    }
    if let Some((a, b)) = s.split_once('-') {
        let first: u32 = a
            .trim()
            .parse()
            .map_err(|_| Error::Tool(format!("invalid page range start: {a:?}")))?;
        let last: u32 = b
            .trim()
            .parse()
            .map_err(|_| Error::Tool(format!("invalid page range end: {b:?}")))?;
        if first == 0 || last < first {
            return Err(Error::Tool(format!(
                "invalid page range: {first}-{last} (pages are 1-indexed; end must be >= start)"
            )));
        }
        return Ok((Some(first), Some(last)));
    }
    let n: u32 = s
        .parse()
        .map_err(|_| Error::Tool(format!("invalid page spec: {spec:?}")))?;
    if n == 0 {
        return Err(Error::Tool("page numbers are 1-indexed, got 0".into()));
    }
    Ok((Some(n), Some(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_page_range_all() {
        assert_eq!(parse_page_range("all").unwrap(), (None, None));
        assert_eq!(parse_page_range("ALL").unwrap(), (None, None));
        assert_eq!(parse_page_range("").unwrap(), (None, None));
    }

    #[test]
    fn parse_page_range_single() {
        assert_eq!(parse_page_range("3").unwrap(), (Some(3), Some(3)));
    }

    #[test]
    fn parse_page_range_span() {
        assert_eq!(parse_page_range("1-5").unwrap(), (Some(1), Some(5)));
        assert_eq!(parse_page_range(" 2 - 7 ").unwrap(), (Some(2), Some(7)));
    }

    #[test]
    fn parse_page_range_rejects_bad_input() {
        assert!(parse_page_range("0").is_err());
        assert!(parse_page_range("abc").is_err());
        assert!(parse_page_range("5-3").is_err());
        assert!(parse_page_range("1-").is_err());
    }

    /// The sparseness heuristic must:
    /// - Treat empty extracted text as scanned (no chars at all)
    /// - Treat a single-line title as scanned (way under the threshold)
    /// - Pass dense paragraphs through as text-PDF
    /// - Average across pages (form-feed separated)
    #[test]
    fn text_sparseness_matches_intent() {
        // Empty → scanned.
        assert!(text_is_too_sparse(""));
        assert!(text_is_too_sparse("   \n\n  "));

        // One-line title across one page → still under 50 chars
        // non-whitespace → treated as scanned. Conservative on
        // purpose; vision OCR adds little cost for a single page.
        assert!(text_is_too_sparse("Cover page"));

        // Dense single-page paragraph → not scanned.
        let dense = "A".repeat(500);
        assert!(!text_is_too_sparse(&dense));

        // Two pages, one dense + one empty → average dilutes but
        // overall still well above threshold.
        let mixed = format!("{}\u{000C}", "A".repeat(500));
        assert!(!text_is_too_sparse(&mixed));

        // Five pages of title-only content → very sparse, should
        // trip even though chars-per-page math hits exactly the
        // boundary. 5 pages × 10 chars = 50 chars total, divided
        // by 5 pages = 10 chars/page average → below the 50
        // threshold.
        let titles = (0..5)
            .map(|_| "Cover page")
            .collect::<Vec<_>>()
            .join("\u{000C}");
        assert!(text_is_too_sparse(&titles));
    }

    fn pdftotext_available() -> bool {
        std::process::Command::new("pdftotext")
            .arg("-v")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// End-to-end: PdfCreateTool writes a Thai+Latin PDF to a tempfile,
    /// PdfReadTool extracts it via pdftotext, and we assert that both
    /// scripts survive the round-trip. Skipped if poppler-utils isn't
    /// installed (CI macOS runners need `brew install poppler` in the
    /// workflow setup; ubuntu uses `apt install poppler-utils`).
    #[tokio::test]
    async fn round_trips_thai_latin_via_pdftotext() {
        if !pdftotext_available() {
            eprintln!("skipping: pdftotext not in PATH");
            return;
        }
        use crate::tools::PdfCreateTool;
        use serde_json::json;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pdf = dir.path().join("rt.pdf");
        let _ = PdfCreateTool
            .call(json!({
                "path": pdf.to_string_lossy(),
                "content": "# Hello สวัสดี\n\nMixed paragraph with English and ภาษาไทย together."
            }))
            .await
            .unwrap();

        let extracted = PdfReadTool
            .call(json!({"path": pdf.to_string_lossy()}))
            .await
            .unwrap();

        assert!(
            extracted.contains("Hello"),
            "Latin should survive round-trip, got: {extracted:?}"
        );
        assert!(
            extracted
                .chars()
                .any(|c| matches!(c, '\u{0E00}'..='\u{0E7F}')),
            "Thai should survive round-trip, got: {extracted:?}"
        );
    }

    /// Multimodal entry: a normal text PDF returns Text, not Blocks.
    /// Regression guard so the fallback doesn't accidentally fire on
    /// every invocation.
    #[tokio::test]
    async fn call_multimodal_returns_text_for_text_pdf() {
        if !pdftotext_available() {
            eprintln!("skipping: pdftotext not in PATH");
            return;
        }
        use crate::tools::PdfCreateTool;
        use serde_json::json;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pdf = dir.path().join("text.pdf");
        // Long enough body that the per-page average comfortably
        // clears MIN_CHARS_PER_PAGE.
        let body = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, \
                    sed do eiusmod tempor incididunt ut labore et dolore magna \
                    aliqua. Ut enim ad minim veniam, quis nostrud exercitation \
                    ullamco laboris nisi ut aliquip ex ea commodo consequat.";
        PdfCreateTool
            .call(json!({"path": pdf.to_string_lossy(), "content": format!("# Doc\n\n{body}")}))
            .await
            .unwrap();

        let result = PdfReadTool
            .call_multimodal(json!({"path": pdf.to_string_lossy()}))
            .await
            .unwrap();
        match result {
            ToolResultContent::Text(t) => {
                assert!(t.contains("Lorem"), "expected text body, got: {t:?}");
            }
            ToolResultContent::Blocks(_) => {
                panic!("text PDF should not trigger image fallback")
            }
        }
    }

    #[test]
    fn normalize_thai_reattaches_orphaned_marks() {
        // pdftotext -layout orphans combining marks behind spaces;
        // re-attaching them is unambiguous (a mark can't start a word).
        assert_eq!(normalize_thai_spacing("ผู ้"), "ผู้"); // orphaned tone over vowel
        assert_eq!(normalize_thai_spacing("ษ ัท"), "ษัท"); // orphaned mai-han-akat
        assert_eq!(normalize_thai_spacing("ก ำหนด"), "กำหนด"); // orphaned sara-am
        assert_eq!(normalize_thai_spacing("ปฏิบ ัติ"), "ปฏิบัติ");
        assert_eq!(normalize_thai_spacing("ษ ั ้"), "ษั้"); // stacked marks, two passes
    }

    #[test]
    fn normalize_thai_never_merges_real_words() {
        // Non-Thai untouched.
        assert_eq!(normalize_thai_spacing("Hello world"), "Hello world");
        // A real phrase space — the next word starts with a consonant, not
        // an orphaned mark — must survive, even though "เวลา" ends in a
        // vowel. This is the case a "mark + space + consonant" rule would
        // wrongly merge.
        assert_eq!(normalize_thai_spacing("เวลา ทำงาน"), "เวลา ทำงาน");
        assert_eq!(normalize_thai_spacing("คน รถ"), "คน รถ");
    }

    #[test]
    fn repairs_known_saraam_cmap_corruption() {
        // ำ→า corruption in non-word forms is repaired, even glued mid-run
        // (substring) and after the space-join collapses a fragment.
        assert_eq!(normalize_thai_spacing("การทางานของบริษัท"), "การทำงานของบริษัท");
        assert_eq!(normalize_thai_spacing("กาหนดเวลา"), "กำหนดเวลา");
        assert_eq!(normalize_thai_spacing("จานวนวันลา"), "จำนวนวันลา");
        assert_eq!(normalize_thai_spacing("ก าหนด"), "กำหนด"); // space-join then repair
                                                               // Real า words must NEVER be touched (never in the table), incl. ones
                                                               // that share a prefix with a corrupted form (ประจาน vs ประจำ).
        for w in ["ด้าน", "หน้าที่", "ภาพ", "เป้าหมาย", "ทางการ", "ประจาน", "ราคา"]
        {
            assert_eq!(
                normalize_thai_spacing(w),
                w,
                "must not corrupt real word {w}"
            );
        }
    }

    #[test]
    fn garbled_detector_flags_fragmented_thai_not_clean() {
        // Clean Thai prose: no marks orphaned behind spaces → trustworthy.
        let clean = "พนักงานทุกคนมีสิทธิได้รับค่าจ้างตามที่กฎหมายกำหนดไว้อย่างเป็นธรรมเสมอ";
        assert!(!thai_looks_garbled(clean));

        // Heavily fragmented Thai (≥6 marks orphaned behind spaces) → the
        // text layer is untrustworthy, so route to the vision path.
        let garbled = "บริ ษ ัท ผู ้ ปฏิบ ัติ หน้ า ค่ าจ้าง ก ำหนด ท ำงาน จ ำเป็น \
                       สิ ทธิ พนักงานทุกคนในองค์กร";
        assert!(thai_looks_garbled(garbled));
    }

    /// The numbers behind SHRED_PER_1K_RETRY, taken from the survey corpus
    /// (docs/pdf-thai-extraction-modes.md): the worst document reads 36.3
    /// breaks per 1k Thai characters under `-layout`, budget tables read
    /// 0.0-0.8, and Samkok reads 2.3-2.6 with nothing to gain from `-raw`.
    #[test]
    fn shredded_thai_is_told_from_merely_spaced_thai() {
        // Real `-layout` output: the spaces fall inside the words.
        let shredded = "บริ ษทั ฯ จาแนกประเภทของพนักงานไว้ดงั นี้ พนักงานที่บริ ษทั ฯ                         ตกลงจ้างโดยกำหนดค่าจ้างเป็ นรายเดือน ปฏิบตัิงานเป็ นระยะเวลา                         โดยผูบ้ งั คับบัญชาจะประเมินการทดลองงานจาก ผลการปฏิบตัิงาน                         หากผลการประเมินไม่ผา่ นตามมาตรฐาน บริ ษทั ฯ จะเลิ กจ้าง"
            .repeat(2);
        assert!(shred_per_1k(&shredded) > fallback::SHRED_PER_1K_RETRY);
        assert!(
            thai_is_shredded(&shredded),
            "this is the document that needs -raw"
        );

        // The same prose intact: Thai spaces between phrases, never inside a
        // word. Must NOT trigger a second extraction.
        let clean = "พนักงานทุกคนมีสิทธิได้รับค่าจ้างตามที่กฎหมายกำหนดไว้อย่างเป็นธรรม                      บริษัทจำแนกประเภทของพนักงานไว้ดังนี้ พนักงานรายเดือนและพนักงานรายวัน                      ผู้บังคับบัญชาจะประเมินผลการปฏิบัติงานตามมาตรฐานที่บริษัทกำหนด"
            .repeat(2);
        assert!(
            shred_per_1k(&clean) <= fallback::SHRED_PER_1K_RETRY,
            "{}",
            shred_per_1k(&clean)
        );
        assert!(!thai_is_shredded(&clean));

        // Too little Thai to judge — never spend a second extraction on it.
        assert!(!thai_is_shredded("บริ ษทั ฯ"));
    }

    /// A text layer with no words in it reaches the vision path. The orphan-
    /// mark check cannot see this case: the characters are isolated, so no
    /// mark ever sits behind a space.
    #[test]
    fn a_text_layer_of_isolated_characters_is_garbled() {
        let isolated: String = "ข้อบังคับเกี่ยวกับการทำงานของพนักงานบริษัท"
            .chars()
            .map(|c| format!("{c} "))
            .collect::<Vec<_>>()
            .join("")
            .repeat(3);
        assert!(mean_thai_run(&isolated) < fallback::MIN_THAI_RUN);
        assert!(
            thai_looks_garbled(&isolated),
            "isolated characters must route to vision"
        );

        // Ordinary Thai prose runs long and stays on the text path.
        let prose = "พนักงานทุกคนมีสิทธิได้รับค่าจ้างตามที่กฎหมายกำหนดไว้อย่างเป็นธรรมเสมอ".repeat(2);
        assert!(mean_thai_run(&prose) > 10.0, "{}", mean_thai_run(&prose));
        assert!(!thai_looks_garbled(&prose));
    }

    /// dev-plan/66: the no-poppler fallback, end to end against a stand-in
    /// for `thclaws-cloud/pdf-text/` — multipart out, page range carried,
    /// text back, and a service error surfaced as the service worded it
    /// rather than as a status code.
    #[tokio::test]
    async fn cloud_fallback_posts_the_file_and_returns_its_text() {
        use axum::extract::Multipart;
        use axum::routing::post;

        async fn extract(mut mp: Multipart) -> (axum::http::StatusCode, String) {
            let (mut name, mut bytes, mut first) = (String::new(), 0usize, String::new());
            while let Ok(Some(field)) = mp.next_field().await {
                match field.name().unwrap_or("").to_string().as_str() {
                    "file" => {
                        name = field.file_name().unwrap_or("").to_string();
                        bytes = field.bytes().await.map(|b| b.len()).unwrap_or(0);
                    }
                    "first" => first = field.text().await.unwrap_or_default(),
                    _ => {}
                }
            }
            if name.ends_with(".bad") {
                return (
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    r#"{"error":"not a readable PDF"}"#.to_string(),
                );
            }
            (
                axum::http::StatusCode::OK,
                format!("ได้รับ {name} ({bytes} bytes) first={first}"),
            )
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = axum::Router::new().route("/extract", post(extract));
            let _ = axum::serve(listener, app).await;
        });
        let url = format!("http://{addr}/extract");

        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("สัญญา.pdf");
        std::fs::write(&pdf, vec![b'x'; 1234]).unwrap();
        let got = extract_via_cloud(&url, &pdf, Some(2), None).await.unwrap();
        assert!(got.contains("สัญญา.pdf"), "filename carried: {got}");
        assert!(got.contains("1234 bytes"), "body carried: {got}");
        assert!(got.contains("first=2"), "page range carried: {got}");

        // A refusal must reach the user as the service's own sentence.
        let bad = dir.path().join("x.bad");
        std::fs::write(&bad, b"nope").unwrap();
        let err = extract_via_cloud(&url, &bad, None, None)
            .await
            .expect_err("422 must be an error");
        assert!(
            err.to_string().contains("not a readable PDF"),
            "service wording must survive: {err}"
        );
    }

    /// The switch has to actually switch: with it off there is no endpoint,
    /// so `extract_text_raw` reports what to install instead of uploading.
    #[test]
    fn cloud_fallback_can_be_switched_off() {
        // Cargo runs lib tests in parallel and these are process-global.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("THCLAWS_PDF_CLOUD", "0");
        assert!(
            cloud_endpoint().is_none(),
            "THCLAWS_PDF_CLOUD=0 disables it"
        );
        std::env::set_var("THCLAWS_PDF_CLOUD", "1");
        std::env::set_var("THCLAWS_PDF_TEXT_API", "https://example.invalid/x");
        assert_eq!(
            cloud_endpoint().as_deref(),
            Some("https://example.invalid/x"),
            "the endpoint is overridable for a self-hosted copy"
        );
        std::env::remove_var("THCLAWS_PDF_TEXT_API");
        std::env::remove_var("THCLAWS_PDF_CLOUD");
    }

    /// With the fallback off, a machine without poppler must be told how to
    /// install it — on every platform, including the one the old message left
    /// out entirely.
    #[tokio::test]
    async fn no_poppler_and_no_fallback_names_every_install_route() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("THCLAWS_PDF_CLOUD", "0");
        let err = without_poppler(
            std::path::Path::new("/tmp/none.pdf"),
            None,
            None,
            CloudFallback::Allowed,
        )
        .await
        .expect_err("no local binary and no fallback is an error");
        std::env::remove_var("THCLAWS_PDF_CLOUD");
        let msg = err.to_string();
        for hint in [
            "brew install poppler",
            "apt install poppler-utils",
            "winget",
            "scoop",
        ] {
            assert!(msg.contains(hint), "missing {hint} in: {msg}");
        }
    }
}
