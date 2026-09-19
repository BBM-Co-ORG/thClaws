//! Streaming preview edits (dev-plan/29 Tier 3.1) — the most visible
//! Telegram UX win: instead of one reply at the end of a turn, post a
//! placeholder and **edit it in place** as the agent generates, so the
//! user watches the answer arrive.
//!
//! Telegram throttles repeated edits to the *same* message hard, so the
//! pure [`PreviewCoalescer`] rate-limits edits to at most one per
//! [`MIN_EDIT_INTERVAL`] and skips no-op edits (text unchanged). The
//! async [`TelegramPreview`] wraps it with the network side: lazily send
//! the first preview as a new message, edit it on later deltas, and on
//! [`TelegramPreview::finish`] replace it with the final (fully
//! formatted, chunked) reply.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::client::TelegramClient;
use super::filter::{format_for_telegram, format_preview};
use super::protocol::{EditMessageText, SendMessage};
use super::topic::thread_id_for_send;

/// Minimum gap between edits to the live preview message. Telegram
/// throttles same-message edits well below the global 30/sec; ~1.2s is a
/// smooth, safe cadence. (The plan's "≤20/sec/chat" is the global ceiling
/// — same-message editing wants something much gentler.)
pub const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(1200);

/// Pure preview rate-limiter. The caller accumulates the full assistant
/// text and asks, per delta, whether an edit is due. Rendering (clean +
/// HTML-escape + single-message truncation) goes through
/// [`format_preview`].
pub struct PreviewCoalescer {
    last_rendered: String,
    last_edit: Option<Instant>,
    min_interval: Duration,
    ceiling: u32,
}

impl PreviewCoalescer {
    pub fn new(ceiling: u32) -> Self {
        Self {
            last_rendered: String::new(),
            last_edit: None,
            min_interval: MIN_EDIT_INTERVAL,
            ceiling,
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.min_interval = interval;
        self
    }

    /// Given the full accumulated text and the current time, return the
    /// rendered preview to push **iff** the interval has elapsed since the
    /// last edit *and* the rendered text changed. Updates internal state
    /// when it returns `Some`.
    pub fn due(&mut self, full_text: &str, now: Instant) -> Option<String> {
        let rendered = format_preview(full_text, self.ceiling);
        if rendered == self.last_rendered {
            return None;
        }
        let ready = match self.last_edit {
            None => true,
            Some(t) => now.duration_since(t) >= self.min_interval,
        };
        if !ready {
            return None;
        }
        self.last_edit = Some(now);
        self.last_rendered = rendered.clone();
        Some(rendered)
    }

    /// Final render, ignoring the interval — returns the preview text iff
    /// it differs from what was last pushed. Used as a last edit before
    /// the full reply replaces the preview.
    pub fn flush(&mut self, full_text: &str) -> Option<String> {
        let rendered = format_preview(full_text, self.ceiling);
        if rendered == self.last_rendered {
            return None;
        }
        self.last_rendered = rendered.clone();
        Some(rendered)
    }
}

/// What the message handler pushes streaming deltas to. The handler
/// (which owns the agent's event stream) calls [`PreviewSink::update`]
/// with the full text so far on each delta; the impl decides, rate-
/// limited, whether to edit the live Telegram message now.
#[async_trait]
pub trait PreviewSink: Send + Sync {
    async fn update(&self, full_text: &str);
}

struct PreviewState {
    coalescer: PreviewCoalescer,
    /// `Some` once the placeholder/first-preview message has been sent.
    message_id: Option<i64>,
}

/// Edits one Telegram message in place as a turn streams, then swaps in
/// the final reply. Created by the session sink (which has the chat/topic
/// \+ client), handed to the handler as a [`PreviewSink`], and finalised
/// by the sink via [`TelegramPreview::finish`].
pub struct TelegramPreview {
    client: Arc<TelegramClient>,
    chat_id: i64,
    topic: Option<i64>,
    ceiling: u32,
    state: Mutex<PreviewState>,
}

impl TelegramPreview {
    pub fn new(
        client: Arc<TelegramClient>,
        chat_id: i64,
        topic: Option<i64>,
        ceiling: u32,
    ) -> Self {
        Self {
            client,
            chat_id,
            topic,
            ceiling,
            state: Mutex::new(PreviewState {
                coalescer: PreviewCoalescer::new(ceiling),
                message_id: None,
            }),
        }
    }

    fn current_message_id(&self) -> Option<i64> {
        self.state.lock().ok().and_then(|s| s.message_id)
    }

