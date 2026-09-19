//! Format thClaws output for Telegram (HTML parse mode).
//!
//! Telegram constraints (Tier 1):
//! - Single text message capped at 4096 chars; we chunk below
//!   `output_ceiling` (default 4000).
//! - HTML parse mode renders a small tag set (`<b> <i> <code> <pre>
//!   <a> …`); any literal `< > &` in the text MUST be escaped or the
//!   API rejects the whole message with "can't parse entities".
//! - ANSI escape sequences and the GUI's tool-call narration are
//!   noise on a phone — stripped via [`crate::line::filter`]'s shared
//!   `clean_for_stream` (the LINE adapter already solved this).
//!
//! ### Why chunk-then-escape (not escape-then-chunk)
//!
//! Splitting *escaped* text by length can cut an entity in half
//! (`&amp;` → `&am` + `p;`) or leave an unclosed `<pre>`, both of
//! which Telegram 400s on. Instead we chunk the **plain** text on line
//! boundaries first, then escape each chunk as a self-contained unit.
//! An entity therefore never spans a chunk, and a fenced code block
//! that happens to straddle a boundary degrades safely: the chunk's
//! unbalanced fences fail the balance check and fall back to plain
//! escaping rather than emitting a dangling tag.

/// Telegram's hard per-message ceiling (chars). We chunk strictly
/// below the configurable `output_ceiling`, which itself defaults
/// below this.
pub const TELEGRAM_MAX_CHARS: usize = 4096;

/// HTML-escape the three characters Telegram's HTML parser is
/// sensitive to. Order matters: `&` first so we don't double-escape
/// the `&` we introduce for `<`/`>`.
pub fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Full pipeline: clean (ANSI + tool-narration strip) → chunk plain
/// text → HTML-render each chunk. Returns one string per outbound
/// `sendMessage`. Empty input yields an empty vec (caller sends
/// nothing).
pub fn format_for_telegram(body: &str, ceiling: u32) -> Vec<String> {
    let cleaned = crate::line::filter::clean_for_stream(body);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return Vec::new();
    }
    let ceiling = (ceiling as usize).clamp(1, TELEGRAM_MAX_CHARS);
    chunk_plain(cleaned, ceiling)
        .into_iter()
        .map(|chunk| render_html(&chunk))
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// Render a **single-message** live preview (dev-plan/29 Tier 3.1
/// streaming preview edits). Unlike [`format_for_telegram`] this never
/// chunks — a preview edits one message in place — so it head-truncates
/// the cleaned text to `ceiling` chars (UTF-8 safe) with a trailing `…`
/// when clipped, then HTML-escapes the (truncated) plain text as a whole
/// so no entity is ever split. Returns an empty string for empty input.
pub fn format_preview(body: &str, ceiling: u32) -> String {
    let cleaned = crate::line::filter::clean_for_stream(body);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return String::new();
    }
    let ceiling = (ceiling as usize).clamp(1, TELEGRAM_MAX_CHARS);
    let char_count = cleaned.chars().count();
    let plain = if char_count > ceiling {
        // Reserve one char for the ellipsis cursor.
        let head: String = cleaned.chars().take(ceiling.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        cleaned.to_string()
    };
    // Escape the whole truncated plain string at once (no entity split).
    // Code-fence `<pre>` handling is skipped for the live preview — a
    // half-streamed fence would be unbalanced anyway; the final reply
    // goes through `format_for_telegram` with full fence handling.
    escape_html(&plain)
}

