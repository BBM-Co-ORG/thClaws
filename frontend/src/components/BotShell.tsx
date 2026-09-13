import { Activity, useCallback, useEffect, useRef, useState } from "react";
import App from "../App";
import {
  basePath,
  botQuery,
  configureBots,
  send,
  setActiveBot,
  subscribe,
  subscribeAny,
  type IPCMessage,
} from "../hooks/useIPC";
import { BotRail, type BotStatus } from "./BotRail";
import { HostPanel } from "./HostPanel";

// dev-plan/59 §6.3: the workspace shell.
//
// A workspace host supervises one process per bot; this decides which bot the
// user is looking at. Every bot gets its own `<App/>`, and the ones you are
// not looking at are wrapped in React's `<Activity mode="hidden">`, which
// keeps their state and refs but DESTROYS THEIR EFFECTS. That is what makes
// per-bot trees safe: all 236 `send()` call sites route to the active bot's
// socket, and a hidden tree has no timers, no subscriptions and no sends to
// misroute. Switching back re-runs the effects over state that never went
// away — xterm buffers, scroll positions, half-typed drafts.
//
// When the backend is not a host (a v2 workspace, or the desktop's in-process
// bridge) this renders `<App/>` and nothing else, so that path is byte-for-
// byte what it was.

type BotRow = {
  slug: string;
  state?: string;
  name?: string;
};

const POLL_MS = 4000;
/** Rail width, mirrored into CSS so `App`'s fixed root can offset by it. */
const RAIL_W = "44px";
/** Remembered per workspace so a reload lands on the bot you were using. */
const LAST_BOT_KEY = "thclaws.lastBot";

async function fetchBots(): Promise<BotRow[] | null> {
  try {
    const res = await fetch(`${basePath()}bots${botQuery(null)}`);
    if (!res.ok) return null; // not a host — a plain `--serve`
    const body = (await res.json()) as { bots?: BotRow[] };
    return Array.isArray(body.bots) ? body.bots : null;
  } catch {
    return null;
  }
}

