import { useEffect, useRef, useState } from "react";
import { send, subscribe, type IPCMessage } from "../hooks/useIPC";

type Defaults = {
  cwd?: string;
  cron?: string;
  timeoutSecs?: number;
};

type FormState = {
  id: string;
  cron: string;
  prompt: string;
  cwd: string;
  model: string;
  maxIterations: string;
  timeoutSecs: string;
  disabled: boolean;
  watchWorkspace: boolean;
};

const initialForm = (defaults: Defaults): FormState => ({
  id: "",
  cron: defaults.cron ?? "30 8 * * MON-FRI",
  prompt: "",
  cwd: defaults.cwd ?? "",
  model: "",
  maxIterations: "",
  timeoutSecs: String(defaults.timeoutSecs ?? 600),
  disabled: false,
  watchWorkspace: false,
});

/**
 * Schedule-add form, opened by `/schedule add` from the GUI Chat tab.
 * Subscribes to `schedule_add_open` from the backend (which carries
 * sensible defaults — current cwd, a sample cron) and submits via
 * `schedule_add_submit`. The backend validates required fields + cron
 * syntax + cwd existence, then writes to ~/.config/thclaws/schedules.json
 * and dispatches `schedule_add_result` with `{ok, error?, id?}`.
 *
 * Mirrors the ApprovalModal pattern: self-contained, mounted once in
 * App.tsx, no parent props for visibility — open/close state lives
 * inside the component and is driven by IPC events.
 */
type CronPreview =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ok"; fires: string[] }
  | { kind: "error"; message: string };

// Common cron presets the chips fill in. Order matters — most-frequent
// patterns first so the eye lands on them without scanning.
const CRON_PRESETS: { label: string; cron: string }[] = [
  { label: "Every 5 min", cron: "*/5 * * * *" },
  { label: "Every 30 min", cron: "*/30 * * * *" },
  { label: "Hourly", cron: "0 * * * *" },
  { label: "Daily 9am", cron: "0 9 * * *" },
  { label: "Weekdays 8:30", cron: "30 8 * * MON-FRI" },
  { label: "Weekly Mon 9am", cron: "0 9 * * MON" },
  { label: "Monthly 1st", cron: "0 0 1 * *" },
];

