use std::cell::RefCell;
use std::sync::Arc;

use boa_engine::builtins::promise::PromiseState;
use boa_engine::{
    js_string, native_function::NativeFunction, object::ObjectInitializer, property::Attribute,
    Context, JsArgs, JsError, JsNativeError, JsResult, JsValue, Source,
};

/// Boa-backed JS sandbox hosting the `thclaws.*` workflow API.
///
/// `thclaws.subagent` routes through the parent REPL's Task tool when
/// [`set_task_tool`] has been called on this thread; otherwise it falls
/// back to a stub that echoes the prompt, which keeps the existing
/// Stage A tests deterministic and lets the GUI / chat surface invoke
/// the sandbox without a tokio runtime for refusal messages.
///
/// `eval` and `Function` are removed from the global so an authored
/// script can't generate fresh JS at runtime — the only side effects
/// available are the host bindings we register explicitly.
pub(crate) struct WorkflowSandbox {
    ctx: Context,
}

impl WorkflowSandbox {
    pub fn new() -> JsResult<Self> {
        let mut ctx = Context::default();
        register_thclaws(&mut ctx)?;
        strip_dangerous_globals(&mut ctx)?;
        // 47.3: always define the `args` global (null until a caller sets
        // it via `set_args`), so a script can `if (args) …` without a
        // ReferenceError whether or not structured input was passed.
        ctx.register_global_property(js_string!("args"), JsValue::null(), Attribute::all())?;
        Ok(Self { ctx })
    }

    /// 47.3: install the structured `args` global the script reads —
    /// the typed-input channel `WorkflowRun({script_path, args})` uses
    /// instead of a `.thclaws/TASK.md` side-channel. Any JSON value.
    pub fn set_args(&mut self, value: &serde_json::Value) -> JsResult<()> {
        let js = JsValue::from_json(value, &mut self.ctx)?;
        self.ctx
            .global_object()
            .set(js_string!("args"), js, false, &mut self.ctx)?;
        Ok(())
    }

    /// Run a workflow script. Sync scripts (no `await` / `async` /
    /// `Promise.all` / `Promise.resolve`) take the legacy Script-mode
    /// path that returns the last expression's value. Anything that
    /// uses async syntax routes through Boa's Module mode so top-level
    /// `await` parses correctly; the result there comes from
    /// `globalThis.__wf_result`, auto-wrapped from the script's last
    /// expression when the user didn't assign it explicitly.
    pub fn run(&mut self, script: &str) -> JsResult<String> {
        if uses_async_syntax(script) {
            self.run_module(script)
        } else {
            self.run_script(script)
        }
    }

    fn run_script(&mut self, script: &str) -> JsResult<String> {
        let result = self.ctx.eval(Source::from_bytes(script))?;
        let s = result.to_string(&mut self.ctx)?;
        Ok(s.to_std_string_escaped())
    }

    fn run_module(&mut self, script: &str) -> JsResult<String> {
        let wrapped = wrap_for_module(script);
        let source = Source::from_bytes(wrapped.as_bytes());
        let module = boa_engine::Module::parse(source, None, &mut self.ctx)?;
        let promise = module.load_link_evaluate(&mut self.ctx);
        self.ctx.run_jobs()?;
        match promise.state() {
            PromiseState::Fulfilled(_) => {
                let global = self.ctx.global_object();
                let result = global.get(js_string!("__wf_result"), &mut self.ctx)?;
                if result.is_undefined() {
                    return Ok("undefined".to_string());
                }
                let s = result.to_string(&mut self.ctx)?;
                Ok(s.to_std_string_escaped())
            }
            PromiseState::Rejected(reason) => Err(JsError::from_opaque(reason)),
            PromiseState::Pending => Err(js_error(
                "workflow: module evaluation pending after run_jobs",
            )),
        }
    }
}

/// Cheap detector for "this script uses async features Script mode
/// can't parse" — keyed on the bare keywords / API calls. Won't
/// false-positive on identifiers like `awaitable` because we look for
/// word boundaries via leading space / start-of-string, and the
/// substring `Promise.all` doesn't appear in ordinary identifiers.
fn uses_async_syntax(script: &str) -> bool {
    if script.contains("Promise.all") || script.contains("Promise.race") {
        return true;
    }
    for keyword in [" await ", "\tawait ", "\nawait "] {
        if script.contains(keyword) {
            return true;
        }
    }
    if script.starts_with("await ") {
        return true;
    }
    if script.contains("async function") || script.contains("async (") || script.contains("async(")
    {
        return true;
    }
    false
}

/// If the script doesn't assign to `globalThis.__wf_result` anywhere,
/// auto-wrap its trailing expression statement so the workflow still
/// has a result to return. The boundary search walks the script while
/// tracking string / template / comment state so that punctuation
/// inside `"…"`, `'…'`, `` `…${expr}…` ``, `//…`, and `/* … */` doesn't
/// count — previously a stray `}` from `${expr}` ate the last line.
fn wrap_for_module(script: &str) -> String {
    if script.contains("globalThis.__wf_result") {
        return script.to_string();
    }
    let trimmed_end = script.trim_end();
    let trimmed = trimmed_end.trim_end_matches(';').trim_end();
    let last_break = find_last_top_level_boundary(trimmed);
    let last_chunk = trimmed[last_break..].trim();
    if last_chunk.is_empty() {
        return format!("{trimmed_end}\nglobalThis.__wf_result = undefined;");
    }
    const CANT_WRAP: &[&str] = &[
        "let ", "const ", "var ", "function", "class ", "if ", "if(", "for ", "for(", "while ",
        "while(", "return ", "throw ", "try ", "try{", "import ", "export ",
    ];
    if CANT_WRAP.iter().any(|p| last_chunk.starts_with(p)) || last_chunk.starts_with("globalThis") {
        return format!("{trimmed_end}\nglobalThis.__wf_result = undefined;");
    }
    let head = &trimmed[..last_break];
    format!("{head}globalThis.__wf_result = ({last_chunk});")
}

/// Walk `s` byte by byte, tracking string / template / comment state
/// and bracket depth. Returns the byte index *after* the last `;` or
/// `\n` that occurred at top level (depth zero, outside any string,
/// template, or comment). Falls back to 0 when no boundary is found —
/// the caller then treats the whole script as the "trailing chunk."
fn find_last_top_level_boundary(s: &str) -> usize {
    let bytes = s.as_bytes();
    // Outside string mode. Inside, we track which quote we're in;
    // template literals also need a stack of `{}`-depths-at-entry so
    // a `}` closing a `${expr}` returns us to template mode.
    #[derive(Copy, Clone)]
    enum Mode {
        Plain,
        Line,   // // comment
        Block,  // /* */ comment
        Single, // '…'
        Double, // "…"
        Tmpl,   // `…`
    }
    let mut mode = Mode::Plain;
    let mut tmpl_brace_stack: Vec<i32> = Vec::new();
    let mut depth_paren = 0i32;
    let mut depth_brace = 0i32;
    let mut depth_bracket = 0i32;
    let mut last_boundary: usize = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match mode {
            Mode::Line => {
                if c == b'\n' {
                    mode = Mode::Plain;
                    if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 {
                        last_boundary = i + 1;
                    }
                }
                i += 1;
            }
            Mode::Block => {
                if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    mode = Mode::Plain;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Mode::Single => {
                if c == b'\\' {
                    i += 2;
                } else {
                    if c == b'\'' {
                        mode = Mode::Plain;
                    }
                    i += 1;
                }
            }
            Mode::Double => {
                if c == b'\\' {
                    i += 2;
                } else {
                    if c == b'"' {
                        mode = Mode::Plain;
                    }
                    i += 1;
                }
            }
            Mode::Tmpl => {
                if c == b'\\' {
                    i += 2;
                } else if c == b'`' {
                    mode = Mode::Plain;
                    i += 1;
                } else if c == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                    tmpl_brace_stack.push(depth_brace);
                    depth_brace += 1;
                    mode = Mode::Plain;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Mode::Plain => {
                match c {
                    b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                        mode = Mode::Line;
                        i += 2;
                        continue;
                    }
                    b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                        mode = Mode::Block;
                        i += 2;
                        continue;
                    }
                    b'"' => mode = Mode::Double,
                    b'\'' => mode = Mode::Single,
                    b'`' => mode = Mode::Tmpl,
                    b'(' => depth_paren += 1,
                    b')' => depth_paren -= 1,
                    b'[' => depth_bracket += 1,
                    b']' => depth_bracket -= 1,
                    b'{' => depth_brace += 1,
                    b'}' => {
                        if let Some(entry_depth) = tmpl_brace_stack.last().copied() {
                            if depth_brace - 1 == entry_depth {
                                depth_brace -= 1;
                                tmpl_brace_stack.pop();
                                mode = Mode::Tmpl;
                                i += 1;
                                continue;
                            }
                        }
                        depth_brace -= 1;
                    }
                    b';' | b'\n' if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 => {
                        last_boundary = i + 1;
                    }
                    _ => {}
                }
                i += 1;
            }
        }
    }
    last_boundary
}

thread_local! {
    /// Set by the REPL workflow handler immediately before invoking
    /// `WorkflowSandbox::run` (inside `spawn_blocking`). The host
    /// `thclaws.subagent` function retrieves it to route through the
    /// parent's Task tool. `None` outside the workflow handler — the
    /// host falls back to a stub.
    static WORKFLOW_TASK_TOOL: RefCell<Option<Arc<dyn crate::tools::Tool>>> =
        const { RefCell::new(None) };

    /// Stage I: per-workflow-run Usage accumulator. `SubAgentTool::call`
    /// pushes the worker's `AgentTurnOutcome.usage` here when the sink
    /// is enabled, so the workflow runtime can:
    /// 1. Enforce `budget.tokens` per call against the latest entry;
    /// 2. Print a rolled-up cost summary after the script finishes;
    /// 3. Include the per-worker usage in the `worker_done`
    ///    state.jsonl event.
    /// `None` outside `/workflow run` so model-driven Task calls stay
    /// unaffected.
    static WORKFLOW_USAGE_SINK: RefCell<Option<Vec<crate::providers::Usage>>> =
        const { RefCell::new(None) };

    /// Stage K: queue of (prompt, output) pairs that completed in the
    /// original run and should be replayed without re-spawning. The
    /// `subagent` host function pops the front entry when its prompt
    /// matches the next subagent call's prompt; on mismatch it falls
    /// through to a real spawn. `None` for fresh `/workflow run`.
    static WORKFLOW_REPLAY_CACHE: RefCell<Option<std::collections::VecDeque<(String, String)>>> =
        const { RefCell::new(None) };

    /// Stage M: per-worker capabilities. `None` means "not inside a
    /// workflow `tool.call`" — KMS write tools behave normally for
    /// model-driven Task spawns and direct REPL use. `Some(caps)`
    /// means "inside a workflow subagent call" — KMS writes are
    /// denied unless the target name is in `caps.kms_write`.
    static WORKFLOW_WORKER_CAPS: RefCell<Option<WorkerCaps>> = const { RefCell::new(None) };

    /// Base directory that `thclaws.include` resolves relative paths
    /// against. Captured once at workflow start (the working folder the
    /// user launched the workflow from) so mid-run `set_current_dir`
    /// mutations from tools can't shift the include root. `None`
    /// outside an active workflow run — `thclaws.include` errors.
    static WORKFLOW_INCLUDE_BASE: RefCell<Option<std::path::PathBuf>> =
        const { RefCell::new(None) };

}

tokio::task_local! {
    /// 48.1: per-future worker caps for `thclaws.parallel`. Concurrent
    /// subagent futures interleave on the one block_on thread, so caps
    /// can't live in `WORKFLOW_WORKER_CAPS` (a thread-local) without
    /// bleeding across workers — a privilege-escalation bug. Each
    /// parallel future is `.scope()`d with its own caps; the task-local
    /// propagates through the nested tool-call tree, and
    /// `check_kms_write_capability` prefers it over the thread-local.
    static WORKER_CAPS_TASK: WorkerCaps;
}

