//! M6.29: Goal-state tracking for `/goal start` + audit-driven completion.
//!
//! A goal is a user-supplied objective with optional token + time budgets.
//! State persists per-session (carried in `WorkerState.goal` and serialized
//! to session JSONL as `{"type": "goal_snapshot", ...}` events). The `/goal
//! continue` command builds an audit prompt from the current state +
//! consumed budget; the model mutates state via the three goal-lifecycle
//! tools (Phase C1 — authority split):
//!   - `RecordGoalProgress` — mid-loop checkpoint, status stays Active
//!   - `MarkGoalComplete`   — terminal Complete (audit required)
//!   - `MarkGoalBlocked`    — terminal Blocked (reason required)
//!
//! All three call `apply()` which fires the broadcaster (same pattern as
//! `plan_state`).
//!
//! The `goal-continue` audit prompt template (see
//! `default_prompts/goal_continue.md`) bakes in the discipline:
//! - Restate objective as concrete deliverables
//! - Build a prompt-to-artifact checklist
//! - Inspect concrete evidence (files, tests, output)
//! - Don't accept proxy signals as completion
//! - Treat uncertainty as not achieved
//!
//! Loop integration: when goal status becomes terminal (Complete /
//! Abandoned / Blocked), the active loop (if its body is `/goal
//! continue`) auto-stops via the broadcaster.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Active,
    Complete,
    Abandoned,
    /// Model called UpdateGoal with status=blocked. The blocker reason
    /// surfaces to the user; the loop pauses and waits for the user to
    /// /goal continue (manually) or /goal abandon.
    Blocked,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Complete => "complete",
            GoalStatus::Abandoned => "abandoned",
            GoalStatus::Blocked => "blocked",
        }
    }

    /// Terminal = goal is no longer actionable; the loop should stop.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            GoalStatus::Complete | GoalStatus::Abandoned | GoalStatus::Blocked
        )
    }
}

/// Per-session goal. Persisted to session JSONL as a `goal_state` event
/// (snapshot wins on load, like `plan_snapshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalState {
    pub objective: String,
    pub started_at: u64,
    pub budget_tokens: Option<u64>,
    pub budget_time_secs: Option<u64>,
    /// Approximate token consumption since goal start. Updated by
    /// `WorkerState` after each turn from the agent's usage counters.
    /// Approximate because providers vary in how they report usage.
    pub tokens_used: u64,
    /// Number of `/goal continue` iterations fired since start.
    pub iterations_done: u64,
    pub status: GoalStatus,
    /// Last audit summary captured from `UpdateGoal` calls. The model
    /// emits this when it concludes a checkpoint; future loops see it
    /// in the prompt so they don't re-audit from scratch.
    pub last_audit: Option<String>,
    /// Reason set by Blocked / Abandoned / Complete, surfaced to user.
    pub last_message: Option<String>,
    /// Wall-clock timestamp when status moved to terminal.
    pub completed_at: Option<u64>,
    /// Phase D1: when true, the worker auto-queues another `/goal
    /// continue` after each finishing turn (provided tool calls were
    /// made, status is still Active, and no `/loop` is wrapping).
    /// Default false — opt in via `/goal start ... --auto`.
    /// `#[serde(default)]` so older sessions that pre-date this field
    /// still deserialize cleanly on /load.
    #[serde(default)]
    pub auto_continue: bool,
    /// Engine-level completion gate: file paths (relative to cwd) that
    /// MUST exist on disk before `MarkGoalComplete` is accepted. Empty =
    /// no artifact requirement (prompt-level "done" only). Set via
    /// `/goal start ... --require <path>` (repeatable). Turns the agent's
    /// prompt convention into a hard, unfakeable gate: a missing artifact
    /// keeps the goal Active so the `--auto` loop keeps working (bounded
    /// by the hard cap). `#[serde(default)]` for older sessions.
    #[serde(default)]
    pub require_paths: Vec<String>,
}

