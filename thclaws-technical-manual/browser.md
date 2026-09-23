# Browser automation

Engine-managed web browser for the agent: thClaws injects Microsoft's
official `@playwright/mcp` as a synthetic MCP server named **`browser`**,
*and* owns the underlying Chromium process directly over the Chrome
DevTools Protocol (CDP) so the agent's Playwright tools and a
human-facing live view / takeover drive **one shared browser**. Shipped
across v0.48–v0.52 (browserEnabled → default-on → screenshots-to-vision
→ CDP live view + input → cookie persistence).

Two design decisions frame everything below:

1. **MCP over native Rust for the agent's tools.** Playwright's
   auto-waiting, accessibility snapshots, and battle-tested selectors
   are worth keeping — re-implementing them in Rust would be strictly
   worse. The agent never sees CDP; it sees `browser_*` MCP tools.
2. **Native CDP only for the human channel.** The live screencast,
   click/scroll/type takeover, console/navigation events, and cookie
   snapshot/restore are things Playwright-MCP doesn't expose to *us*,
   so the engine opens its own CDP session against the same Chromium
   that playwright-mcp is attached to (via `--cdp-endpoint`).

The agent has **two perception channels**: `browser_snapshot`
(accessibility tree — its default "eyes", text, cheap) and
`browser_take_screenshot` (rendered pixels, routed to vision models —
for canvas/chart/visual-only pages the a11y tree can't describe).

## Enablement & config injection

`crates/core/src/config.rs`:

- `AppConfig.browser_enabled` — `#[serde(default = "default_browser_enabled")]`,
  compiled default **`true`** (v0.49.2), but two things override it in
  practice: `THCLAWS_BROWSER_ENABLED=0` flips the *default* off for a whole
  fleet (every cloud runner sets it — headless Chromium OOM-killed 1 Gi
  runners that never asked for a browser), and the new-workspace settings
  template writes `"browserEnabled": false`. An explicit setting always
  wins over the env. Anything reporting this to a user must report the
  **resolved** `AppConfig` value, not the project file's — the Settings
  toggle read the latter and disagreed with the engine on cloud
  (dev-plan/65 §2.6). `browser_headless: Option<bool>` — `None` means
  "platform default" (headed on desktop, headless on cloud/serve).
- `AppConfig::load()` injects a synthetic MCP server into the config's
  `mcp_servers` map under the key `"browser"` by calling
  `browser_mcp_config(headless_override)`. The injection is **skipped**
  when:
  - `cfg!(test)` (tests must not spawn a real browser),
  - `command_on_path()` can't find the launch binary (no Node / no
    `npx` / no `playwright-mcp` → degrade gracefully, agent just runs
    without browser tools),
  - the config sets `external_mcp_disallowed` (EE / locked-down).
- `browser_mcp_config(headless)` reads the launch command from
  `THCLAWS_BROWSER_MCP_CMD` (default `npx -y @playwright/mcp@latest`),
  appends `--headless` when headless is resolved on, and appends
  `--caps=vision` (unless the override already pins `--caps`) so the
  screenshot + coordinate tools are exposed. Returns an
  `McpServerConfig` with **`engine_managed = true`**.
- `command_on_path()` — PATH probe used both for the skip-decision and
  for the Browser tab's "command not found" status hint.

## `engine_managed` — bypassing the spawn allowlist (security)

`crates/core/src/mcp.rs`:

`McpServerConfig.engine_managed: bool` is **`#[serde(skip)]`** — it can
never be set from user/agent JSON, only constructed in Rust by
`browser_mcp_config`. This is load-bearing: stdio MCP servers normally
hit `check_stdio_command_allowed` (the spawn-allowlist prompt that stops
a malicious config from running arbitrary commands). An engine-managed
server bypasses that gate — which is only safe *because* the flag is
serde-skipped and the command is engine-chosen. Test
`engine_managed_cannot_be_set_from_json` pins that a JSON payload with
`"engine_managed": true` deserializes to `false`.

## Vision: screenshots reaching the model

`crates/core/src/mcp.rs`:

