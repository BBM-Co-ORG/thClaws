# docs/browser — the engine-managed browser

> **Where this lives.** `thclaws/docs/browser/` in the workspace,
> `docs/browser/` in the public mirror — the same relative path the source
> comments use, because the source is published and the comments have to
> resolve for whoever reads it there. The third-party article the design came
> from stays workspace-only at `docs/browser/12gram.md`; it is someone else's
> writing and is not ours to republish.

**This is the document the code means.** Roughly thirty comments across
`browser_cdp.rs`, `config.rs`, `mcp.rs`, `ipc.rs`, `shared_session.rs` and
`BrowserView.tsx` cite "docs/browser Phase 1 / Phase 2 slice N". That plan was
never committed — `git log -- docs/browser` shows this directory has only ever
held `12gram.md`. Until 2026-09-22 every one of those references pointed at
nothing. This file is the retrospective replacement; the phase names below are
the ones the comments use, so a reader can resolve them.

Companion documents:

- `12gram.md` — the article the whole design came from: an agent that sits
  beside a real browser session beats an agent clicking pixel coordinates.
- `dev-plan/65-browser-one-browser-you-can-trust.md` — the 2026-09-22 audit,
  with measurements, and the phased repair plan. Findings are cited in code as
  `dev-plan/65 #N`.
- `thclaws-technical-manual/browser.md` — the maintained internals reference.
- `user-manual/ch28-browser-automation.md` — the user-facing chapter.

## The shape of it

One Chromium. Two clients.

```
   agent ──tools──▶ playwright-mcp ──┐
                                     ├─▶ one Chromium (engine-launched)
   human ──CDP────▶ engine ──────────┘        --remote-debugging-port=<p>
                    (screencast, input,        --user-data-dir=<profile>
                     console, cookies)         --window-size=<viewport>
```

- **The agent never sees CDP.** It sees `browser_*` MCP tools from Microsoft's
  `@playwright/mcp`, because Playwright's auto-waiting, accessibility snapshots
  and selectors are worth keeping.
- **The human channel is native CDP**, because playwright-mcp does not expose
  screencasting, raw input, console streaming or cookie storage *to us*.
- **The engine owns the process** so both attach to the same browser: it
  launches Chromium and hands playwright-mcp `--cdp-endpoint`.
- **Graceful fallback is the invariant.** No Chromium found ⇒ `arm()` returns
  `None`, no `--cdp-endpoint` is injected, playwright-mcp launches its own
  browser, and the tab degrades to screenshots. Nothing errors.

## Phase history (what the comments refer to)

| Phase | Shipped | What it added |
|---|---|---|
| Phase 0–1 | v0.48–v0.49.2 | `browserEnabled` + the synthetic `browser` MCP server (`engine_managed`, bypassing the stdio spawn allowlist because the command is engine-chosen); the Browser tab: status, screenshots, activity feed. Default flipped ON at v0.49.2. |
| Phase 2 slice 1 | v0.50 | Screenshots reach vision models (`call_multimodal`, `mcp_content_to_blocks`, 5 MB/image cap). Before this the engine dropped the pixels. |
| Phase 2 slice 2 | v0.50 | Interactive takeover: coordinate input via allowlisted MCP tools (`browser_mouse_*_xy`, `browser_press_key`), `--caps=vision` to expose them. |
| Phase 2 slice 3 | v0.51 | The inversion: the **engine** launches Chromium with a DevTools port and attaches its own CDP session — live `Page.startScreencast`, `Input.*` for native click/type/scroll, `Runtime.consoleAPICalled` into the activity feed. |
| — | v0.52 | Cookie/session persistence: snapshot + restore over `Storage.getCookies`/`setCookies`, independent of Chromium's ~30 s SQLite flush. |
| — | v0.53 | Branded browsers made opt-in for the CDP path (`THCLAWS_BROWSER_ALLOW_BRANDED=1`): playwright-mcp intermittently fails over CDP against Google Chrome. Orphan Chromium is now **reaped**, not re-attached to. |
| — | v0.134 | dev-plan/65 Phase 0+1: the engine sizes its own window (playwright-mcp ignores `--viewport-size` over CDP), takeover coordinates map through the frame's page size, `ensure_up` probes liveness instead of trusting a `launched` flag, `--serve` flushes cookies on SIGTERM, a logout is no longer resurrected, `/doctor` reports the browser. |