    /// Replace the live preview with the final, fully-formatted reply:
    /// edit the preview message to the first chunk, then send any
    /// remaining chunks into the same topic. If streaming never fired
    /// (no preview message), just send the chunks normally.
    pub async fn finish(&self, final_text: &str) {
        let chunks = format_for_telegram(final_text, self.ceiling);
        let message_id = self.current_message_id();
        let thread = thread_id_for_send(self.topic);
        match (message_id, chunks.split_first()) {
            (Some(id), Some((first, rest))) => {
                let mut edit = EditMessageText::new(self.chat_id, id, first.clone());
                edit.parse_mode = Some(super::protocol::PARSE_MODE_HTML.to_string());
                if let Err(e) = self.client.edit_message_text(&edit).await {
                    eprintln!("[telegram] final preview edit failed: {e}");
                }
                for chunk in rest {
                    let mut m = SendMessage::text(self.chat_id, chunk.clone());
                    m.message_thread_id = thread;
                    if let Err(e) = self.client.send_message(&m).await {
                        eprintln!("[telegram] final overflow chunk failed: {e}");
                        break;
                    }
                }
            }
            (None, Some(_)) => {
                // Streaming never produced a preview message — send the
                // reply as fresh chunked messages.
                for chunk in &chunks {
                    let mut m = SendMessage::text(self.chat_id, chunk.clone());
                    m.message_thread_id = thread;
                    if let Err(e) = self.client.send_message(&m).await {
                        eprintln!("[telegram] reply send failed: {e}");
                        break;
                    }
                }
            }
            // No final text (e.g. tool-only turn). Leave any preview as-is.
            (_, None) => {}
        }
    }
}

#[async_trait]
impl PreviewSink for TelegramPreview {
    async fn update(&self, full_text: &str) {
        // Decide (rate-limited) without holding the lock across the await.
        let due = {
            let Ok(mut s) = self.state.lock() else { return };
            s.coalescer.due(full_text, Instant::now())
        };
        let Some(text) = due else { return };

        let message_id = self.current_message_id();
        match message_id {
            None => {
                // First preview: send a new message, remember its id.
                let mut m = SendMessage::text(self.chat_id, text);
                m.message_thread_id = thread_id_for_send(self.topic);
                match self.client.send_message_returning_id(&m).await {
                    Ok(id) => {
                        if let Ok(mut s) = self.state.lock() {
                            s.message_id = Some(id);
                        }
                    }
                    Err(e) => eprintln!("[telegram] preview send failed: {e}"),
                }
            }
            Some(id) => {
                let mut edit = EditMessageText::new(self.chat_id, id, text);
                edit.parse_mode = Some(super::protocol::PARSE_MODE_HTML.to_string());
                if let Err(e) = self.client.edit_message_text(&edit).await {
                    // A "message is not modified" 400 is harmless; log
                    // others but keep streaming.
                    eprintln!("[telegram] preview edit failed: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_is_due_then_rate_limited() {
        let mut c = PreviewCoalescer::new(4000);
        let t0 = Instant::now();
        // First delta with content ⇒ due immediately.
        assert_eq!(c.due("hello", t0).as_deref(), Some("hello"));
        // More text but within the interval ⇒ suppressed.
        assert_eq!(c.due("hello world", t0 + Duration::from_millis(100)), None);
        // After the interval ⇒ due again with the new text.
        let later = t0 + MIN_EDIT_INTERVAL + Duration::from_millis(1);
        assert_eq!(c.due("hello world", later).as_deref(), Some("hello world"));
    }

    #[test]
    fn unchanged_text_is_never_due() {
        let mut c = PreviewCoalescer::new(4000);
        let t0 = Instant::now();
        assert_eq!(c.due("same", t0).as_deref(), Some("same"));
        // Even past the interval, identical text produces no edit.
        let later = t0 + MIN_EDIT_INTERVAL * 3;
        assert_eq!(c.due("same", later), None);
    }

    #[test]
    fn empty_text_is_not_due() {
        let mut c = PreviewCoalescer::new(4000);
        assert_eq!(c.due("", Instant::now()), None);
        assert_eq!(c.due("   ", Instant::now()), None);
    }

    #[test]
    fn flush_ignores_interval_but_not_no_op() {
        let mut c = PreviewCoalescer::new(4000);
        let t0 = Instant::now();
        c.due("partial", t0);
        // flush right away (within interval) still emits the newer text.
        assert_eq!(
            c.flush("partial and more").as_deref(),
            Some("partial and more")
        );
        // A second flush with no change emits nothing.
        assert_eq!(c.flush("partial and more"), None);
    }

    #[test]
    fn due_renders_through_preview_formatter() {
        let mut c = PreviewCoalescer::new(4000);
        // HTML-escaped + tool-narration stripped via format_preview.
        let out = c.due("⏺ Read(/x)\na < b", Instant::now());
        assert_eq!(out.as_deref(), Some("a &lt; b"));
    }
}