/// Split plain text into pieces of at most `ceiling` chars, preferring
/// to break on line boundaries. A single line longer than `ceiling` is
/// hard-split on a char boundary (UTF-8 safe — we collect `char`s).
fn chunk_plain(text: &str, ceiling: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize; // chars in `cur`

    let push_cur = |chunks: &mut Vec<String>, cur: &mut String, cur_len: &mut usize| {
        if !cur.is_empty() {
            chunks.push(std::mem::take(cur));
            *cur_len = 0;
        }
    };

    for line in text.split_inclusive('\n') {
        let line_len = line.chars().count();
        if line_len > ceiling {
            // Flush what we have, then hard-split the oversized line.
            push_cur(&mut chunks, &mut cur, &mut cur_len);
            let mut piece = String::new();
            let mut piece_len = 0;
            for ch in line.chars() {
                if piece_len + 1 > ceiling {
                    chunks.push(std::mem::take(&mut piece));
                    piece_len = 0;
                }
                piece.push(ch);
                piece_len += 1;
            }
            if !piece.is_empty() {
                cur = piece;
                cur_len = piece_len;
            }
            continue;
        }
        if cur_len + line_len > ceiling {
            push_cur(&mut chunks, &mut cur, &mut cur_len);
        }
        cur.push_str(line);
        cur_len += line_len;
    }
    push_cur(&mut chunks, &mut cur, &mut cur_len);
    chunks
}

/// Render one plain chunk to Telegram HTML. Fenced ``` blocks within a
/// *balanced* chunk become `<pre>…</pre>` with their inner text
/// escaped; everything else is escaped inline. An unbalanced chunk
/// (odd number of fences — e.g. a code block split across the chunk
/// boundary) is escaped wholesale with no `<pre>`, so we never emit a
/// dangling tag.
fn render_html(chunk: &str) -> String {
    let segments: Vec<&str> = chunk.split("```").collect();
    // n fences → n+1 segments. Balanced pairs ⇒ even fences ⇒ odd
    // segment count. Anything else is unbalanced.
    if segments.len().is_multiple_of(2) {
        return escape_html(chunk);
    }
    let mut out = String::with_capacity(chunk.len() + 16);
    for (i, seg) in segments.iter().enumerate() {
        if i % 2 == 0 {
            // Prose between/around code fences.
            out.push_str(&escape_html(seg));
        } else {
            // Code block. Drop an optional leading language hint
            // (```rust\n…) — Telegram's <pre> doesn't render it.
            let code = strip_lang_hint(seg);
            out.push_str("<pre>");
            out.push_str(&escape_html(code));
            out.push_str("</pre>");
        }
    }
    out
}