### dev-plan/65 (the audit)

| Phase | Version | What changed |
|---|---|---|
| P0 | v0.135 | `/doctor` browser line, status carries chromium/viewport, `docs/browser/` written |
| P1 | v0.135 | `--window-size` from the resolved viewport, frame-size in every envelope, liveness probe, SIGTERM cookie flush |
| P2 | (unreleased) | per-tool result caps (`browser_snapshot` 48 KB), truncation notices that name `depth`/`target`/`filename`, the prompt names the cheap parameters |
| P3 | (unreleased) | the view follows the agent's tab, multi-viewer, reconnect on target death, server-side frame floor |
| P4 | (unreleased) | real keyboard passthrough with modifiers, paste as `Input.insertText`, `Page.bringToFront` on attach, console object logs |
| P5 | (unreleased) | browser refused under multiuser (one cookie jar, every member); playwright-mcp output redirected into stripped state + capped |
| P6 | (unreleased) | this doc ships with the source; technical manual + user manual (EN/TH) brought current |

## Where things live

| Concern | Code |
|---|---|
| Enablement, injection, launch flags, viewport | `config.rs`: `default_browser_enabled`, `browser_mcp_config`, `browser_viewport` |
| Chromium discovery, launch, reap, cookies, screencast, input | `browser_cdp.rs` |
| Keyboard / modifier translation | `browser_cdp.rs`: `key_descriptor`, `modifier_mask`, `key_events` |
| Which tab the live view is on | `browser_cdp.rs`: `choose_target`, `reconcile`, `start_watcher` |
| Who is watching (ref-counted) | `browser_cdp.rs`: `viewers`, `broadcast`; identity is `IpcContext::viewer_id` |
| Arming CDP at MCP bootstrap | `shared_session.rs` (the `merged_mcp` loop) |
| Model-facing tool trim, result caps, lazy launch | `mcp.rs`: `BROWSER_MODEL_HIDDEN_TOOLS`, `tool_text_budget` / `narrowing_hint` / `cap_mcp_text`, `lazy_browser_up` |
| How the model is told to read a page | `prompts.rs::services_prompt_section` |
| Browser tab ↔ engine | `ipc.rs`: `browser_status_get`, `_screenshot_get`, `_screencast_start/_stop`, `_cdp_input`, `_input_call`, `_enabled_get/_set` |
| The tab | `frontend/src/components/BrowserView.tsx` |
| Publish/seed safety | `cloud/pack.rs::STRIP_PREFIXES`, `multi_tenant/registry.rs::seed_def_into` |
| Runner image | `thclaws/Dockerfile` (`PLAYWRIGHT_BROWSERS_PATH`, `THCLAWS_BROWSER_MCP_CMD`) |

## Things that will bite you

- **`--viewport-size` is ignored when playwright-mcp is handed
  `--cdp-endpoint`** — it adopts the page the engine already opened. Measured:
  756×469 against a requested 1920×1080. The engine's `--window-size` is what
  actually sizes a page in CDP mode; `--viewport-size` still covers the
  self-launch fallback. Keep both fed from `AppConfig::browser_viewport`.
- **Screencast frames are scaled**, so frame pixels ≠ page CSS pixels. Input
  coordinates must map through `metadata.deviceWidth/deviceHeight`, which the
  engine forwards with every frame as `w`/`h`.
- **The screencast ack must be fire-and-forget.** A reply-awaiting
  `Page.screencastFrameAck` inside the reader task deadlocks it — the only task
  that can route the reply is the one blocked waiting for it. That bug read as
  "1 frame per 15 s". Use `PageSession::notify`.
- **Never re-attach to an orphan Chromium.** A fresh playwright-mcp connecting
  over CDP into a browser a previous run's playwright-mcp already drove fails
  with "Browser context management is not supported". Reap it; cookies live in
  the persistent profile and survive the relaunch.
- **A client's `dispatch` `Arc` is NOT a client identity.** `server.rs` builds
  one per socket, but `gui.rs` rebuilds it on every IPC message — so anything
  ref-counted per viewer must key on `IpcContext::viewer_id`, which is minted
  once per connection. Keying the live view on the closure's address looks
  right and leaks a viewer per start on the desktop.