impl GoalState {
    pub fn new(
        objective: String,
        budget_tokens: Option<u64>,
        budget_time_secs: Option<u64>,
        auto_continue: bool,
    ) -> Self {
        Self {
            objective,
            started_at: now_secs(),
            budget_tokens,
            budget_time_secs,
            tokens_used: 0,
            iterations_done: 0,
            status: GoalStatus::Active,
            last_audit: None,
            last_message: None,
            completed_at: None,
            auto_continue,
            require_paths: Vec::new(),
        }
    }

    /// Attach engine-enforced completion artifacts (see `require_paths`).
    pub fn with_require_paths(mut self, paths: Vec<String>) -> Self {
        self.require_paths = paths;
        self
    }

    /// Required artifacts that are not present on disk (relative to cwd).
    /// Empty when every requirement is met (or none were declared).
    pub fn missing_required_paths(&self) -> Vec<String> {
        self.require_paths
            .iter()
            .filter(|p| !std::path::Path::new(p).exists())
            .cloned()
            .collect()
    }

    /// Wall-clock seconds since goal started.
    pub fn time_used_secs(&self) -> u64 {
        now_secs().saturating_sub(self.started_at)
    }

    /// Tokens remaining (if a budget is set), saturating at 0.
    pub fn tokens_remaining(&self) -> Option<u64> {
        self.budget_tokens
            .map(|b| b.saturating_sub(self.tokens_used))
    }

    /// Gap 1 — hard backstop against a runaway `/goal continue` loop.
    ///
    /// The soft `GOAL_BUDGET_LIMIT` prompt fires at 1.0× the token budget but
    /// only *asks* the model to wrap up — a stubborn model can keep calling
    /// `RecordGoalProgress` forever, and with NO budget set the soft prompt
    /// never fires at all. This is the deterministic kill switch: returns
    /// `Some(reason)` when the loop MUST stop regardless of the model —
    /// either a token/time budget overrun past a 1.5× grace margin, or the
    /// absolute iteration cap (the only backstop when no budget is set).
    pub fn hard_limit_reached(&self) -> Option<String> {
        if self.iterations_done >= HARD_MAX_ITERATIONS {
            return Some(format!(
                "iteration cap reached ({HARD_MAX_ITERATIONS} /goal continue firings)"
            ));
        }
        // tokens_used >= budget × 1.5, integer-safe (×2 vs ×3).
        if let Some(budget) = self.budget_tokens {
            if self.tokens_used.saturating_mul(2) >= budget.saturating_mul(3) {
                return Some(format!(
                    "token budget overrun ({} used ≥ 1.5× {budget} budget)",
                    self.tokens_used
                ));
            }
        }
        if let Some(budget) = self.budget_time_secs {
            let used = self.time_used_secs();
            if used.saturating_mul(2) >= budget.saturating_mul(3) {
                return Some(format!(
                    "time budget overrun ({used}s used ≥ 1.5× {budget}s budget)"
                ));
            }
        }
        None
    }
}

/// Absolute backstop on `/goal continue` firings — caps unbounded cost when
/// no budget is set (or the model ignores the budget soft-stop). Generous:
/// a real deep audit converges well under this; it only catches runaways.
pub const HARD_MAX_ITERATIONS: u64 = 100;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Wire shape for the `goal_state` JSONL event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalSnapshotEvent {
    #[serde(rename = "type")]
    pub kind: String,
    pub goal: Option<GoalState>,
    pub timestamp: u64,
}

// ────────────────────────────────────────────────────────────────────────
// Broadcaster — same pattern as `plan_state`.
//
// `UpdateGoal` tool calls invoke `apply()`; subscribers (the worker)
// receive the new snapshot via the registered broadcaster and can:
//   1. Persist to session JSONL via `Session::append_goal_snapshot_to`
//   2. Stop the active loop if status becomes terminal
//   3. Surface a sidebar event

type Broadcaster = Box<dyn Fn(Option<&GoalState>) + Send + Sync>;