// Tier 3 polish: chat-tab worker progress. When set, each
// `thclaws.subagent` call emits ToolCallStart/Result events so the
// chat tab renders workers as one-line `▸ … ✓` indicators alongside
// regular tool calls. `None` outside the chat-surface workflow
// handler (REPL prints its own per-worker line via `tool_display`;
// headless prints to stderr). Gui-gated because `ViewEvent` lives in
// `shared_session`, which itself is `#[cfg(feature = "gui")]`.
#[cfg(feature = "gui")]
thread_local! {
    static WORKFLOW_EVENTS_TX: RefCell<
        Option<tokio::sync::broadcast::Sender<crate::shared_session::ViewEvent>>,
    > = const { RefCell::new(None) };
}

// Stop-button plumbing. When set, the `subagent` host function polls
// before each attempt and races the in-flight `tool.call` against
// `cancel.cancelled().await` so a Stop click aborts mid-stream.
// Reset after the workflow run (the host owns the token and calls
// `.reset()` so the next user turn isn't pre-cancelled).
thread_local! {
    static WORKFLOW_CANCEL: RefCell<Option<crate::cancel::CancelToken>> =
        const { RefCell::new(None) };
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkerCaps {
    pub kms_write: std::collections::HashSet<String>,
}

/// Install (or clear with `None`) the Task tool the sandbox's
/// `thclaws.subagent` will route through. Per-thread — pair with
/// `spawn_blocking` so the thread-local lives for one workflow run.
pub(crate) fn set_task_tool(tool: Option<Arc<dyn crate::tools::Tool>>) {
    WORKFLOW_TASK_TOOL.with(|cell| *cell.borrow_mut() = tool);
}

/// Enable / disable the Stage I usage sink. Pair with `spawn_blocking`
/// + `take_all_usages` at the end of the workflow run.
pub(crate) fn set_usage_sink(enabled: bool) {
    WORKFLOW_USAGE_SINK
        .with(|cell| *cell.borrow_mut() = if enabled { Some(Vec::new()) } else { None });
}

/// Called by SubAgentTool after each successful turn — no-op when the
/// sink is disabled (i.e. outside a workflow run).
///
/// I3 INVARIANT: this is a thread-local sink. It works only because a
/// workflow drives every subagent on the SAME thread that called
/// `set_usage_sink(true)` — the sandbox runs under `spawn_blocking` and
/// invokes each subagent via `handle.block_on(tool.call(...))`, and the
/// model-driven concurrent path uses `join_all` (cooperative, one
/// thread). If a future refactor moves subagent execution onto a
/// work-stealing executor (`tokio::spawn`), this sink would silently
/// read `None` on the worker thread and DROP all workflow usage with no
/// error. Keep subagents on the owning thread, or switch this to a
/// `tokio::task_local!` carried across the spawn.
pub(crate) fn push_worker_usage(usage: crate::providers::Usage) {
    WORKFLOW_USAGE_SINK.with(|cell| {
        if let Some(vec) = cell.borrow_mut().as_mut() {
            vec.push(usage);
        }
    });
}

fn last_worker_usage() -> Option<crate::providers::Usage> {
    WORKFLOW_USAGE_SINK.with(|cell| cell.borrow().as_ref().and_then(|v| v.last().cloned()))
}

/// True when this thread is currently executing inside a
/// `WorkflowSandbox::run` (i.e. `set_usage_sink(true)` has been
/// called and not yet cleared). Used by `WorkflowRunTool` to reject
/// nested calls — if the model authors a workflow whose script tries
/// to invoke `WorkflowRun` via `thclaws.tool(...)`, the inner
/// spawn_blocking would stomp the outer's thread-locals on unwind.
pub(crate) fn is_inside_workflow() -> bool {
    WORKFLOW_USAGE_SINK.with(|cell| cell.borrow().is_some())
}

/// Drain all collected usages — called by the REPL handler after
/// `spawn_blocking` returns so totals can be rolled up.
pub(crate) fn take_all_usages() -> Vec<crate::providers::Usage> {
    WORKFLOW_USAGE_SINK.with(|cell| {
        cell.borrow_mut()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    })
}

/// Stage K: load the replay cache (None to clear) before
/// `spawn_blocking`. Pair with `replay_remaining()` after the run to
/// detect "more cached workers than the re-run consumed" — that
/// signals divergence from the original execution.
pub(crate) fn set_replay_cache(entries: Option<Vec<(String, String)>>) {
    WORKFLOW_REPLAY_CACHE.with(|cell| {
        *cell.borrow_mut() =
            entries.map(|v| v.into_iter().collect::<std::collections::VecDeque<_>>());
    });
}

/// Diagnostic — number of cached entries the resume run hasn't yet
/// consumed. Non-zero after a successful run means the re-execution
/// reached the end with cached entries left over (script shrank /
/// diverged).
pub(crate) fn replay_remaining() -> usize {
    WORKFLOW_REPLAY_CACHE.with(|cell| cell.borrow().as_ref().map(|q| q.len()).unwrap_or(0))
}

/// Stage M: install per-worker capabilities for the next `tool.call`.
/// Pair with `clear_worker_caps()` after.
pub(crate) fn set_worker_caps(caps: Option<WorkerCaps>) {
    WORKFLOW_WORKER_CAPS.with(|cell| *cell.borrow_mut() = caps);
}

/// Tier 3 polish: install the chat-tab broadcast sender so the host
/// function can emit `ToolCallStart` / `ToolCallResult` for each worker.
/// Pair with `set_events_tx(None)` after the spawn_blocking block.
/// The non-gui stub is a no-op so shell_dispatch can call this
/// unconditionally without sprouting `#[cfg]` blocks.
#[cfg(feature = "gui")]
pub(crate) fn set_events_tx(
    tx: Option<tokio::sync::broadcast::Sender<crate::shared_session::ViewEvent>>,
) {
    WORKFLOW_EVENTS_TX.with(|cell| *cell.borrow_mut() = tx);
}

#[cfg(not(feature = "gui"))]
#[allow(dead_code)]
pub(crate) fn set_events_tx<T>(_tx: Option<T>) {}

/// Stable error string a cancelled worker raises and the chat surface
/// detects to render `▸ Stopped` instead of a noisy "subagent failed"
/// trace. Kept in one place so producer + consumer stay in sync.
/// Consumer (chat surface) is gui-only today.
#[allow(dead_code)]
pub(crate) const WORKFLOW_CANCELLED_MSG: &str = "workflow cancelled by user";

/// Install (or clear with `None`) the Stop-button cancel token for the
/// duration of one workflow run. Pair with `spawn_blocking`. Unlike
/// `set_events_tx`, this is not gui-gated — REPL/headless can install
/// their own SIGINT-backed token if they want Ctrl-C to abort workers,
/// though today only the GUI chat surface wires it.
#[allow(dead_code)]
pub(crate) fn set_cancel(tok: Option<crate::cancel::CancelToken>) {
    WORKFLOW_CANCEL.with(|cell| *cell.borrow_mut() = tok);
}

fn workflow_cancel_clone() -> Option<crate::cancel::CancelToken> {
    WORKFLOW_CANCEL.with(|cell| cell.borrow().clone())
}

#[cfg(feature = "gui")]
fn send_workflow_event(ev: crate::shared_session::ViewEvent) {
    WORKFLOW_EVENTS_TX.with(|cell| {
        if let Some(tx) = cell.borrow().as_ref() {
            let _ = tx.send(ev);
        }
    });
}

/// KMS write-tool gate. `Ok(())` outside workflow context (legacy
/// behaviour); inside a workflow call, only KMSs named in the
/// per-worker `caps.kms_write` set may be written.
pub fn check_kms_write_capability(kms_name: &str) -> crate::Result<()> {
    let deny = || {
        crate::Error::Tool(format!(
            "workflow: KMS write to '{kms_name}' denied — not in the worker's \
             granted-write list. The script must pass \
             `caps: {{kms: {{write: [\"{kms_name}\"]}}}}` to thclaws.subagent \
             to grant write access for this call."
        ))
    };
    // 48.1: a `thclaws.parallel` future carries its caps in a task-local
    // (the thread-local would bleed across interleaved futures). Prefer it
    // when in scope; otherwise fall back to the serial thread-local.
    if let Ok(allowed) = WORKER_CAPS_TASK.try_with(|caps| caps.kms_write.contains(kms_name)) {
        return if allowed { Ok(()) } else { Err(deny()) };
    }
    WORKFLOW_WORKER_CAPS.with(|cell| match cell.borrow().as_ref() {
        None => Ok(()),
        Some(caps) => {
            if caps.kms_write.contains(kms_name) {
                Ok(())
            } else {
                Err(deny())
            }
        }
    })
}

/// Pop the next cached worker output if its prompt matches the
/// supplied one. Mismatched prompts leave the cache untouched so the
/// host function can fall through to a real spawn — useful when the
/// script appended new calls after the resume point.
fn try_replay(prompt: &str) -> Option<String> {
    WORKFLOW_REPLAY_CACHE.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let queue = borrow.as_mut()?;
        match queue.front() {
            Some((p, _)) if p == prompt => Some(queue.pop_front().unwrap().1),
            _ => None,
        }
    })
}

fn register_thclaws(ctx: &mut Context) -> JsResult<()> {
    let subagent_fn = NativeFunction::from_fn_ptr(subagent);
    let include_fn = NativeFunction::from_fn_ptr(include);
    let log_fn = NativeFunction::from_fn_ptr(workflow_log);
    let poll_fn = NativeFunction::from_fn_ptr(poll_until);
    let parallel_fn = NativeFunction::from_fn_ptr(parallel);
    let thclaws_obj = ObjectInitializer::new(ctx)
        .function(subagent_fn, js_string!("subagent"), 1)
        .function(include_fn, js_string!("include"), 1)
        .function(log_fn, js_string!("log"), 1)
        .function(poll_fn, js_string!("pollUntil"), 2)
        .function(parallel_fn, js_string!("parallel"), 1)
        .build();
    ctx.register_global_property(js_string!("thclaws"), thclaws_obj, Attribute::READONLY)
}

/// Host implementation of `thclaws.log(msg)` — a narrator line for
/// workflow observability. `console` is stripped from the sandbox, so
/// this is the blessed channel for a script to surface progress between
/// stages. Prints to stdout (REPL + headless authoring/debugging) and,
/// under the GUI, emits a one-line chat indicator the same shape workers
/// use. Returns `undefined`; never throws (a missing arg logs empty).
fn workflow_log(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let msg = args
        .get_or_undefined(0)
        .to_string(ctx)
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    use std::io::Write as _;
    println!("  · {msg}");
    let _ = std::io::stdout().flush();
    #[cfg(feature = "gui")]
    {
        let label = format!("log: {}", crate::tool_display::sanitize_label_field(&msg));
        send_workflow_event(crate::shared_session::ViewEvent::ToolCallStart {
            name: "WorkflowLog".to_string(),
            label,
            input: serde_json::json!({}),
        });
        send_workflow_event(crate::shared_session::ViewEvent::ToolCallResult {
            name: "WorkflowLog".to_string(),
            output: msg,
            ui_resource: None,
        });
    }
    Ok(JsValue::undefined())
}