export function BotShell() {
  // The desktop talks over wry's IPC bridge rather than a socket. It may
  // still be a host — dev-plan/59 §7.6 step 4 points that bridge at a bot —
  // so the question is asked over the bridge instead of over HTTP.
  const isWry = typeof window !== "undefined" && !!window.ipc;
  // `undefined` = still deciding, `null` = not a host.
  const [bots, setBots] = useState<BotRow[] | null | undefined>(
    isWry ? null : undefined,
  );
  const [active, setActive] = useState<string | null>(null);
  const [flags, setFlags] = useState<
    Record<string, { busy?: boolean; unread?: boolean }>
  >({});
  const [hostOpen, setHostOpen] = useState(false);
  const [needsApproval, setNeedsApproval] = useState<string | null>(null);
  // Read by the `subscribeAny` listener, which is registered once and must
  // not re-subscribe on every switch.
  const activeRef = useRef<string | null>(null);
  useEffect(() => {
    activeRef.current = active;
  }, [active]);

  // Every refresh reconciles the sockets, not only the first: a bot added
  // from the panel had no socket until the page was reloaded, and a removed
  // one kept a dead socket reconnecting forever.
  const reload = useCallback(async () => {
    const rows = await fetchBots();
    setBots(rows);
    if (rows && rows.length > 0) {
      const cur = activeRef.current;
      const keep = cur && rows.some((r) => r.slug === cur) ? cur : rows[0].slug;
      configureBots(
        rows.map((r) => r.slug),
        keep,
        rows[0].slug,
      );
      if (keep !== cur) setActive(keep);
    }
    return rows;
  }, []);

  // Decide host vs single-bot once, then keep the list fresh for the rail.
  useEffect(() => {
    if (isWry) {
      // The window answers `bots_list` itself; a non-host desktop never
      // replies, which leaves `bots` null and renders the app alone. Asked
      // again on a timer so the rail's states track restarts and crashes.
      send({ type: "bots_list" });
      const t = window.setInterval(() => send({ type: "bots_list" }), POLL_MS);
      return () => window.clearInterval(t);
    }
    let stop = false;
    (async () => {
      const rows = await fetchBots();
      if (stop) return;
      setBots(rows);
      if (!rows || rows.length === 0) return;
      const remembered = (() => {
        try {
          return window.localStorage.getItem(LAST_BOT_KEY);
        } catch {
          return null;
        }
      })();
      const want =
        remembered && rows.some((r) => r.slug === remembered)
          ? remembered
          : rows[0].slug;
      configureBots(
        rows.map((r) => r.slug),
        want,
        rows[0].slug,
      );
      setActive(want);
    })();
    const t = window.setInterval(() => {
      if (!stop) void reload();
    }, POLL_MS);
    return () => {
      stop = true;
      window.clearInterval(t);
    };
  }, [isWry, reload]);

  // Desktop: the bot list and the active bot both arrive over the bridge.
  useEffect(() => {
    if (!isWry) return;
    return subscribe((msg: IPCMessage) => {
      if (msg.type === "bots_list_result" && Array.isArray(msg.bots)) {
        setBots(msg.bots as BotRow[]);
        if (typeof msg.active === "string" && msg.active) {
          setActive(msg.active as string);
        }
      } else if (msg.type === "bot_switched" && typeof msg.slug === "string") {
        setActive(msg.slug as string);
        setActiveBot(msg.slug as string);
      }
    });
  }, [isWry]);

  // The two signals a background bot may raise.
  useEffect(() => {
    return subscribeAny((slug: string | null, msg: IPCMessage) => {
      if (!slug || slug === activeRef.current) return;
      if (msg.type === "approval_request") {
        // A dot cannot say "blocked waiting for you"; this can.
        setNeedsApproval(slug);
        return;
      }
      if (msg.type === "chat_done") {
        setFlags((f) => ({ ...f, [slug]: { busy: false, unread: true } }));
      } else if (
        msg.type === "chat_text_delta" ||
        msg.type === "chat_tool_call"
      ) {
        setFlags((f) => ({ ...f, [slug]: { ...f[slug], busy: true } }));
      }
    });
  }, []);

  // The rail is `fixed`, and so is `App`'s root (deliberately — issue #168),
  // so the offset travels through CSS rather than through layout.
  useEffect(() => {
    const show = Array.isArray(bots) && bots.length > 0;
    document.documentElement.style.setProperty("--rail-w", show ? RAIL_W : "0px");
  }, [bots]);

  const select = useCallback((slug: string) => {
    if (typeof window !== "undefined" && window.ipc) {
      // One bridge, repointed by the window. The bot being left keeps its
      // process and its in-flight turn; it just stops streaming here.
      send({ type: "bot_switch", slug });
      setActive(slug);
      // The wry dispatch filters stamped frames by this too.
      setActiveBot(slug);
      return;
    }
    setActive(slug);
    setActiveBot(slug);
    setFlags((f) => ({ ...f, [slug]: { busy: false, unread: false } }));
    setNeedsApproval((n) => (n === slug ? null : n));
    try {
      window.localStorage.setItem(LAST_BOT_KEY, slug);
    } catch {
      /* private window — the rail just won't be remembered */
    }
  }, []);

  if (bots === undefined) return null; // one paint, not a flash of the wrong shell
  if (bots === null || bots.length === 0) return <App />;

  const rows: BotStatus[] = bots.map((b) => ({
    slug: b.slug,
    name: b.name || b.slug,
    state: b.state ?? "starting",
    busy: !!flags[b.slug]?.busy,
    unread: !!flags[b.slug]?.unread,
  }));

  return (
    <>
      <BotRail
        bots={rows}
        active={active}
        onSelect={select}
        onOpenHost={() => setHostOpen(true)}
      />
      {needsApproval && (
        <button
          onClick={() => select(needsApproval)}
          className="fixed top-2 left-1/2 -translate-x-1/2 z-50 px-3 py-1.5 rounded-md text-xs bg-[var(--warning)] text-[var(--bg-primary)] shadow-lg"
        >
          {needsApproval} is waiting for your approval — open it
        </button>
      )}
      {/* Each bot keeps its own tree. Hidden ones retain state and refs but
          run no effects, so nothing outside the focused bot can `send()`. */}
      {bots.map((b) => (
        <Activity key={b.slug} mode={b.slug === active ? "visible" : "hidden"}>
          <App />
        </Activity>
      ))}
      {hostOpen && (
        <HostPanel
          bots={rows}
          onClose={() => setHostOpen(false)}
          onChanged={isWry ? async () => send({ type: "bots_list" }) : reload}
        />
      )}
    </>
  );
}