static STATE: OnceLock<Arc<Mutex<Option<GoalState>>>> = OnceLock::new();
static BROADCASTER: OnceLock<Mutex<Option<Broadcaster>>> = OnceLock::new();

fn state() -> &'static Arc<Mutex<Option<GoalState>>> {
    STATE.get_or_init(|| Arc::new(Mutex::new(None)))
}

fn broadcaster_slot() -> &'static Mutex<Option<Broadcaster>> {
    BROADCASTER.get_or_init(|| Mutex::new(None))
}

/// Read the current goal snapshot (clone). Returns `None` when no goal
/// is active.
pub fn current() -> Option<GoalState> {
    state().lock().ok().and_then(|g| g.clone())
}

/// Replace (or clear) the active goal. Fires the broadcaster.
pub fn set(new_state: Option<GoalState>) {
    if let Ok(mut g) = state().lock() {
        *g = new_state.clone();
    }
    fire_broadcaster();
}

/// Apply a delta — used by the `UpdateGoal` tool. `f` receives the
/// current state by reference and may mutate it. If `f` returns false,
/// the broadcaster is NOT fired (no-op apply).
pub fn apply<F>(f: F) -> bool
where
    F: FnOnce(&mut GoalState) -> bool,
{
    let changed = if let Ok(mut g) = state().lock() {
        match g.as_mut() {
            Some(gs) => f(gs),
            None => false,
        }
    } else {
        false
    };
    if changed {
        fire_broadcaster();
    }
    changed
}

/// Increment the iteration count + add `tokens` to the running counter.
/// Called by the worker after each `/goal continue` turn finishes.
pub fn record_iteration(tokens: u64) {
    let _ = apply(|g| {
        g.iterations_done = g.iterations_done.saturating_add(1);
        g.tokens_used = g.tokens_used.saturating_add(tokens);
        true
    });
}

/// Register a broadcaster. The previous registration (if any) is
/// dropped. Called by the worker at boot — receives goal snapshots and
/// persists them + stops the loop on terminal status.
pub fn set_broadcaster<F>(f: F)
where
    F: Fn(Option<&GoalState>) + Send + Sync + 'static,
{
    if let Ok(mut slot) = broadcaster_slot().lock() {
        *slot = Some(Box::new(f));
    }
}

/// Restore goal state on session load. Mirrors plan_state restore.
pub fn restore_from_session(snapshot: Option<GoalState>) {
    set(snapshot);
}

fn fire_broadcaster() {
    let snapshot = state().lock().ok().and_then(|g| g.clone());
    if let Ok(slot) = broadcaster_slot().lock() {
        if let Some(b) = slot.as_ref() {
            b(snapshot.as_ref());
        }
    }
}