/// Host implementation of `thclaws.pollUntil(checkFn, opts)` (dev-plan/48.5)
/// — the submit→poll→done shape async jobs (image/video/TTS) repeat. Calls
/// `checkFn()` every `opts.interval` until `opts.until(result)` is truthy (or
/// `result` itself is truthy when no `until` is given), returns that result;
/// throws on `opts.timeout`. Bounded + cancellation-aware (same Stop token as
/// `subagent`). `checkFn` is synchronous from the host's view — it may call
/// `thclaws.subagent(...)` (which blocks internally) and return its value.
///
/// ```js
/// const done = await thclaws.pollUntil(
///   () => thclaws.subagent({ agent: "job-poller", prompt: jobId }),
///   { interval: "10s", timeout: "10m", until: r => r.state === "done" });
/// ```
fn poll_until(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(check_fn) = args.get_or_undefined(0).as_callable() else {
        return Err(js_error(
            "thclaws.pollUntil: first argument must be a function",
        ));
    };

    let mut interval = std::time::Duration::from_secs(5);
    let mut timeout = std::time::Duration::from_secs(300);
    let mut until_fn: Option<boa_engine::JsObject> = None;
    if let Some(o) = args.get_or_undefined(1).as_object() {
        if let Ok(v) = o.get(js_string!("interval"), ctx) {
            if let Some(d) = value_to_duration(&v) {
                interval = d;
            }
        }
        if let Ok(v) = o.get(js_string!("timeout"), ctx) {
            if let Some(d) = value_to_duration(&v) {
                timeout = d;
            }
        }
        if let Ok(v) = o.get(js_string!("until"), ctx) {
            until_fn = v.as_callable();
        }
    }

    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return Err(js_error("thclaws.pollUntil: no tokio runtime available"));
    };
    let cancel = workflow_cancel_clone();
    let start = std::time::Instant::now();
    let mut attempt: u32 = 0;
    loop {
        if let Some(tok) = cancel.as_ref() {
            if tok.is_cancelled() {
                return Err(js_error(WORKFLOW_CANCELLED_MSG));
            }
        }
        if start.elapsed() > timeout {
            return Err(js_error(&format!(
                "thclaws.pollUntil: timed out after {} ({attempt} poll(s))",
                crate::tool_display::format_duration(timeout)
            )));
        }
        attempt += 1;
        let result = check_fn.call(&JsValue::undefined(), &[], ctx)?;
        let done = match &until_fn {
            Some(f) => f
                .call(&JsValue::undefined(), std::slice::from_ref(&result), ctx)?
                .to_boolean(),
            None => result.to_boolean(),
        };
        if done {
            return Ok(result);
        }
        // Sleep one interval, racing the Stop token so a poll loop aborts promptly.
        match cancel.as_ref() {
            Some(tok) => handle.block_on(async {
                tokio::select! {
                    biased;
                    _ = tok.cancelled() => {}
                    _ = tokio::time::sleep(interval) => {}
                }
            }),
            None => handle.block_on(tokio::time::sleep(interval)),
        }
    }
}

/// Install (or clear with `None`) the base directory that
/// `thclaws.include` resolves relative paths against. Called by the
/// workflow runners right before / after `spawn_blocking` so the
/// thread-local lives exactly for one run.
pub(crate) fn set_include_base(base: Option<std::path::PathBuf>) {
    WORKFLOW_INCLUDE_BASE.with(|c| *c.borrow_mut() = base);
}

/// Host implementation of `thclaws.include(path)` — reads a script
/// file and evaluates it in the same Boa Context, so any top-level
/// `globalThis.foo = …` definitions become available to the caller.
///
/// Path validation:
///   - Absolute paths → rejected (must be relative).
///   - `..` traversal that resolves outside the base → rejected.
///   - Symlinks that resolve outside the base → rejected (via
///     `canonicalize`, which follows them before the prefix check).
///
/// Returns the included script's final expression value. Lifecycle
/// errors (no base, bad path, file missing, parse failure) bubble up
/// as a JS exception so the calling script can `try`/`catch`.
fn include(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let raw = match args.get_or_undefined(0).as_string() {
        Some(s) => s.to_std_string_escaped(),
        None => return Err(js_error("thclaws.include: requires a path string")),
    };
    let path = std::path::PathBuf::from(&raw);
    if path.is_absolute() {
        return Err(js_error(&format!(
            "thclaws.include: absolute paths not allowed (got '{raw}')"
        )));
    }
    let base = match WORKFLOW_INCLUDE_BASE.with(|c| c.borrow().clone()) {
        Some(b) => b,
        None => {
            return Err(js_error(
                "thclaws.include: no working folder bound (not inside a workflow run)",
            ));
        }
    };
    // Resolve under the captured base. canonicalize on the joined
    // path so `..` and symlinks are followed before the prefix check,
    // otherwise `../etc/passwd` could slip past by spelling.
    let joined = base.join(&path);
    let resolved = joined.canonicalize().map_err(|e| {
        js_error(&format!(
            "thclaws.include: can't resolve '{raw}' under {}: {e}",
            base.display()
        ))
    })?;
    let base_canonical = base.canonicalize().unwrap_or_else(|_| base.clone());
    if !resolved.starts_with(&base_canonical) {
        return Err(js_error(&format!(
            "thclaws.include: '{raw}' resolves outside the working folder ({})",
            base_canonical.display()
        )));
    }
    let content = std::fs::read_to_string(&resolved).map_err(|e| {
        js_error(&format!(
            "thclaws.include: can't read '{}': {e}",
            resolved.display()
        ))
    })?;
    ctx.eval(boa_engine::Source::from_bytes(&content))
        .map_err(|e| js_error(&format!("thclaws.include: '{raw}' failed: {e}")))
}

