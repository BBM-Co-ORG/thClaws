//! TodoWrite tool — write/update the project's todo list.
//!
//! Stores todos in `.thclaws/state/todos.md` (markdown format).
//! Input: `{ "todos": [{"id": "1", "content": "Fix bug", "status": "in_progress"|"pending"|"completed"}] }`
//! The tool overwrites the entire todo list (full state replacement, not append).
//!
//! M6.30 audit fixes (`dev-log/146`):
//! - TW1: refuse if `.thclaws/` is a symlink (`std::fs::write` follows symlinks
//!   by default; an attacker-planted symlink would let writes escape the project root)
//! - TW2: sanitize `id` + `content` — reject newlines / tabs / control chars
//!   (corrupt the markdown line structure and downstream reminder parser)
//! - TW3: validate `status` server-side — provider compliance with JSON Schema
//!   `enum` varies; pre-fix unknown values silently degraded to `pending`
//! - TW4: reject duplicate `id` values — pre-fix the file kept both bullets,
//!   the frontend logged React key collisions, the next read was ambiguous

use super::Tool;
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Per-field length cap. IDs are slug-like (id="1", id="abc-3"); 64 is generous.
const MAX_ID_LEN: usize = 64;
/// Content is a one-line task description; 500 chars is well above any
/// reasonable use. Forces longer descriptions into a separate notes file.
const MAX_CONTENT_LEN: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
}

impl TodoItem {
    /// Render as a markdown checklist line.
    fn to_markdown(&self) -> String {
        let checkbox = match self.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[-]",
            _ => "[ ]", // pending or unknown
        };
        format!("- {} {} (id: {})", checkbox, self.content, self.id)
    }
}

pub struct TodoWriteTool;

impl TodoWriteTool {
    fn parse_todos(input: &Value) -> Result<Vec<TodoItem>> {
        let todos_val = input
            .get("todos")
            .ok_or_else(|| Error::Tool("missing required field: todos".into()))?;

        serde_json::from_value(todos_val.clone())
            .map_err(|e| Error::Tool(format!("invalid todos array: {e}")))
    }

    /// M6.30 TW2: validate a string field — reject empty, control chars
    /// (`\n`, `\r`, `\t`, `\0`, etc.), and oversize. Newlines especially
    /// are a real footgun: a multi-line `content` would corrupt the
    /// markdown bullet structure (the second line wouldn't be a `[ ]`
    /// bullet) AND poison the `build_todos_reminder` parser.
    fn sanitize_field(s: &str, field: &str, max_len: usize) -> Result<()> {
        if s.is_empty() {
            return Err(Error::Tool(format!("{field} must not be empty")));
        }
        if let Some(c) = s.chars().find(|c| c.is_control()) {
            // Pretty-print the offender for the error message — the
            // model's first attempt at fixing this needs to know what
            // character to remove.
            let printable = match c {
                '\n' => "newline (\\n)".to_string(),
                '\r' => "carriage-return (\\r)".to_string(),
                '\t' => "tab (\\t)".to_string(),
                '\0' => "null byte (\\0)".to_string(),
                other => format!("U+{:04X}", other as u32),
            };
            return Err(Error::Tool(format!(
                "{field} must not contain control characters (found {printable})"
            )));
        }
        if s.len() > max_len {
            return Err(Error::Tool(format!(
                "{field} too long ({} chars; max {max_len})",
                s.len()
            )));
        }
        Ok(())
    }

    /// M6.30 TW3: validate `status` server-side. JSON Schema `enum` is
    /// sent to the provider but compliance varies — Anthropic and
    /// OpenAI usually respect, Gemini and local Ollama may not. Pre-fix
    /// an unknown value silently rendered as `[ ]` AND counted as
    /// neither pending/in_progress/completed (counter showed 0/0/0
    /// while the file had a bullet) — model thought state was lost.
    fn validate_status(s: &str) -> Result<()> {
        match s {
            "pending" | "in_progress" | "completed" => Ok(()),
            other => Err(Error::Tool(format!(
                "invalid status '{other}' — must be 'pending' | 'in_progress' | 'completed'"
            ))),
        }
    }