- `McpTool::call_multimodal` + `mcp_content_to_blocks` convert an MCP
  tool result's `image` content parts into model content blocks (base64
  → image block), with a **5 MB cap** per image. Without this,
  `browser_take_screenshot` returned pixels that the engine dropped on
  the floor — the model "knew nothing about the webpage". Test
  `mcp_content_to_blocks_preserves_images`.
- `McpTool::call_tool_raw()` is the lower-level call used by the IPC
  layer (below) to invoke browser tools out-of-band from the agent
  loop.
- `McpTool::lazy_browser_up()` triggers the lazy Chromium launch (below)
  the first time a browser tool is actually called.

## CDP module — the human channel

`crates/core/src/browser_cdp.rs` (~2,250 lines, **not** `gui`-gated — it's
referenced unconditionally by `mcp.rs`/`config.rs`; ungating it was the
v0.51.0 `make install` fix, commit `66cc9c14`):

- `arm()` — registers the CDP machinery onto the shared session;
  cheap, no process spawn.
- `ensure_up()` — **lazy launch**. Finds Chromium (`find_chromium()`),
  launches it with `--remote-debugging-port`, `--user-data-dir` and
  `--window-size`, persists the resolved `devtools-endpoint` to a file
  under the profile dir, and hands playwright-mcp the matching
  `--cdp-endpoint` so both attach to the same browser.
