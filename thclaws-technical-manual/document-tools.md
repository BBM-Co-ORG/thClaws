# Document tools

Twelve tools that read, create, and edit office-format documents (Word, Excel, PowerPoint, PDF). They share a consistent surface — Create / Edit / Read per format, all gated by `Sandbox::check_write` for mutations, all run on a `tokio::task::spawn_blocking` worker so the synchronous office-format crates don't block the async runtime, all bundle Noto Sans Thai for first-class Thai text rendering. The 12 tools are an overlay on the broader built-in tool registry covered in [`built-in-tools.md`](built-in-tools.md); this manual focuses on what's specific to office-format generation.

| Format | Create | Edit | Read |
|---|---|---|---|
| Word (`.docx`) | `DocxCreate` | `DocxEdit` | `DocxRead` |
| Excel (`.xlsx`) | `XlsxCreate` | `XlsxEdit` | `XlsxRead` |
| PowerPoint (`.pptx`) | `PptxCreate` | `PptxEdit` | `PptxRead` |
| PDF | `PdfCreate` | — (PDF edit not in scope) | `PdfRead` |
| EPUB | `EpubCreate` | — | — (compiles Markdown → EPUB; `src/tools/epub_create.rs`, `requires_approval`) |

**Source:** `crates/core/src/tools/{docx,xlsx,pptx,pdf}_*.rs`
**Bundled fonts:** `crates/core/resources/fonts/NotoSans-Regular.ttf` + `NotoSansThai-Regular.ttf` (PDF only — Docx/Pptx reference Noto Sans Thai by name and rely on the OS having it installed)
**Bundled template:** `crates/core/resources/pptx/template-light.pptx` (~28KB, used as starting point for PptxCreate)

**Cross-references:**
- [`built-in-tools.md`](built-in-tools.md) — `Tool` trait, `ToolRegistry`, sandbox gates
- [`permissions.md`](permissions.md) §7 — `Sandbox::check_write` enforcement
- [`agentic-loop.md`](agentic-loop.md) — `tokio::spawn_blocking` is the runtime escape hatch

---

## 1. Shared surface conventions

All 12 tools follow the same skeleton:

```rust
pub struct FooCreateTool;     // unit struct — no per-instance state

#[async_trait]
impl Tool for FooCreateTool {
    fn name(&self) -> &'static str { "FooCreate" }
    fn description(&self) -> &'static str { "..." }
    fn input_schema(&self) -> Value { json!({...}) }

    fn requires_approval(&self, _input: &Value) -> bool {
        true   // Create + Edit always require approval
        // false for Read tools
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let validated = crate::sandbox::Sandbox::check_write(raw_path)?;
        // ... (Read tools use Sandbox::check instead)

        // Synchronous office-format crate offloaded to blocking worker:
        tokio::task::spawn_blocking(move || render_foo(&validated, ...))
            .await
            .map_err(|e| Error::Tool(format!("FOO worker join failed: {e}")))??
    }
}
```

The pattern matters because:
- **`Sandbox::check_write` blocks `.thclaws/`** — same as the rest of the file tools. A model can't accidentally rewrite team state by `DocxCreate(path: ".thclaws/settings.json")`.
- **`spawn_blocking` for the actual format work** — `docx-rs`, `umya-spreadsheet`, `rust_xlsxwriter`, `printpdf`, and the `zip` crate are synchronous CPU-bound. Running them in the async task would block the runtime's worker pool. The `spawn_blocking` pool is dedicated to this kind of work.
- **All Create/Edit tools require approval** — they mutate the filesystem and may produce arbitrary content. Read tools don't.
- **Parent directory auto-creation** — `std::fs::create_dir_all(parent)` for the output path. `DocxCreate path: "out/reports/q4.docx"` works even if `out/reports/` doesn't exist yet.

---

## 2. Thai script handling — the core design constraint

Every Create tool ships a **per-run script-aware font selection** so a single paragraph mixing Thai and Latin renders correctly without manual splitting. The trick: in OOXML (Word/PowerPoint), each text Run can declare different fonts for ASCII / hi-ANSI / complex-script (`cs`) ranges; the renderer picks per-codepoint. In PDF, we explicitly switch fonts per codepoint segment.

