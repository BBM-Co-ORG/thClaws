import { useEffect, useState } from "react";
import { subscribe } from "../hooks/useIPC";

/// Goal-state sidebar (M6.29 Phase A). Subscribes to `chat_goal_update`
/// IPC events from the worker and renders a compact indicator above the
/// plan sidebar showing what /goal start put in flight: objective,
/// elapsed iterations, tokens used / budget, status. Renders nothing
/// when no goal is active — the broadcaster fires `null` on /goal
/// abandon, terminal status, or session swap to a goal-less session.
///
/// Distinct from PlanSidebar — a session can carry both (a goal that
/// drives a long-horizon objective + a plan that breaks it into steps),
/// or just one, or neither. They share the right column.

type GoalStatus = "active" | "complete" | "abandoned" | "blocked";

type GoalState = {
  objective: string;
  started_at: number;
  budget_tokens: number | null;
  budget_time_secs: number | null;
  tokens_used: number;
  iterations_done: number;
  status: GoalStatus;
  last_audit?: string | null;
  last_message?: string | null;
  completed_at?: number | null;
};

const STATUS_COLOR: Record<GoalStatus, string> = {
  active: "var(--accent)",
  complete: "var(--accent)",
  abandoned: "var(--text-secondary)",
  blocked: "var(--warning, #d4a72c)",
};

const STATUS_LABEL: Record<GoalStatus, string> = {
  active: "ACTIVE",
  complete: "COMPLETE",
  abandoned: "ABANDONED",
  blocked: "BLOCKED",
};

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

function formatElapsed(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return `${h}h${m % 60}m`;
}

export function GoalSidebar() {
  const [goal, setGoal] = useState<GoalState | null>(null);
  // Re-render every 10s so the elapsed-time line stays fresh while a
  // long-running /loop /goal continue is in flight. Cheap — only
  // mounted when a goal is active anyway.
  const [, setTick] = useState(0);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "chat_goal_update") {
        setGoal((msg.goal as GoalState | null) ?? null);
      }
    });
    return unsub;
  }, []);

  useEffect(() => {
    if (!goal || goal.status !== "active") return;
    const t = window.setInterval(() => setTick((n) => n + 1), 10_000);
    return () => window.clearInterval(t);
  }, [goal]);

  if (!goal) return null;

  // A wall clock read during render, re-rendered by the 10s tick above
  // only while the goal is active — so elapsed advances on a running
  // goal and freezes on a finished one. `useSyncExternalStore` is the
  // idiomatic alternative, but a snapshot that changes every second can
  // trip its "should be cached" guard, so this stays.
  // eslint-disable-next-line react-hooks/purity
  const elapsedSecs = Math.max(0, Math.floor(Date.now() / 1000) - goal.started_at);
  const statusColor = STATUS_COLOR[goal.status];
  const statusLabel = STATUS_LABEL[goal.status];
  const budgetPct =
    goal.budget_tokens && goal.budget_tokens > 0
      ? Math.min(
          100,
          Math.floor((goal.tokens_used / goal.budget_tokens) * 100),
        )
      : null;

  return (
    <div
      className="flex flex-col shrink-0 border-l px-3 py-2"
      style={{
        width: "240px",
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
      }}
    >
      <div
        className="text-[10px] uppercase tracking-wider flex items-center gap-2 mb-1"
        style={{ color: "var(--text-secondary)" }}
      >
        <span>Goal</span>
        <span
          className="px-1.5 py-px rounded"
          style={{
            fontSize: "9px",
            background: goal.status === "active" ? statusColor : "var(--bg-tertiary)",
            color:
              goal.status === "active"
                ? "var(--accent-fg, #fff)"
                : statusColor,
            border:
              goal.status === "active"
                ? "none"
                : `1px solid ${statusColor}`,
          }}
        >
          {statusLabel}
        </span>
      </div>
      <div
        className="text-xs mb-1.5"
        style={{
          color: "var(--text-primary)",
          lineHeight: "1.35",
          // Truncate to 3 lines so a long objective doesn't push the
          // plan sidebar off-screen.
          display: "-webkit-box",
          WebkitLineClamp: 3,
          WebkitBoxOrient: "vertical",
          overflow: "hidden",
        }}
        title={goal.objective}
      >
        {goal.objective}
      </div>
      <div
        className="flex items-center justify-between"
        style={{
          color: "var(--text-secondary)",
          fontSize: "10px",
          lineHeight: "1.3",
        }}
      >
        <span>
          {goal.iterations_done} iter{goal.iterations_done === 1 ? "" : "s"}
          {" · "}
          {formatElapsed(elapsedSecs)}
        </span>
        <span title={goal.budget_tokens ? `${goal.tokens_used} / ${goal.budget_tokens} tokens` : undefined}>
          {formatTokens(goal.tokens_used)}
          {goal.budget_tokens != null && (
            <>
              {" / "}
              {formatTokens(goal.budget_tokens)}
              {budgetPct !== null && budgetPct >= 80 && (
                <span
                  style={{
                    color: budgetPct >= 100 ? "var(--danger, #e06c75)" : "var(--warning, #d4a72c)",
                    marginLeft: 4,
                  }}
                >
                  ({budgetPct}%)
                </span>
              )}
            </>
          )}
        </span>
      </div>
      {goal.last_message && goal.status !== "active" && (
        <div
          className="mt-1.5 italic"
          style={{
            color: "var(--text-secondary)",
            fontSize: "10px",
            lineHeight: "1.35",
          }}
          title={goal.last_message}
        >
          {goal.last_message}
        </div>
      )}
    </div>
  );
}