    /// M6.30 TW4: reject duplicate ids. Pre-fix two todos with the same
    /// id produced duplicate bullets in the file, React key warnings
    /// in the frontend (only first rendered correctly), and ambiguous
    /// state on the next round-trip.
    fn check_unique_ids(todos: &[TodoItem]) -> Result<()> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(todos.len());
        for t in todos {
            if !seen.insert(&t.id) {
                return Err(Error::Tool(format!(
                    "duplicate todo id: '{}' — every todo must have a unique id",
                    t.id
                )));
            }
        }
        Ok(())
    }

    /// M6.30 TW1: refuse to write if `.thclaws/` is a symlink. Pre-fix
    /// `std::fs::write` followed the symlink, letting an attacker-
    /// planted `.thclaws -> /tmp/anywhere` symlink in the project root
    /// escape the sandbox carve-out. Verified empirically: a 30-line
    /// repro confirmed the write lands at the symlink target. Same
    /// defense pattern as `kms::writable_page_path` and
    /// `memory::writable_entry_path`.
    fn check_thclaws_not_symlinked(root: &Path) -> Result<()> {
        let dir = root.join(".thclaws");
        if let Ok(md) = std::fs::symlink_metadata(&dir) {
            if md.file_type().is_symlink() {
                return Err(Error::Tool(
                    "refusing to write — .thclaws/ is a symlink. Remove the \
                     symlink (or its target) and let TodoWrite create a real \
                     directory."
                        .into(),
                ));
            }
        }
        // Doesn't exist yet → create_dir_all below will mkdir it as a
        // real directory; no defense needed.
        Ok(())
    }

    /// Validate every field across every todo before any disk write.
    /// Single pass — first error wins. The model receives a specific
    /// error pointing at the offender.
    fn validate_todos(todos: &[TodoItem]) -> Result<()> {
        for (idx, t) in todos.iter().enumerate() {
            Self::sanitize_field(&t.id, &format!("todos[{idx}].id"), MAX_ID_LEN)?;
            Self::sanitize_field(
                &t.content,
                &format!("todos[{idx}].content"),
                MAX_CONTENT_LEN,
            )?;
            Self::validate_status(&t.status)?;
        }
        Self::check_unique_ids(todos)?;
        Ok(())
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &'static str {
        "TodoWrite"
    }

    fn description(&self) -> &'static str {
        "Casual scratchpad for YOUR OWN task tracking during informal \
         multi-step work — writes to .thclaws/state/todos.md as a markdown \
         checklist. The user sees it live in the Todos sidebar and can \
         clear it themselves (`/todo clear`). No driver, no sequential \
         enforcement.\n\n\
         \
         **At session start, if `.thclaws/state/todos.md` already exists, read \
         it first.** Incomplete items (pending or in_progress) are work \
         from a prior session — surface them and either resume or \
         replace based on the user's intent. Don't silently start fresh \
         on top of stale work.\n\n\
         \
         For STRUCTURED PLANS the user wants to review and watch you \
         execute step by step (sidebar with checkmarks, sequential \
         gating, per-step verification, audit), use EnterPlanMode → \
         SubmitPlan instead. TodoWrite is the lower-ceremony tool for \
         work that doesn't need user approval.\n\n\
         \
         Discipline: mark ONE item `in_progress` at a time before \
         starting it; mark `completed` IMMEDIATELY after finishing \
         (don't batch); remove items that are no longer relevant. \
         Never mark `completed` if tests are failing or the work is \
         partial. Each todo has an id, content string, and status \
         (pending, in_progress, or completed)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The complete list of todo items. This replaces the entire existing list.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Unique identifier for the todo item"
                            },
                            "content": {
                                "type": "string",
                                "description": "Description of the task"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Current status of the todo item"
                            }
                        },
                        "required": ["id", "content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let todos = Self::parse_todos(&input)?;
        write_todos(Path::new("."), &todos)
    }
}