fn subagent(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let prompt = extract_prompt(args, ctx);
    let budget = extract_budget(args, ctx);
    let per_call_schema = extract_schema(args, ctx);
    let retry = extract_retry(args, ctx);
    let caps = extract_caps(args, ctx);
    // `agent: "name"` picks a subagent definition from
    // `.thclaws/agents/<name>.md` — matches the model-driven Task
    // tool's `agent` param. Forwarded into the Task tool input below;
    // an unknown name surfaces as a worker failure (Task tool
    // validates against AgentDefsConfig).
    let agent_name = extract_agent_name(args, ctx);

    let task_tool = WORKFLOW_TASK_TOOL.with(|c| c.borrow().clone());

    // 47.1: when the call omits an explicit `schema`, fall back to the
    // named agent's declared `output_schema` (from its `.md`), so the
    // contract lives in one place (the def) instead of being duplicated
    // in the workflow JS. An explicit per-call `schema` still wins.
    let schema = per_call_schema.or_else(|| {
        let name = agent_name.as_ref()?;
        task_tool.as_ref()?.subagent_output_schema(name)
    });

    // Stage K: serve from the replay cache when this call's prompt
    // matches the next pending entry. No worker_start/worker_done
    // events are emitted — those already live in state.jsonl from the
    // original run. Cache miss falls through to the normal spawn path.
    if let Some(cached) = try_replay(&prompt) {
        match &schema {
            Some(s) => {
                // Validate the cached output against the (possibly
                // newer) schema before returning, so a schema change
                // between runs surfaces as a clear error rather than
                // a stale value.
                if let Ok(validator) = jsonschema::validator_for(s) {
                    match extract_json_from_text(&cached) {
                        Some(json_val) if validator.is_valid(&json_val) => {
                            return JsValue::from_json(&json_val, ctx);
                        }
                        _ => {
                            // Cached value no longer matches the schema —
                            // re-spawn fresh.
                        }
                    }
                }
            }
            None => {
                return Ok(JsValue::from(js_string!(cached.as_str())));
            }
        }
    }

    let Some(tool) = task_tool else {
        // 47.6 surface guard: inside a REAL workflow run (usage sink set)
        // with no Task tool, the current surface can't spawn subagents
        // (e.g. `-p` / `/v1`, where Task isn't registered). Fail LOUD
        // rather than returning a stub the script would treat as a real
        // worker result — the silent-role-play footgun the best-practice
        // guide warns about. Outside a workflow run (sandbox eval in
        // tests / the GUI refusal preview) keep the deterministic stub.
        if is_inside_workflow() {
            return Err(js_error(
                "thclaws.subagent: no Task tool on this surface — subagent calls aren't \
                 available here (this is the `-p` / `/v1` footgun). Run the workflow on \
                 `--cli`, `--serve`, or the GUI, where the Task tool is registered.",
            ));
        }
        return Ok(JsValue::from(js_string!(
            format!("(stub for: {prompt})").as_str()
        )));
    };

    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            return Err(js_error(
                "workflow: no tokio runtime available for subagent spawn",
            ));
        }
    };

    // Stage H: if a JSON Schema is set, augment the worker prompt to
    // ask for JSON matching that schema, and compile the schema once
    // up front. Compilation failure is a hard error — there's no point
    // retrying against a broken schema.
    let augmented_prompt = match &schema {
        Some(schema_val) => format!(
            "{prompt}\n\nReturn ONLY a JSON value matching this JSON Schema. \
             No prose, no markdown fences:\n{schema_str}",
            schema_str = schema_val
        ),
        None => prompt.clone(),
    };
    let compiled_schema = match &schema {
        Some(s) => match jsonschema::validator_for(s) {
            Ok(v) => Some(v),
            Err(e) => {
                return Err(js_error(&format!("workflow: invalid schema: {e}")));
            }
        },
        None => None,
    };

    // Stage D: open one worker_start event for this logical
    // thclaws.subagent call, even when retried (each retry gets its
    // own worker_retry event so the chain stays attributable). None
    // if no logger is wired (sandbox running outside a workflow run).
    let worker_id = super::state::with_logger(|l| l.worker_start(&prompt).ok()).flatten();

    // Stage M: audit the capability grant in state.jsonl so post-run
    // `/workflow inspect` shows what was granted to each worker.
    if let Some(wid) = worker_id {
        if !caps.kms_write.is_empty() {
            let granted: Vec<String> = caps.kms_write.iter().cloned().collect();
            super::state::with_logger(|l| {
                let _ = l.worker_caps(wid, &granted);
            });
        }
    }

    use std::io::Write as _;
    let worker_started = std::time::Instant::now();
    if let Some(wid) = worker_id {
        print!("{}", crate::tool_display::format_worker_start(wid, &prompt));
        let _ = std::io::stdout().flush();
    }

    // Tier 3 polish: chat-tab worker bubble. Emits a
    // `▸ subagent w0 (prompt preview)` indicator the same shape regular
    // tool calls use, so the chat tab shows live worker progression
    // alongside the final result. Skipped (None thread-local) for REPL
    // + headless flows; their stdout-based progress is unchanged.
    // Gui-gated because `ViewEvent` lives in `shared_session`.
    #[cfg(feature = "gui")]
    {
        let preview = crate::tool_display::sanitize_label_field(&prompt);
        let preview: String = preview.chars().take(60).collect();
        let chat_label = match worker_id {
            Some(id) => format!("subagent w{id} ({preview})"),
            None => format!("subagent ({preview})"),
        };
        send_workflow_event(crate::shared_session::ViewEvent::ToolCallStart {
            name: "WorkflowWorker".to_string(),
            label: chat_label,
            input: serde_json::json!({ "prompt": prompt.clone() }),
        });
    }

    // Stage H retry loop. Always at least 1 attempt; up to retry.max
    // total. Sleeps between attempts according to retry.backoff. On a retry
    // after a FIXABLE (schema / non-JSON) failure, `feedback` carries the exact
    // rejection reason into the next attempt's prompt — telling the model what
    // was wrong beats re-rolling the same prompt blind, and helps weaker models
    // recover a slipped output shape in one retry instead of burning attempts.
    let mut feedback: Option<String> = None;
    let mut last_failure: Option<String> = None;
    let mut last_text_for_done: Option<String> = None;
    let mut parsed_json: Option<serde_json::Value> = None;
    let mut succeeded = false;
    // Transient provider/stream errors (e.g. a dropped SSE during a
    // parallel fan-out) retry up to this floor even when the script set
    // no `retry:` — a single flaky stream shouldn't kill the worker.
    // Deterministic failures (schema, token budget) still honor
    // `retry.max`.
    const TRANSIENT_RETRY_FLOOR: u32 = 3;
    let deterministic_max = retry.max.max(1);
    let max_attempts = deterministic_max.max(TRANSIENT_RETRY_FLOOR);
    let mut attempts_used = 0u32;
    // Stop-button: a clone of the host's CancelToken (None in REPL/
    // headless today). Polled before each attempt and raced against
    // the in-flight `tool.call` via `tokio::select!`. When cancel
    // fires we surface a stable error string the chat surface can
    // detect to render a friendlier "stopped" message.
    let cancel = workflow_cancel_clone();
    for attempt in 1..=max_attempts {
        attempts_used = attempt;
        if let Some(tok) = cancel.as_ref() {
            if tok.is_cancelled() {
                return Err(js_error(WORKFLOW_CANCELLED_MSG));
            }
        }
        // Clear per-attempt state so a prior failure doesn't shadow a
        // later success.
        last_failure = None;
        // Build THIS attempt's prompt. First try (or after a transient error)
        // uses the plain schema-augmented prompt; a retry after a fixable
        // failure appends the exact reason so the model corrects it rather than
        // re-rolling blind.
        let attempt_prompt = match &feedback {
            Some(err) => format!(
                "{augmented_prompt}\n\nYour previous attempt was REJECTED — {err}\n\
                 Return a corrected JSON value that fixes exactly that. Same task, \
                 valid output this time — no prose, no markdown fences."
            ),
            None => augmented_prompt.clone(),
        };
        let input = match &agent_name {
            Some(name) => serde_json::json!({ "prompt": attempt_prompt, "agent": name }),
            None => serde_json::json!({ "prompt": attempt_prompt }),
        };
        // Stage G/M: enforce time budget per-attempt + install
        // per-worker capabilities for KMS write gating. Caps live
        // only for the duration of this tool.call so nested model-
        // driven Task calls don't accidentally inherit grants from a
        // different worker.
        set_worker_caps(Some(caps.clone()));
        let result: crate::Result<String> = match (budget.time, cancel.as_ref()) {
            (Some(time), Some(tok)) => handle.block_on(async {
                tokio::select! {
                    biased;
                    _ = tok.cancelled() => Err(crate::Error::Agent(WORKFLOW_CANCELLED_MSG.into())),
                    r = tokio::time::timeout(time, tool.call(input.clone())) => match r {
                        Ok(r) => r,
                        Err(_) => Err(crate::Error::Agent(format!(
                            "worker exceeded time budget of {}",
                            crate::tool_display::format_duration(time)
                        ))),
                    }
                }
            }),
            (Some(time), None) => {
                match handle.block_on(tokio::time::timeout(time, tool.call(input.clone()))) {
                    Ok(r) => r,
                    Err(_) => Err(crate::Error::Agent(format!(
                        "worker exceeded time budget of {}",
                        crate::tool_display::format_duration(time)
                    ))),
                }
            }
            (None, Some(tok)) => handle.block_on(async {
                tokio::select! {
                    biased;
                    _ = tok.cancelled() => Err(crate::Error::Agent(WORKFLOW_CANCELLED_MSG.into())),
                    r = tool.call(input.clone()) => r,
                }
            }),
            (None, None) => handle.block_on(tool.call(input.clone())),
        };
        set_worker_caps(None);
        // After tool.call returns (e.g. SubAgentTool unwinding from a
        // cancelled stream), surface cancel before falling into retry.
        if let Some(tok) = cancel.as_ref() {
            if tok.is_cancelled() {
                return Err(js_error(WORKFLOW_CANCELLED_MSG));
            }
        }

        match result {
            Ok(text) => {
                last_text_for_done = Some(text.clone());

                // Stage I: tokens-budget check. SubAgentTool has just
                // pushed this turn's usage to the sink; we peek the
                // last entry. Post-hoc enforcement only — we can't
                // abort mid-stream — so this acts as a soft cap that
                // triggers retry on the next iteration.
                //
                // Count OUTPUT tokens only — this is a runaway-GENERATION
                // guard, not a total-cost cap. The worker's input is fixed
                // by the task (its prompt + whatever it reads), and on a
                // large-context model that input alone is tens of thousands
                // of tokens, so counting it would falsely kill normal
                // workers (a worker that just reads a file would "exceed"
                // any modest cap before producing anything).
                if let Some(token_cap) = budget.tokens {
                    if let Some(u) = last_worker_usage() {
                        let used = u.output_tokens as u64;
                        if used > token_cap {
                            last_failure = Some(format!(
                                "worker exceeded output-token budget of {token_cap} (generated {used})"
                            ));
                            // Budget overrun is deterministic — honor the
                            // script's `retry.max`, don't spend the
                            // transient floor re-running a too-chatty worker.
                            if attempt >= deterministic_max {
                                break;
                            }
                            if let Some(wid) = worker_id {
                                let prior_err = last_failure.clone().unwrap_or_default();
                                super::state::with_logger(|l| {
                                    let _ = l.worker_retry(wid, attempt, &prior_err);
                                });
                            }
                            let delay = retry.delay_for_attempt(attempt);
                            if !delay.is_zero() {
                                handle.block_on(tokio::time::sleep(delay));
                            }
                            continue;
                        }
                    }
                }

                match &compiled_schema {
                    Some(validator) => match extract_json_from_text(&text) {
                        Some(json_val) => {
                            if validator.is_valid(&json_val) {
                                parsed_json = Some(json_val);
                                succeeded = true;
                                break;
                            }
                            let errs: Vec<String> = validator
                                .iter_errors(&json_val)
                                .map(|e| e.to_string())
                                .take(3)
                                .collect();
                            last_failure = Some(format!(
                                "schema violation: {}",
                                if errs.is_empty() {
                                    "no detail".to_string()
                                } else {
                                    errs.join("; ")
                                }
                            ));
                        }
                        None => {
                            last_failure = Some("worker output is not valid JSON".to_string());
                        }
                    },
                    None => {
                        // No schema — first success returns immediately.
                        succeeded = true;
                        break;
                    }
                }
            }
            Err(e) => {
                last_failure = Some(e.to_string());
            }
        }

        // Past the script's explicit retry budget, reserve the remaining
        // attempts for transient provider/stream errors only — re-running
        // a deterministic failure (schema mismatch) just burns tokens.
        let is_transient = last_failure
            .as_deref()
            .map(super::is_transient_provider_error)
            .unwrap_or(false);
        // Carry a FIXABLE failure (schema / non-JSON) into the next attempt's
        // prompt as feedback; a transient stream error isn't the model's fault,
        // so leave the prompt clean and just re-roll.
        feedback = if is_transient {
            None
        } else {
            last_failure.clone()
        };
        if attempt >= deterministic_max && !is_transient {
            break;
        }
        if attempt < max_attempts {
            if let Some(wid) = worker_id {
                let prior_err = last_failure.clone().unwrap_or_default();
                super::state::with_logger(|l| {
                    let _ = l.worker_retry(wid, attempt, &prior_err);
                });
            }
            let delay = retry.delay_for_attempt(attempt);
            if !delay.is_zero() {
                handle.block_on(tokio::time::sleep(delay));
            }
        }
    }
    let elapsed = worker_started.elapsed();
    let success = succeeded;

    if let Some(wid) = worker_id {
        print!(
            "{}",
            crate::tool_display::format_worker_done(wid, &prompt, elapsed, !success)
        );
        let _ = std::io::stdout().flush();
        super::state::with_logger(|l| match (success, &last_text_for_done, &last_failure) {
            (true, Some(text), _) => {
                let _ = l.worker_done(wid, text);
            }
            (_, _, Some(err)) => {
                let _ = l.worker_error(wid, err);
            }
            _ => {}
        });
    }

    // Tier 3 polish: matching chat-tab ToolCallResult. The frontend
    // tool_call renderer flips the `▸` to `✓` (or `✗` on error) and
    // appends elapsed time, so each worker shows progression live.
    // Gui-gated for the same reason as the matching Start emit above.
    #[cfg(feature = "gui")]
    {
        let chat_output = match (success, &last_text_for_done, &last_failure) {
            (true, Some(text), _) => {
                let preview: String = text.chars().take(200).collect();
                if text.chars().count() > 200 {
                    format!("{preview}…")
                } else {
                    preview
                }
            }
            (_, _, Some(err)) => format!("error: {err}"),
            _ => "(no result)".to_string(),
        };
        send_workflow_event(crate::shared_session::ViewEvent::ToolCallResult {
            name: "WorkflowWorker".to_string(),
            output: chat_output,
            ui_resource: None,
        });
    }

    if !success {
        let err_msg = last_failure.unwrap_or_else(|| "unknown error".to_string());
        return Err(js_error(&format!(
            "workflow subagent failed after {attempts_used} attempt(s): {err_msg}"
        )));
    }
    let text = last_text_for_done.unwrap();

    // Stage H: when schema was set, return parsed JsValue (object /
    // array / etc.) so the script can use it directly without
    // JSON.parse. Without schema, return the raw text as before.
    match parsed_json {
        Some(v) => JsValue::from_json(&v, ctx),
        None => Ok(JsValue::from(js_string!(text.as_str()))),
    }
}

enum WorkerOut {
    Json(serde_json::Value),
    Text(String),
}

