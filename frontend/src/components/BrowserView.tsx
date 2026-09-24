import { useEffect, useRef, useState } from "react";
import { send, subscribe, type IPCMessage } from "../hooks/useIPC";
import { ChatMarkdown } from "./ChatMarkdown";

// docs/browser Phase 1 — the Browser tab for the engine-managed
// Playwright MCP browser (`browserEnabled` in settings.json).
//
// Layout: main column = status card + live screenshot + activity feed;
// right sidebar = a compact chat so the user can direct the agent
// ("take over and fill this form") without leaving the tab — the
// 12gram split-screen workflow in one place.
//
//   - status        ← `browser_status_get` IPC
//   - screenshot    ← `browser_screenshot_get` IPC: runs directly on
//                     the managed MCP client (no agent loop, no
//                     tokens), auto-captured ~1s after each browser
//                     tool result while the tab is visible
//   - activity      ← the same `chat_tool_call`/`chat_tool_result`
//                     dispatches Chat renders, filtered to `browser__*`
//   - sidebar chat  ← `shell_input` (same pipe as the Chat tab) +
//                     `chat_user_message` / `chat_text_delta` events

type BrowserStatus = {
  enabled: boolean;
  headless: boolean;
  command: string;
  command_found: boolean;
  cdp: boolean;
  chromium: string;
  chromium_found: boolean;
  viewport: string;
};

type ActivityEntry = {
  id: number;
  at: string;
  kind: "call" | "result" | "console";
  tool: string;
  detail: string;
};

type ChatMsg = {
  id: number;
  role: "user" | "assistant" | "system";
  text: string;
};

const MAX_ENTRIES = 200;
const MAX_CHAT = 80;
const SHOT_DEBOUNCE_MS = 1000;
// Ceiling on how often a screencast frame is painted. Chromium emits a frame
// per paint, which during an agent run is far more than a human can read; each
// one is a base64 data: URL swap, so the cost is ours, not the browser's.
// ~12 fps still reads as live and leaves the render thread alone.
const FRAME_MIN_INTERVAL_MS = 80;

function shorten(s: string, n: number): string {
  return s.length > n ? s.slice(0, n) + "…" : s;
}