fn todos_path(root: &Path) -> PathBuf {
    root.join(".thclaws").join("state").join("todos.md")
}

/// The one write path for todos.md — TodoWrite, the `/todo` command and
/// the sidebar all go through here so validation, the symlink guard and
/// the sidebar broadcast can't drift apart.
pub fn write_todos(root: &Path, todos: &[TodoItem]) -> Result<String> {
    // M6.30: validate ALL inputs before touching disk. First error
    // wins; nothing written.
    TodoWriteTool::validate_todos(todos)?;

    let mut md = String::from("# Todos\n\n");
    if todos.is_empty() {
        md.push_str("_No todos._\n");
    } else {
        for todo in todos {
            md.push_str(&todo.to_markdown());
            md.push('\n');
        }
    }

    TodoWriteTool::check_thclaws_not_symlinked(root)?;

    let path = todos_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Tool(format!("failed to create .thclaws dir: {e}")))?;
    }
    std::fs::write(&path, &md)
        .map_err(|e| Error::Tool(format!("failed to write todos.md: {e}")))?;

    super::todo_state::fire(todos.to_vec());

    let completed = todos.iter().filter(|t| t.status == "completed").count();
    let in_progress = todos.iter().filter(|t| t.status == "in_progress").count();
    let pending = todos.iter().filter(|t| t.status == "pending").count();

    Ok(format!(
        "Wrote {} todo(s) to .thclaws/state/todos.md ({} pending, {} in progress, {} completed)",
        todos.len(),
        pending,
        in_progress,
        completed,
    ))
}

/// Delete todos.md and broadcast the empty list. Removing (rather than
/// writing `_No todos._`) keeps a cleared workspace identical to one
/// that never had a list. Returns whether a file was removed.
pub fn clear_todos(root: &Path) -> bool {
    let removed = std::fs::remove_file(todos_path(root)).is_ok();
    super::todo_state::fire(Vec::new());
    removed
}

/// `chat_todo_update` frame for a (re)connecting page. The worker's boot
/// broadcast lands before any page subscribes, so without this the
/// sidebar stays empty until the model next calls TodoWrite.
pub fn todo_update_frame(root: &Path) -> String {
    json!({
        "type": "chat_todo_update",
        "todos": read_todos_from_disk(root),
    })
    .to_string()
}

/// Plain-text listing for the `/todo` command (CLI + GUI chat).
pub fn render_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "no todos (.thclaws/state/todos.md)".into();
    }
    let done = todos.iter().filter(|t| t.status == "completed").count();
    let mut out = format!(
        "todos — {done}/{} done (.thclaws/state/todos.md)\n",
        todos.len()
    );
    for (i, t) in todos.iter().enumerate() {
        let mark = match t.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[-]",
            _ => "[ ]",
        };
        out.push_str(&format!("  {}. {mark} {}\n", i + 1, t.content));
    }
    out.push_str("clear with /todo clear");
    out
}