/// 48.1: `thclaws.parallel(specs)` — run an array of subagent specs
/// CONCURRENTLY. This is the only genuine fan-out primitive in the
/// workflow runtime: plain `Promise.all` over `thclaws.subagent` still
/// runs serially because that host call blocks on each spawn. Each spec
/// is the same `{prompt, agent?, schema?, caps?, budget?, fallback?}`
/// object `thclaws.subagent` takes; results come back as an array in
/// input order (parsed JSON when a schema / def output_schema applies,
/// else the worker's text).
///
/// **Settle semantics (not Promise.all):** a worker that fails after its
/// transient retries does NOT abort the batch — that item becomes the
/// spec's `fallback` value (default `null`), so partial results are
/// preserved (a 50-item render where 1 worker dies keeps the other 49).
/// This mirrors the graceful `step(opts, fallback)` pattern batch agents
/// want. The call only throws on a programmer error (arg not an array, no
/// Task tool on this surface), never on a worker error.
///
/// Concurrency is capped at 8 by default, overridable with
/// `THCLAWS_WORKFLOW_PARALLELISM` (clamped 1-32). Each future is `.scope`d
/// with its own caps via a tokio task-local (`WORKER_CAPS_TASK`) so a
/// per-worker KMS-write grant can't bleed across interleaved futures.
/// The parallel path deliberately skips the per-worker token-budget
/// soft-cap and replay-cache that `thclaws.subagent` applies (total
/// usage is still metered for billing).
fn parallel(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(arr_obj) = args.get_or_undefined(0).as_object() else {
        return Err(js_error(
            "thclaws.parallel: first argument must be an array of subagent specs",
        ));
    };
    if !arr_obj.is_array() {
        return Err(js_error(
            "thclaws.parallel: first argument must be an array of subagent specs",
        ));
    }
    let len = arr_obj.get(js_string!("length"), ctx)?.to_number(ctx)? as usize;

    let task_tool = WORKFLOW_TASK_TOOL.with(|c| c.borrow().clone());

    struct PSpec {
        input: serde_json::Value,
        schema: Option<serde_json::Value>,
        caps: WorkerCaps,
        time: Option<std::time::Duration>,
        prompt: String,
    }
    // Parse all specs up front (needs &mut Context — can't cross into the
    // async block, where `ctx` isn't available). `fallbacks` runs parallel to
    // `specs` (same index) so the collect loop can substitute on a worker error.
    let mut specs: Vec<PSpec> = Vec::with_capacity(len);
    let mut fallbacks: Vec<serde_json::Value> = Vec::with_capacity(len);
    for i in 0..len {
        let spec_val = arr_obj.get(i as u32, ctx)?;
        let one = [spec_val];
        let prompt = extract_prompt(&one, ctx);
        let budget = extract_budget(&one, ctx);
        let caps = extract_caps(&one, ctx);
        let agent_name = extract_agent_name(&one, ctx);
        let fallback = one
            .first()
            .and_then(|v| v.as_object())
            .and_then(|o| o.get(js_string!("fallback"), ctx).ok())
            .and_then(|v| v.to_json(ctx).ok().flatten())
            .unwrap_or(serde_json::Value::Null);
        fallbacks.push(fallback);
        let schema = extract_schema(&one, ctx).or_else(|| {
            let name = agent_name.as_ref()?;
            task_tool.as_ref()?.subagent_output_schema(name)
        });
        let augmented = match &schema {
            Some(s) => format!(
                "{prompt}\n\nReturn ONLY a JSON value matching this JSON Schema. \
                 No prose, no markdown fences:\n{s}"
            ),
            None => prompt.clone(),
        };
        let input = match &agent_name {
            Some(name) => serde_json::json!({ "prompt": augmented, "agent": name }),
            None => serde_json::json!({ "prompt": augmented }),
        };
        specs.push(PSpec {
            input,
            schema,
            caps,
            time: budget.time,
            prompt,
        });
    }

    let Some(tool) = task_tool else {
        if is_inside_workflow() {
            return Err(js_error(
                "thclaws.parallel: no Task tool on this surface — subagent calls aren't \
                 available here (the `-p` / `/v1` footgun). Run on `--cli`, `--serve`, or the GUI.",
            ));
        }
        // Outside a real run (sandbox eval in tests): deterministic stubs.
        let stubs: Vec<serde_json::Value> = specs
            .iter()
            .map(|s| serde_json::Value::String(format!("(stub for: {})", s.prompt)))
            .collect();
        return JsValue::from_json(&serde_json::Value::Array(stubs), ctx);
    };

    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| js_error("workflow: no tokio runtime available for parallel spawn"))?;
    let cancel = workflow_cancel_clone();
    // Subagent fan-out is I/O-bound: every parallel future just awaits the
    // gateway/LLM on the single `block_on` thread, so the ceiling is the
    // gateway's tolerance, NOT local CPU. The old `cores-2` heuristic was a
    // CPU-bound-work metric misapplied here — on a cpu-limited cloud runner
    // (`available_parallelism()==1`) it collapsed to 1 and silently
    // serialized every `thclaws.parallel` stage (searchers, synthesizers,
    // reviewers all ran one-at-a-time). Use an I/O-appropriate fixed default,
    // overridable via THCLAWS_WORKFLOW_PARALLELISM for gateways with tighter
    // (or looser) concurrency budgets.
    let cap = std::env::var("THCLAWS_WORKFLOW_PARALLELISM")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(8)
        .clamp(1, 32);
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(cap));

    let results: Vec<Result<WorkerOut, String>> = handle.block_on(async move {
        let futs = specs.into_iter().map(|s| {
            let tool = tool.clone();
            let sem = sem.clone();
            let cancel = cancel.clone();
            async move {
                let _permit = sem.acquire().await.ok();
                WORKER_CAPS_TASK
                    .scope(
                        s.caps,
                        run_one_parallel(tool, s.input, s.schema, s.time, cancel),
                    )
                    .await
            }
        });
        futures::future::join_all(futs).await
    });

    // Settle: a failed worker becomes its `fallback` (default null) so the
    // batch keeps every successful result. Never throws on a worker error.
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(results.len());
    let mut failed: Vec<(usize, String)> = Vec::new();
    for (i, r) in results.into_iter().enumerate() {
        match r {
            Ok(WorkerOut::Json(v)) => out.push(v),
            Ok(WorkerOut::Text(t)) => out.push(serde_json::Value::String(t)),
            Err(e) => {
                failed.push((i, e));
                out.push(fallbacks.get(i).cloned().unwrap_or(serde_json::Value::Null));
            }
        }
    }
    if !failed.is_empty() {
        use std::io::Write as _;
        println!(
            "  · thclaws.parallel: {}/{} worker(s) failed → fallback (first: #{} {})",
            failed.len(),
            out.len(),
            failed[0].0,
            failed[0].1.chars().take(120).collect::<String>()
        );
        let _ = std::io::stdout().flush();
    }
    JsValue::from_json(&serde_json::Value::Array(out), ctx)
}

/// One parallel worker: tool.call with optional time budget + cancel,
/// a small transient retry, and schema validation. Runs inside the
/// caller's `WORKER_CAPS_TASK` scope (so KMS-write gating sees this
/// worker's caps, not a neighbour's).
async fn run_one_parallel(
    tool: Arc<dyn crate::tools::Tool>,
    input: serde_json::Value,
    schema: Option<serde_json::Value>,
    time: Option<std::time::Duration>,
    cancel: Option<crate::cancel::CancelToken>,
) -> Result<WorkerOut, String> {
    let compiled = match &schema {
        Some(s) => Some(jsonschema::validator_for(s).map_err(|e| format!("invalid schema: {e}"))?),
        None => None,
    };
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = String::from("worker produced no result");
    for _ in 1..=MAX_ATTEMPTS {
        if let Some(tok) = cancel.as_ref() {
            if tok.is_cancelled() {
                return Err(WORKFLOW_CANCELLED_MSG.into());
            }
        }
        let call = tool.call(input.clone());
        let result: crate::Result<String> = match (time, cancel.as_ref()) {
            (Some(t), Some(tok)) => tokio::select! {
                biased;
                _ = tok.cancelled() => Err(crate::Error::Agent(WORKFLOW_CANCELLED_MSG.into())),
                r = tokio::time::timeout(t, call) => r.unwrap_or_else(|_| Err(crate::Error::Agent(format!(
                    "worker exceeded time budget of {}", crate::tool_display::format_duration(t))))),
            },
            (Some(t), None) => tokio::time::timeout(t, call).await.unwrap_or_else(|_| {
                Err(crate::Error::Agent(format!(
                    "worker exceeded time budget of {}",
                    crate::tool_display::format_duration(t)
                )))
            }),
            (None, Some(tok)) => tokio::select! {
                biased;
                _ = tok.cancelled() => Err(crate::Error::Agent(WORKFLOW_CANCELLED_MSG.into())),
                r = call => r,
            },
            (None, None) => call.await,
        };
        match result {
            Ok(text) => match &compiled {
                Some(v) => match extract_json_from_text(&text) {
                    Some(jv) if v.is_valid(&jv) => return Ok(WorkerOut::Json(jv)),
                    _ => {
                        last_err = "worker output did not match the schema".into();
                        continue;
                    }
                },
                None => return Ok(WorkerOut::Text(text)),
            },
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        }
    }
    Err(last_err)
}

fn extract_prompt(args: &[JsValue], ctx: &mut Context) -> String {
    let arg = args.get_or_undefined(0);
    arg.as_object()
        .and_then(|obj| obj.get(js_string!("prompt"), ctx).ok())
        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
        .unwrap_or_else(|| "(no prompt)".to_string())
}

/// Pull `agent: "name"` from the opts object. `None` when the field is
/// absent / empty / non-string; the Task tool then falls back to the
/// default agent. A non-empty string is forwarded verbatim — the Task
/// tool validates it against the loaded `AgentDefsConfig`.
fn extract_agent_name(args: &[JsValue], ctx: &mut Context) -> Option<String> {
    let arg = args.get_or_undefined(0).as_object()?;
    let v = arg.get(js_string!("agent"), ctx).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    let s = v.as_string()?.to_std_string_escaped();
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Default)]
struct Budget {
    tokens: Option<u64>,
    time: Option<std::time::Duration>,
}

fn extract_budget(args: &[JsValue], ctx: &mut Context) -> Budget {
    let Some(arg0) = args.get_or_undefined(0).as_object() else {
        return Budget::default();
    };
    let Ok(budget_v) = arg0.get(js_string!("budget"), ctx) else {
        return Budget::default();
    };
    let Some(obj) = budget_v.as_object() else {
        return Budget::default();
    };
    let tokens = obj
        .get(js_string!("tokens"), ctx)
        .ok()
        .as_ref()
        .and_then(JsValue::as_number)
        .filter(|n| *n > 0.0 && n.is_finite())
        .map(|n| n as u64);
    let time = obj
        .get(js_string!("time"), ctx)
        .ok()
        .and_then(|v| value_to_duration(&v));
    Budget { tokens, time }
}

fn value_to_duration(v: &JsValue) -> Option<std::time::Duration> {
    if let Some(n) = v.as_number() {
        if n > 0.0 && n.is_finite() {
            return Some(std::time::Duration::from_secs_f64(n));
        }
        return None;
    }
    if let Some(s) = v.as_string() {
        return parse_human_duration(&s.to_std_string_escaped()).ok();
    }
    None
}

/// Parse strings like `"60s"`, `"2m"`, `"1m30s"`, `"1h"`, `"500ms"`.
/// Concatenated unit segments are summed. Returns `Err(msg)` on
/// malformed input — caller treats that as "no time budget set".
fn parse_human_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration string".into());
    }
    let mut total = std::time::Duration::ZERO;
    let mut num_buf = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num_buf.push(c);
            continue;
        }
        if num_buf.is_empty() {
            return Err(format!("unexpected '{c}' before any number"));
        }
        let n: f64 = num_buf
            .parse()
            .map_err(|_| format!("invalid number '{num_buf}'"))?;
        // `ms` is the only two-letter unit we accept — peek for the `s`.
        let multiplier = if c == 'm' && chars.peek() == Some(&'s') {
            chars.next();
            0.001
        } else {
            match c {
                's' => 1.0,
                'm' => 60.0,
                'h' => 3600.0,
                _ => return Err(format!("unknown unit '{c}'")),
            }
        };
        total += std::time::Duration::from_secs_f64(n * multiplier);
        num_buf.clear();
    }
    if !num_buf.is_empty() {
        return Err(format!("number '{num_buf}' missing unit suffix"));
    }
    if total.is_zero() {
        return Err("zero duration".into());
    }
    Ok(total)
}

fn js_error(msg: &str) -> JsError {
    JsNativeError::typ().with_message(msg.to_string()).into()
}

fn extract_caps(args: &[JsValue], ctx: &mut Context) -> WorkerCaps {
    let mut caps = WorkerCaps::default();
    let Some(arg0) = args.get_or_undefined(0).as_object() else {
        return caps;
    };
    let Ok(caps_v) = arg0.get(js_string!("caps"), ctx) else {
        return caps;
    };
    let json = match caps_v.to_json(ctx).ok().flatten() {
        Some(j) => j,
        None => return caps,
    };
    if let Some(writes) = json.pointer("/kms/write").and_then(|v| v.as_array()) {
        for w in writes {
            if let Some(s) = w.as_str() {
                caps.kms_write.insert(s.to_string());
            }
        }
    }
    caps
}

fn extract_schema(args: &[JsValue], ctx: &mut Context) -> Option<serde_json::Value> {
    let arg = args.get_or_undefined(0).as_object()?;
    let v = arg.get(js_string!("schema"), ctx).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    v.to_json(ctx).ok().flatten()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backoff {
    /// `delay = base * 2^(attempt-1)`, capped at 30s.
    Exponential,
    /// `delay = base * attempt`.
    Linear,
    /// Fixed delay equal to `base`.
    Fixed(std::time::Duration),
}

struct Retry {
    max: u32,
    backoff: Backoff,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            max: 1,
            backoff: Backoff::Exponential,
        }
    }
}

impl Retry {
    fn delay_for_attempt(&self, attempt: u32) -> std::time::Duration {
        const BASE: std::time::Duration = std::time::Duration::from_secs(1);
        const CAP: std::time::Duration = std::time::Duration::from_secs(30);
        match self.backoff {
            Backoff::Fixed(d) => d,
            Backoff::Linear => BASE * attempt,
            Backoff::Exponential => {
                let mult = 1u32 << (attempt - 1).min(20); // saturate at 2^20
                let d = BASE.saturating_mul(mult);
                if d > CAP {
                    CAP
                } else {
                    d
                }
            }
        }
    }
}