function formatFireTime(rfc3339: string): string {
  // The backend ships RFC 3339 in UTC; render in the user's local
  // timezone since cron expressions are typically read as wall-clock.
  // Fallback to the raw string if parsing fails.
  const d = new Date(rfc3339);
  if (Number.isNaN(d.getTime())) return rfc3339;
  return d.toLocaleString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

export function ScheduleAddModal() {
  const [form, setForm] = useState<FormState | null>(null);
  const [status, setStatus] = useState<
    | { kind: "idle" }
    | { kind: "submitting" }
    | { kind: "ok"; id: string }
    | { kind: "error"; message: string }
  >({ kind: "idle" });
  const [preview, setPreview] = useState<CronPreview>({ kind: "idle" });
  const idInputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "schedule_add_open") {
        const defaults = (msg.defaults as Defaults) ?? {};
        setForm(initialForm(defaults));
        setStatus({ kind: "idle" });
        setPreview({ kind: "idle" });
      } else if (msg.type === "schedule_add_result") {
        if (msg.ok) {
          setStatus({ kind: "ok", id: String(msg.id ?? "") });
          // Auto-dismiss the success state after a short beat so the
          // user sees the green confirm without having to click.
          setTimeout(() => setForm(null), 900);
        } else {
          setStatus({
            kind: "error",
            message: String(msg.error ?? "unknown error"),
          });
        }
      } else if (msg.type === "schedule_cron_preview_result") {
        // Only apply the preview if the request still matches the
        // current cron field — if the user has typed more characters
        // since we sent the IPC, the response is stale.
        const cron = String(msg.cron ?? "");
        setForm((prev) => {
          if (!prev || prev.cron.trim() !== cron) return prev;
          if (msg.ok) {
            const fires = Array.isArray(msg.fires)
              ? (msg.fires as string[])
              : [];
            setPreview({ kind: "ok", fires });
          } else {
            setPreview({
              kind: "error",
              message: String(msg.error ?? "invalid cron"),
            });
          }
          return prev;
        });
      }
    });
    return unsub;
  }, []);

  // Debounced cron-preview IPC: when the user pauses typing for
  // 300ms, ask the backend to validate + project the next 3 fires.
  // Cheap on the backend (pure parser call), no spam during typing.
  useEffect(() => {
    if (!form) {
      // A closed form must not keep the last cron's projection on screen.
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setPreview({ kind: "idle" });
      return;
    }
    const trimmed = form.cron.trim();
    if (!trimmed) {
      setPreview({ kind: "idle" });
      return;
    }
    setPreview({ kind: "loading" });
    const handle = window.setTimeout(() => {
      send({ type: "schedule_cron_preview", cron: trimmed });
    }, 300);
    return () => window.clearTimeout(handle);
  }, [form?.cron]);

  // Auto-focus the id field when the modal opens — most common
  // typing target.
  useEffect(() => {
    if (form && idInputRef.current) {
      idInputRef.current.focus();
    }
  }, [form?.id === ""]);

  if (!form) return null;

  const onChange = <K extends keyof FormState>(key: K, value: FormState[K]) => {
    setForm((prev) => (prev ? { ...prev, [key]: value } : prev));
    if (status.kind === "error") setStatus({ kind: "idle" });
  };

  const onSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!form) return;
    setStatus({ kind: "submitting" });
    const payload: IPCMessage = {
      type: "schedule_add_submit",
      id: form.id,
      cron: form.cron,
      prompt: form.prompt,
      cwd: form.cwd,
      disabled: form.disabled,
      watchWorkspace: form.watchWorkspace,
    };
    if (form.model.trim()) payload.model = form.model.trim();
    const iter = parseInt(form.maxIterations, 10);
    if (!Number.isNaN(iter) && iter > 0) payload.maxIterations = iter;
    const timeout = parseInt(form.timeoutSecs, 10);
    if (!Number.isNaN(timeout)) payload.timeoutSecs = timeout;
    send(payload);
  };

  const onCancel = () => {
    setForm(null);
    setStatus({ kind: "idle" });
  };

  const submitting = status.kind === "submitting";
  const success = status.kind === "ok";

  return (
    <div
      className="fixed inset-0 z-[60] flex items-center justify-center"
      style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
      onClick={onCancel}
    >
      <form
        className="rounded-lg border shadow-xl w-[560px] max-w-[92vw] max-h-[90vh] overflow-auto"
        style={{
          background: "var(--bg-primary)",
          borderColor: "var(--border)",
          color: "var(--text-primary)",
        }}
        onClick={(e) => e.stopPropagation()}
        onSubmit={onSubmit}
      >
        <div
          className="px-4 py-2 border-b text-sm font-semibold flex items-center gap-2"
          style={{ borderColor: "var(--border)" }}
        >
          <span style={{ color: "var(--accent)" }}>●</span>
          <span>Add schedule</span>
        </div>

        <div className="px-4 py-3 space-y-3 text-xs">
          <Field label="ID" hint="Stable lookup key (e.g. morning-brief)">
            <input
              ref={idInputRef}
              type="text"
              value={form.id}
              onChange={(e) => onChange("id", e.target.value)}
              required
              pattern="[A-Za-z0-9_\-]+"
              placeholder="morning-brief"
              // It's a slug, not prose — kill the WebKit text assists so
              // they don't capitalize, autocorrect, or suggest saved
              // values into the id (e.g. "morning-brief" → "Morning Brief").
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              className="w-full px-2 py-1.5 rounded border font-mono text-xs"
              style={inputStyle}
            />
          </Field>

          <Field
            label="Cron"
            hint='Standard 5-field POSIX cron (e.g. "30 8 * * MON-FRI")'
          >
            <div className="flex flex-wrap gap-1 mb-1.5">
              {CRON_PRESETS.map((p) => {
                const active = form.cron.trim() === p.cron;
                return (
                  <button
                    key={p.cron}
                    type="button"
                    onClick={() => onChange("cron", p.cron)}
                    className="text-[10px] px-1.5 py-0.5 rounded border transition-colors"
                    style={{
                      background: active
                        ? "var(--accent)"
                        : "var(--bg-secondary)",
                      borderColor: active ? "var(--accent)" : "var(--border)",
                      color: active
                        ? "var(--accent-fg, #fff)"
                        : "var(--text-secondary)",
                    }}
                    title={p.cron}
                  >
                    {p.label}
                  </button>
                );
              })}
            </div>
            <input
              type="text"
              value={form.cron}
              onChange={(e) => onChange("cron", e.target.value)}
              required
              placeholder="30 8 * * MON-FRI"
              className="w-full px-2 py-1.5 rounded border font-mono text-xs"
              style={inputStyle}
            />
            {preview.kind === "ok" && preview.fires.length > 0 && (
              <div
                className="mt-1.5 text-[10px] font-mono"
                style={{ color: "var(--text-secondary)" }}
              >
                next fires: {preview.fires.map(formatFireTime).join("  ·  ")}
              </div>
            )}
            {preview.kind === "ok" && preview.fires.length === 0 && (
              <div
                className="mt-1.5 text-[10px]"
                style={{ color: "var(--accent-error, #c33)" }}
              >
                cron parses but has no upcoming fires
              </div>
            )}
            {preview.kind === "error" && (
              <div
                className="mt-1.5 text-[10px]"
                style={{ color: "var(--accent-error, #c33)" }}
              >
                {preview.message}
              </div>
            )}
            {preview.kind === "loading" && (
              <div
                className="mt-1.5 text-[10px]"
                style={{ color: "var(--text-secondary)" }}
              >
                checking…
              </div>
            )}
          </Field>

          <Field
            label="Prompt"
            hint="Text passed to `thclaws --print` when this fires"
          >
            <textarea
              value={form.prompt}
              onChange={(e) => onChange("prompt", e.target.value)}
              required
              rows={4}
              placeholder="summarize today's commits and open PRs to ~/Desktop/brief.md"
              className="w-full px-2 py-1.5 rounded border font-mono text-xs resize-y"
              style={inputStyle}
            />
          </Field>

          <Field
            label="Working directory"
            hint="Determines which .thclaws/settings.json + sandbox the job uses"
          >
            <input
              type="text"
              value={form.cwd}
              onChange={(e) => onChange("cwd", e.target.value)}
              required
              className="w-full px-2 py-1.5 rounded border font-mono text-xs"
              style={inputStyle}
            />
          </Field>

          <div className="grid grid-cols-3 gap-3">
            <Field
              label="Model"
              hint="Optional override (defaults to cwd's settings)"
            >
              <input
                type="text"
                value={form.model}
                onChange={(e) => onChange("model", e.target.value)}
                placeholder="(default)"
                className="w-full px-2 py-1.5 rounded border font-mono text-xs"
                style={inputStyle}
              />
            </Field>
            <Field label="Max iterations" hint="Tool-call cap; blank = default">
              <input
                type="number"
                min={1}
                value={form.maxIterations}
                onChange={(e) => onChange("maxIterations", e.target.value)}
                placeholder="(default)"
                className="w-full px-2 py-1.5 rounded border font-mono text-xs"
                style={inputStyle}
              />
            </Field>
            <Field label="Timeout (sec)" hint="0 disables the timeout">
              <input
                type="number"
                min={0}
                value={form.timeoutSecs}
                onChange={(e) => onChange("timeoutSecs", e.target.value)}
                className="w-full px-2 py-1.5 rounded border font-mono text-xs"
                style={inputStyle}
              />
            </Field>
          </div>

          <label className="flex items-center gap-2 select-none">
            <input
              type="checkbox"
              checked={form.watchWorkspace}
              onChange={(e) => onChange("watchWorkspace", e.target.checked)}
            />
            <span style={{ color: "var(--text-secondary)" }}>
              Run when file in workspace changes
              <span
                className="ml-1.5 text-[10px]"
                style={{ color: "var(--text-secondary)" }}
              >
                (daemon-only — fires on debounced filesystem events under the cwd)
              </span>
            </span>
          </label>

          <label className="flex items-center gap-2 select-none">
            <input
              type="checkbox"
              checked={form.disabled}
              onChange={(e) => onChange("disabled", e.target.checked)}
            />
            <span style={{ color: "var(--text-secondary)" }}>
              Add as disabled — won't fire until enabled
            </span>
          </label>

          {status.kind === "error" && (
            <div
              className="px-2 py-1.5 rounded border text-xs"
              style={{
                borderColor: "var(--accent-error, #c33)",
                color: "var(--accent-error, #c33)",
                background: "rgba(204,51,51,0.06)",
              }}
            >
              {status.message}
            </div>
          )}
          {success && (
            <div
              className="px-2 py-1.5 rounded border text-xs"
              style={{
                borderColor: "var(--accent-success, #2a9)",
                color: "var(--accent-success, #2a9)",
                background: "rgba(42,153,102,0.06)",
              }}
            >
              schedule '{(status as { kind: "ok"; id: string }).id}' saved
            </div>
          )}
        </div>

        <div
          className="px-4 py-3 border-t flex items-center justify-end gap-2"
          style={{ borderColor: "var(--border)" }}
        >
          <button
            type="button"
            onClick={onCancel}
            className="text-xs px-3 py-1.5 rounded hover:bg-white/5"
            style={{ color: "var(--text-secondary)" }}
            disabled={submitting}
          >
            Cancel
          </button>
          <button
            type="submit"
            className="text-xs px-3 py-1.5 rounded"
            style={{
              background: "var(--accent)",
              color: "var(--accent-fg, #ffffff)",
              opacity: submitting || success ? 0.6 : 1,
            }}
            disabled={submitting || success}
          >
            {submitting ? "Saving…" : success ? "Saved" : "Save"}
          </button>
        </div>
      </form>
    </div>
  );
}

const inputStyle: React.CSSProperties = {
  background: "var(--bg-secondary)",
  borderColor: "var(--border)",
  color: "var(--text-primary)",
};

function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <label className="block">
      <span className="block mb-1 text-[11px] uppercase tracking-wide"
        style={{ color: "var(--text-secondary)" }}>
        {label}
      </span>
      {children}
      {hint && (
        <span className="block mt-1 text-[10px]"
          style={{ color: "var(--text-secondary)" }}>
          {hint}
        </span>
      )}
    </label>
  );
}
