import { useEffect, useMemo, useRef, useState } from "react";
import type { ViewerTarget } from "./KmsBrowserSidebar";
import { ChevronRight, X, Search } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import { money } from "./kmsRunCost";

/// M6.39.5: verbose right-edge sidebar for active research jobs.
/// Mirrors the visual rhythm of `PlanSidebar` / `TodoSidebar` but shows
/// `/research` pipeline progression in detail — current phase,
/// iteration-by-iteration score history, accumulated source count,
/// and a scrolling phase log so the user can watch the pipeline
/// think rather than just see "done" at the end.
///
/// The left-side Sidebar's Research panel still shows the list of
/// all jobs (compact); this right-side panel shows the most recent
/// active or completed one in detail.

type ResearchStatus =
  | "pending"
  | "running"
  | "done"
  | "cancelled"
  | "failed";

type ResearchJobInfo = {
  id: string;
  status: ResearchStatus;
  phase: string;
  query: string;
  iterations_done: number;
  source_count: number;
  last_score: number | null;
  kms_target: string | null;
  result_page: string | null;
  error: string | null;
  started_at: number | null;
  finished_at: number | null;
  /// dev-plan/64 P5.4: what the run has spent so far, priced by the
  /// engine (only it has the catalogue) and refreshed as it goes.
  cost_usd?: number | null;
  /// How many pages the run wrote. Absent until it finishes.
  pages_written?: number | null;
  /// dev-plan/64 P5.5: the command that would run this job again, built
  /// by the engine at registration. `null` where a correct retry cannot
  /// be written — an ingest whose path the job never kept, a page
  /// researched out of another page's selection.
  retry_command?: string | null;
};

/// Per-job in-memory progression log accumulated by the frontend
/// from successive `research_update` broadcasts. The backend payload
/// only carries the latest snapshot; we keep a chronological log of
/// distinct phases + iteration milestones for the verbose view.
type JobProgress = {
  /// Most recent envelope shape — mirrors `ResearchJobInfo`.
  view: ResearchJobInfo;
  /// Distinct `phase` strings in order seen, deduped on consecutive
  /// repeats so a fast burst of identical phase updates doesn't
  /// crowd the log.
  phaseLog: string[];
  /// One entry per iteration completed, recording its end-of-loop
  /// score + source count delta. Empty during iteration 0;
  /// populated when `iterations_done` increments.
  iterationHistory: { iter: number; score: number | null; sourceCount: number }[];
};

const STATUS_COLOR: Record<ResearchStatus, string> = {
  pending: "var(--text-secondary)",
  running: "var(--warning, #d4a657)",
  done: "var(--accent)",
  cancelled: "var(--text-secondary)",
  failed: "var(--danger, #e06c75)",
};

const STATUS_LABEL: Record<ResearchStatus, string> = {
  pending: "queued",
  running: "running",
  done: "done",
  cancelled: "cancelled",
  failed: "failed",
};

/// `result_page` is `<kms>/<page>.md` (v2: the map-of-content note) or
/// `<kms>/runs/<file>.md` for a `--dry-run`. Only pages open in the
/// viewer; anything else falls back to printing in chat.
function resultAsViewerTarget(resultPage: string): ViewerTarget | null {
  const slash = resultPage.indexOf("/");
  if (slash <= 0) return null;
  const kms = resultPage.slice(0, slash);
  const rest = resultPage.slice(slash + 1);
  if (rest.startsWith("runs/") || rest.includes("/")) return null;
  return { kms, kind: "page", name: rest.replace(/\.md$/, "") };
}

function fmtElapsed(startedAt: number | null, finishedAt: number | null, nowSec: number): string {
  if (startedAt === null) return "";
  const end = finishedAt ?? nowSec;
  const s = Math.max(0, end - startedAt);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const mm = String(m).padStart(2, "0");
  const ss = String(sec).padStart(2, "0");
  return h > 0 ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
}