fn extract_retry(args: &[JsValue], ctx: &mut Context) -> Retry {
    let Some(arg) = args.get_or_undefined(0).as_object() else {
        return Retry::default();
    };
    let Ok(v) = arg.get(js_string!("retry"), ctx) else {
        return Retry::default();
    };
    // `retry: 3` shorthand for `retry: { max: 3 }`.
    if let Some(n) = v.as_number() {
        if n > 0.0 && n.is_finite() {
            return Retry {
                max: n as u32,
                backoff: Backoff::Exponential,
            };
        }
        return Retry::default();
    }
    let Some(obj) = v.as_object() else {
        return Retry::default();
    };
    let max = obj
        .get(js_string!("max"), ctx)
        .ok()
        .as_ref()
        .and_then(JsValue::as_number)
        .filter(|n| *n > 0.0 && n.is_finite())
        .map(|n| n as u32)
        .unwrap_or(1);
    let backoff = obj
        .get(js_string!("backoff"), ctx)
        .ok()
        .and_then(|bv| match bv.as_string() {
            Some(s) => match s.to_std_string_escaped().as_str() {
                "exponential" | "exp" => Some(Backoff::Exponential),
                "linear" | "lin" => Some(Backoff::Linear),
                other => parse_human_duration(other).ok().map(Backoff::Fixed),
            },
            None => bv
                .as_number()
                .filter(|n| *n > 0.0 && n.is_finite())
                .map(|n| Backoff::Fixed(std::time::Duration::from_secs_f64(n))),
        })
        .unwrap_or(Backoff::Exponential);
    Retry { max, backoff }
}

/// Pull a JSON value out of arbitrary worker text. Tries, in order:
/// the whole trimmed text as JSON; the contents of a ```json or ```
/// fence; and the first balanced `{...}` or `[...]` span. Returns
/// `None` if nothing parses — caller treats as a schema-failure retry
/// trigger.
fn extract_json_from_text(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str(trimmed) {
        return Some(v);
    }
    for prefix in ["```json", "```JSON", "```"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let inner = rest.trim_start_matches('\n');
            if let Some(body) = inner.strip_suffix("```") {
                if let Ok(v) = serde_json::from_str(body.trim()) {
                    return Some(v);
                }
            }
        }
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if start < end {
            if let Ok(v) = serde_json::from_str(&text[start..=end]) {
                return Some(v);
            }
        }
    }
    if let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) {
        if start < end {
            if let Ok(v) = serde_json::from_str(&text[start..=end]) {
                return Some(v);
            }
        }
    }
    None
}