export function BrowserView({ active }: { active: boolean }) {
  const [status, setStatus] = useState<BrowserStatus | null>(null);
  const [entries, setEntries] = useState<ActivityEntry[]>([]);
  const [shot, setShot] = useState<{ src: string; at: string } | null>(null);
  const [shotErr, setShotErr] = useState<string>("");
  const [shotBusy, setShotBusy] = useState(false);
  const [chat, setChat] = useState<ChatMsg[]>([]);
  const [chatInput, setChatInput] = useState("");
  const [askPrompt, setAskPrompt] = useState<{ id: number; question: string } | null>(null);
  const [busy, setBusy] = useState(false);
  // Interactive takeover (Phase 2 slice 2): when on, the screenshot is
  // clickable/typeable — every action routes through the allowlisted
  // `browser_input_call` arm and refreshes the screenshot.
  const [takeover, setTakeover] = useState(false);
  // slice 3: live CDP screencast — frames stream in while takeover is
  // on and the engine owns the browser; falls back to screenshots.
  const [live, setLive] = useState(false);
  // dev-plan/65 P3: the agent's open tabs, and which one the live view is on.
  // `pinned` is the human steering — null means "follow whatever tab the
  // agent just opened", which is what the view does by itself.
  const [tabs, setTabs] = useState<{ id: string; url: string; title: string }[]>([]);
  const [activeTab, setActiveTab] = useState<string | null>(null);
  const [pinned, setPinned] = useState<string | null>(null);
  // "live" | "attaching" | "detached" | "error" — the state of the VIEW, as
  // distinct from the page. A dead target used to leave the last frame up
  // forever, which reads as a frozen page rather than a lost connection.
  const [viewState, setViewState] = useState<string>("live");
  const [pageUrl, setPageUrl] = useState("");
  const [urlInput, setUrlInput] = useState("");
  const [typeInput, setTypeInput] = useState("");
  const [inputErr, setInputErr] = useState("");

  const nextId = useRef(1);
  const listRef = useRef<HTMLDivElement | null>(null);
  const chatRef = useRef<HTMLDivElement | null>(null);
  const activeRef = useRef(active);
  const shotTimer = useRef<number | null>(null);
  const lastFrameAt = useRef(0);
  // Page size in CSS pixels, reported with each screencast frame.
  const frameSize = useRef<{ w: number; h: number } | null>(null);
  // Proof that Chromium is already running, so the screencast can attach
  // without being the thing that launches it.
  const [browserUp, setBrowserUp] = useState(false);
  // Browser activity that happened while the tab was hidden — capture
  // one fresh screenshot when the user comes back.
  const staleShot = useRef(false);

  activeRef.current = active;

  function requestShot() {
    setShotBusy(true);
    send({ type: "browser_screenshot_get" });
  }

  function sendInput(tool: string, args: Record<string, unknown>) {
    setInputErr("");
    send({ type: "browser_input_call", tool, args });
  }

  // Map a click on the rendered screenshot to page coordinates. The
  // <img> uses object-contain, so the drawn picture may be letterboxed
  // inside the element box — account for that before scaling to the
  // image's natural (viewport) size.
  function imgClickCoords(e: React.MouseEvent<HTMLImageElement>) {
    const img = e.currentTarget;
    const rect = img.getBoundingClientRect();
    // In live mode the frame is a SCALED picture of the page (Chromium fits it
    // to the engine's startScreencast cap), so the image's natural size is not
    // the coordinate space CDP input wants — the page's CSS-pixel size that
    // arrives with each frame is. The two agreed only while the viewport was
    // smaller than the cap, which is why this looked correct before the
    // viewport was fixed (dev-plan/65 #1). Screenshot mode still maps through
    // the image: playwright-mcp returns it at viewport size, unscaled.
    const page = live ? frameSize.current : null;
    const natW = page?.w || img.naturalWidth || 1;
    const natH = page?.h || img.naturalHeight || 1;
    const scale = Math.min(rect.width / natW, rect.height / natH);
    const drawnW = natW * scale;
    const drawnH = natH * scale;
    const offX = (rect.width - drawnW) / 2;
    const offY = (rect.height - drawnH) / 2;
    const x = (e.clientX - rect.left - offX) / scale;
    const y = (e.clientY - rect.top - offY) / scale;
    if (x < 0 || y < 0 || x > natW || y > natH) return null;
    return { x: Math.round(x), y: Math.round(y) };
  }

  // Takeover relays the pointer itself — press, every move with the
  // button held, release — so a drag is the user's own drag, with its
  // own path and timing. A slider CAPTCHA is exactly that, and the
  // click this used to send could never move one. A click is the same
  // gesture with no movement between press and release.
  //
  // Live (engine-owned Chromium): straight over CDP, moves coalesced to
  // one per animation frame, hovers included so pages that watch the
  // pointer before a click see it arrive.
  //
  // Screenshot mode (playwright-mcp's own browser): the same gesture as
  // `browser_mouse_move_xy` / `_down` / `_up` tool calls, one at a time
  // in order — a call takes tens of milliseconds, and a release that
  // overtook the last move would end the drag short — with queued moves
  // collapsed to the latest position.
  const dragging = useRef(false);
  const mcpQueue = useRef<{ tool: string; args: Record<string, unknown> }[]>([]);
  const mcpInFlight = useRef(false);
  function pumpMcp() {
    if (mcpInFlight.current) return;
    const next = mcpQueue.current.shift();
    if (!next) return;
    mcpInFlight.current = true;
    sendInput(next.tool, next.args);
  }
  function enqueueMcp(tool: string, args: Record<string, unknown>) {
    const q = mcpQueue.current;
    const last = q[q.length - 1];
    if (tool === "browser_mouse_move_xy" && last?.tool === tool) q[q.length - 1] = { tool, args };
    else q.push({ tool, args });
    pumpMcp();
  }
  const mcpInFlightRef = mcpInFlight;
  const pumpMcpRef = useRef(pumpMcp);
  pumpMcpRef.current = pumpMcp;
  function mcpMove(pt: { x: number; y: number }) {
    enqueueMcp("browser_mouse_move_xy", { element: "user takeover pointer", x: pt.x, y: pt.y });
  }
  const pendingMove = useRef<{ x: number; y: number; buttons: number } | null>(null);
  const moveRaf = useRef<number | null>(null);
  function flushMove() {
    moveRaf.current = null;
    const m = pendingMove.current;
    pendingMove.current = null;
    if (m) send({ type: "browser_cdp_input", kind: "move", args: m });
  }
  function relayMove(pt: { x: number; y: number }, buttons: number) {
    pendingMove.current = { x: pt.x, y: pt.y, buttons };
    if (moveRaf.current === null) moveRaf.current = requestAnimationFrame(flushMove);
  }
  function onShotPointerDown(e: React.PointerEvent<HTMLImageElement>) {
    if (!takeoverRef.current || e.button !== 0) return;
    const pt = imgClickCoords(e);
    if (!pt) return;
    e.preventDefault();
    try {
      e.currentTarget.setPointerCapture(e.pointerId);
    } catch {
      /* a pointer that is already gone — the gesture still relays */
    }
    dragging.current = true;
    // `preventDefault` above suppresses the focus a mousedown would normally
    // give the focusable wrapper, so take it explicitly — otherwise clicking
    // the page and then typing sends the keystrokes nowhere.
    frameBoxRef.current?.focus();
    if (!liveRef.current) {
      mcpMove(pt);
      enqueueMcp("browser_mouse_down", {});
      return;
    }
    // The press lands where the pointer already is, as it does for a mouse.
    if (moveRaf.current !== null) {
      cancelAnimationFrame(moveRaf.current);
      flushMove();
    }
    send({ type: "browser_cdp_input", kind: "down", args: { x: pt.x, y: pt.y } });
  }
  function onShotPointerUp(e: React.PointerEvent<HTMLImageElement>) {
    if (!dragging.current) return;
    dragging.current = false;
    try {
      if (e.currentTarget.hasPointerCapture(e.pointerId)) {
        e.currentTarget.releasePointerCapture(e.pointerId);
      }
    } catch {
      /* same */
    }
    // Wherever the hand let go — clamped, since a drag may run off the image.
    const pt = imgClickCoords(e) ?? hoverPos.current;
    if (!liveRef.current) {
      mcpMove(pt);
      enqueueMcp("browser_mouse_up", {});
      return;
    }
    if (moveRaf.current !== null) {
      cancelAnimationFrame(moveRaf.current);
      flushMove();
    }
    send({ type: "browser_cdp_input", kind: "up", args: { x: pt.x, y: pt.y } });
  }

  // dev-plan/65 P4 — real keyboard passthrough. The frame takes focus and
  // forwards the human's own keydown/keyup, modifiers included, so a chord
  // (Cmd-A, Ctrl-L), a held arrow and autorepeat all behave. The text box
  // below stays: over a 200 ms link, typing a long string one key at a time
  // is genuinely worse than sending it whole.
  const [frameFocused, setFrameFocused] = useState(false);
  const frameBoxRef = useRef<HTMLTextAreaElement | null>(null);
  // When ⌘/Ctrl-V was last struck, cleared by the native paste event. Non-zero
  // after the timeout means the event never came and the backstop should run.
  const pasteKeyAt = useRef(0);

  function keyArgs(e: React.KeyboardEvent): Record<string, unknown> {
    return {
      key: e.key,
      ctrl: e.ctrlKey,
      meta: e.metaKey,
      shift: e.shiftKey,
      alt: e.altKey,
    };
  }

  function onFrameKey(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (!takeoverRef.current || !liveRef.current) return;
    // Let the user out: Escape with nothing held blurs the frame instead of
    // reaching the page, so the keyboard cannot be captured with no way back.
    if (e.type === "keydown" && e.key === "Escape" && !e.ctrlKey && !e.metaKey && !e.altKey) {
      e.preventDefault();
      e.currentTarget.blur();
      return;
    }
    // Paste is handled by onPaste — letting the chord through as well would
    // send the key AND the clipboard. A textarea is editable, so the native
    // paste event does fire there; the async clipboard read below is a
    // BACKSTOP for a webview that still refuses it, and `pastedAt` keeps the
    // two from both firing.
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "v") {
      if (e.type !== "keydown") return;
      const struckAt = Date.now();
      pasteKeyAt.current = struckAt;
      window.setTimeout(() => {
        if (pasteKeyAt.current !== struckAt) return; // onPaste already took it
        pasteKeyAt.current = 0;
        navigator.clipboard
          ?.readText()
          .then((text) => {
            if (text) send({ type: "browser_cdp_input", kind: "text", args: { text } });
          })
          .catch(() => {
            /* no clipboard permission in this webview — nothing more to try */
          });
      }, 150);
      return;
    }
    e.preventDefault();
    e.stopPropagation();
    send({
      type: "browser_cdp_input",
      kind: e.type === "keyup" ? "keyup" : "keydown",
      args: keyArgs(e),
    });
  }

  function onFramePaste(e: React.ClipboardEvent<HTMLTextAreaElement>) {
    if (!takeoverRef.current || !liveRef.current) return;
    pasteKeyAt.current = 0; // the native event came; the backstop stands down
    const text = e.clipboardData.getData("text");
    if (!text) return;
    e.preventDefault();
    // `Input.insertText` puts the whole string in at once — the page sees it
    // as typed, and it costs one round trip rather than one per character.
    send({ type: "browser_cdp_input", kind: "text", args: { text } });
  }

  function onShotMouseMove(e: React.PointerEvent<HTMLImageElement>) {
    if (!takeoverRef.current) return;
    const pt = imgClickCoords(e);
    if (!pt) return;
    hoverPos.current = pt;
    if (liveRef.current) relayMove(pt, dragging.current ? 1 : 0);
    else if (dragging.current) mcpMove(pt);
  }

  // Wheel → remote scroll, throttled by accumulating deltas. Attached
  // via ref with passive:false so the local pane doesn't also scroll.
  const wheelAcc = useRef({ x: 0, y: 0, timer: null as number | null });
  const takeoverRef = useRef(takeover);
  takeoverRef.current = takeover;
  const liveRef = useRef(live);
  liveRef.current = live;
  const statusRef = useRef<BrowserStatus | null>(null);
  statusRef.current = status;
  // Last pointer position over the page image (page coordinates) —
  // CDP wheel events want x/y context.
  const hoverPos = useRef({ x: 0, y: 0 });
  const shotImgRef = useRef<HTMLImageElement | null>(null);
  useEffect(() => {
    const img = shotImgRef.current;
    if (!img) return;
    const onWheel = (e: WheelEvent) => {
      if (!takeoverRef.current) return;
      e.preventDefault();
      wheelAcc.current.x += e.deltaX;
      wheelAcc.current.y += e.deltaY;
      if (wheelAcc.current.timer === null) {
        wheelAcc.current.timer = window.setTimeout(() => {
          const { x, y } = wheelAcc.current;
          wheelAcc.current = { x: 0, y: 0, timer: null };
          sendInput("browser_mouse_wheel", {
            deltaX: Math.round(x),
            deltaY: Math.round(y),
          });
        }, 150);
      }
    };
    img.addEventListener("wheel", onWheel, { passive: false });
    return () => img.removeEventListener("wheel", onWheel);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shot !== null]);

  function scheduleShot() {
    if (liveRef.current) return; // screencast frames already flow
    if (!activeRef.current) {
      staleShot.current = true;
      return;
    }
    if (shotTimer.current !== null) window.clearTimeout(shotTimer.current);
    shotTimer.current = window.setTimeout(() => {
      shotTimer.current = null;
      requestShot();
    }, SHOT_DEBOUNCE_MS);
  }

  useEffect(() => {
    const unsub = subscribe((msg: IPCMessage) => {
      if (msg.type === "browser_status") {
        setStatus({
          enabled: Boolean(msg.enabled),
          headless: Boolean(msg.headless),
          command: typeof msg.command === "string" ? msg.command : "",
          command_found: Boolean(msg.command_found),
          cdp: Boolean(msg.cdp),
          chromium: typeof msg.chromium === "string" ? msg.chromium : "",
          chromium_found: Boolean(msg.chromium_found),
          viewport: typeof msg.viewport === "string" ? msg.viewport : "",
        });
        return;
      }
      if (msg.type === "browser_screenshot") {
        setShotBusy(false);
        if (msg.ok && typeof msg.data === "string") {
          const mime = typeof msg.mime === "string" ? msg.mime : "image/png";
          setShot({
            src: `data:${mime};base64,${msg.data}`,
            at: new Date().toLocaleTimeString([], { hour12: false }),
          });
          setShotErr("");
          staleShot.current = false;
        } else {
          setShotErr(typeof msg.error === "string" ? msg.error : "capture failed");
        }
        return;
      }
      if (msg.type === "browser_frame" && typeof msg.data === "string") {
        // The page's own size in CSS pixels, which is the space CDP input
        // works in. Chromium scales the JPEG down to the engine's frame cap,
        // so the image's pixels are a different space — recorded on every
        // frame (not just the first) because the page can be resized.
        if (typeof msg.w === "number" && typeof msg.h === "number" && msg.w > 0 && msg.h > 0) {
          frameSize.current = { w: msg.w, h: msg.h };
        }
        // Drop frames that arrive inside the interval rather than queueing
        // them — a stale frame has no value once a newer one exists.
        const now = Date.now();
        if (now - lastFrameAt.current < FRAME_MIN_INTERVAL_MS) return;
        lastFrameAt.current = now;
        setShot({
          src: `data:image/jpeg;base64,${msg.data}`,
          at: new Date().toLocaleTimeString([], { hour12: false }),
        });
        return;
      }
      if (msg.type === "browser_screencast") {
        setLive(Boolean(msg.active));
        if (!msg.ok && typeof msg.error === "string") setInputErr(msg.error);
        return;
      }
      if (msg.type === "browser_tabs" && Array.isArray(msg.tabs)) {
        setTabs(msg.tabs as { id: string; url: string; title: string }[]);
        setActiveTab(typeof msg.active === "string" ? msg.active : null);
        // A pinned tab the agent closed: drop the pin rather than leave the
        // strip showing a selection that no longer exists.
        setPinned((p) =>
          p && !(msg.tabs as { id: string }[]).some((t) => t.id === p) ? null : p,
        );
        return;
      }
      if (msg.type === "browser_view" && typeof msg.state === "string") {
        setViewState(msg.state);
        if (typeof msg.url === "string" && msg.url) setPageUrl(msg.url);
        if (msg.state === "error" && typeof msg.error === "string") setInputErr(msg.error);
        return;
      }
      if (msg.type === "browser_console" && typeof msg.text === "string") {
        const level = typeof msg.level === "string" ? msg.level : "log";
        if (level === "error" || level === "warning") {
          push("console", level, shorten(msg.text, 300));
        }
        return;
      }
      if (msg.type === "browser_nav" && typeof msg.url === "string") {
        setPageUrl(msg.url);
        setBrowserUp(true);
        return;
      }
      if (msg.type === "browser_input_result") {
        if (
          typeof msg.tool === "string" &&
          msg.tool.startsWith("browser_mouse_") &&
          mcpInFlightRef.current
        ) {
          mcpInFlightRef.current = false;
          pumpMcpRef.current();
        }
        if (msg.ok) {
          // The page just changed under user input — refresh promptly.
          scheduleShot();
        } else if (typeof msg.error === "string") {
          setInputErr(msg.error);
        }
        return;
      }
      if (msg.type === "gui_busy_changed") {
        setBusy(Boolean(msg.busy));
        return;
      }
      // Sidebar chat transcript — the SAME shared conversation the
      // Chat + Terminal tabs render. Session-level events (slash
      // output, /clear, /load, /new) keep all three views in sync.
      if (msg.type === "chat_user_message" && typeof msg.text === "string") {
        pushChat("user", msg.text);
        return;
      }
      if (msg.type === "chat_text_delta" && typeof msg.text === "string") {
        appendAssistant(msg.text);
        return;
      }
      if (msg.type === "chat_slash_output" && typeof msg.text === "string") {
        pushChat("system", msg.text);
        return;
      }
      if (msg.type === "chat_error" && typeof msg.text === "string") {
        pushChat("system", `⚠ ${msg.text}`);
        return;
      }
      if (msg.type === "new_session_ack") {
        setChat([]);
        setAskPrompt(null);
        return;
      }
      if (msg.type === "chat_history_replaced") {
        const restored: ChatMsg[] = [];
        if (Array.isArray(msg.messages)) {
          for (const m of msg.messages as { role: string; content: string }[]) {
            if (typeof m.content !== "string" || !m.content) continue;
            if (m.role === "user") restored.push({ id: nextId.current++, role: "user", text: m.content });
            else if (m.role === "assistant") restored.push({ id: nextId.current++, role: "assistant", text: m.content });
            // tool/system entries stay in the full Chat tab; the
            // sidebar keeps the compact user/assistant thread.
          }
        }
        setChat(restored.slice(-MAX_CHAT));
        return;
      }
      if (msg.type === "ask_user_question") {
        const id = typeof msg.id === "number" ? msg.id : null;
        const question = typeof msg.question === "string" ? msg.question : "";
        if (id !== null) {
          // The model paused the turn to ask. Surface the question and
          // route the next sidebar input to the pending-ask responder
          // (see sendChat) rather than starting a fresh turn — otherwise
          // the turn hangs with no way to answer from this tab.
          setAskPrompt({ id, question });
          pushChat("assistant", question);
        }
        return;
      }
      if (msg.type === "chat_done") {
        setBusy(false);
        setAskPrompt(null);
        return;
      }
      // Activity: MCP tools register as `browser__<tool>`.
      if (msg.type === "chat_tool_call") {
        const raw = typeof msg.tool_name === "string" ? msg.tool_name : "";
        if (!raw.startsWith("browser__")) return;
        const detail = msg.input ? shorten(JSON.stringify(msg.input), 300) : "";
        push("call", raw.slice("browser__".length), detail);
      } else if (msg.type === "chat_tool_result") {
        const raw = typeof msg.name === "string" ? msg.name : "";
        if (!raw.startsWith("browser__")) return;
        const detail = typeof msg.output === "string" ? shorten(msg.output, 300) : "";
        push("result", raw.slice("browser__".length), detail);
        // A browser tool just returned, so Chromium is up — the screencast
        // can attach from here without launching anything itself.
        setBrowserUp(true);
        // The page just (probably) changed — refresh the screenshot.
        scheduleShot();
      }
    });
    send({ type: "browser_status_get" });
    return unsub;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function push(kind: ActivityEntry["kind"], tool: string, detail: string) {
    const at = new Date().toLocaleTimeString([], { hour12: false });
    setEntries((prev) => {
      const next = [...prev, { id: nextId.current++, at, kind, tool, detail }];
      return next.length > MAX_ENTRIES ? next.slice(next.length - MAX_ENTRIES) : next;
    });
  }

  function pushChat(role: ChatMsg["role"], text: string) {
    setChat((prev) => {
      const next = [...prev, { id: nextId.current++, role, text }];
      return next.length > MAX_CHAT ? next.slice(next.length - MAX_CHAT) : next;
    });
  }

  function appendAssistant(delta: string) {
    setChat((prev) => {
      const last = prev[prev.length - 1];
      if (last && last.role === "assistant") {
        const next = prev.slice(0, -1);
        next.push({ ...last, text: last.text + delta });
        return next;
      }
      const next = [...prev, { id: nextId.current++, role: "assistant" as const, text: delta }];
      return next.length > MAX_CHAT ? next.slice(next.length - MAX_CHAT) : next;
    });
  }

  function sendChat() {
    const text = chatInput.trim();
    if (!text) return;
    setChatInput("");
    if (askPrompt) {
      // Answering an AskUserQuestion the model raised — route to the
      // pending-ask responder, not a fresh turn. The backend echoes the
      // reply to the Terminal only, so push a local user bubble here.
      pushChat("user", text);
      send({ type: "ask_user_response", id: askPrompt.id, text });
      setAskPrompt(null);
      setBusy(true);
      return;
    }
    setBusy(true);
    send({ type: "shell_input", text, attachments: [] });
  }

  // Catch up on a screenshot missed while the tab was hidden.
  useEffect(() => {
    if (active && staleShot.current) requestShot();
  }, [active]);

  // Screencast lifecycle. It used to require takeover, which reserved the
  // live view for the one case where a HUMAN was driving, while an agent
  // working the page got the slow path: one `browser_take_screenshot`
  // debounced 1s behind each tool result, trailing edge only, so a burst of
  // tool calls left the view frozen until the agent paused. The screencast
  // rides the engine's own CDP session, so unlike that screenshot — which
  // shares the one stdio pipe the agent's tool calls use
  // (`browser_screenshot_get` in ipc.rs) — it costs the agent nothing.
  //
  // But it cannot simply key off `status.cdp`: that means the endpoint is
  // ARMED, not that Chromium is running, and `screencast_start` calls
  // `ensure_up()`, which launches it. Opening this tab to look around would
  // then start the heaviest process the engine owns for nothing. So: takeover
  // still forces it (the user asked, and waiting for Chromium is expected
  // there), and otherwise it waits for proof the browser is already up — the
  // first browser tool result or navigation of the session.
  useEffect(() => {
    const want = active && Boolean(status?.cdp) && (takeover || browserUp);
    if (want && !live) {
      send({ type: "browser_screencast_start" });
    } else if (!want && live) {
      send({ type: "browser_screencast_stop" });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [takeover, browserUp, active, status?.cdp]);

  useEffect(() => {
    if (active && listRef.current) {
      listRef.current.scrollTop = listRef.current.scrollHeight;
    }
  }, [entries, active]);

  useEffect(() => {
    if (active && chatRef.current) {
      chatRef.current.scrollTop = chatRef.current.scrollHeight;
    }
  }, [chat, busy, active]);

  const browserUsed = entries.length > 0;

  return (
    <div className="h-full flex gap-3 p-4 overflow-hidden">
      {/* ── Main column: status + screenshot + activity ── */}
      <div className="flex-1 min-w-0 flex flex-col gap-3 overflow-hidden">
        <div
          className="rounded-lg border p-3 shrink-0"
          style={{ borderColor: "var(--border)", background: "var(--bg-secondary)" }}
        >
          <div className="flex items-center gap-2 mb-1">
            <span className="text-sm font-semibold" style={{ color: "var(--text-primary)" }}>
              Managed browser
            </span>
            {status && (
              <span
                className="text-[10px] px-2 py-0.5 rounded-full font-medium"
                style={{
                  background: status.enabled ? "var(--accent)" : "var(--bg-primary)",
                  color: status.enabled ? "white" : "var(--text-secondary)",
                  border: status.enabled ? "none" : "1px solid var(--border)",
                }}
              >
                {status.enabled ? (status.headless ? "headless" : "headed") : "disabled"}
              </span>
            )}
            <div className="flex-1" />
            {status?.enabled && (
              <button
                onClick={() => setTakeover((t) => !t)}
                className="text-[11px] px-2 py-0.5 rounded border font-medium"
                style={{
                  borderColor: takeover ? "var(--accent)" : "var(--border)",
                  color: takeover ? "white" : "var(--text-secondary)",
                  background: takeover ? "var(--accent)" : "transparent",
                }}
                title="Interact with the page directly — click, type, and scroll on the screenshot"
              >
                🖱 {takeover ? "Taking over" : "Take over"}
              </button>
            )}
            {status?.enabled && (
              <button
                onClick={requestShot}
                disabled={shotBusy}
                className="text-[11px] px-2 py-0.5 rounded border"
                style={{
                  borderColor: "var(--border)",
                  color: "var(--text-secondary)",
                  opacity: shotBusy ? 0.5 : 1,
                }}
                title="Capture a screenshot of the managed browser now"
              >
                {shotBusy ? "capturing…" : "📷 capture"}
              </button>
            )}
          </div>
          {!status && (
            <p className="text-xs" style={{ color: "var(--text-secondary)" }}>Loading…</p>
          )}
          {status && !status.enabled && (
            <p className="text-xs leading-relaxed" style={{ color: "var(--text-secondary)" }}>
              Browser automation is off. Set <code>&quot;browserEnabled&quot;: true</code> in{" "}
              <code>.thclaws/settings.json</code> and reload — the engine then manages the
              official Playwright MCP server and the agent gains <code>browser_*</code> tools.
            </p>
          )}
          {status && status.enabled && (
            <>
              <p className="text-xs font-mono" style={{ color: "var(--text-secondary)" }}>
                {status.command}
              </p>
              {!status.command_found && (
                <p className="text-xs mt-1 leading-relaxed" style={{ color: "#dc2626" }}>
                  ⚠ the browser server&apos;s command isn&apos;t on PATH — it can&apos;t start.
                  On desktop, install Node.js (e.g. <code>brew install node</code>) and
                  restart thClaws.
                </p>
              )}
              {/* Why the live view is missing, rather than leaving the user to
                  conclude it doesn't exist: with no Playwright Chromium the
                  engine can't own the browser, so takeover falls back to ~1 fps
                  screenshots and nothing said so. */}
              {status.command_found && !status.cdp && (
                <p className="text-xs mt-1 leading-relaxed" style={{ color: "var(--text-secondary)" }}>
                  {status.chromium_found ? (
                    <>
                      ⓘ Live view off — the engine isn&apos;t driving this browser.
                      Screenshots and takeover still work, at about one frame a second.
                    </>
                  ) : (
                    <>
                      ⓘ No Playwright Chromium found, so the <strong>live view and
                      takeover run on ~1 fps screenshots</strong>. Install it once for a
                      real live stream: <code>npx playwright install chromium</code>,
                      then restart thClaws.
                    </>
                  )}
                </p>
              )}
              {status.cdp && status.viewport && (
                <p className="text-[10px] mt-1" style={{ color: "var(--text-secondary)" }}>
                  live view ready · viewport {status.viewport}
                  {status.chromium ? ` · ${status.chromium}` : ""}
                </p>
              )}
            </>
          )}
        </div>

        {/* Screenshot panel — the in-tab view of the page. */}
        {status?.enabled && (
          <div
            className="rounded-lg border shrink-0 overflow-hidden"
            style={{ borderColor: "var(--border)", background: "var(--bg-primary)" }}
          >
            {/* Tab strip (dev-plan/65 P3). The view follows the tab the agent
                just opened; clicking one pins it there until the pin is
                cleared, so a human reading a page is not yanked away. */}
            {live && tabs.length > 1 && (
              <div
                className="flex gap-1 items-center px-1.5 py-1 overflow-x-auto"
                style={{ borderBottom: "1px solid var(--border)" }}
              >
                {tabs.map((t) => {
                  const on = t.id === activeTab;
                  return (
                    <button
                      key={t.id}
                      onClick={() => {
                        const next = pinned === t.id ? null : t.id;
                        setPinned(next);
                        send({ type: "browser_tab_select", target: next });
                      }}
                      title={`${t.url}${pinned === t.id ? " — pinned (click to follow the agent again)" : ""}`}
                      className="text-[10px] px-2 py-0.5 rounded border shrink-0 max-w-[14rem] truncate"
                      style={{
                        borderColor: on ? "var(--accent)" : "var(--border)",
                        color: on ? "var(--text-primary)" : "var(--text-secondary)",
                        background: on ? "var(--bg-secondary)" : "transparent",
                      }}
                    >
                      {pinned === t.id ? "📌 " : ""}
                      {shorten(t.title || t.url || "tab", 34)}
                    </button>
                  );
                })}
                {pinned && (
                  <button
                    onClick={() => {
                      setPinned(null);
                      send({ type: "browser_tab_select", target: null });
                    }}
                    className="text-[10px] px-2 py-0.5 rounded border shrink-0"
                    style={{ borderColor: "var(--border)", color: "var(--text-secondary)" }}
                    title="Follow whichever tab the agent is on"
                  >
                    follow agent
                  </button>
                )}
              </div>
            )}
            {/* The view's own state, not the page's. Without this a dead
                target just froze on its last frame and read as a hung page. */}
            {live && viewState !== "live" && (
              <div
                className="text-[10px] px-2 py-1"
                style={{
                  borderBottom: "1px solid var(--border)",
                  color: "var(--text-secondary)",
                  background: "var(--bg-secondary)",
                }}
              >
                {viewState === "attaching"
                  ? "◌ view detached — reattaching…"
                  : viewState === "detached"
                    ? "◌ no page open — the view will attach when one is"
                    : "⚠ view error — the frame below is the last one received"}
              </div>
            )}
            {shot ? (
              <div>
                {/* The keyboard target is an invisible TEXTAREA laid over the
                    frame, not the frame itself. `paste` only fires on an
                    editable element — WebKit (which is what the desktop app
                    runs) never fires it on a focusable <div>, so ⌘V arrived
                    as a keydown, was skipped in favour of a paste event that
                    could not come, and nothing happened. A textarea is
                    editable, so the native paste lands here and we forward
                    the text. `pointer-events: none` keeps clicks, drags and
                    the wheel going to the image below; focus is given
                    programmatically on pointerdown. Every keydown is
                    preventDefault'd, so nothing ever accumulates in it. */}
                <div className="relative">
                <textarea
                  ref={frameBoxRef}
                  tabIndex={takeover && live ? 0 : -1}
                  onKeyDown={onFrameKey}
                  onKeyUp={onFrameKey}
                  onPaste={onFramePaste}
                  onFocus={() => setFrameFocused(true)}
                  onBlur={() => setFrameFocused(false)}
                  aria-label="Browser takeover keyboard"
                  spellCheck={false}
                  autoComplete="off"
                  className="absolute inset-0 w-full h-full resize-none outline-none border-0 p-0 pointer-events-none"
                  // Transparent, NOT `opacity: 0`. ⌘V pastes into the chat
                  // input of this same app, so WebKit does handle the key
                  // equivalent without a native Edit menu — it just would not
                  // do it for a zero-opacity element, which it treats as
                  // nothing to paste into. Fully transparent ink on a
                  // transparent background is invisible to the eye and a
                  // normal visible editable element to the engine. Nothing is
                  // ever legible in it anyway: every keydown is
                  // preventDefault'd, so it stays empty.
                  style={{
                    caretColor: "transparent",
                    color: "transparent",
                    background: "transparent",
                  }}
                />
                <img
                  ref={shotImgRef}
                  src={shot.src}
                  alt="Latest browser screenshot"
                  className="w-full max-h-[45vh] object-contain select-none"
                  style={{
                    background: "#fff",
                    cursor: takeover ? "crosshair" : "default",
                    outline: takeover
                      ? frameFocused
                        ? "2px solid var(--accent)"
                        : "2px dashed var(--accent)"
                      : "none",
                    outlineOffset: -2,
                  }}
                  onPointerMove={onShotMouseMove}
                  onPointerDown={onShotPointerDown}
                  onPointerUp={onShotPointerUp}
                  onPointerCancel={onShotPointerUp}
                  draggable={false}
                />
                </div>
                <div
                  className="text-[10px] px-2 py-1 flex justify-between"
                  style={{ color: "var(--text-secondary)", borderTop: "1px solid var(--border)" }}
                >
                  <span>
                    {takeover
                      ? live
                        ? frameFocused
                          ? `● LIVE — keyboard goes to the page · Esc to release${pageUrl ? ` · ${shorten(pageUrl, 60)}` : ""}`
                          : `● LIVE — click the page to type into it${pageUrl ? ` · ${shorten(pageUrl, 60)}` : ""}`
                        : "takeover: click / scroll on the page, type below"
                      : "auto-captured after browser actions"}
                  </span>
                  <span>{shot.at}</span>
                </div>
              </div>
            ) : (
              <div className="p-3 text-xs" style={{ color: "var(--text-secondary)" }}>
                {shotErr
                  ? `Screenshot: ${shotErr}`
                  : browserUsed
                    ? "Capturing…"
                    : takeover
                      ? "Enter a URL below to start browsing."
                      : "The page preview appears here after the agent's first browser action."}
              </div>
            )}
            {takeover && (
              <div
                className="p-2 flex flex-col gap-1.5"
                style={{ borderTop: "1px solid var(--border)" }}
              >
                <div className="flex gap-1.5">
                  <button
                    onClick={() => sendInput("browser_navigate_back", {})}
                    className="text-[11px] px-2 rounded border"
                    style={{ borderColor: "var(--border)", color: "var(--text-secondary)" }}
                    title="Back"
                  >
                    ←
                  </button>
                  <input
                    value={urlInput}
                    onChange={(e) => setUrlInput(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && urlInput.trim()) {
                        const u = urlInput.trim();
                        sendInput("browser_navigate", {
                          url: /^[a-z]+:\/\//i.test(u) ? u : `https://${u}`,
                        });
                      }
                    }}
                    placeholder="Go to URL… (Enter)"
                    className="flex-1 min-w-0 text-[11px] px-2 py-1 rounded border outline-none font-mono"
                    style={{
                      borderColor: "var(--border)",
                      background: "var(--bg-secondary)",
                      color: "var(--text-primary)",
                    }}
                  />
                </div>
                <div className="flex gap-1.5 items-center">
                  <input
                    value={typeInput}
                    onChange={(e) => setTypeInput(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && typeInput) {
                        if (live) {
                          send({ type: "browser_cdp_input", kind: "text", args: { text: typeInput } });
                        } else {
                          sendInput("type_text", { text: typeInput });
                        }
                        setTypeInput("");
                      }
                    }}
                    placeholder="Type into the focused field… (Enter sends)"
                    className="flex-1 min-w-0 text-[11px] px-2 py-1 rounded border outline-none"
                    style={{
                      borderColor: "var(--border)",
                      background: "var(--bg-secondary)",
                      color: "var(--text-primary)",
                    }}
                  />
                  {["Enter", "Tab", "Escape", "Backspace"].map((k) => (
                    <button
                      key={k}
                      onClick={() =>
                        live
                          ? send({ type: "browser_cdp_input", kind: "key", args: { key: k } })
                          : sendInput("browser_press_key", { key: k })
                      }
                      className="text-[10px] px-1.5 py-1 rounded border font-mono"
                      style={{ borderColor: "var(--border)", color: "var(--text-secondary)" }}
                      title={`Press ${k}`}
                    >
                      {k === "Escape" ? "Esc" : k === "Backspace" ? "⌫" : k}
                    </button>
                  ))}
                  {/* macOS eats ⌘V before the web content sees it — the app
                      has no native Edit menu, so the key equivalent never
                      becomes a DOM keydown. This button takes the same path
                      the Ctrl-V backstop does and works today, whatever the
                      platform does with the chord. */}
                  {live && (
                    <button
                      onClick={() => {
                        navigator.clipboard
                          ?.readText()
                          .then((text) => {
                            if (text) {
                              send({
                                type: "browser_cdp_input",
                                kind: "text",
                                args: { text },
                              });
                            }
                          })
                          .catch(() =>
                            setInputErr("clipboard unavailable — paste into the box above instead"),
                          );
                      }}
                      className="text-[10px] px-1.5 py-1 rounded border"
                      style={{ borderColor: "var(--border)", color: "var(--text-secondary)" }}
                      title="Paste the clipboard into the page"
                    >
                      Paste
                    </button>
                  )}
                </div>
                {inputErr && (
                  <div className="text-[10px]" style={{ color: "#dc2626" }}>
                    {inputErr}
                  </div>
                )}
              </div>
            )}
          </div>
        )}

        <div className="text-xs font-semibold shrink-0" style={{ color: "var(--text-secondary)" }}>
          Activity {entries.length > 0 && `(${entries.length})`}
        </div>
        <div
          ref={listRef}
          className="flex-1 min-h-0 overflow-y-auto rounded-lg border p-2 font-mono text-[11px] leading-relaxed"
          style={{ borderColor: "var(--border)", background: "var(--bg-primary)" }}
        >
          {entries.length === 0 ? (
            <div className="p-2" style={{ color: "var(--text-secondary)" }}>
              No browser activity yet. Ask the agent (here in the sidebar →) something like
              “open example.com and summarize the page”.
            </div>
          ) : (
            entries.map((e) => (
              <div key={e.id} className="px-1 py-0.5 flex gap-2 items-baseline">
                <span style={{ color: "var(--text-secondary)" }}>{e.at}</span>
                <span
                  className="shrink-0"
                  style={{
                    color:
                      e.kind === "call"
                        ? "var(--accent)"
                        : e.kind === "console"
                          ? e.tool === "error"
                            ? "#dc2626"
                            : "#d97706"
                          : "var(--text-secondary)",
                  }}
                >
                  {e.kind === "call" ? "→" : e.kind === "console" ? "◆" : "←"}
                </span>
                <span className="shrink-0 font-semibold" style={{ color: "var(--text-primary)" }}>
                  {e.tool}
                </span>
                <span className="break-all" style={{ color: "var(--text-secondary)" }}>
                  {e.detail}
                </span>
              </div>
            ))
          )}
        </div>
      </div>

      {/* ── Chat sidebar — direct the agent without leaving the tab ── */}
      <div
        className="w-[320px] shrink-0 flex flex-col rounded-lg border overflow-hidden"
        style={{ borderColor: "var(--border)", background: "var(--bg-secondary)" }}
      >
        <div
          className="px-3 py-2 text-xs font-semibold flex items-center gap-2 shrink-0"
          style={{ color: "var(--text-primary)", borderBottom: "1px solid var(--border)" }}
        >
          Agent
          {busy && (
            <span className="text-[10px] font-normal" style={{ color: "var(--accent)" }}>
              ● working…
            </span>
          )}
        </div>
        <div ref={chatRef} className="flex-1 min-h-0 overflow-y-auto p-2 flex flex-col gap-2">
          {chat.length === 0 && (
            <div className="text-[11px] p-1 leading-relaxed" style={{ color: "var(--text-secondary)" }}>
              Same conversation as the Chat tab. Tell the agent what to do in the
              browser — “log in is done, take over and export the report”.
            </div>
          )}
          {chat.map((m) => (
            <div
              key={m.id}
              className={`rounded-md px-2 py-1.5 text-[12px] leading-relaxed break-words${
                m.role === "assistant" ? "" : " whitespace-pre-wrap"
              }`}
              style={
                m.role === "user"
                  ? { background: "var(--accent)", color: "white", alignSelf: "flex-end", maxWidth: "92%" }
                  : m.role === "system"
                    ? { background: "transparent", color: "var(--text-secondary)", alignSelf: "stretch", maxWidth: "100%", fontFamily: "ui-monospace, monospace", fontSize: 11, whiteSpace: "pre-wrap" }
                    : { background: "var(--bg-primary)", color: "var(--text-primary)", alignSelf: "flex-start", maxWidth: "92%", border: "1px solid var(--border)" }
              }
            >
              {/* Assistant replies go through markdown for the same reason
                  the Chat tab does it: a table or a **bold** run rendered
                  raw here reads as literal pipes and asterisks. User and
                  system lines stay verbatim — one is what the user typed,
                  the other is log output. */}
              {m.role === "assistant" ? <ChatMarkdown text={m.text} /> : m.text}
            </div>
          ))}
        </div>
        <div
          className="p-2 flex gap-1.5 shrink-0"
          style={{ borderTop: "1px solid var(--border)" }}
        >
          <input
            value={chatInput}
            onChange={(e) => setChatInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                sendChat();
              }
            }}
            placeholder={askPrompt ? "Answer the question above…" : "Direct the agent…"}
            className="flex-1 min-w-0 text-[12px] px-2 py-1.5 rounded border outline-none"
            style={{
              borderColor: "var(--border)",
              background: "var(--bg-primary)",
              color: "var(--text-primary)",
            }}
          />
          <button
            onClick={sendChat}
            className="text-[12px] px-2.5 rounded font-medium"
            style={{ background: "var(--accent)", color: "white" }}
          >
            →
          </button>
        </div>
      </div>
    </div>
  );
}