/// If a fenced block opens with a bare language token on its own line
/// (`rust\n`, `python\n`, `\n`), drop that line. Matches only a
/// wordless first line so it can't eat a real line of code.
fn strip_lang_hint(code: &str) -> &str {
    let Some(nl) = code.find('\n') else {
        return code;
    };
    let first = &code[..nl];
    let is_lang = !first.is_empty()
        && first
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'+' || b == b'-' || b == b'#');
    // A leading blank line (```\n) is also a hint-less opener worth
    // trimming so the <pre> doesn't start with an empty line.
    if first.is_empty() || is_lang {
        &code[nl + 1..]
    } else {
        code
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_only_html_sensitive_chars() {
        assert_eq!(
            escape_html("a < b && c > d"),
            "a &lt; b &amp;&amp; c &gt; d"
        );
        // No double-escape of the ampersands we introduce.
        assert_eq!(escape_html("<&>"), "&lt;&amp;&gt;");
    }

    #[test]
    fn short_plain_text_is_one_escaped_chunk() {
        let out = format_for_telegram("The answer is 42.", 4000);
        assert_eq!(out, vec!["The answer is 42.".to_string()]);
    }

    #[test]
    fn html_special_chars_escape_in_output() {
        // Acceptance test: `<`, `>`, `&` escape correctly.
        let out = format_for_telegram("if a < b && b > c { ok }", 4000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "if a &lt; b &amp;&amp; b &gt; c { ok }");
    }

    #[test]
    fn strips_ansi_and_tool_narration_via_line_filter() {
        let input = "\x1b[2m[tool: Read /tmp/x]\x1b[0m\n⏺ Read(/x.rs)\nDone & dusted.";
        let out = format_for_telegram(input, 4000);
        assert_eq!(out, vec!["Done &amp; dusted.".to_string()]);
    }

    #[test]
    fn fenced_code_block_becomes_pre() {
        let input = "Here:\n```rust\nlet x = a < b;\n```\nthat's it";
        let out = format_for_telegram(input, 4000);
        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert!(s.contains("<pre>let x = a &lt; b;\n</pre>"), "got: {s}");
        // Prose around the block is still escaped (no raw tags leak).
        assert!(s.starts_with("Here:\n"));
        assert!(s.ends_with("that's it"));
        // Language hint dropped.
        assert!(!s.contains("rust"));
    }

    #[test]
    fn long_text_chunks_below_ceiling() {
        // 30 lines of 200 chars → must split into multiple ≤ceiling
        // chunks. Use a small ceiling to exercise the path cheaply.
        let body = (0..30)
            .map(|i| format!("line {i} ").repeat(20))
            .collect::<Vec<_>>()
            .join("\n");
        let out = format_for_telegram(&body, 500);
        assert!(out.len() > 1, "expected multiple chunks, got {}", out.len());
        for chunk in &out {
            assert!(
                chunk.chars().count() <= TELEGRAM_MAX_CHARS,
                "chunk over hard ceiling"
            );
        }
        // No content lost: concatenated chunk char count ≈ source
        // (escaping only grew nothing here — no <>& present).
        let rejoined: String = out.join("");
        assert!(rejoined.contains("line 0"));
        assert!(rejoined.contains("line 29"));
    }

    #[test]
    fn single_oversized_line_hard_splits() {
        let body = "x".repeat(1200);
        let out = format_for_telegram(&body, 500);
        assert_eq!(out.len(), 3); // 500 + 500 + 200
        assert_eq!(out[0].chars().count(), 500);
        assert_eq!(out[2].chars().count(), 200);
    }

    #[test]
    fn unbalanced_fence_falls_back_to_plain_escape() {
        // An unterminated ``` (e.g. a code block split by chunking)
        // must NOT emit a dangling <pre> — escape the whole chunk.
        let chunk = "before ```rust\nlet x = a < b;";
        let rendered = render_html(chunk);
        assert!(!rendered.contains("<pre>"), "got: {rendered}");
        assert!(rendered.contains("&lt;"));
        assert!(rendered.contains("```"));
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        assert!(format_for_telegram("", 4000).is_empty());
        assert!(format_for_telegram("   \n  ", 4000).is_empty());
        // Tool-narration-only input also collapses to nothing.
        assert!(format_for_telegram("⏺ Read(/x)\n✓", 4000).is_empty());
    }

    #[test]
    fn thai_text_survives_chunking() {
        let body = "สวัสดีครับ ".repeat(100);
        let out = format_for_telegram(&body, 200);
        assert!(out.len() > 1);
        let rejoined: String = out.join("");
        assert!(rejoined.contains("สวัสดีครับ"));
    }

    #[test]
    fn preview_is_single_message_escaped() {
        assert_eq!(format_preview("a < b & c", 4000), "a &lt; b &amp; c");
        // ANSI / tool narration stripped, like the chunked path.
        assert_eq!(
            format_preview("⏺ Read(/x)\nDone & dusted.", 4000),
            "Done &amp; dusted."
        );
        assert_eq!(format_preview("", 4000), "");
    }

    #[test]
    fn preview_head_truncates_with_ellipsis() {
        let out = format_preview(&"x".repeat(1000), 100);
        assert_eq!(out.chars().count(), 100); // 99 x's + '…'
        assert!(out.ends_with('…'));
        // Truncation is on a char boundary for multibyte text.
        let thai = format_preview(&"ก".repeat(1000), 50);
        assert!(thai.ends_with('…'));
        assert_eq!(thai.chars().count(), 50);
    }
}