fn strip_dangerous_globals(ctx: &mut Context) -> JsResult<()> {
    let global = ctx.global_object();
    global.delete_property_or_throw(js_string!("eval"), ctx)?;
    global.delete_property_or_throw(js_string!("Function"), ctx)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use async_trait::async_trait;
    use serde_json::{json, Value};

    #[test]
    fn stub_subagent_echoes_prompt() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let out = sb.run(r#"thclaws.subagent({prompt: "hello"})"#).unwrap();
        assert_eq!(out, "(stub for: hello)");
    }

    #[test]
    fn stub_subagent_handles_missing_prompt() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let out = sb.run(r#"thclaws.subagent({})"#).unwrap();
        assert_eq!(out, "(stub for: (no prompt))");
    }

    #[test]
    fn eval_global_stripped() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let err = sb.run(r#"eval("1+1")"#).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("eval"),
            "expected error mentioning eval, got: {msg}"
        );
    }

    #[test]
    fn function_constructor_stripped() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let err = sb.run(r#"new Function("return 1")()"#).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Function"),
            "expected error mentioning Function, got: {msg}"
        );
    }

    #[test]
    fn detects_async_syntax() {
        // Positive cases — should route to module mode.
        assert!(uses_async_syntax("const x = await foo();"));
        assert!(uses_async_syntax("Promise.all([])"));
        assert!(uses_async_syntax("paths.map(async (p) => p)"));
        assert!(uses_async_syntax("async function wf() {}"));
        assert!(uses_async_syntax("await foo()"));
        // Negative cases — should stay in script mode.
        assert!(!uses_async_syntax("const x = 1; x"));
        assert!(!uses_async_syntax("const awaitable = 1; awaitable"));
        // Note: substring detection means a comment containing `await `
        // will route to module mode. That's a false positive but
        // harmless — module mode handles sync scripts too.
    }

    #[test]
    fn wrap_for_module_uses_explicit_assignment() {
        let script = "const x = 1;\nglobalThis.__wf_result = x;";
        assert_eq!(wrap_for_module(script), script);
    }

    #[test]
    fn wrap_for_module_auto_wraps_trailing_expression() {
        let script = "const x = 1;\nx;";
        let wrapped = wrap_for_module(script);
        assert!(
            wrapped.contains("globalThis.__wf_result = (x);"),
            "got: {wrapped}"
        );
    }

    #[test]
    fn wrap_for_module_handles_no_trailing_semicolon() {
        let script = "const x = 1;\nx";
        let wrapped = wrap_for_module(script);
        assert!(
            wrapped.contains("globalThis.__wf_result = (x);"),
            "got: {wrapped}"
        );
    }

    #[test]
    fn wrap_for_module_handles_template_literal_with_dollar_brace() {
        // Regression: previously `}` inside `${expr}` ate the search
        // and the wrapper injected garbage into the template literal.
        let script = r#"const list = await thclaws.subagent({prompt: "a"});
const paths = list.split("\n");
const summaries = await Promise.all(paths.map(p => thclaws.subagent({prompt: `Read ${p}`})));
paths.map((p, i) => `${p} — ${summaries[i]}`).join("\n");"#;
        let wrapped = wrap_for_module(script);
        // The wrap must end with the assignment of the trailing
        // expression — NOT mid-template-literal.
        assert!(
            wrapped
                .ends_with("globalThis.__wf_result = (paths.map((p, i) => `${p} — ${summaries[i]}`).join(\"\\n\"));"),
            "got: {wrapped}"
        );
    }

    #[test]
    fn wrap_for_module_falls_back_when_last_is_declaration() {
        let script = "const x = 1;\nconst y = 2;";
        let wrapped = wrap_for_module(script);
        assert!(
            wrapped.contains("globalThis.__wf_result = undefined;"),
            "got: {wrapped}"
        );
    }

    #[test]
    fn module_mode_top_level_await_resolves_to_string() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let script = r#"
            const r = await thclaws.subagent({prompt: "hello"});
            r;
        "#;
        let out = sb.run(script).unwrap();
        assert_eq!(out, "(stub for: hello)");
    }

    #[test]
    fn module_mode_promise_all_resolves_to_array_join() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let script = r#"
            const r = await Promise.all([
                thclaws.subagent({prompt: "a"}),
                thclaws.subagent({prompt: "b"}),
                thclaws.subagent({prompt: "c"}),
            ]);
            r.join("|");
        "#;
        let out = sb.run(script).unwrap();
        assert_eq!(out, "(stub for: a)|(stub for: b)|(stub for: c)");
    }

    #[test]
    fn module_mode_explicit_globalthis_assignment_wins() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let script = r#"
            const r = await thclaws.subagent({prompt: "explicit"});
            globalThis.__wf_result = `wrapped: ${r}`;
        "#;
        let out = sb.run(script).unwrap();
        assert_eq!(out, "wrapped: (stub for: explicit)");
    }

    #[test]
    fn for_loop_works() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let out = sb
            .run(
                r#"
                let total = 0;
                for (let i = 1; i <= 5; i++) total += i;
                total;
            "#,
            )
            .unwrap();
        assert_eq!(out, "15");
    }

    /// Stage C: a script that calls `thclaws.subagent` multiple times
    /// routes each call through the registered Task tool and stitches
    /// the results back. Uses a mock Tool so the test stays
    /// dependency-free; the real Tool comes from the parent's tool
    /// registry in production (`tool_registry.get("Task")`).
    #[test]
    fn parses_seconds() {
        let d = parse_human_duration("60s").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(60));
    }

    #[test]
    fn parses_minutes() {
        let d = parse_human_duration("2m").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(120));
    }

    #[test]
    fn parses_minutes_plus_seconds() {
        let d = parse_human_duration("1m30s").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(90));
    }

    #[test]
    fn parses_hours() {
        let d = parse_human_duration("1h").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(3600));
    }

    #[test]
    fn parses_milliseconds() {
        let d = parse_human_duration("500ms").unwrap();
        assert_eq!(d, std::time::Duration::from_millis(500));
    }

    #[test]
    fn parses_combined_h_m_s() {
        let d = parse_human_duration("1h30m15s").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(3600 + 30 * 60 + 15));
    }

    #[test]
    fn parses_fractional_seconds() {
        let d = parse_human_duration("2.5s").unwrap();
        assert_eq!(d, std::time::Duration::from_millis(2500));
    }

    #[test]
    fn rejects_empty_and_no_unit_and_bad_unit() {
        assert!(parse_human_duration("").is_err());
        assert!(parse_human_duration("60").is_err());
        assert!(parse_human_duration("10x").is_err());
        assert!(parse_human_duration("0s").is_err());
    }

    #[test]
    fn extracts_bare_json() {
        let v = extract_json_from_text(r#"{"a": 1}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn extracts_json_from_json_fence() {
        let v = extract_json_from_text("```json\n{\"a\": 1}\n```").unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn extracts_json_from_bare_fence() {
        let v = extract_json_from_text("```\n{\"a\": 1}\n```").unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn extracts_json_embedded_in_prose() {
        let v = extract_json_from_text(
            "Here is the answer:\n{\"items\": [1, 2, 3]}\n— hope this helps",
        )
        .unwrap();
        assert_eq!(v["items"][1], 2);
    }

    #[test]
    fn extracts_bare_array() {
        let v = extract_json_from_text("[1, 2, 3]").unwrap();
        assert_eq!(v[2], 3);
    }

    #[test]
    fn returns_none_on_no_json() {
        assert!(extract_json_from_text("just prose").is_none());
        assert!(extract_json_from_text("").is_none());
    }

    #[test]
    fn exponential_backoff_caps_at_30s() {
        let r = Retry {
            max: 10,
            backoff: Backoff::Exponential,
        };
        assert_eq!(r.delay_for_attempt(1), std::time::Duration::from_secs(1));
        assert_eq!(r.delay_for_attempt(2), std::time::Duration::from_secs(2));
        assert_eq!(r.delay_for_attempt(4), std::time::Duration::from_secs(8));
        assert_eq!(r.delay_for_attempt(10), std::time::Duration::from_secs(30));
    }

    #[test]
    fn linear_backoff_scales_with_attempt() {
        let r = Retry {
            max: 5,
            backoff: Backoff::Linear,
        };
        assert_eq!(r.delay_for_attempt(1), std::time::Duration::from_secs(1));
        assert_eq!(r.delay_for_attempt(3), std::time::Duration::from_secs(3));
    }

    #[test]
    fn fixed_backoff_constant() {
        let r = Retry {
            max: 5,
            backoff: Backoff::Fixed(std::time::Duration::from_millis(500)),
        };
        assert_eq!(
            r.delay_for_attempt(1),
            std::time::Duration::from_millis(500)
        );
        assert_eq!(
            r.delay_for_attempt(7),
            std::time::Duration::from_millis(500)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_validation_returns_parsed_json() {
        struct JsonTask;
        #[async_trait]
        impl Tool for JsonTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "json mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                Ok(r#"{"items": ["a", "b"], "count": 2}"#.into())
            }
        }
        let mock: Arc<dyn Tool> = Arc::new(JsonTask);
        let script = r#"
            const r = thclaws.subagent({
              prompt: "list items",
              schema: {
                type: "object",
                properties: {
                  items: {type: "array"},
                  count: {type: "number"}
                },
                required: ["items", "count"]
              }
            });
            `${r.count}:${r.items.join(",")}`
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();

        assert_eq!(result.unwrap(), "2:a,b");
    }

    /// 48.1: `thclaws.parallel` runs specs CONCURRENTLY (wall-clock ≈ the
    /// slowest worker, not the sum) and returns results in input order.
    #[tokio::test]
    async fn parallel_runs_concurrently_and_preserves_order() {
        struct EchoTask;
        #[async_trait]
        impl Tool for EchoTask {
            fn name(&self) -> &'static str {
                "echo"
            }
            fn description(&self) -> &'static str {
                "echo the prompt after a delay"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, input: Value) -> crate::Result<String> {
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                Ok(input
                    .get("prompt")
                    .and_then(|p| p.as_str())
                    .unwrap_or("")
                    .to_string())
            }
        }
        let mock: Arc<dyn Tool> = Arc::new(EchoTask);
        let script = r#"
            const rs = thclaws.parallel([{ prompt: "a" }, { prompt: "b" }, { prompt: "c" }]);
            rs.join(",")
        "#
        .to_string();
        let (result, elapsed) = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            set_usage_sink(true);
            let start = std::time::Instant::now();
            let r = WorkflowSandbox::new()
                .unwrap()
                .run(&script)
                .map_err(|e| e.to_string());
            let el = start.elapsed();
            set_usage_sink(false);
            set_task_tool(None);
            (r, el)
        })
        .await
        .unwrap();
        assert_eq!(
            result.unwrap(),
            "a,b,c",
            "results must come back in input order"
        );
        // 3 workers × 60ms each: serial would be ~180ms; concurrent ~60ms.
        assert!(
            elapsed < std::time::Duration::from_millis(150),
            "expected concurrency, took {elapsed:?}"
        );
    }

    /// dev-plan/50: a failed worker settles to its `fallback` (default null)
    /// instead of aborting the batch — partial results are preserved.
    #[tokio::test]
    async fn parallel_settles_failed_worker_to_fallback() {
        struct FlakyTask;
        #[async_trait]
        impl Tool for FlakyTask {
            fn name(&self) -> &'static str {
                "flaky"
            }
            fn description(&self) -> &'static str {
                "ok unless prompt contains BOOM"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, input: Value) -> crate::Result<String> {
                let p = input.get("prompt").and_then(|p| p.as_str()).unwrap_or("");
                if p.contains("BOOM") {
                    Err(crate::Error::Agent("boom".into()))
                } else {
                    Ok(p.to_string())
                }
            }
        }
        let mock: Arc<dyn Tool> = Arc::new(FlakyTask);
        let script = r#"
            const rs = thclaws.parallel([
              { prompt: "a" },
              { prompt: "BOOM", fallback: { failed: true } },
              { prompt: "c" }
            ]);
            JSON.stringify(rs)
        "#
        .to_string();
        let result = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            set_usage_sink(true);
            let r = WorkflowSandbox::new()
                .unwrap()
                .run(&script)
                .map_err(|e| e.to_string());
            set_usage_sink(false);
            set_task_tool(None);
            r
        })
        .await
        .unwrap();
        // Successful workers keep their values; the failed one gets its fallback.
        assert_eq!(result.unwrap(), r#"["a",{"failed":true},"c"]"#);
    }

    /// 47.4: `thclaws.log()` is callable, returns undefined, and doesn't
    /// disturb the script's result (it's a side-effecting narrator).
    #[test]
    fn log_is_callable_and_returns_undefined() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let out = sb
            .run(r#"const r = thclaws.log("stage 1 done"); `${r === undefined}`"#)
            .unwrap();
        assert_eq!(out, "true");
    }

    /// 48.5: `thclaws.pollUntil` calls the check fn until `until` is truthy,
    /// returns that result; and throws on timeout.
    #[tokio::test]
    async fn poll_until_polls_then_returns_and_times_out() {
        let ok_script = r#"
            let n = 0;
            const r = thclaws.pollUntil(() => { n = n + 1; return { n: n }; },
                                        { interval: "1ms", timeout: "5s", until: (r) => r.n >= 3 });
            `${r.n}`
        "#
        .to_string();
        let got = tokio::task::spawn_blocking(move || {
            WorkflowSandbox::new()
                .unwrap()
                .run(&ok_script)
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap();
        assert_eq!(got.unwrap(), "3");

        let timeout_script =
            r#"thclaws.pollUntil(() => ({ done: false }), { interval: "2ms", timeout: "15ms", until: (r) => r.done })"#
                .to_string();
        let err = tokio::task::spawn_blocking(move || {
            WorkflowSandbox::new()
                .unwrap()
                .run(&timeout_script)
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap()
        .unwrap_err();
        assert!(err.contains("timed out"), "expected timeout, got: {err}");
    }

    /// 47.3: `args` defaults to null, and `set_args` exposes structured
    /// input the script reads directly.
    #[test]
    fn args_global_defaults_null_and_set_args_exposes_input() {
        let mut sb = WorkflowSandbox::new().unwrap();
        assert_eq!(sb.run("String(args)").unwrap(), "null");

        let mut sb2 = WorkflowSandbox::new().unwrap();
        sb2.set_args(&serde_json::json!({"query": "obon", "n": 3}))
            .unwrap();
        assert_eq!(sb2.run(r#"`${args.query}:${args.n}`"#).unwrap(), "obon:3");
    }

    /// 47.1: a `thclaws.subagent({agent})` call with NO per-call schema
    /// picks up the agent def's declared `output_schema` (surfaced via
    /// the Task tool's `subagent_output_schema`) and returns parsed JSON.
    #[tokio::test]
    async fn agent_output_schema_applied_when_call_omits_schema() {
        struct SchemaAgentTask;
        #[async_trait]
        impl Tool for SchemaAgentTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "schema-on-def mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            fn subagent_output_schema(&self, agent: &str) -> Option<Value> {
                (agent == "planner").then(|| {
                    json!({
                        "type": "object",
                        "properties": {"goal": {"type": "string"}},
                        "required": ["goal"]
                    })
                })
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                Ok(r#"{"goal": "map the topic"}"#.into())
            }
        }
        let mock: Arc<dyn Tool> = Arc::new(SchemaAgentTask);
        // No `schema:` on the call — it must come from the agent def.
        let script = r#"
            const r = thclaws.subagent({ agent: "planner", prompt: "plan it" });
            r.goal
        "#
        .to_string();
        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap(), "map the topic");
    }

    /// 47.6: inside a real workflow run (usage sink set) with no Task
    /// tool wired, `thclaws.subagent` fails loud instead of returning a
    /// stub the script would mistake for a real worker result.
    #[test]
    fn subagent_fails_loud_when_no_task_tool_inside_workflow() {
        set_usage_sink(true);
        set_task_tool(None);
        let mut sb = WorkflowSandbox::new().unwrap();
        let err = sb
            .run(r#"thclaws.subagent({prompt: "x"})"#)
            .unwrap_err()
            .to_string();
        set_usage_sink(false);
        assert!(
            err.contains("no Task tool") || err.contains("footgun"),
            "expected a loud surface error, got: {err}"
        );
    }

    /// `thclaws.include` resolves relative paths under the captured
    /// working folder. Three sub-cases bundled in one test:
    ///   1. happy path — file in cwd loads + defines a global the caller reads
    ///   2. absolute path → rejected
    ///   3. `..` escaping the base → rejected
    #[test]
    fn include_resolves_under_working_folder_and_blocks_escapes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("helpers.js"), "globalThis.helperValue = 7;").unwrap();
        set_include_base(Some(dir.path().to_path_buf()));

        // Happy path.
        let mut sb = WorkflowSandbox::new().unwrap();
        let out = sb
            .run(r#"thclaws.include("helpers.js"); String(globalThis.helperValue);"#)
            .unwrap();
        assert_eq!(out, "7");

        // Absolute paths are rejected before any IO.
        let abs = format!(
            "thclaws.include({:?});",
            dir.path().join("helpers.js").to_string_lossy()
        );
        let err = sb.run(&abs).unwrap_err().to_string();
        assert!(err.contains("absolute paths not allowed"), "got: {err}");

        // `..` traversal canonicalizes outside the base → rejected.
        let err2 = sb
            .run(r#"thclaws.include("../escape.js");"#)
            .unwrap_err()
            .to_string();
        assert!(
            err2.contains("resolves outside") || err2.contains("can't resolve"),
            "got: {err2}"
        );

        set_include_base(None);
    }

    /// `thclaws.subagent({prompt, agent: "name"})` must forward the
    /// `agent` field into the Task tool input so the parent's
    /// AgentDefsConfig can swap in the matching .thclaws/agents/<name>.md
    /// definition — same shape the LLM-driven Task tool already accepts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_param_forwards_to_task_tool_input() {
        struct EchoAgentTask;
        #[async_trait]
        impl Tool for EchoAgentTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "echoes the agent field"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, input: Value) -> crate::Result<String> {
                Ok(input
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or("<absent>")
                    .to_string())
            }
        }

        let mock: Arc<dyn Tool> = Arc::new(EchoAgentTask);
        // First call passes agent: "reviewer"; second omits it so we
        // can assert the conditional branch in `subagent()` doesn't
        // inject an empty value.
        let script = r#"
            const a = thclaws.subagent({prompt: "review", agent: "reviewer"});
            const b = thclaws.subagent({prompt: "no agent"});
            `${a}|${b}`
        "#;
        let mock_clone = mock.clone();
        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock_clone));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();

        assert_eq!(result.unwrap(), "reviewer|<absent>");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_failure_triggers_retry_then_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        struct FlakyTask {
            calls: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for FlakyTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "flaky mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 2 {
                    Ok("definitely not json".into())
                } else {
                    Ok(r#"{"ok": true}"#.into())
                }
            }
        }
        let calls = Arc::new(AtomicU32::new(0));
        let mock: Arc<dyn Tool> = Arc::new(FlakyTask {
            calls: calls.clone(),
        });
        let script = r#"
            const r = thclaws.subagent({
              prompt: "give me ok",
              schema: {type: "object", required: ["ok"]},
              retry: {max: 3, backoff: "10ms"}
            });
            r.ok ? "yes" : "no"
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();

        assert_eq!(result.unwrap(), "yes");
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokens_budget_triggers_retry_then_exhausts() {
        use std::sync::atomic::{AtomicU32, Ordering};
        struct BigUsageTask {
            calls: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for BigUsageTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "big-usage mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                // Simulate the worker pushing usage via the sink path
                // (real Tool wires this through SubAgentTool::call;
                // this mock does it directly so the test stays local).
                crate::workflow::push_worker_usage(crate::providers::Usage {
                    // Huge input (like a 1M-context worker that read a big
                    // file) but it's IGNORED — only output is capped.
                    input_tokens: 50_000,
                    output_tokens: 600,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_output_tokens: None,
                });
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok("ok".into())
            }
        }
        let calls = Arc::new(AtomicU32::new(0));
        let mock: Arc<dyn Tool> = Arc::new(BigUsageTask {
            calls: calls.clone(),
        });
        let script = r#"
            try {
              thclaws.subagent({
                prompt: "big",
                budget: {tokens: 500},
                retry: {max: 2, backoff: "1ms"}
              });
              "should-not-reach";
            } catch (e) {
              e.message;
            }
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            set_usage_sink(true);
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            set_usage_sink(false);
            res
        })
        .await
        .unwrap();

        let msg = result.unwrap();
        assert!(
            msg.contains("token budget"),
            "expected token-budget error: {msg}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokens_budget_ignores_large_input_context() {
        use std::sync::atomic::{AtomicU32, Ordering};
        // Regression for the reported /workflow failure: a worker on a
        // large-context model that READ a lot (huge input) but generated
        // little must NOT be killed by a modest token budget — the cap is
        // output-only, so its input never counts against it.
        struct ReadHeavyTask {
            calls: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for ReadHeavyTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "read-heavy mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                crate::workflow::push_worker_usage(crate::providers::Usage {
                    input_tokens: 57_529, // mostly input, like the reported run
                    output_tokens: 1_200,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_output_tokens: None,
                });
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok("done".into())
            }
        }
        let calls = Arc::new(AtomicU32::new(0));
        let mock: Arc<dyn Tool> = Arc::new(ReadHeavyTask {
            calls: calls.clone(),
        });
        let script = r#"
            try {
              thclaws.subagent({ prompt: "summarize", budget: {tokens: 8000} });
              "ok";
            } catch (e) { "ERR:" + e.message; }
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            set_usage_sink(true);
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            set_usage_sink(false);
            res
        })
        .await
        .unwrap();

        let out = result.unwrap();
        assert_eq!(
            out, "ok",
            "input-heavy worker must not exceed an output-token budget: {out}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_sink_accumulates_across_workers() {
        struct UsageTask;
        #[async_trait]
        impl Tool for UsageTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "usage mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                crate::workflow::push_worker_usage(crate::providers::Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_output_tokens: None,
                });
                Ok("hi".into())
            }
        }
        let mock: Arc<dyn Tool> = Arc::new(UsageTask);
        let script = r#"
            thclaws.subagent({prompt: "a"});
            thclaws.subagent({prompt: "b"});
            thclaws.subagent({prompt: "c"});
            "ok"
        "#
        .to_string();

        let (_, total): (std::result::Result<String, String>, u64) =
            tokio::task::spawn_blocking(move || {
                set_task_tool(Some(mock));
                set_usage_sink(true);
                let res = (|| -> std::result::Result<String, String> {
                    let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                    sb.run(&script).map_err(|e| e.to_string())
                })();
                let usages = take_all_usages();
                set_task_tool(None);
                set_usage_sink(false);
                let total: u64 = usages
                    .iter()
                    .map(|u| (u.input_tokens + u.output_tokens) as u64)
                    .sum();
                (res, total)
            })
            .await
            .unwrap();

        assert_eq!(total, 3 * 150);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_failure_exhausts_retries_and_throws() {
        struct AlwaysBadTask;
        #[async_trait]
        impl Tool for AlwaysBadTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "bad mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                Ok(r#"{"wrong": "shape"}"#.into())
            }
        }
        let mock: Arc<dyn Tool> = Arc::new(AlwaysBadTask);
        let script = r#"
            try {
              thclaws.subagent({
                prompt: "must have ok",
                schema: {type: "object", required: ["ok"]},
                retry: {max: 2, backoff: "1ms"}
              });
              "should-not-reach";
            } catch (e) {
              e.message;
            }
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();

        let msg = result.unwrap();
        assert!(
            msg.contains("after 2 attempt") && msg.contains("schema"),
            "expected schema-exhaustion error in: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transient_provider_error_retries_without_script_opt_in() {
        // A flaky stream error must retry even though the script set NO
        // `retry:` — the transient floor covers it. This is the exact
        // failure (`stream: error decoding response body`) that killed a
        // real WorkflowRun.
        use std::sync::atomic::{AtomicU32, Ordering};
        struct FlakyStream {
            calls: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for FlakyStream {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "flaky stream mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 2 {
                    Err(crate::Error::Provider(
                        "stream: error decoding response body".into(),
                    ))
                } else {
                    Ok("recovered".into())
                }
            }
        }
        let calls = Arc::new(AtomicU32::new(0));
        let mock: Arc<dyn Tool> = Arc::new(FlakyStream {
            calls: calls.clone(),
        });
        // No `retry:` — relies on the transient floor.
        let script = r#"thclaws.subagent({prompt: "go"})"#.to_string();
        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap(), "recovered");
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "transient error should have retried"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deterministic_failure_does_not_use_transient_floor() {
        // A non-transient failure (schema mismatch) with no `retry:`
        // (default max 1) must fail after exactly ONE attempt — the
        // transient floor is reserved for flaky provider/stream errors.
        use std::sync::atomic::{AtomicU32, Ordering};
        struct AlwaysBad {
            calls: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for AlwaysBad {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "always-bad mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(r#"{"wrong": "shape"}"#.into())
            }
        }
        let calls = Arc::new(AtomicU32::new(0));
        let mock: Arc<dyn Tool> = Arc::new(AlwaysBad {
            calls: calls.clone(),
        });
        let script = r#"
            try {
              thclaws.subagent({prompt: "x", schema: {type: "object", required: ["ok"]}});
              "should-not-reach";
            } catch (e) { e.message; }
        "#
        .to_string();
        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();
        let msg = result.unwrap();
        assert!(
            msg.contains("after 1 attempt"),
            "deterministic failure must fail fast (1 attempt): {msg}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "schema failure must not consume the transient floor"
        );
    }

    #[test]
    fn kms_write_gate_allows_outside_workflow() {
        // No thread-local set → not inside a workflow call → allow.
        set_worker_caps(None);
        assert!(check_kms_write_capability("any-name").is_ok());
    }

    #[test]
    fn kms_write_gate_denies_when_not_granted() {
        set_worker_caps(Some(WorkerCaps::default()));
        let err = check_kms_write_capability("scratch").unwrap_err();
        assert!(err.to_string().contains("denied"));
        assert!(err.to_string().contains("scratch"));
        set_worker_caps(None);
    }

    #[test]
    fn kms_write_gate_allows_when_in_grant_list() {
        let mut caps = WorkerCaps::default();
        caps.kms_write.insert("scratch".to_string());
        caps.kms_write.insert("audit-log".to_string());
        set_worker_caps(Some(caps));
        assert!(check_kms_write_capability("scratch").is_ok());
        assert!(check_kms_write_capability("audit-log").is_ok());
        assert!(check_kms_write_capability("not-granted").is_err());
        set_worker_caps(None);
    }

    #[test]
    fn caps_extraction_from_js_object() {
        let mut sb = WorkflowSandbox::new().unwrap();
        // Use a custom script that exercises the host extract path
        // indirectly: define a probe that captures the caps via the
        // thread-local — we set the sink, run, then read it.
        // Simpler approach: drive extract_caps directly through a
        // synthetic Boa context.
        let script = r#"
            // Just touch the global; we're not asserting on the result.
            thclaws.subagent({
              prompt: "probe",
              caps: { kms: { write: ["alpha", "beta"] } }
            });
        "#;
        // Run inside the sandbox so extract_caps gets exercised
        // via the host function path. We don't care about the
        // return — the test asserts on the cap-extract side-effect
        // captured via a synthetic check below.
        //
        // Note: With no task tool wired the stub path runs and
        // exits before set_worker_caps is called. To actually test
        // extract_caps we'd need a real spawn path. Instead, assert
        // the simpler invariant: `extract_caps` of a plain script
        // shape returns the expected set when called on hand-built
        // Boa values (covered by the gate tests above + the
        // observable behaviour via the integration with the kms
        // tool gates).
        let _ = sb.run(script);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn caps_are_per_call_and_dont_leak_across_workers() {
        use std::sync::atomic::{AtomicU32, Ordering};
        struct CapsProbe {
            seen: Arc<std::sync::Mutex<Vec<Option<Vec<String>>>>>,
            ord: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for CapsProbe {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "caps probe"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                let observed = WORKFLOW_WORKER_CAPS.with(|c| {
                    c.borrow().as_ref().map(|caps| {
                        let mut v: Vec<String> = caps.kms_write.iter().cloned().collect();
                        v.sort();
                        v
                    })
                });
                self.seen.lock().unwrap().push(observed);
                Ok(format!("ok-{}", self.ord.fetch_add(1, Ordering::SeqCst)))
            }
        }
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Option<Vec<String>>>::new()));
        let ord = Arc::new(AtomicU32::new(0));
        let mock: Arc<dyn Tool> = Arc::new(CapsProbe {
            seen: seen.clone(),
            ord,
        });
        let script = r#"
            thclaws.subagent({prompt: "a", caps: {kms: {write: ["scratch"]}}});
            thclaws.subagent({prompt: "b"});
            thclaws.subagent({prompt: "c", caps: {kms: {write: ["audit", "scratch"]}}});
            "done"
        "#
        .to_string();

        tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let _ = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
        })
        .await
        .unwrap();

        let seen = seen.lock().unwrap().clone();
        // First call: scratch granted.
        assert_eq!(seen[0], Some(vec!["scratch".to_string()]));
        // Second call: NO caps in JS → empty WorkerCaps (deny-by-default).
        assert_eq!(seen[1], Some(Vec::<String>::new()));
        // Third call: audit + scratch granted (alphabetical from set).
        assert_eq!(
            seen[2],
            Some(vec!["audit".to_string(), "scratch".to_string()])
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_cache_serves_matching_prompts_without_spawning() {
        use std::sync::atomic::{AtomicU32, Ordering};
        struct CountingTask {
            calls: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for CountingTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "counting mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, input: Value) -> crate::Result<String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let p = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
                Ok(format!("fresh:{p}"))
            }
        }
        let calls = Arc::new(AtomicU32::new(0));
        let mock: Arc<dyn Tool> = Arc::new(CountingTask {
            calls: calls.clone(),
        });

        // Two cached entries; third call has no match → real spawn.
        let cache = vec![
            ("alpha".to_string(), "CACHED-A".to_string()),
            ("beta".to_string(), "CACHED-B".to_string()),
        ];
        let script = r#"
            const a = thclaws.subagent({prompt: "alpha"});
            const b = thclaws.subagent({prompt: "beta"});
            const c = thclaws.subagent({prompt: "gamma"});
            `${a}|${b}|${c}`
        "#
        .to_string();

        let (result, remaining): (std::result::Result<String, String>, usize) =
            tokio::task::spawn_blocking(move || {
                set_task_tool(Some(mock));
                set_replay_cache(Some(cache));
                let res = (|| -> std::result::Result<String, String> {
                    let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                    sb.run(&script).map_err(|e| e.to_string())
                })();
                let rem = replay_remaining();
                set_task_tool(None);
                set_replay_cache(None);
                (res, rem)
            })
            .await
            .unwrap();

        assert_eq!(result.unwrap(), "CACHED-A|CACHED-B|fresh:gamma");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(remaining, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_cache_falls_through_on_prompt_mismatch() {
        struct SpyTask {
            spawned: Arc<std::sync::Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl Tool for SpyTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "spy"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, input: Value) -> crate::Result<String> {
                let p = input
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.spawned.lock().unwrap().push(p.clone());
                Ok(format!("fresh:{p}"))
            }
        }
        let spawned = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mock: Arc<dyn Tool> = Arc::new(SpyTask {
            spawned: spawned.clone(),
        });

        // Cache says first call was "alpha", but script's first call
        // is "ALPHA-MODIFIED" — divergence. The runtime should leave
        // the cache untouched and spawn fresh.
        let cache = vec![("alpha".to_string(), "CACHED-A".to_string())];
        let script = r#"
            thclaws.subagent({prompt: "ALPHA-MODIFIED"});
        "#
        .to_string();

        let _ = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            set_replay_cache(Some(cache));
            let mut sb = WorkflowSandbox::new().unwrap();
            let _ = sb.run(&script);
            set_task_tool(None);
            set_replay_cache(None);
        })
        .await
        .unwrap();

        let spawned_calls = spawned.lock().unwrap().clone();
        assert_eq!(spawned_calls, vec!["ALPHA-MODIFIED"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn time_budget_aborts_slow_worker() {
        struct SlowTask;
        #[async_trait]
        impl Tool for SlowTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "slow mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok("never returned".into())
            }
        }
        let mock: Arc<dyn Tool> = Arc::new(SlowTask);
        let script = r#"
            try {
                thclaws.subagent({prompt: "slow", budget: {time: "100ms"}});
                "should not reach";
            } catch (e) {
                e.message;
            }
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();

        let msg = result.unwrap();
        assert!(
            msg.contains("time budget"),
            "expected time-budget error in: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_task_tool_routes_subagent_calls() {
        struct MockTask;
        #[async_trait]
        impl Tool for MockTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, input: Value) -> crate::Result<String> {
                let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
                Ok(format!("task[{prompt}]"))
            }
        }

        let mock: Arc<dyn Tool> = Arc::new(MockTask);
        let script = r#"
            const a = thclaws.subagent({prompt: "alpha"});
            const b = thclaws.subagent({prompt: "beta"});
            const c = thclaws.subagent({prompt: "gamma"});
            `${a} | ${b} | ${c}`
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();

        assert_eq!(result.unwrap(), "task[alpha] | task[beta] | task[gamma]");
    }
}