| Tool | Latin font | Thai font | Mechanism |
|---|---|---|---|
| `DocxCreate` | Calibri | Noto Sans Thai | Run-level `RunFonts { ascii: Calibri, hi_ansi: Calibri, cs: Noto Sans Thai }` |
| `DocxEdit` (append_paragraph) | Calibri | Noto Sans Thai | Same RunFonts pattern as Create |
| `PptxCreate` | (template default — Calibri) | Noto Sans Thai | Per-text-run `<a:cs typeface="Noto Sans Thai"/>` |
| `PdfCreate` | Noto Sans (embedded TTF) | Noto Sans Thai (embedded TTF) | Per-codepoint script detection + font switch via printpdf's `use_text` |
| `XlsxCreate` | (Excel default) | (Excel falls back per-cell) | No explicit Thai font; Excel uses system font fallback |

**Why bundle Noto Sans Thai for PDF specifically?** PDF needs the actual font glyphs at write-time. We `include_bytes!("../../resources/fonts/NotoSansThai-Regular.ttf")` (~600KB). Word/PowerPoint reference fonts BY NAME — the file just needs to declare "Noto Sans Thai" and the recipient's system supplies the glyphs. Modern Win/Mac/Linux ship Noto Sans Thai; Word falls back to Tahoma / Cordia New if absent. No font embedding for OOXML in v1.

**Why no Thai font for Excel?** Excel's default behavior is per-cell font fallback driven by the locale — for Thai content, Windows/Mac Excel renders correctly without explicit font declaration. Forcing a font would override user/locale preferences.

---

## 3. Word (`.docx`)

### `DocxCreate`

| | |
|---|---|
| Crate | `docx-rs` |
| Approval | yes |
| Schema | `{path: string, content: string (markdown), title?: string, font_size?: integer (6-72)}` |
| Default body | 11pt (22 half-points in OOXML convention) |