export function ResearchSidebar({
  onOpenResult,
}: {
  /// When given, "Show result" opens the note in the KMS viewer pane
  /// (App wires this to `setViewerTarget`). Without it the button
  /// prints the note into chat via `/research show`.
  onOpenResult?: (target: ViewerTarget) => void;
}) {
  // null = no envelope yet; the sidebar suppresses entirely on a
  // fresh session that's never seen a /research run.
  const [progressMap, setProgressMap] = useState<Map<string, JobProgress> | null>(null);
  // Which job's details to show. Defaults to "most recent active or
  // finished"; user can switch by clicking another job's chevron in
  // the left Sidebar (a future enhancement could surface a dropdown
  // here too).
  const [focusedId, setFocusedId] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState(false);
  /// Job ids the user has explicitly dismissed AFTER the job
  /// reached a terminal state (done / cancelled / failed). Pre-fix
  /// the X button always collapsed to a chevron tab regardless of
  /// run state — fine while a job is running (user might want to
  /// peek back at progress) but pointless after the run finished
  /// (the work is done; the sidebar is dead weight). With this set
  /// tracked, terminal-state X closes the sidebar entirely for that
  /// job by filtering it out of the focused-job selection and
  /// suppressing the panel when no other live jobs remain.
  const dismissedTerminalRef = useRef<Set<string>>(new Set());
  const [dismissedVersion, setDismissedVersion] = useState(0);
  const lastSeenIterRef = useRef<Map<string, number>>(new Map());
  // Wall clock for the elapsed timer — ticks once a second while any job
  // is running, mirroring the chat turn's clock.
  const [nowSec, setNowSec] = useState(() => Math.floor(Date.now() / 1000));
  useEffect(() => {
    const t = setInterval(() => setNowSec(Math.floor(Date.now() / 1000)), 1000);
    return () => clearInterval(t);
  }, []);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "research_focus" && typeof msg.id === "string") {
        // `/research show <id>` re-opens a closed panel on that job,
        // even after a terminal-state dismiss.
        dismissedTerminalRef.current.delete(msg.id);
        setDismissedVersion((v) => v + 1);
        setDismissed(false);
        setFocusedId(msg.id);
        return;
      }
      if (msg.type !== "research_update") return;
      const jobs = (msg.jobs as ResearchJobInfo[]) ?? [];
      setProgressMap((prev) => {
        const next = new Map(prev ?? new Map());
        for (const j of jobs) {
          const existing = next.get(j.id);
          // Phase log: append distinct phase strings only.
          let phaseLog = existing?.phaseLog ?? [];
          if (
            phaseLog.length === 0 ||
            phaseLog[phaseLog.length - 1] !== j.phase
          ) {
            phaseLog = [...phaseLog, j.phase];
            // Cap length so a long-running job doesn't grow the
            // array unbounded.
            if (phaseLog.length > 40) {
              phaseLog = phaseLog.slice(phaseLog.length - 40);
            }
          }
          // Iteration history: append once per detected iteration
          // increment. Tracked outside React state so the comparison
          // doesn't fight reconciliation. New entries are appended
          // with `score: null` — the score gets filled in by the
          // patch step below when the post-evaluate broadcast lands.
          // (Backend sends two broadcasts per iter: one at iter-start
          // with last_score=null, one after evaluate with the real
          // score. The previous append-time-only logic locked iter N
          // to last_score-at-append, which was iter N-1's value.)
          // Explicit type — `existing?.iterationHistory ?? []` falls
          // back to `never[]` on first encounter, which then poisons
          // the inferred element type of subsequent `findIndex` /
          // `map` callbacks (params come out as `any`, lint fails).
          let iterationHistory: JobProgress["iterationHistory"] =
            existing?.iterationHistory ?? [];
          const lastIter = lastSeenIterRef.current.get(j.id) ?? 0;
          if (j.iterations_done > lastIter) {
            for (let n = lastIter + 1; n <= j.iterations_done; n++) {
              iterationHistory = [
                ...iterationHistory,
                {
                  iter: n,
                  score: null,
                  sourceCount: j.source_count,
                },
              ];
            }
            lastSeenIterRef.current.set(j.id, j.iterations_done);
          }
          // Patch: keep the entry matching the current iter in sync
          // with the latest broadcast. The post-evaluate broadcast
          // arrives with iterations_done unchanged but last_score
          // freshly populated — without this step the entry would
          // freeze at score=null since the iter-increment branch
          // doesn't fire a second time. `source_count` is also
          // tracked here because fetch-phase deltas update it
          // independently of evaluate.
          if (iterationHistory.length > 0) {
            const idx = iterationHistory.findIndex(
              (e) => e.iter === j.iterations_done,
            );
            if (idx >= 0) {
              const entry = iterationHistory[idx];
              if (
                entry.score !== j.last_score ||
                entry.sourceCount !== j.source_count
              ) {
                iterationHistory = iterationHistory.map((e, i) =>
                  i === idx
                    ? {
                        ...e,
                        score: j.last_score,
                        sourceCount: j.source_count,
                      }
                    : e,
                );
              }
            }
          }
          next.set(j.id, { view: j, phaseLog, iterationHistory });
        }
        // Drop entries for jobs the backend no longer reports — manager
        // pruning eventually removes terminal jobs older than its
        // retention window. Keeping them in our local map would show
        // ghosts.
        const liveIds = new Set(jobs.map((j) => j.id));
        for (const id of Array.from(next.keys())) {
          if (!liveIds.has(id)) {
            next.delete(id);
            lastSeenIterRef.current.delete(id);
          }
        }
        return next;
      });
      // Auto-focus selection: prefer the most recently-started
      // running job; fall back to most-recent terminal. User can
      // override (sticky once a job is selected manually — but we
      // don't expose manual selection in this revision).
      // Jobs the user dismissed after they reached a terminal state
      // are filtered out of the auto-focus selection so a closed
      // sidebar stays closed for that job.
      const dismissedIds = dismissedTerminalRef.current;
      setFocusedId((prev) => {
        // A live job always wins: starting a new /research while a
        // finished one is still on screen must switch the panel to the
        // new run (pre-fix the finished job stayed focused forever).
        // `jobs` is newest-first, so the first running one is the latest.
        const running = jobs.find(
          (j) => (j.status === "running" || j.status === "pending") && !dismissedIds.has(j.id),
        );
        if (running) return running.id;
        const prevStillExists =
          prev !== null && jobs.some((j) => j.id === prev) && !dismissedIds.has(prev);
        if (prevStillExists) return prev;
        const liveJobs = jobs.filter((j) => !dismissedIds.has(j.id));
        if (liveJobs.length > 0) return liveJobs[0].id;
        return null;
      });
      // Re-open if dismissed and a new running job appeared.
      if (jobs.some((j) => j.status === "running" || j.status === "pending")) {
        setDismissed(false);
      }
    });
    return unsub;
  }, []);

  const focused = useMemo(() => {
    if (!progressMap || !focusedId) return null;
    // After a terminal-state X-click, the dismissed job id is in
    // dismissedTerminalRef. Treat as "no focused job" so the panel
    // returns null below — same suppression path as a session that
    // never ran /research.
    if (dismissedTerminalRef.current.has(focusedId)) return null;
    return progressMap.get(focusedId) ?? null;
    // `dismissedVersion` is intentionally in deps so a dismiss
    // triggers re-evaluation (the ref's `.has` check has no
    // reactivity on its own).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [progressMap, focusedId, dismissedVersion]);

  // Suppress entirely when no /research run has been observed.
  if (progressMap === null || progressMap.size === 0) return null;
  if (!focused) return null;

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
        title={`Research: ${focused.view.status} · ${fmtElapsed(focused.view.started_at, focused.view.finished_at, nowSec)} · round ${focused.view.iterations_done}${
          focused.view.last_score !== null
            ? ` · novelty ${(focused.view.last_score * 100).toFixed(0)}%`
            : ""
        }`}
      >
        <ChevronRight size={14} style={{ transform: "rotate(180deg)" }} />
      </button>
    );
  }

  const { view, phaseLog, iterationHistory } = focused;
  const isRunning = view.status === "running" || view.status === "pending";

  // Iteration bar: 8 segments by default; if a custom max-iter
  // changed the cap we fallback to inferring from history length.
  // Backend doesn't ship max_iter in the envelope, so we use 8 as
  // the JobConfig default; if a real run goes past, we pad.
  const maxIter = Math.max(3, iterationHistory.length, view.iterations_done);

  const showResult = () => {
    const target = view.result_page ? resultAsViewerTarget(view.result_page) : null;
    if (onOpenResult && target) {
      onOpenResult(target);
      return;
    }
    send({ type: "chat_prompt", text: `/research show ${view.id}` });
  };
  const cancel = () => {
    send({ type: "chat_prompt", text: `/research cancel ${view.id}` });
  };
  // Digests are cached per source, so a second attempt pays only for
  // what the first never reached — which is what makes a retry worth
  // offering rather than asking the owner to retype the query.
  const retry = () => {
    if (view.retry_command) {
      send({ type: "chat_prompt", text: view.retry_command });
    }
  };

  return (
    <div
      className="flex flex-col shrink-0 border-l"
      style={{
        width: "280px",
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
      }}
    >
      {/* Header */}
      <div
        className="flex items-center justify-between px-3 py-2 border-b shrink-0"
        style={{ borderColor: "var(--border)" }}
      >
        <div
          className="text-[10px] uppercase tracking-wider flex items-center gap-2"
          style={{ color: "var(--text-secondary)" }}
        >
          <Search size={11} />
          <span>Research</span>
          <span
            className="px-1.5 py-px rounded font-mono"
            style={{
              fontSize: "9px",
              background: "var(--bg-tertiary)",
              color: STATUS_COLOR[view.status],
              border: `1px solid ${STATUS_COLOR[view.status]}`,
            }}
          >
            {STATUS_LABEL[view.status]}
          </span>
          {typeof view.cost_usd === "number" && (
            <span
              className="px-1.5 py-px rounded font-mono"
              style={{
                fontSize: "9px",
                background: "var(--bg-tertiary)",
                color: "var(--text-secondary)",
                border: "1px solid var(--border)",
              }}
              title={
                isRunning
                  ? "What this run has spent so far"
                  : "What this run spent"
              }
            >
              {money(view.cost_usd)}
            </span>
          )}
          {typeof view.pages_written === "number" && view.pages_written > 0 && (
            <span
              className="px-1.5 py-px rounded"
              style={{
                fontSize: "9px",
                background: "var(--bg-tertiary)",
                color: "var(--accent)",
                border: "1px solid var(--accent)",
              }}
              title="Pages this run wrote into the knowledge base"
            >
              {view.pages_written === 1
                ? "1 new page"
                : `${view.pages_written} new pages`}
            </span>
          )}
        </div>
        <button
          type="button"
          onClick={() => {
            // Two close behaviors based on job state:
            // - Job still in flight (running / pending) → collapse to
            //   the chevron tab so the user can peek back at progress.
            //   This is the historical behavior.
            // - Job already finished (done / cancelled / failed) → fully
            //   dismiss for this job id. The completed sidebar isn't
            //   useful UI weight; the audit is in the KMS now. Re-running
            //   `/research` mints a new job id and the sidebar re-opens
            //   for that fresh run.
            const isTerminal =
              view.status === "done" ||
              view.status === "cancelled" ||
              view.status === "failed";
            if (isTerminal) {
              dismissedTerminalRef.current.add(view.id);
              setDismissedVersion((v) => v + 1);
            } else {
              setDismissed(true);
            }
          }}
          className="p-0.5 rounded hover:bg-white/10"
          style={{ color: "var(--text-secondary)" }}
          title={
            view.status === "running" || view.status === "pending"
              ? "Hide sidebar (job continues; restore via chevron)"
              : "Close sidebar (job is finished; the audit lives in the KMS)"
          }
        >
          <X size={14} />
        </button>
      </div>

      {/* Body — scrollable */}
      <div className="flex-1 overflow-auto px-3 py-3 flex flex-col gap-3">
        {/* Query */}
        <div>
          <div
            className="text-[9px] uppercase tracking-wider mb-1"
            style={{ color: "var(--text-secondary)" }}
          >
            Query
          </div>
          <div
            className="text-xs leading-snug break-words"
            style={{ color: "var(--text-primary)" }}
          >
            {view.query}
          </div>
        </div>

        {/* Current phase (highlighted) */}
        <div>
          <div
            className="text-[9px] uppercase tracking-wider mb-1 flex items-center justify-between"
            style={{ color: "var(--text-secondary)" }}
          >
            <span>Phase</span>
            <span className="font-mono normal-case tracking-normal" title="elapsed">
              {isRunning ? "⏱ " : ""}
              {fmtElapsed(view.started_at, view.finished_at, nowSec)}
            </span>
          </div>
          <div
            className="text-xs leading-snug font-mono"
            style={{
              color: isRunning ? "var(--warning, #d4a657)" : "var(--text-primary)",
              fontWeight: isRunning ? 600 : 400,
            }}
          >
            {view.phase}
          </div>
        </div>

        {/* Iteration progress bar */}
        <div>
          <div
            className="text-[9px] uppercase tracking-wider mb-1 flex justify-between"
            style={{ color: "var(--text-secondary)" }}
          >
            <span>Iterations</span>
            <span>{view.iterations_done} / {maxIter}</span>
          </div>
          <div className="flex gap-0.5">
            {Array.from({ length: maxIter }).map((_, i) => {
              const completed = i < view.iterations_done;
              const inProgress = i === view.iterations_done && isRunning;
              return (
                <div
                  key={i}
                  className="flex-1 rounded-sm"
                  style={{
                    height: "6px",
                    background: completed
                      ? "var(--accent)"
                      : inProgress
                        ? "var(--warning, #d4a657)"
                        : "var(--bg-tertiary)",
                    border: "1px solid var(--border)",
                  }}
                />
              );
            })}
          </div>
        </div>

        {/* Iteration scores (one row per completed iter) */}
        {iterationHistory.length > 0 && (
          <div>
            <div
              className="text-[9px] uppercase tracking-wider mb-1"
              style={{ color: "var(--text-secondary)" }}
            >
              Novelty per round
            </div>
            <div className="flex flex-col gap-0.5">
              {iterationHistory.map((h) => (
                <div
                  key={h.iter}
                  className="flex items-center gap-2 text-[10px] font-mono"
                  style={{ color: "var(--text-primary)" }}
                >
                  <span
                    style={{ color: "var(--text-secondary)", width: "26px" }}
                  >
                    r{h.iter}
                  </span>
                  <div
                    className="flex-1 rounded-sm overflow-hidden"
                    style={{
                      height: "4px",
                      background: "var(--bg-tertiary)",
                      border: "1px solid var(--border)",
                    }}
                  >
                    {h.score !== null && (
                      <div
                        style={{
                          width: `${Math.max(0, Math.min(1, h.score)) * 100}%`,
                          height: "100%",
                          background: "var(--accent)",
                        }}
                      />
                    )}
                  </div>
                  <span style={{ width: "30px", textAlign: "right" }}>
                    {h.score !== null ? `${(h.score * 100).toFixed(0)}%` : "—"}
                  </span>
                  <span
                    style={{
                      color: "var(--text-secondary)",
                      width: "44px",
                      textAlign: "right",
                    }}
                  >
                    {h.sourceCount} src
                  </span>
                </div>
              ))}
            </div>
          </div>
        )}

        {/* Sources accumulated */}
        <div className="flex items-center justify-between">
          <span
            className="text-[9px] uppercase tracking-wider"
            style={{ color: "var(--text-secondary)" }}
          >
            Sources accumulated
          </span>
          <span
            className="text-xs font-mono"
            style={{ color: "var(--text-primary)" }}
          >
            {view.source_count}
          </span>
        </div>

        {/* Phase log — chronological, deduped */}
        <div>
          <div
            className="text-[9px] uppercase tracking-wider mb-1"
            style={{ color: "var(--text-secondary)" }}
          >
            Phase log
          </div>
          <ul
            className="flex flex-col gap-0.5 font-mono"
            style={{ fontSize: "10px", color: "var(--text-secondary)" }}
          >
            {phaseLog.slice(-10).map((p, idx, arr) => {
              const isCurrent = idx === arr.length - 1;
              return (
                <li
                  key={`${p}-${idx}`}
                  className="leading-snug break-words"
                  style={{
                    color: isCurrent
                      ? "var(--text-primary)"
                      : "var(--text-secondary)",
                    opacity: isCurrent ? 1 : 0.7,
                  }}
                >
                  {isCurrent ? "→ " : "  "}
                  {p}
                </li>
              );
            })}
          </ul>
        </div>

        {/* Result / error footer */}
        {view.status === "done" && view.result_page && (
          <div>
            <div
              className="text-[9px] uppercase tracking-wider mb-1"
              style={{ color: "var(--text-secondary)" }}
            >
              Result
            </div>
            <div
              className="text-[10px] font-mono break-all cursor-pointer hover:underline"
              style={{ color: "var(--accent)" }}
              onClick={showResult}
              title="Open"
            >
              {view.result_page}
            </div>
          </div>
        )}
        {view.status === "failed" && view.error && (
          <div>
            <div
              className="text-[9px] uppercase tracking-wider mb-1"
              style={{ color: "var(--danger, #e06c75)" }}
            >
              Error
            </div>
            <div
              className="text-[10px] break-words"
              style={{ color: "var(--danger, #e06c75)" }}
            >
              {view.error}
            </div>
          </div>
        )}
      </div>

      {/* Footer — actions */}
      <div
        className="px-3 py-2 border-t shrink-0 flex items-center gap-1.5"
        style={{
          borderColor: "var(--border)",
          fontSize: "10px",
          color: "var(--text-secondary)",
        }}
      >
        {view.status === "done" && (
          <button
            onClick={showResult}
            className="px-2 py-1 rounded font-medium"
            style={{
              background: "var(--accent)",
              color: "#fff",
            }}
            title={
              onOpenResult
                ? "Open the map of content in the KMS viewer"
                : "Print the result in chat"
            }
          >
            Show result
          </button>
        )}
        {!isRunning &&
          (view.status === "failed" || view.status === "cancelled") &&
          view.retry_command && (
            <button
              onClick={retry}
              className="px-2 py-1 rounded font-medium"
              style={{
                background: "var(--bg-tertiary)",
                color: "var(--accent)",
                border: "1px solid var(--accent)",
              }}
              title={`Run it again: ${view.retry_command}\nSources already digested are cached, so this pays only for what the first attempt never reached.`}
            >
              Retry
            </button>
          )}
        {isRunning && (
          <button
            onClick={cancel}
            className="px-2 py-1 rounded"
            style={{
              background: "var(--bg-tertiary)",
              color: "var(--text-secondary)",
              border: "1px solid var(--border)",
            }}
            title="Cancel this research job"
          >
            Cancel
          </button>
        )}
        <span className="ml-auto" style={{ opacity: 0.6 }}>
          {view.id}
        </span>
      </div>
    </div>
  );
}