/// M6.29: build the goal-continue audit prompt by filling the embedded
/// template with the current goal's objective + budget consumption.
///
/// Phase B1: when a token budget is set AND it's been exhausted
/// (`tokens_used >= budget_tokens`), swap to the `GOAL_BUDGET_LIMIT`
/// soft-stop template instead. The runtime keeps firing iterations
/// until the model marks the goal terminal, but each fire injects the
/// "wrap up" prompt — discouraging new substantive work and pushing the
/// model toward summarize / identify blockers / give next step.
/// Mirrors codex's runtime continuation soft-stop behavior.
pub fn build_audit_prompt(g: &GoalState) -> String {
    let token_budget = g
        .budget_tokens
        .map(|n| n.to_string())
        .unwrap_or_else(|| "(unlimited)".to_string());
    let tokens_remaining = g
        .tokens_remaining()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "(unlimited)".to_string());
    let time_used = g.time_used_secs();
    let budget_exhausted = g.budget_tokens.map(|b| g.tokens_used >= b).unwrap_or(false);
    let template = if budget_exhausted {
        crate::prompts::defaults::GOAL_BUDGET_LIMIT
    } else {
        crate::prompts::defaults::GOAL_CONTINUE
    };
    let prior_audit = g
        .last_audit
        .as_deref()
        .unwrap_or("(none — this is the first iteration or no audit recorded yet)");
    template
        .replace("{{ objective }}", &g.objective)
        .replace("{{ time_used_seconds }}", &time_used.to_string())
        .replace("{{ tokens_used }}", &g.tokens_used.to_string())
        .replace("{{ token_budget }}", &token_budget)
        .replace("{{ remaining_tokens }}", &tokens_remaining)
        .replace("{{ iterations_done }}", &g.iterations_done.to_string())
        .replace("{{ prior_audit }}", prior_audit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests are serialized via the global state — use a mutex to avoid
    /// races when running in parallel.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset() {
        set(None);
    }

    #[test]
    fn current_is_none_initially() {
        let _g = lock();
        reset();
        assert!(current().is_none());
    }

    #[test]
    fn missing_required_paths_reports_absent_files() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("here.txt");
        std::fs::write(&present, b"x").unwrap();
        let absent = dir.path().join("nope.txt");

        let g = GoalState::new("x".into(), None, None, false).with_require_paths(vec![
            present.to_string_lossy().into_owned(),
            absent.to_string_lossy().into_owned(),
        ]);
        let missing = g.missing_required_paths();
        assert_eq!(missing, vec![absent.to_string_lossy().into_owned()]);

        // No requirements declared → nothing missing → completable.
        let g2 = GoalState::new("y".into(), None, None, false);
        assert!(g2.missing_required_paths().is_empty());
    }

    #[test]
    fn set_and_get_round_trip() {
        let _g = lock();
        reset();
        let gs = GoalState::new("ship feature X".into(), Some(100_000), None, false);
        set(Some(gs.clone()));
        assert_eq!(
            current().as_ref().map(|c| c.objective.as_str()),
            Some("ship feature X")
        );
        reset();
    }

    #[test]
    fn apply_mutates_active_goal() {
        let _g = lock();
        reset();
        set(Some(GoalState::new("test".into(), None, None, false)));
        let changed = apply(|g| {
            g.tokens_used = 500;
            true
        });
        assert!(changed);
        assert_eq!(current().unwrap().tokens_used, 500);
        reset();
    }

    #[test]
    fn apply_returns_false_when_no_goal() {
        let _g = lock();
        reset();
        let changed = apply(|g| {
            g.tokens_used = 500;
            true
        });
        assert!(!changed);
    }

    #[test]
    fn record_iteration_increments_counters() {
        let _g = lock();
        reset();
        set(Some(GoalState::new("test".into(), None, None, false)));
        record_iteration(100);
        record_iteration(250);
        let g = current().unwrap();
        assert_eq!(g.iterations_done, 2);
        assert_eq!(g.tokens_used, 350);
        reset();
    }

    #[test]
    fn status_is_terminal_only_for_complete_abandoned_blocked() {
        assert!(!GoalStatus::Active.is_terminal());
        assert!(GoalStatus::Complete.is_terminal());
        assert!(GoalStatus::Abandoned.is_terminal());
        assert!(GoalStatus::Blocked.is_terminal());
    }

    #[test]
    fn tokens_remaining_handles_no_budget() {
        let g = GoalState::new("x".into(), None, None, false);
        assert_eq!(g.tokens_remaining(), None);
    }

    #[test]
    fn tokens_remaining_saturates() {
        let mut g = GoalState::new("x".into(), Some(1_000), None, false);
        g.tokens_used = 1_500;
        assert_eq!(g.tokens_remaining(), Some(0));
    }

    #[test]
    fn hard_limit_none_when_under_caps() {
        let g = GoalState::new("x".into(), None, None, false);
        assert!(g.hard_limit_reached().is_none());
    }

    #[test]
    fn hard_limit_iteration_cap() {
        let mut g = GoalState::new("x".into(), None, None, false);
        g.iterations_done = HARD_MAX_ITERATIONS;
        assert!(g.hard_limit_reached().is_some());
    }

    #[test]
    fn hard_limit_token_overrun_is_1_5x_not_1x() {
        let mut g = GoalState::new("x".into(), Some(1_000), None, false);
        // At 1.0× the budget the *soft* GOAL_BUDGET_LIMIT prompt nudges; the
        // hard kill switch must NOT fire yet.
        g.tokens_used = 1_000;
        assert!(
            g.hard_limit_reached().is_none(),
            "1.0× budget must not hard-stop"
        );
        // 1.5× → hard stop.
        g.tokens_used = 1_500;
        assert!(
            g.hard_limit_reached().is_some(),
            "1.5× budget must hard-stop"
        );
    }

    #[test]
    fn hard_limit_time_overrun() {
        let mut g = GoalState::new("x".into(), None, Some(10), false);
        g.started_at = 0; // epoch start → time_used ≈ now ≫ 1.5×10s
        assert!(g.hard_limit_reached().is_some());
    }

    #[test]
    fn build_audit_prompt_substitutes_template_vars() {
        let _g = lock();
        let g = GoalState::new("ship X".into(), Some(100_000), None, false);
        let p = build_audit_prompt(&g);
        assert!(p.contains("ship X"));
        assert!(p.contains("100000"));
    }

    #[test]
    fn build_audit_prompt_uses_continue_template_under_budget() {
        let _g = lock();
        let mut g = GoalState::new("ship X".into(), Some(100_000), None, false);
        g.tokens_used = 50_000;
        let p = build_audit_prompt(&g);
        // Continue template includes the audit checklist instruction.
        assert!(p.contains("completion audit"));
        assert!(!p.contains("budget-exhausted"));
    }

    #[test]
    fn build_audit_prompt_swaps_to_budget_limit_template_when_exhausted() {
        let _g = lock();
        let mut g = GoalState::new("ship X".into(), Some(100_000), None, false);
        g.tokens_used = 100_000; // exactly at budget
        let p = build_audit_prompt(&g);
        assert!(p.contains("budget-exhausted"));
        assert!(p.contains("Wrap up this turn"));
        // Soft-stop template doesn't carry the full audit checklist.
        assert!(!p.contains("completion audit"));
    }

    #[test]
    fn build_audit_prompt_uses_continue_template_when_no_budget_set() {
        let _g = lock();
        let mut g = GoalState::new("ship X".into(), None, None, false);
        g.tokens_used = 9_999_999;
        let p = build_audit_prompt(&g);
        assert!(p.contains("completion audit"));
        assert!(!p.contains("budget-exhausted"));
    }

    #[test]
    fn auto_continue_defaults_off_and_can_be_enabled() {
        // Phase D1: --auto on /goal start flips this; default new() is
        // false so the historical manual / /loop-driven cadence stays.
        let g = GoalState::new("ship X".into(), None, None, false);
        assert!(!g.auto_continue);
        let g2 = GoalState::new("ship X".into(), None, None, true);
        assert!(g2.auto_continue);
    }

    #[test]
    fn auto_continue_round_trips_through_serde() {
        // Phase D1: persistence — goal_snapshot must round-trip the
        // auto_continue flag so /load resumes a session in the same
        // continuation mode it was started in.
        let mut g = GoalState::new("ship X".into(), Some(50_000), None, true);
        g.tokens_used = 12_345;
        let json = serde_json::to_string(&g).unwrap();
        let back: GoalState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.auto_continue, true);
        assert_eq!(back.tokens_used, 12_345);
        // Older snapshots without the field deserialize as false
        // (#[serde(default)]).
        let legacy = serde_json::json!({
            "objective": "old goal",
            "started_at": 100,
            "budget_tokens": null,
            "budget_time_secs": null,
            "tokens_used": 0,
            "iterations_done": 0,
            "status": "active",
            "last_audit": null,
            "last_message": null,
            "completed_at": null,
        });
        let g_legacy: GoalState = serde_json::from_value(legacy).unwrap();
        assert!(!g_legacy.auto_continue);
    }
}