/// Parse `.thclaws/state/todos.md` back into a `Vec<TodoItem>` so the GUI
/// worker can hydrate the sidebar at session boot. Mirror of the
/// `to_markdown()` writer — recognizes `[x]` / `[-]` / `[ ]` and
/// `(id: <id>)` at end of line. Ignores blank lines, headings, and
/// the empty-state placeholder. Returns an empty vec on any I/O or
/// parse trouble (the sidebar simply shows nothing — better than
/// crashing the worker).
pub fn read_todos_from_disk(root: &Path) -> Vec<TodoItem> {
    let path = todos_path(root);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim_start();
        let after_dash = match trimmed.strip_prefix("- ") {
            Some(s) => s,
            None => continue,
        };
        let (status, after_box) = if let Some(rest) = after_dash.strip_prefix("[x] ") {
            ("completed", rest)
        } else if let Some(rest) = after_dash.strip_prefix("[-] ") {
            ("in_progress", rest)
        } else if let Some(rest) = after_dash.strip_prefix("[ ] ") {
            ("pending", rest)
        } else {
            continue;
        };
        // Split off ` (id: <id>)` suffix; require both pieces.
        let (content, id) = match after_box.rsplit_once(" (id: ") {
            Some((c, id_with_paren)) => match id_with_paren.strip_suffix(')') {
                Some(id) => (c.to_string(), id.to_string()),
                None => continue,
            },
            None => continue,
        };
        out.push(TodoItem {
            id,
            content,
            status: status.into(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_todos_creates_markdown() {
        let dir = tempfile::tempdir().unwrap();

        let todos = vec![
            TodoItem {
                id: "1".into(),
                content: "Fix bug".into(),
                status: "completed".into(),
            },
            TodoItem {
                id: "2".into(),
                content: "Add tests".into(),
                status: "in_progress".into(),
            },
            TodoItem {
                id: "3".into(),
                content: "Deploy".into(),
                status: "pending".into(),
            },
        ];

        let result = write_todos(dir.path(), &todos).unwrap();
        assert!(result.contains("3 todo(s)"));
        assert!(result.contains("1 pending"));
        assert!(result.contains("1 in progress"));
        assert!(result.contains("1 completed"));

        let contents = std::fs::read_to_string(dir.path().join(".thclaws/state/todos.md")).unwrap();
        assert!(contents.contains("# Todos"));
        assert!(contents.contains("[x] Fix bug (id: 1)"));
        assert!(contents.contains("[-] Add tests (id: 2)"));
        assert!(contents.contains("[ ] Deploy (id: 3)"));
    }

    #[tokio::test]
    async fn write_empty_todos() {
        let dir = tempfile::tempdir().unwrap();

        let result = write_todos(dir.path(), &[]).unwrap();
        assert!(result.contains("0 todo(s)"));

        let contents = std::fs::read_to_string(dir.path().join(".thclaws/state/todos.md")).unwrap();
        assert!(contents.contains("_No todos._"));
    }

    #[tokio::test]
    async fn overwrites_existing_todos() {
        let dir = tempfile::tempdir().unwrap();

        // First write
        let todos1 = vec![TodoItem {
            id: "1".into(),
            content: "Old task".into(),
            status: "pending".into(),
        }];
        write_todos(dir.path(), &todos1).unwrap();

        // Second write (full replacement)
        let todos2 = vec![TodoItem {
            id: "2".into(),
            content: "New task".into(),
            status: "completed".into(),
        }];
        write_todos(dir.path(), &todos2).unwrap();

        let contents = std::fs::read_to_string(dir.path().join(".thclaws/state/todos.md")).unwrap();
        assert!(!contents.contains("Old task"));
        assert!(contents.contains("[x] New task (id: 2)"));
    }

    #[tokio::test]
    async fn missing_todos_field_errors() {
        let tool = TodoWriteTool;
        let err = tool.call(json!({})).await.unwrap_err();
        assert!(format!("{err}").contains("missing required field"));
    }

    #[test]
    fn tool_metadata() {
        let tool = TodoWriteTool;
        assert_eq!(tool.name(), "TodoWrite");
        assert!(tool.requires_approval(&json!({})));
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["todos"].is_object());
    }

    #[test]
    fn description_positions_todowrite_as_scratchpad() {
        // The description must clearly mark TodoWrite as a scratchpad
        // (not a structured plan tool) and point users at SubmitPlan
        // for the structured workflow. This is the load-bearing
        // guidance that keeps the model from blurring the two tools.
        let d = TodoWriteTool.description();
        assert!(d.contains("scratchpad"), "scratchpad framing missing: {d}",);
        assert!(
            d.contains("SubmitPlan"),
            "must point at SubmitPlan for structured plans: {d}",
        );
        assert!(
            d.contains("EnterPlanMode"),
            "must mention plan mode entry: {d}",
        );
    }

    #[test]
    fn description_tells_model_to_resume_from_existing_todos_md() {
        // When .thclaws/state/todos.md already exists at session start, the
        // model should read it and surface incomplete work — not
        // silently start fresh on top of a stale list. The tool
        // description carries this rule because the system prompt
        // alone may not survive truncation in long sessions.
        let d = TodoWriteTool.description();
        assert!(
            d.contains(".thclaws/state/todos.md"),
            "must name the todos file path: {d}",
        );
        assert!(
            d.contains("read it first") || d.contains("read it"),
            "must instruct to read existing todos at session start: {d}",
        );
        assert!(
            d.contains("resume or") || d.contains("resume"),
            "must mention resume option for existing incomplete todos: {d}",
        );
    }

    #[test]
    fn description_includes_iteration_discipline() {
        // One item in_progress at a time, mark completed immediately,
        // remove stale items — these are the rules borrowed from
        // Claude Code's TodoWrite prompt that prevent the casual
        // scratchpad from drifting into incoherence.
        let d = TodoWriteTool.description();
        assert!(
            d.contains("ONE item") || d.contains("one"),
            "single-in-progress rule missing: {d}",
        );
        assert!(
            d.contains("IMMEDIATELY") || d.contains("immediately"),
            "mark-completed-immediately rule missing: {d}",
        );
        assert!(
            d.contains("don't batch") || d.contains("don't batch"),
            "no-batching rule missing: {d}",
        );
    }

    #[test]
    fn read_todos_from_disk_round_trips_known_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let todos = vec![
            TodoItem {
                id: "1".into(),
                content: "Fix bug".into(),
                status: "completed".into(),
            },
            TodoItem {
                id: "abc-2".into(),
                content: "Add tests".into(),
                status: "in_progress".into(),
            },
            TodoItem {
                id: "3".into(),
                content: "Deploy".into(),
                status: "pending".into(),
            },
        ];
        write_todos(dir.path(), &todos).unwrap();
        let parsed = read_todos_from_disk(dir.path());
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].id, "1");
        assert_eq!(parsed[0].content, "Fix bug");
        assert_eq!(parsed[0].status, "completed");
        assert_eq!(parsed[1].id, "abc-2");
        assert_eq!(parsed[1].status, "in_progress");
        assert_eq!(parsed[2].status, "pending");
    }

    #[test]
    fn read_todos_from_disk_returns_empty_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        // No .thclaws/state/todos.md written.
        assert!(read_todos_from_disk(dir.path()).is_empty());
    }

    #[test]
    fn clear_todos_removes_file_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let todos = vec![TodoItem {
            id: "1".into(),
            content: "Stale".into(),
            status: "in_progress".into(),
        }];
        write_todos(dir.path(), &todos).unwrap();
        assert!(clear_todos(dir.path()));
        assert!(!dir.path().join(".thclaws/state/todos.md").exists());
        assert!(read_todos_from_disk(dir.path()).is_empty());
        assert!(
            !clear_todos(dir.path()),
            "second clear has nothing to remove"
        );
    }

    #[test]
    fn write_todos_validates_for_every_caller() {
        let dir = tempfile::tempdir().unwrap();
        let bad = vec![TodoItem {
            id: "1".into(),
            content: "two\nlines".into(),
            status: "pending".into(),
        }];
        assert!(write_todos(dir.path(), &bad).is_err());
        assert!(!dir.path().join(".thclaws/state/todos.md").exists());
    }

    #[test]
    fn todo_update_frame_carries_disk_state() {
        let dir = tempfile::tempdir().unwrap();
        let todos = vec![TodoItem {
            id: "a".into(),
            content: "Resume me".into(),
            status: "pending".into(),
        }];
        write_todos(dir.path(), &todos).unwrap();
        let v: Value = serde_json::from_str(&todo_update_frame(dir.path())).unwrap();
        assert_eq!(v["type"], "chat_todo_update");
        assert_eq!(v["todos"][0]["content"], "Resume me");
        assert_eq!(v["todos"][0]["status"], "pending");
    }

    #[test]
    fn render_todos_numbers_items_and_names_clear() {
        assert!(render_todos(&[]).contains("no todos"));
        let out = render_todos(&[
            TodoItem {
                id: "1".into(),
                content: "Done".into(),
                status: "completed".into(),
            },
            TodoItem {
                id: "2".into(),
                content: "Next".into(),
                status: "pending".into(),
            },
        ]);
        assert!(out.contains("1/2 done"));
        assert!(out.contains("1. [x] Done"));
        assert!(out.contains("2. [ ] Next"));
        assert!(out.contains("/todo clear"));
    }

    #[test]
    fn read_todos_from_disk_skips_empty_state_marker() {
        let dir = tempfile::tempdir().unwrap();
        write_todos(dir.path(), &[]).unwrap();
        // The file contains "_No todos._" — the parser must not
        // treat that as a todo item.
        assert!(read_todos_from_disk(dir.path()).is_empty());
    }

    #[test]
    fn todo_item_markdown_rendering() {
        let completed = TodoItem {
            id: "1".into(),
            content: "Done".into(),
            status: "completed".into(),
        };
        assert_eq!(completed.to_markdown(), "- [x] Done (id: 1)");

        let in_prog = TodoItem {
            id: "2".into(),
            content: "Working".into(),
            status: "in_progress".into(),
        };
        assert_eq!(in_prog.to_markdown(), "- [-] Working (id: 2)");

        let pending = TodoItem {
            id: "3".into(),
            content: "Later".into(),
            status: "pending".into(),
        };
        assert_eq!(pending.to_markdown(), "- [ ] Later (id: 3)");
    }

    #[test]
    fn parse_todos_from_json() {
        let input = json!({
            "todos": [
                {"id": "1", "content": "Test", "status": "pending"}
            ]
        });
        let todos = TodoWriteTool::parse_todos(&input).unwrap();
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].id, "1");
        assert_eq!(todos[0].content, "Test");
        assert_eq!(todos[0].status, "pending");
    }

    // ─── M6.30 audit fixes ────────────────────────────────────────────────

    /// TW2: newlines in `content` would corrupt the markdown bullet
    /// structure (the second line wouldn't be a `[ ]` checkbox bullet)
    /// and poison the `build_todos_reminder` parser. Sanitization
    /// rejects with a specific error message naming the offender.
    #[tokio::test]
    async fn rejects_newline_in_content() {
        let err = TodoWriteTool
            .call(json!({
                "todos": [
                    {"id": "1", "content": "line one\nline two", "status": "pending"}
                ]
            }))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("control characters"), "msg: {msg}");
        assert!(
            msg.contains("newline"),
            "msg should name the offender: {msg}"
        );
    }

    /// TW2: tab in `id` similarly breaks the rendered `(id: ...)` token
    /// when something later parses it.
    #[tokio::test]
    async fn rejects_tab_in_id() {
        let err = TodoWriteTool
            .call(json!({
                "todos": [
                    {"id": "1\tx", "content": "ok", "status": "pending"}
                ]
            }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("tab"));
    }

    /// TW2: empty content / id rejected.
    #[tokio::test]
    async fn rejects_empty_id() {
        let err = TodoWriteTool
            .call(json!({
                "todos": [
                    {"id": "", "content": "ok", "status": "pending"}
                ]
            }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("must not be empty"));
    }

    /// TW2: oversize content rejected (>500 chars).
    #[tokio::test]
    async fn rejects_oversized_content() {
        let huge = "x".repeat(501);
        let err = TodoWriteTool
            .call(json!({
                "todos": [
                    {"id": "1", "content": huge, "status": "pending"}
                ]
            }))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("too long"), "{msg}");
        assert!(msg.contains("max 500"), "{msg}");
    }

    /// TW3: server-side status validation. JSON Schema enum is sent to
    /// providers but compliance varies; this catches off-spec values.
    #[tokio::test]
    async fn rejects_invalid_status() {
        let err = TodoWriteTool
            .call(json!({
                "todos": [
                    {"id": "1", "content": "ok", "status": "InProgress"}
                ]
            }))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid status"), "{msg}");
        assert!(
            msg.contains("InProgress"),
            "msg should echo the bad value: {msg}"
        );
    }

    /// TW3: hyphen variant (common typo) also rejected with helpful message.
    #[tokio::test]
    async fn rejects_hyphenated_status() {
        let err = TodoWriteTool
            .call(json!({
                "todos": [
                    {"id": "1", "content": "ok", "status": "in-progress"}
                ]
            }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid status"));
    }

    /// TW4: duplicate ids rejected. Pre-fix the file kept both bullets,
    /// frontend logged React key collisions, next read was ambiguous.
    #[tokio::test]
    async fn rejects_duplicate_ids() {
        let err = TodoWriteTool
            .call(json!({
                "todos": [
                    {"id": "1", "content": "first", "status": "pending"},
                    {"id": "1", "content": "second", "status": "pending"}
                ]
            }))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("duplicate todo id"), "{msg}");
        assert!(
            msg.contains("'1'"),
            "msg should echo the duplicate id: {msg}"
        );
    }

    /// TW1: `.thclaws/` symlink rejected. Pre-fix `std::fs::write`
    /// followed the symlink, escaping the project root. Verified
    /// empirically before fix.
    ///
    /// Touches process cwd via std::env::set_current_dir — needs the
    /// shared kms test_env_lock to serialize against parallel tests.
    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinked_thclaws_dir() {
        use std::os::unix::fs::symlink;
        let _g = crate::kms::test_env_lock();

        let prev_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let scratch = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::env::set_current_dir(scratch.path()).unwrap();

        // Plant the malicious symlink: .thclaws → outside dir
        symlink(target.path(), scratch.path().join(".thclaws")).unwrap();

        let result = TodoWriteTool
            .call(json!({
                "todos": [
                    {"id": "1", "content": "leak attempt", "status": "pending"}
                ]
            }))
            .await;

        // Restore cwd before any assertion can panic and leave us stranded.
        let _ = std::env::set_current_dir(&prev_cwd);

        let err = result.expect_err("symlinked .thclaws must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("symlink"), "msg should mention symlink: {msg}");

        // Verify the write did NOT escape to the symlink target.
        assert!(
            !target.path().join("todos.md").exists(),
            "write leaked to symlink target despite the rejection",
        );
    }

    /// Sanity: clean inputs still write successfully. (Regression
    /// guard — make sure the new validation chain doesn't reject
    /// legitimate use.)
    #[tokio::test]
    async fn clean_inputs_still_write() {
        let _g = crate::kms::test_env_lock();
        let prev_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let scratch = tempfile::tempdir().unwrap();
        std::env::set_current_dir(scratch.path()).unwrap();

        let result = TodoWriteTool
            .call(json!({
                "todos": [
                    {"id": "1", "content": "normal task", "status": "pending"},
                    {"id": "2", "content": "another", "status": "in_progress"},
                    {"id": "3", "content": "third", "status": "completed"}
                ]
            }))
            .await;

        // Restore cwd before any assertion can panic and leave us stranded.
        let _ = std::env::set_current_dir(&prev_cwd);

        let msg = result.expect("clean inputs must succeed");
        assert!(msg.contains("3 todo(s)"));
        assert!(msg.contains("1 pending"));
        assert!(msg.contains("1 in progress"));
        assert!(msg.contains("1 completed"));
        // File written under the scratch dir.
        let written =
            std::fs::read_to_string(scratch.path().join(".thclaws/state/todos.md")).unwrap();
        assert!(written.contains("- [ ] normal task (id: 1)"));
        assert!(written.contains("- [-] another (id: 2)"));
        assert!(written.contains("- [x] third (id: 3)"));
    }
}