- **A socket that closes never sends `browser_screencast_stop`**, so
  `handle_socket` releases the viewer itself on teardown. Without it Chromium
  keeps screencasting into a channel nobody drains.
- **Chromium neither paints nor delivers input to a background tab.** Attach
  to a tab that is not in front and the live view is black and every keystroke
  times out, with no error anywhere to say why — so `attach_and_start` calls
  `Page.bringToFront`. The cost is that pinning the view to one tab makes the
  agent's tab `hidden`, which pauses some pages' timers. Found by running the
  live tests against a profile that had restored seven tabs.
- **A chord must carry no `text`.** macOS Chromium suppresses it for Meta
  anyway, so a live "cmd-A does not type an a" test passes even with the rule
  deleted — the rule is asserted on the event payload
  (`a_chord_carries_no_text_and_a_plain_key_does`), which is where removing it
  fails. On Linux Ctrl-A would select all *and* type an "a".
- **playwright-mcp writes a page dump per navigation, unasked.** An
  accessibility snapshot (`page-*.yml`) and a console log land in its
  `--output-dir` whether or not anyone passed `filename:` — a real 12-minute
  session left 36 files / 704 KB, and a `page-*.yml` is the full text of a
  page the user was on, logged in or not. `browser_mcp_config` points that at
  `.thclaws/state/browser-output` (stripped on publish, kept out of a seeded
  member workspace) and caps it with `--output-max-size`. `.playwright-mcp/`
  is in both exclusion lists as a backstop. An EXPLICIT `filename:` still
  resolves against the workspace root, so the snapshot-to-a-file trick the
  prompt teaches is unaffected.
- **The DevTools HTTP probe must be HTTP/1.1 with `Connection: close`.**
  Chromium ignores HTTP/1.0 requests entirely.
- **The profile is where the user's real logins are.** It stays out of the
  workspace on desktop, and on cloud it sits at
  `.thclaws/state/browser-profile/` — which `cloud/pack.rs` strips from a
  published agent and `seed_def_into` must exclude from a seeded member
  workspace. Both lists have to be checked against the CURRENT layout after
  any `.thclaws/` move.
- **One `browser_snapshot` can be 889 KB** (measured, one Wikipedia article).
  `browser_find` answering the same question: 57 bytes. Snapshots are capped at
  48 KB — the size of the Hacker News front page, i.e. what "a whole real page"
  costs — and the truncation notice names `depth`/`target`/`filename` so the
  model has something to do about it. Every other tool keeps the 256 KB
  blanket; `THCLAWS_MCP_MAX_TEXT_BYTES` overrides both.

## Env knobs

| Var | Effect |
|---|---|
| `THCLAWS_BROWSER_ENABLED=0` | flips the *default* off for a whole fleet (cloud runners set it); explicit `browserEnabled` in settings still wins |
| `THCLAWS_BROWSER_MCP_CMD` | replaces the playwright-mcp launch command (the runner image pins the preinstalled binary) |
| — (no env) | `--output-dir`/`--output-max-size` are appended by `browser_mcp_config` unless `THCLAWS_BROWSER_MCP_CMD` already pins them |
| `THCLAWS_BROWSER_VIEWPORT="W,H"` | page size for both the engine window and `--viewport-size` (default `1920,1080`) |
| `THCLAWS_BROWSER_FRAME_MS` | server-side floor between screencast frames (default `80`; `0` sends every paint) |
| `THCLAWS_BROWSER_CDP=0` | kill switch for the whole human channel; agent tools keep working |
| `THCLAWS_BROWSER_EXECUTABLE` | explicit Chromium path |
| `THCLAWS_BROWSER_ALLOW_BRANDED=1` | allow driving Chrome/Edge over CDP (unreliable; see above) |
| `THCLAWS_BROWSER_ALL_TOOLS=1` | register the raw playwright-mcp tool list, untrimmed |
| `THCLAWS_MCP_MAX_TEXT_BYTES` | text cap on ANY MCP tool result; wins over the per-tool floor (`browser_snapshot` is 48 KB, everything else 256 KB). `0` disables capping |
| `PLAYWRIGHT_BROWSERS_PATH` | Chromium cache root; `find_chromium` understands the classic `chromium-<rev>/` layout |
