import { useEffect, useMemo, useState } from "react";
import { ChevronRight, Trash2, X } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";

/// Todo-list sidebar. Subscribes to `chat_todo_update` IPC envelopes
/// from the worker and renders the model's `TodoWrite` checklist as a
/// vertical list on the right edge of the window — same visual rhythm
/// as `PlanSidebar`. The only action is Clear (deletes
/// `.thclaws/state/todos.md`); editing stays with the model. The engine
/// re-sends the list on every `frontend_ready`, so reopening a project
/// shows the previous list immediately.

type TodoStatus = "pending" | "in_progress" | "completed";

type TodoItem = {
  id: string;
  content: string;
  status: TodoStatus;
};

const STATUS_ICON: Record<TodoStatus, string> = {
  pending: "☐",
  in_progress: "◉",
  completed: "✓",
};

const STATUS_COLOR: Record<TodoStatus, string> = {
  pending: "var(--text-secondary)",
  in_progress: "var(--accent)",
  completed: "var(--accent)",
};

export function TodoSidebar() {
  /// Empty array initial state (pre-fix: `null`). The original `null`
  /// design suppressed the sidebar entirely until the first
  /// `chat_todo_update` envelope arrived — meant to avoid a half-
  /// second flash on session boot before the worker's hydration
  /// broadcast landed. Bug: the worker fires the boot
  /// `TodoUpdate(read_todos_from_disk(cwd))` event before
  /// `gui.rs::spawn_event_translator` has subscribed to the broadcast
  /// channel (the worker spawns first; the translator a couple lines
  /// later). Tokio's broadcast channel doesn't retain messages for
  /// late subscribers, so the boot event is silently dropped and
  /// `todos` stays `null` forever — the sidebar never appears even
  /// after the model calls TodoWrite later in the session (subsequent
  /// fires arrive, but `todos === null` only becomes `[]` then immediately
  /// `[items]`, and the user only notices the first time the model
  /// uses it on a multi-step task, not on every session). Starting at
  /// `[]` sidesteps the race: we always render the empty-state panel
  /// from mount; live envelopes patch in whenever they arrive.
  const [todos, setTodos] = useState<TodoItem[]>([]);
  const [dismissed, setDismissed] = useState(false);
  const [confirmClear, setConfirmClear] = useState(false);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "chat_todo_update") {
        const next = (msg.todos as TodoItem[]) ?? [];
        setTodos(next);
        setConfirmClear(false);
        // A non-empty list re-opens the sidebar so a fresh TodoWrite
        // is hard to miss. An empty list does NOT auto-reopen — if
        // the user dismissed it, leave it dismissed.
        if (next.length > 0) setDismissed(false);
      }
    });
    return unsub;
  }, []);

  const counts = useMemo(() => {
    return {
      done: todos.filter((t) => t.status === "completed").length,
      in_progress: todos.filter((t) => t.status === "in_progress").length,
      total: todos.length,
    };
  }, [todos]);

  /// Empty list → render nothing at all. The sidebar has nothing to
  /// show until the model calls `TodoWrite`; previously we rendered a
  /// "No todos yet" empty-state panel that opened unsolicited on
  /// every session start (and alongside the research sidebar on
  /// `/research` launch). Wait until todos exist, then auto-open.
  if (todos.length === 0) return null;

  // Collapsed: chevron tab on the right edge re-opens.
  if (dismissed) {
    return (
      <button
        type="button"
        onClick={() => setDismissed(false)}
        className="flex items-center justify-center shrink-0 border-l"
        style={{
          width: "20px",
          background: "var(--bg-secondary)",
          borderColor: "var(--border)",
          color: "var(--text-secondary)",
          cursor: "pointer",
        }}
        title={`Todos: ${counts.done}/${counts.total} done${
          counts.in_progress > 0 ? ` · ${counts.in_progress} in progress` : ""
        }`}
      >
        <ChevronRight size={14} style={{ transform: "rotate(180deg)" }} />
      </button>
    );
  }

  return (
    <div
      className="flex flex-col shrink-0 border-l"
      style={{
        width: "260px",
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
      }}
    >
      <div
        className="flex items-center justify-between px-3 py-2 border-b shrink-0"
        style={{ borderColor: "var(--border)" }}
      >
        <div
          className="text-[10px] uppercase tracking-wider flex items-center gap-2"
          style={{ color: "var(--text-secondary)" }}
        >
          <span>Todos</span>
          {todos.length > 0 && (
            <span
              className="px-1.5 py-px rounded"
              style={{
                fontSize: "9px",
                background: "var(--bg-tertiary)",
                color: "var(--text-secondary)",
                border: "1px solid var(--border)",
              }}
              title="Casual scratchpad — for structured plans use /plan"
            >
              SCRATCHPAD
            </span>
          )}
        </div>
        <div className="flex items-center gap-1">
          <button
            type="button"
            onClick={() => setConfirmClear(true)}
            className="p-0.5 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="Clear all todos (deletes .thclaws/state/todos.md)"
          >
            <Trash2 size={13} />
          </button>
          <button
            type="button"
            onClick={() => {
              // When every todo is `completed` and the user closes the
              // sidebar, drop the on-disk file too. The "all done"
              // checklist would otherwise hydrate back on next session
              // start as a stale list of all-checked items.
              const allDone =
                todos.length > 0 && todos.every((t) => t.status === "completed");
              if (allDone) {
                send({ type: "clear_todos" });
                setTodos([]);
              }
              setDismissed(true);
            }}
            className="p-0.5 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="Hide sidebar (all-done lists are cleared from .thclaws/state/todos.md; in-progress lists are kept)"
          >
            <X size={14} />
          </button>
        </div>
      </div>

      {confirmClear && (
        <div
          className="px-3 py-2 border-b shrink-0 text-xs flex items-center justify-between gap-2"
          style={{ borderColor: "var(--border)", color: "var(--text-primary)" }}
        >
          <span>Clear all {counts.total} todos?</span>
          <span className="flex gap-1 shrink-0">
            <button
              type="button"
              onClick={() => {
                send({ type: "clear_todos" });
                setTodos([]);
                setConfirmClear(false);
              }}
              className="px-2 py-0.5 rounded"
              style={{ background: "var(--accent)", color: "var(--bg-primary)" }}
            >
              Clear
            </button>
            <button
              type="button"
              onClick={() => setConfirmClear(false)}
              className="px-2 py-0.5 rounded hover:bg-white/10"
              style={{ color: "var(--text-secondary)" }}
            >
              Cancel
            </button>
          </span>
        </div>
      )}

      <div className="flex-1 overflow-auto">
        <ul className="px-3 py-2 space-y-1.5">
          {todos.map((todo, idx) => {
              const status = (todo.status as TodoStatus) ?? "pending";
              const inProgress = status === "in_progress";
              const done = status === "completed";
              return (
                <li
                  key={todo.id}
                  className="flex items-start gap-2 text-xs leading-snug"
                  style={{
                    color: done
                      ? "var(--text-secondary)"
                      : "var(--text-primary)",
                    textDecoration: done ? "line-through" : "none",
                    opacity: done ? 0.7 : 1,
                  }}
                >
                  <span
                    className="font-mono shrink-0"
                    style={{
                      color: STATUS_COLOR[status],
                      width: "16px",
                      textAlign: "center",
                    }}
                  >
                    {STATUS_ICON[status]}
                  </span>
                  <span
                    className="font-mono shrink-0"
                    style={{
                      color: "var(--text-secondary)",
                      width: "20px",
                      fontSize: "10px",
                      paddingTop: "2px",
                    }}
                  >
                    {idx + 1}.
                  </span>
                  <span
                    style={{
                      fontWeight: inProgress ? 600 : 400,
                    }}
                  >
                    {todo.content}
                  </span>
              </li>
            );
          })}
        </ul>
      </div>

      <div
        className="px-3 py-1.5 border-t shrink-0 flex items-center justify-between"
        style={{
          borderColor: "var(--border)",
          fontSize: "10px",
          color: "var(--text-secondary)",
        }}
      >
        <span>
          {counts.done} / {counts.total} done
          {counts.in_progress > 0 && (
            <span style={{ color: "var(--accent)", marginLeft: "6px" }}>
              · {counts.in_progress} in progress
            </span>
          )}
        </span>
        <span style={{ opacity: 0.6 }}>.thclaws/state/todos.md</span>
      </div>
    </div>
  );
}