- **Liveness, not a flag** (v0.134, dev-plan/65 #3): re-entry probes the
  endpoint (cached 2 s) instead of trusting `launched`, and relaunches on
  the **same port** when Chromium has gone — a closed window or an OOM kill
  used to break every browser tool call and the live view for the rest of
  the session with nothing ever clearing the flag.
- **`--window-size` is what sizes a page in CDP mode** (dev-plan/65 #1):
  playwright-mcp ignores its own `--viewport-size` when handed
  `--cdp-endpoint`, because it adopts the page the engine already opened.
  Both come from `AppConfig::browser_viewport()`
  (`THCLAWS_BROWSER_VIEWPORT`, default 1920×1080) so they cannot drift.
- **Orphan handling is reap, not re-attach** (v0.53): a previous engine's
  Chromium is closed (`Browser.close` → SIGKILL by saved pid → clear
  `Singleton*` locks) and a fresh one launched, because a new
  playwright-mcp connecting over CDP into a browser the *previous* run's
  playwright-mcp drove fails every time with "Browser context management
  is not supported". Cookies live in the persistent `--user-data-dir`, so
  nothing is lost by relaunching. The endpoint probe is **HTTP/1.1 +
  `Connection: close`** (HTTP/1.0 was the original bug — Chromium's
  DevTools HTTP server silently ignores HTTP/1.0 probes).
- `find_chromium()` — discovery across the playwright browser cache and
  system installs. On cloud the runner image pins
  `PLAYWRIGHT_BROWSERS_PATH=/ms-playwright` and `--browser chromium`
  (playwright-mcp otherwise defaults to branded Google Chrome →
  "Chromium distribution 'chrome' is not found", fixed v0.49.4).
- `cdp_active()` — whether a live CDP session exists (drives Browser-tab
  status).
- **Screencast:** `screencast_start()` / `screencast_stop()` issue
  `Page.startScreencast` / `stopScreencast`. The reader task acks frames
  via `notify()` — a **fire-and-forget** `Page.screencastFrameAck` that
  does **not** await a reply. (The original reply-awaiting ack inside the
  reader task deadlocked it: 1 frame per ~15 s. `notify()` is the fix.)
  Frames are capped at 1366×900, so Chromium **scales** them: each
  `browser_frame` envelope therefore carries `w`/`h` from
  `metadata.deviceWidth/deviceHeight` — the page's own CSS-pixel size, which
  is the space `Input.*` works in — and the tab maps clicks through that,
  not through the image's pixels (dev-plan/65 #1). Measured cost: 57.5 KB per
  frame, ~212 KB/s per viewer under continuous scrolling.
- **Input:** `input(kind, args)` dispatches synthetic events —
  `Input.dispatchMouseEvent` (click/move/drag/down/up/wheel),
  `Input.dispatchKeyEvent` (press_key), and `Input.insertText` (the
  synthetic `type_text`). This is the takeover remote control.
- **Diagnostics:** `doctor_summary()` (the `browser:` line in `/doctor`) and
  `chromium_alive()`. Before v0.134 the subsystem had no health check at
  all, so a live view that had silently fallen back to screenshots — the
  no-Chromium case — looked like a missing feature (dev-plan/65 #8). The
  same facts reach the Browser tab via `browser_status_get`
  (`chromium`, `chromium_found`, `viewport`).
- **Cookies:** `snapshot_cookies()` / `restore_cookies()` /
  `flush_cookies()` use `Storage.getCookies` / `setCookies`,
  **independent of Chromium's ~30 s SQLite flush timer** — an abrupt
  `SIGKILL` (cloud pod stop) otherwise loses recent logins. Snapshot on
  pause/stop, restore on launch (v0.52.0). Two v0.134 corrections
  (dev-plan/65 #4): `--serve` now installs a SIGTERM/SIGINT handler that
  flushes and closes before exiting — the shutdown path existed only in
  `gui.rs`, so the cloud, the environment it was written for, never ran it;
  and an **empty** cookie jar is written when a snapshot already exists
  (it means the user logged out) instead of being skipped, which used to
  leave the stale snapshot in place and restore the session on next launch.
- `profile_dir_for(container)` — resolves the on-disk profile location
  (outside the workspace folder).
- `PageSession` — the per-page CDP wrapper; `notify()` is its
  reply-less send primitive (used for screencast acks and other
  fire-and-forget commands).

Kill switch: `THCLAWS_BROWSER_CDP=0` disables the entire CDP/human
channel while leaving the agent's MCP tools working.

## Bootstrap & wiring

`crates/core/src/shared_session.rs`:

- Bootstrap **arms CDP for an engine-managed browser config** (gated on
  `THCLAWS_BROWSER_CDP`), adds a `browser_mcp` slot to
  `SharedSessionHandle`, and on `McpReady` publishes the browser client
  so the IPC layer can reach it.
- `run_worker` reads `config.permissions == "auto"` to decide the
  default permission posture (browser tools are mutating — see below).

## IPC surface (Browser tab ↔ engine)

`crates/core/src/ipc.rs` arms these transport-agnostic handlers (shared
by the wry desktop GUI and the `--serve` WebSocket bridge — see
[`serve-mode.md`](serve-mode.md)):

| Handler | Purpose |
|---|---|
| `browser_status_get` | on/headed/headless, launch cmd, binary-found, CDP-active, Chromium path + found, resolved viewport |
| `browser_screenshot_get` | one-shot `browser_take_screenshot` via `call_tool_raw` |
| `browser_input_call` | takeover input — **allowlisted** verbs only |
| `browser_screencast_start` / `_stop` | join / leave the live stream. Ref-counted per `IpcContext::viewer_id`: the stream stops when the LAST viewer leaves, and a closing socket releases its own |
| `browser_cdp_input` | raw CDP input dispatch for takeover — `click`/`down`/`up`/`move`/`wheel`/`text`/`key`/`keydown`/`keyup`, all carrying the modifier bitmask |
| `browser_tab_select` | pin the live view to one page target (`target: <id>`), or `null` to follow the agent again |

Engine → tab envelopes: `browser_frame` (JPEG + the page's own CSS size),
`browser_console`, `browser_nav`, `browser_tabs` (the strip + which one is
active) and `browser_view` (`attaching` / `live` / `detached` / `error` — the
state of the VIEW, not the page).

`browser_input_call`'s allowlist: `mouse_click_xy`, `mouse_move_xy`,
`mouse_drag_xy`, `mouse_down`, `mouse_up`, `mouse_wheel`, `press_key`,
`navigate`, `navigate_back`, plus the synthetic `type_text`. Anything
else is rejected — the takeover surface can't be coerced into arbitrary
tool calls.

## Frontend

- `frontend/src/components/BrowserView.tsx` — status card,
  screenshot/live-frame panel, activity feed (tool calls + console +
  navigations), the docked agent chat sidebar (same shared session as
  the Chat tab, slash-commands accepted), and takeover mode (coordinate
  input, URL bar, Enter/Tab/Esc/⌫ quick keys, console events).
- `frontend/src/App.tsx` — the **Browser** tab (Globe icon) added to
  `ALL_TABS`, gated on `browserEnabled`.

## Permission posture

Browser MCP tools are mutating, so they flow through the normal approval
gate ([`agentic-loop.md`](agentic-loop.md) approval gate,
[ch05 permissions]). Under `auto` they run unprompted; under
`ask`/`*Gated` each call prompts. The takeover IPC handlers are **not**
agent tool calls — they're operator input routed straight to CDP, so
they neither prompt nor cost tokens.

Headless bot note (issue #160): `crates/core/src/telegram/headless.rs`
`resolve_perm_mode(permissions)` maps `auto → PermissionMode::Auto`,
else `TelegramGated` — so a headless Telegram bot with
`"permissions":"auto"` runs browser tools unattended. CLI `/permissions
auto|ask` now persists to `.thclaws/settings.json` via
`repl.rs::persist_permission_mode_cli`. Test
`auto_disables_prompts_else_telegram_gated`.

## Cloud / runner image

`thclaws/Dockerfile`:

- `PLAYWRIGHT_BROWSERS_PATH=/ms-playwright`, made world-readable
  (`chmod a+rX`) so a non-root runner can launch the cached Chromium.
- Preinstalls `@playwright/mcp` + the Chromium browser at build time
  (no first-run download on the runner).
- `ENV THCLAWS_BROWSER_MCP_CMD="playwright-mcp --no-sandbox --browser
  chromium"` — `--no-sandbox` for the container, `--browser chromium`
  to avoid the branded-Chrome lookup failure.

On cloud the headless browser has no window, so the Browser tab's live
screencast (CDP `Page.startScreencast`) **is** the window — the same CDP
input path powers takeover on a headless runner.

## Cookie-leak prevention on publish

`crates/core/src/cloud/pack.rs`: `STRIP_PREFIXES` includes
`.thclaws/state/browser-profile/`, so the on-disk browser profile (cookies,
sessions) is **never** bundled into a published catalog agent. The
profile lives outside the workspace folder for the same reason. See
[`thclaws-cloud-client.md`](thclaws-cloud-client.md) for the full
pack/strip rules.

## Env knobs

| Var | Effect |
|---|---|
| `THCLAWS_BROWSER_MCP_CMD` | Override the playwright-mcp launch command (default `npx -y @playwright/mcp@latest`) |
| `THCLAWS_BROWSER_CDP=0` | Disable the CDP/human channel; agent MCP tools still work |
| `THCLAWS_BROWSER_VIEWPORT="W,H"` | Page size for the engine's `--window-size` **and** playwright-mcp's `--viewport-size` (default `1920,1080`) |
| `THCLAWS_BROWSER_ALLOW_BRANDED=1` | Allow driving Chrome/Edge over CDP (unreliable — see `find_chromium()`) |
| `THCLAWS_BROWSER_FRAME_MS` | Engine-side floor between screencast frames (default `80`; `0` sends every paint) |
| `THCLAWS_MCP_MAX_TEXT_BYTES` | Text cap on ANY MCP tool result; wins over the per-tool floor (`browser_snapshot` 48 KB, everything else 256 KB). `0` disables capping |
| `PLAYWRIGHT_BROWSERS_PATH` | Chromium cache location (`find_chromium()` searches it) |
| `browserEnabled` / `browserHeadless` (settings.json) | Opt out / force headless |

## Token economy (dev-plan/65 P2)

Two standing costs, paid on every request the browser is on for whether or
not the turn touches a page: **21 visible tool schemas** (~15 KB — 31 are
offered, 10 hidden by `BROWSER_MODEL_HIDDEN_TOOLS`) and the **browser prompt
section** (~3 KB, `prompts.rs::services_prompt_section`). ~4,500 tokens
together. A test fails the build if the section passes 3,200 bytes.

Per call the distribution is one-sided: one `browser_snapshot` of an
encyclopaedia article measures **889 KB**; the `browser_find` that answers the
same question, **57 bytes**.

- **Per-tool result caps** (`mcp.rs::tool_text_budget`). `browser_snapshot`
  gets **48 KB** — the measured size of the Hacker News front page, i.e. what
  "a whole real page" costs. Every other tool keeps the 256 KB blanket.
  `THCLAWS_MCP_MAX_TEXT_BYTES` overrides both, including the floor (`0`
  disables capping).
- **The truncation notice is per tool** (`mcp.rs::narrowing_hint`): a cut
  snapshot names `depth` / `target` / `filename`, a cut network or console
  list names `filter` / `filename`, everything else points at `browser_find`.
  The old generic "narrow the result" told the model nothing it could act on.
- **The prompt names the cheap parameters.** `browser_snapshot(depth: 3)` to
  orient, `target: <ref>` for one subtree, `filename:` + `Grep` for anything
  large, `browser_find(text:|regex:)` by its real argument names, and
  `browser_console_messages` / `browser_network_requests(filter:)` under
  "when a page misbehaves, read what it did, do not screenshot the error".

Measured in the field (v0.136.0, a 12-minute shopping session): **zero**
`browser_snapshot` calls, 32 `browser_evaluate`, 15 `browser_navigate`, 3
`browser_tabs`, 3 `browser_click`, 2 `browser_find` — **118 KB of tool result
across 55 browser calls**, largest single result 17 KB.

## Which tab the live view is on (dev-plan/65 P3)

`/json/list` is polled every 750 ms while anyone is watching
(`browser_cdp::start_watcher` → `reconcile`). Polling rather than a second
websocket running `Target.setDiscoverTargets`: one localhost GET a tick, no
second reader task to deadlock against the first, and nobody watching a video
stream can see 750 ms.

`choose_target(current, pinned, seen, tabs)` decides, in order: a pin the
human set from the tab strip while that tab exists → a target id never seen
before (that is the agent opening a tab, and the view follows it) → where we
already are → whatever is left. The old rule was "the first `page` in
`/json/list`, once, at attach", so the moment the agent opened a second tab
the human watched a page nobody was on.

- **`Page.bringToFront` on attach.** Chromium neither paints nor delivers
  input to a background tab: attaching to one gives a black view and a
  takeover whose every keystroke times out, with no error anywhere. The cost
  is that pinning the view makes the agent's own tab `hidden`, which pauses
  some pages' timers.
- **Multi-viewer.** Frames, console, navigation and tab state broadcast to a
  registry keyed by `IpcContext::viewer_id` — one id per connection, minted in
  `ipc::next_viewer_id`. The desktop window and a phone on thClaws Remote can
  both watch; the stream stops when the LAST viewer leaves. A `dispatch` `Arc`
  is **not** a client identity: `server.rs` builds one per socket but `gui.rs`
  rebuilds it on every IPC message. `handle_socket` releases its viewer on
  teardown, since a browser that closed its tab never sends a stop.
- **`browser_view` carries the state of the VIEW** — `attaching` / `live` /
  `detached` / `error` — as distinct from the page, and the Browser tab renders
  it. A dead target used to leave the last frame up, which reads as a hung page.
- **The frame floor is engine-side**: `THCLAWS_BROWSER_FRAME_MS`, default 80 ms,
  applied before the frame goes on the wire (the client cap stays as a
  backstop). A frame dropped in the browser tab has already been paid for.

## Takeover as a remote control (dev-plan/65 P4)

- **Real keyboard passthrough.** The frame is focusable and forwards the
  human's own `keydown`/`keyup` with `ctrlKey`/`metaKey`/`shiftKey`/`altKey`
  through `Input.dispatchKeyEvent` with Chromium's modifier bitmask
  (Alt=1, Ctrl=2, Meta=4, Shift=8). `key_descriptor` takes named keys,
  F1–F12 and any single character — it used to take nine named keys and
  reject everything else. Escape with nothing held releases the frame.
- **A chord carries no `text`.** A key struck with Ctrl or Meta held is a
  command: Ctrl-A selects, it does not also type an "a". macOS Chromium
  suppresses Meta chords itself, so a live test of this passes even with the
  rule removed — it is asserted on the event payload
  (`key_events`), which is where removing it fails.
- **Paste** is forwarded as `Input.insertText`: one round trip for any length.
  Three routes, because the obvious one does not work on macOS: the focused
  textarea's native `paste` event, `Ctrl-V` caught in `onFrameKey` falling
  back to `navigator.clipboard.readText()` (a flag stops both firing), and a
  **Paste** button using the same clipboard call. **⌘V does not arrive** —
  not as a DOM keydown and not as a paste event — although it pastes normally
  into this same app's chat input. Tried and rejected: `opacity: 0` on the
  target (WebKit will not paste into it; transparent ink on a transparent
  background is used instead, which fixed nothing for ⌘V but is still the
  correct element), and a native Edit menu (the chat input proves WKWebView
  handles the key equivalent without one, so that would have been a
  dependency for a problem that is not there). Left as a documented gap.
- **Mouse events carry modifiers too**, so shift-click and cmd-click work.
- **`console_arg_text`** renders a `RemoteObject` preview. `value` is present
  only for primitives, so object logs used to arrive blank.
- The legacy MCP `type_text` (one `browser_press_key` per character, capped at
  500) is **kept**: with no Playwright Chromium the engine cannot own the
  browser and that path is the whole of takeover for that user.

## Isolation: refused under multiuser (dev-plan/65 P5)

`browser_enabled` resolves to **false** whenever `workdir::is_multiuser()`.
Multiuser isolates each member's *files* into their own folder; the browser is
not a file — one Chromium, one profile, one cookie jar for the whole pod, so a
login by any member is a login for every member and any of them can read it
back. Cleared on the **resolved** `AppConfig`, not at the MCP injection site,
so the Settings toggle and `/doctor` cannot read ON while the engine has it
off; `server::run` says why once, in chat. Partitioning means one Chromium per
active member (~150 MB resident each), which the standard runner is not sized
for: refuse now, partition if a customer asks
(`docs/enterprise/03-admin-guide.md`).

**playwright-mcp's automatic output** is redirected too. It writes an
accessibility snapshot (`page-*.yml`) and a console log on EVERY navigation,
whether or not anyone passed `filename:` — one real session produced 36 files
/ 704 KB, and a `page-*.yml` is the full text of a page the user was on.
`browser_mcp_config` passes `--output-dir .thclaws/state/browser-output` (a
stripped prefix) and `--output-max-size` (64 MiB, evicted oldest-first), both
skipped when `THCLAWS_BROWSER_MCP_CMD` pins its own; `.playwright-mcp/` is in
`cloud/pack.rs::STRIP_PREFIXES` and `seed_def_into` as the backstop. An
EXPLICIT `filename:` still resolves against the workspace root, so the
snapshot-to-a-file trick the prompt teaches is unaffected.

## Known gaps / notes

- Vision images capped at 5 MB per `browser_take_screenshot` result; text at
  48 KB for `browser_snapshot` and 256 KB for everything else.
- The `browser` MCP key is reserved — a user MCP server named `browser`
  would collide with the injected one.
- **⌘V does not reach the takeover frame on macOS** — Ctrl-V and the Paste
  button do. Diagnosed to the element, not the key, but not closed; owner
  accepted it 2026-09-24.
- **Unverified**: two viewers at once against a live engine (the registry is
  unit-tested, nobody has yet had the desktop and a phone on one stream); the
  takeover frame's focus behaviour inside the wry webview, and paste there,
  where clipboard access can differ from a browser.
- **Out of scope, deliberately**: partitioning the browser per member;
  `everyNthFrame` on the CDP side (the engine-side interval covers it);
  replacing playwright-mcp with native Rust.

## See also

- [`docs/browser/README.md`](../docs/browser/README.md) — the design document
  the code's "docs/browser Phase N" comments refer to, plus the gotcha list.
  Published alongside the source (workspace path: `thclaws/docs/browser/`).
- `dev-plan/65-browser-one-browser-you-can-trust.md` — the 2026-09-22 audit
  and repair plan; code cites its findings as `dev-plan/65 #N`. **Internal**,
  not part of the public mirror: the findings it names are summarised here.
- [`mcp.md`](mcp.md) — MCP client subsystem, allowlist, `call_multimodal`.
- [`agentic-loop.md`](agentic-loop.md) — approval gate, tool dispatch.
- [`serve-mode.md`](serve-mode.md) — the IPC bridge the Browser tab rides on `--serve`.
- [`docker.md`](docker.md) — runtime image, why GTK/WebKit are present.
- User-facing: [`user-manual/ch28-browser-automation.md`](../user-manual/ch28-browser-automation.md).