Renders a markdown string to `.docx`. Markdown features supported via `pulldown-cmark`:
- Paragraphs
- Headings H1-H4 (`#`/`##`/`###`/`####`) — applies `pStyle` `Heading1`-`Heading4`
- Bullet lists (`- item`) — `numId=1` (pre-registered abstract numbering with bullet level)
- Numbered lists (`1. item`) — `numId=2` (pre-registered with decimal level)
- Fenced code blocks (` ``` `) — Consolas font, no syntax highlighting
- Inline emphasis (bold/italic in body text)

NOT supported in v1: tables, images, tracked changes, ToC, headers/footers, footnotes (deferred to dev-plan/02).

```rust
const LATIN_FONT: &str = "Calibri";
const THAI_FONT: &str = "Noto Sans Thai";
const MONO_FONT: &str = "Consolas";

const BULLET_ID: usize = 1;        // pre-registered abstract numbering
const DECIMAL_ID: usize = 2;
```

Numbering ids are registered ONCE at document setup time; list paragraphs reference them. Each run carries `RunFonts { ascii: Calibri, hi_ansi: Calibri, cs: Noto Sans Thai }` so mixed-script text renders without splitting.

### `DocxEdit`

| | |
|---|---|
| Crate | `quick-xml` (manual OOXML mutation) + `zip` |
| Approval | yes |
| Operations | `find_replace`, `append_paragraph` |

In-place edit. Only `word/document.xml` is mutated; styles / numbering / headers / footers / images / etc. pass through verbatim.

**`find_replace`** — per-run substring matching. Word splits text into runs on style boundaries; a single visible word `"hello"` might be `<w:r>he</w:r><w:r>llo</w:r>` in the XML if the user italicized "llo" in the source. Naïve cross-run matching would miss those. v1 matches per-run only — fine for documents thClaws authored (DocxCreate produces single-run paragraphs); imperfect for human-authored docs with mid-paragraph styling. Documented as a known limitation in the tool description.

**`append_paragraph`** — adds a new paragraph at end of body with the SAME `RunFonts` (Calibri + Noto Sans Thai) DocxCreate uses, ensuring the appended content matches the rest of the document's font handling.

### `DocxRead`

| | |
|---|---|
| Crate | `quick-xml` + `zip` (NO LibreOffice / pandoc shell-out) |
| Approval | no |
| Schema | `{path: string}` |

Extract body text as markdown-ish. Pure Rust — no system dependency. State machine walks `word/document.xml`:

- `<w:p>` opens a paragraph buffer
- Inside `<w:pPr>`, `<w:pStyle w:val="Heading1">` → `# ` prefix; `Heading2` → `## `; etc.
- `<w:numPr>` → `- ` list-item prefix
- `<w:t>` text → into the buffer
- `<w:tab/>` → tab char; `<w:br/>` → newline
- `</w:p>` → flush with style/list prefix, blank line separator

Why not shell out: PDF can use `pdftotext` (poppler — a package away on macOS and Linux, though see `PdfRead` below: it is *not* everywhere, and it was missing from our own runner image until v0.135); for OOXML we'd need LibreOffice headless which is too heavy. quick-xml is fast enough and keeps the binary self-contained.

---

## 4. Excel (`.xlsx`)

### `XlsxCreate`

| | |
|---|---|
| Crate | `rust_xlsxwriter` (write-only, format-fresh) |
| Approval | yes |
| Schema | `{path: string, data: string\|array, sheet_name?: string, headers?: bool, auto_width?: bool}` |

Renders tabular data. `data` accepts:
1. **CSV string** — first row is headers when `headers: true` (default)
2. **JSON 2D array** `[[...row1...], [...row2...]]` — preserves cell types (numbers stay numbers, booleans stay booleans)

Cell-type detection for CSV cells: parse each cell as f64; on success → number cell; else → string. `true`/`false` literals become Excel booleans.

Header row formatting (when `headers: true`): bold + bottom border. Auto-width (when `auto_width: true`, default): each column sized to fit its longest cell.

Single-sheet for v1; multi-sheet via `add_sheet` op on `XlsxEdit`. `sheet_name` capped at 31 chars (Excel's hard limit).

### `XlsxEdit`

| | |
|---|---|
| Crate | `umya-spreadsheet` (read+write, format-preserving) |
| Approval | yes |
| Operations | `set_cell`, `set_cells`, `add_sheet`, `delete_sheet` |

In-place edit with **format preservation** — styles, formulas, charts, conditional formatting, and data validation in unrelated regions are kept on round-trip. This is critical for Excel where users have invested time in formatting; rewriting the file would clobber it.

Why `umya-spreadsheet` instead of `rust_xlsxwriter`: rust_xlsxwriter is write-only and would reset the workbook on load. umya is purpose-built for round-trip preservation.

| Op | Args | Behavior |
|---|---|---|
| `set_cell` | `cell` (A1 address), `value` (string/number/bool) | Update a single cell; auto-detect type |
| `set_cells` | `data` (2D array), `anchor` (default "A1"), `sheet?` | Bulk update from anchor down-right |
| `add_sheet` | `sheet` (name) | Append a new sheet |
| `delete_sheet` | `sheet` (name) | Remove a sheet (errors if it's the only sheet — workbooks must contain ≥1) |

`sheet` defaults to the first sheet for `set_cell`/`set_cells`; required for `add_sheet`/`delete_sheet`.

### `XlsxRead`

| | |
|---|---|
| Crate | `calamine` (pure-Rust read) |
| Approval | no |
| Schema | `{path: string, sheet?: string, max_rows?: integer}` |

Extract sheet contents as JSON. Returns `{sheets: [...], rows: [...]}`. Numbers stay numeric (whole-number floats render as integers — `42.0` becomes `42`). Default `max_rows = 1000` to bound output for large sheets.

---

## 5. PowerPoint (`.pptx`)

### `PptxCreate`

| | |
|---|---|
| Crate | `zip` + manual OOXML XML construction (Rust pptx ecosystem is immature) |
| Approval | yes |
| Schema | `{path: string, content: string (markdown outline)}` |

The Rust pptx situation: there's no mature high-level pptx crate. Rather than generate the ~30 OOXML files from scratch, we ship a single-slide template at `resources/pptx/template-light.pptx` (~28KB) and:

1. Unpack it into memory
2. Regenerate `ppt/slides/slide1.xml` with the user's first slide
3. Emit additional `ppt/slides/slide{N}.xml` for slides 2..N
4. Update `[Content_Types].xml`, `ppt/_rels/presentation.xml.rels`, and `ppt/presentation.xml` to register the new slides
5. Repack as a new ZIP at the output path

**Markdown outline:**
- Each `# Heading` starts a new slide; the heading text becomes the title
- Bullets under the heading become bullet body text
- Empty body = title-only slide
- At least one `# Heading` is required (errors otherwise)

Per-text-run `<a:cs typeface="Noto Sans Thai"/>` for Thai support — same per-run script-split trick as DocxCreate.

### `PptxEdit`

| | |
|---|---|
| Crate | `quick-xml` + `zip` |
| Approval | yes |
| Operations | `find_replace`, `add_slide` |

In-place edit. `find_replace` — substring replace across all slides' text runs. `add_slide` — append a new slide with title + bullet body, same template-derived layout as Create. Preserves master/layout/theme XML — only `ppt/slides/slideN.xml` and the registration files are touched.

### `PptxRead`

| | |
|---|---|
| Crate | `quick-xml` + `zip` |
| Approval | no |
| Schema | `{path: string}` |

Extract slide titles + body text per slide. Returns one slide per section, formatted as markdown with the title as `# Heading` and bullet body as `- item` lines. Pure Rust — no LibreOffice required.

---

## 6. PDF

### `PdfCreate`

| | |
|---|---|
| Crate | `printpdf` |
| Approval | yes |
| Schema | `{path: string, content: string (markdown), title?: string, font_size?: integer, page_size?: "A4"\|"Letter"\|"Legal"}` |
| Default font | 11pt |
| Default page | A4 (210 × 297 mm) |
| Embedded fonts | Noto Sans Regular + Noto Sans Thai Regular (compiled into binary via `include_bytes!`) |

Renders markdown to PDF. Supports headings (H1-H4), paragraphs, bullet lists, and fenced code blocks via `pulldown-cmark`. Page size options:
- A4: 210 × 297 mm (default)
- Letter: 215.9 × 279.4 mm
- Legal: 215.9 × 355.6 mm

**Per-codepoint font switching:** `printpdf::use_text` takes a font reference per call. We split the text run into segments by Unicode block — codepoints in the Thai block (U+0E00-U+0E7F) use the Noto Sans Thai font, everything else uses Noto Sans. Single paragraph mixing Thai+Latin renders correctly without manual splitting on the user's side.

**Why embedded fonts** (instead of system fonts): PDF needs the glyph data at write-time. Embedded fonts mean the resulting PDF is portable — the recipient's machine doesn't need Noto Sans Thai installed.

**Width estimation:** glyph-naive — uses a per-script multiplier on font size. Good enough for reports and notes; precise typography would require parsing the font's hmtx table, deferred.

Constants:
```rust
const PT_TO_MM: f32 = 0.3528;
const DEFAULT_FONT_SIZE_PT: f32 = 11.0;
const MARGIN_MM: f32 = 20.0;
const PARAGRAPH_GAP_MM: f32 = 3.0;
```

### `PdfRead`

| | |
|---|---|
| Mechanism | shells out to `pdftotext` (poppler-utils); falls back to the public `pdf.thclaws.cloud` service when it is missing |
| Approval | no locally — **yes** when the cloud fallback would run |
| Schema | `{path: string, pages?: string ("all" \| "N" \| "M-N")}` |
| Timeout | 60 seconds |

Why shell out instead of pure-Rust: extraction quality across real-world PDFs (tagged structure, form fields, embedded fonts with non-standard cmaps) is dominated by poppler's twenty-plus years of corner-case handling. Rust pdf crates are good for valid PDFs but break on the long tail.

**Measured, 2026-09-22** (`docs/pdf-extraction-bench.md`, harness in `scripts/pdf-extract-bench.py`): against ground truth — a Thai PDF and the HTML it was printed from — `pdftotext -layout` scores **98.3% / 97.8%** Thai 5-gram recall/precision, PDFium 89.9% / 80.5%, pdf-extract 82.7% / 83.3%. PDFium re-emits a Thai mark cluster every ~28 characters and `thai_looks_garbled` does *not* catch it (0.12% orphan marks, under the 4% trigger), so a swap would degrade Thai silently. pdf-extract returns table text character-spaced (`d e e p s e e k`). Poppler stays; the fix for "not installed" is detection and instructions, not a different engine.

`pdftotext -layout [-f first] [-l last] <path> -` → reads stdout. Layout-aware extraction preserves column / table structure better than the default flow mode.

**Page range parsing:** `"all"` → no `-f`/`-l` flags; `"3"` → `-f 3 -l 3`; `"1-5"` → `-f 1 -l 5`. Other formats error with a clear "expected all/N/M-N" message.

**Missing-binary error:** if `pdftotext` isn't on PATH:
```
pdftotext not found — install poppler-utils (`brew install poppler` on macOS,
`apt install poppler-utils` on Debian/Ubuntu)
```

The error message includes installation instructions for the two most common platforms. Power users on other distros are expected to know their package manager. **Windows users get neither**: the message names brew and apt, and `doctor.rs`'s install recipes (brew / apt / npm / pip) have no winget/scoop/choco arm — open gap, see `docs/pdf-extraction-bench.md`.

Hosted workspaces ship poppler in the engine image (`thclaws/Dockerfile`, from v0.135) — before that every cloud PDF read failed with the message above, which a pod running as uid 1000 cannot act on.

### The no-poppler fallback (dev-plan/66)

A desktop user who has not installed poppler gets one anyway: `extract_text_raw` catches `ErrorKind::NotFound` on the spawn and POSTs the file to `pdf.thclaws.cloud/extract` instead — **keyless, no account**, because the requirement is that a customer with neither works.

| Piece | Where |
|---|---|
| Service (poppler and nothing else, non-root, read-only rootfs, no egress, tmpfs upload dir, page/size/rate caps) | `thclaws-cloud/pdf-text/` |
| Deployment + Service + rate-limit/buffer Middleware + IngressRoute + no-egress NetworkPolicy | `thclaws-cloud/infra/k3s/base/pdf-text.yaml` |
| Client, approval gate, result marker | `tools/pdf_read.rs` (`cloud_endpoint`, `extract_via_cloud`, `poppler_missing`) |

Three properties that are load-bearing rather than decorative:

- **Consent.** `requires_approval` returns true for exactly the condition that sends bytes off the machine, and the new `Tool::approval_summary` hook (default `None`, plumbed through `ApprovalRequest.summary` in `agent.rs`) makes the prompt read *"no poppler on this machine — UPLOADS contract.pdf to …"* instead of a bare `PdfRead`. Accepted text carries a `[extracted by pdf.thclaws.cloud …]` note.
- **Off switch.** `"pdfCloudFallback": false` or `THCLAWS_PDF_CLOUD=0`; then the missing-binary error is raised, now including the Windows recipes. `THCLAWS_PDF_TEXT_API` repoints it at a self-hosted copy, which is the enterprise answer.
- **No vision path without poppler.** Rendering pages is `pdftoppm`, same package, and the service returns text only — so a scanned PDF with no local poppler returns the (thin) text plus an explicit note saying what to install, rather than failing on a second missing binary.

`normalize_thai_spacing` still runs on the result; the service uses the same `pdftotext -layout`, so the output is byte-identical to a local run.

**No PdfEdit:** PDF editing is hard (the format isn't designed for it). Not in scope; the model can use Read + Create to fork-and-rewrite if needed.

---

## 7. The blocking-worker pattern

Every Create/Edit/Read call follows:

```rust
async fn call(&self, input: Value) -> Result<String> {
    // 1. Validate input + path (fast, async-safe)
    let raw_path = req_str(&input, "path")?;
    let validated = Sandbox::check_write(raw_path)?;

    // 2. Move owned data into the blocking worker
    let path_clone = validated.clone();
    let content_clone = content.to_string();

    // 3. Synchronous CPU/IO work in spawn_blocking
    let bytes = tokio::task::spawn_blocking(move || -> Result<usize> {
        render_foo(&path_clone, &content_clone, ...)
    })
    .await
    .map_err(|e| Error::Tool(format!("FOO worker join failed: {e}")))??;

    // 4. Format the success message
    Ok(format!("Wrote FOO to {} ({bytes} bytes)", validated.display()))
}
```

Why this matters:
- **`docx-rs`, `umya-spreadsheet`, `rust_xlsxwriter`, `printpdf`, `zip`, `calamine`, `quick-xml` are all synchronous.** Calling them in an async fn would block the runtime's worker thread, starving other tasks (other tools' I/O, the agent loop's stream, etc.).
- **`spawn_blocking` is tokio's escape hatch** — runs the closure on a dedicated thread pool sized for blocking work. The async caller awaits as normal.
- **`Result<...>` double-unwrap (`?` twice)** — `spawn_blocking` returns `Result<Result<T>, JoinError>`. First `?` handles join failure (panic, cancellation); second `?` handles the inner Result.
- **`pdftotext` (PdfRead) uses `tokio::process::Command`** — already async. No spawn_blocking needed.

PDF read is the only document tool that doesn't use spawn_blocking — it uses `tokio::process::Command::spawn` directly because the work happens in a subprocess (poppler).

---

## 8. Code organization

```
crates/core/src/tools/
├── docx_create.rs (468 LOC)              ── docx-rs + pulldown-cmark + per-run RunFonts
├── docx_edit.rs (362 LOC)                ── quick-xml + zip; find_replace + append_paragraph
├── docx_read.rs (223 LOC)                ── quick-xml + zip; pStyle/numPr → markdown
├── xlsx_create.rs (297 LOC)              ── rust_xlsxwriter; CSV + JSON 2D-array; bold/border/auto-width
├── xlsx_edit.rs (333 LOC)                ── umya-spreadsheet (format-preserving); 4 ops
├── xlsx_read.rs (224 LOC)                ── calamine; sheet-list + cells
├── pptx_create.rs (422 LOC)              ── ZIP repack from template-light.pptx + slide XML gen
├── pptx_edit.rs (297 LOC)                ── quick-xml + zip; find_replace + add_slide
├── pptx_read.rs (229 LOC)                ── quick-xml + zip; slide titles + body
├── pdf_create.rs (484 LOC)               ── printpdf + pulldown-cmark + embedded NotoSans/NotoSansThai
└── pdf_read.rs (239 LOC)                 ── shells out to `pdftotext`; page range parsing

crates/core/resources/
├── fonts/
│   ├── NotoSans-Regular.ttf              (~570 KB) — embedded by PdfCreate
│   └── NotoSansThai-Regular.ttf          (~590 KB) — embedded by PdfCreate
└── pptx/
    └── template-light.pptx               (~28 KB) — embedded by PptxCreate as starting template
```

Total document-tools code: ~3.6k LOC. Plus ~1.2 MB of embedded font + template binary data (tradeoff: bigger binary for portable output that doesn't depend on user-installed fonts).

---

## 9. Testing

Each tool has a `#[cfg(test)]` block with at least:
- happy path (round-trip create → read for the file's format family)
- missing required field error
- format-specific edge cases (Thai text round-trip, code blocks, empty content, etc.)

Highlights:
- `docx_create::tests::round_trip_create_then_read` — write a markdown doc, read it back via DocxRead, assert content preserved
- `xlsx_edit::tests::round_trip_create_edit_read` — create → set_cell → read; preserved styling
- `pptx_edit::tests::round_trip_create_edit_read` — multi-slide round-trip
- `pdf_create::tests::writes_pdf_with_thai_and_latin` — PDF with mixed Thai/Latin; verifies file is non-empty and starts with `%PDF-`
- `pdf_read::tests::round_trips_thai_latin_via_pdftotext` — gated on `pdftotext` availability; skips with warning if missing

Tests use `tempfile::tempdir` for isolation. Tests gated on external tools (pdftotext) check availability in `setup` and skip gracefully — CI on Ubuntu installs poppler-utils; macOS CI installs via Homebrew.

---

## 10. Notable behaviors / gotchas

### General
- **All Create + Edit tools require approval.** Read tools don't. Mirrors the broader Tool approval matrix in [`permissions.md`](permissions.md) §4.
- **All tools use `Sandbox::check_write` for output paths** — `.thclaws/` is denied even though it might otherwise be inside the project root.
- **Worker join failures surface as `Error::Tool`** — typically means a panic inside the format crate (rare; usually only on malformed input).
- **No streaming output.** Document tools return their entire result as a `String` (the success message includes file path + size). The actual file is on disk; the model gets a confirmation. Calling `Read` on the resulting file separately would fetch the bytes if needed.

### Word
- **Cross-run substring matching not supported** in DocxEdit `find_replace`. Word splits text on style changes; matching only works per-run. For thClaws-authored docs (single-run paragraphs from DocxCreate), this is fine.
- **Headings only H1-H4.** H5/H6 fall through to plain text. OOXML supports through H9; we limit to keep the rendering pipeline simple.
- **No table support yet.** Markdown tables get rendered as plain text.
- **No image support.** Image markdown (`![alt](path)`) renders as plain text. Embedding images would require copying bytes into the .docx ZIP and creating image relationships — deferred.

### Excel
- **`XlsxCreate` is single-sheet.** Multi-sheet workbooks are built via `XlsxEdit::add_sheet` on an existing workbook. Could grow to accept `{sheets: [...]}` input — deferred.
- **Sheet names capped at 31 chars** (Excel's hard limit). Longer names are rejected.
- **`umya-spreadsheet` round-trip is format-preserving** but slower than `rust_xlsxwriter`. For pure-write workloads use Create; for edit-existing use Edit.
- **`delete_sheet` errors if it's the only sheet** — workbooks must contain ≥1 sheet.

### PowerPoint
- **Template-bound layouts.** All slides use the layout defined by `template-light.pptx`. Custom layouts (multi-column, image+text) would require additional templates or runtime layout XML generation — deferred.
- **No notes / speaker view / animations / transitions.** Pure content.
- **At least one `# Heading` is required** in PptxCreate input. Pure text without headings produces zero slides → error.

### PDF
- **`pdftotext` external dependency** for PdfRead. Documented in the missing-binary error message with install instructions.
- **No PdfEdit.** PDF format isn't designed for in-place edits; the workaround is Read → modify markdown → Create.
- **Width estimation is glyph-naive.** Long lines might wrap slightly off — OK for reports, suboptimal for precision typesetting.
- **Embedded fonts add ~1.2MB to the binary.** Tradeoff: portable output vs binary size.

### Thai script
- **Three different mechanisms** for Thai font selection across the four formats (RunFonts.cs for OOXML Word/PptX, per-codepoint switching for PDF, system fallback for Excel). All three should produce visually-correct Thai output.
- **Word/Pptx need Noto Sans Thai installed system-wide** on the recipient's machine (or Word falls back to Tahoma/Cordia New). PDF embeds the font directly so the recipient needs nothing.

---

## 11. What's NOT supported

### Cross-cutting
- **No image embedding** in any Create tool. All accept text content only (markdown for DocxCreate/PdfCreate/PptxCreate, CSV/2D-array for XlsxCreate).
- **No `requires_approval` differentiation by input.** All Create/Edit tools blanket-require approval; can't say "approve only when overwriting an existing file."
- **No per-format encryption / password protection.** OOXML supports encrypted files; we don't.
- **No revision tracking / track-changes** in Word.
- **No formulas in XlsxCreate** — values only. XlsxEdit's `set_cell` writes the value verbatim (a leading `=` would become a string, not a formula). Formula injection would need an explicit `formula: true` flag.
- **No chart generation** in XlsxCreate / PptxCreate.

### Per-format
- **DocxCreate**: tables, images, ToC, headers/footers, footnotes, comments, tracked changes, bookmarks, hyperlinks (markdown link → text only).
- **DocxEdit**: cross-run find_replace, undo, multi-occurrence-aware insertion.
- **XlsxCreate**: multi-sheet, formulas, charts, conditional formatting, data validation, named ranges.
- **XlsxEdit**: formula support for `set_cell`, range deletion, sheet rename / reorder.
- **PptxCreate**: image content, animations, transitions, master/layout customization, notes, speaker view, multi-column layouts.
- **PptxEdit**: slide reorder, slide deletion, image insertion.
- **PdfCreate**: tables, images, headers/footers, page numbers, ToC, hyperlinks, bookmarks, forms.
- **PdfRead**: image extraction, form-field extraction, PDF metadata extraction (could be added by parsing `pdftotext`'s `-layout`/`-raw`/`-bbox` output flavors).

### File I/O
- **No streaming** (`tokio::AsyncRead`/`Write`) — all tools read/write entire files synchronously inside `spawn_blocking`. For very large docs, full-file in-memory is the cost. Bound at OS limits (typically multi-GB).
- **No partial-write / atomic-replace via tmp + rename.** A panic mid-write would leave a half-written file. The `spawn_blocking` boundary catches panics → they surface as `Error::Tool`, but the partial file may already be on disk. Not a concern for typical small documents; worth knowing for large generations.
