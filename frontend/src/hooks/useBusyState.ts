/**
 * useBusyState — engine-driven "is the agent doing work?" state.
 *
 * Sourced from the engine's `agent_activity` module (dev-plan/36).
 * On initial connect we send `gui_busy_query` to catch the state that
 * existed BEFORE the WS opened (covers the cold-reopen-mid-batch case
 * that motivated the whole feature). Subsequent flips come in as
 * `gui_busy_changed` events broadcast at the start + end of every
 * user-facing turn.
 *
 * Side-channel turns (auto-learn ingest, reconcile) increment the
 * engine's busy counter (so the cloud heartbeat keeps pinging) but
 * do NOT fire `gui_busy_changed` and don't appear here. The UI
 * surface tracks the user-facing turn, not the engine's internals.
 */
import { useEffect, useState } from "react";
import { send, subscribe, type IPCMessage } from "./useIPC";

export type BusyState = {
  busy: boolean;
  sessionId: string | null;
  startedAtMs: number | null;
  lastProgress: string | null;
};

const INITIAL: BusyState = {
  busy: false,
  sessionId: null,
  startedAtMs: null,
  lastProgress: null,
};

let queryIdSeq = 7_000;

function applyMsg(msg: IPCMessage): BusyState {
  return {
    busy: !!msg.busy,
    sessionId: typeof msg.sessionId === "string" ? msg.sessionId : null,
    startedAtMs: typeof msg.startedAtMs === "number" ? msg.startedAtMs : null,
    lastProgress:
      typeof msg.lastProgress === "string" ? msg.lastProgress : null,
  };
}

export function useBusyState(): BusyState {
  const [state, setState] = useState<BusyState>(INITIAL);

  useEffect(() => {
    const unsub = subscribe((msg: IPCMessage) => {
      if (
        msg?.type === "gui_busy_changed" ||
        msg?.type === "gui_busy_result"
      ) {
        setState(applyMsg(msg));
      } else if (msg?.type === "ws_status" && msg.status === "connected") {
        // Catch the case where the agent was already busy before the
        // WS opened — no `gui_busy_changed` will fire for that one
        // since the transition happened pre-connect.
        send({ type: "gui_busy_query", id: ++queryIdSeq });
      }
    });
    // Initial query for the wry transport (which doesn't emit
    // ws_status events) and to cover the case where the WS reported
    // "connected" before this hook subscribed.
    send({ type: "gui_busy_query", id: ++queryIdSeq });
    return unsub;
  }, []);

  return state;
}
