//! The OpenRouter "Free only" toggle's persistence, split out of
//! `ModelPickerModal` so that file exports components only —
//! `react-refresh` cannot hot-reload a module that mixes the two, and
//! two components import these.

import { send, subscribe } from "../hooks/useIPC";

/// localStorage key for the OpenRouter-only "Free only" toggle.
/// Server-side `ProjectConfig.openrouterFreeOnly` is the source of
/// truth — localStorage is only a fast-paint cache so the toggle
/// renders in its last-known state before the `openrouter_free_only`
/// IPC reply lands. Setting the flag fires `openrouter_free_only_set`
/// so server-side `/models` and the post-key picker see it too.
const OPENROUTER_FREE_ONLY_KEY = "thclaws.openrouter.freeOnly";

export function isOpenRouterFreeOnly(): boolean {
  try {
    return localStorage.getItem(OPENROUTER_FREE_ONLY_KEY) === "1";
  } catch {
    return false;
  }
}

export function setOpenRouterFreeOnly(value: boolean) {
  try {
    if (value) localStorage.setItem(OPENROUTER_FREE_ONLY_KEY, "1");
    else localStorage.removeItem(OPENROUTER_FREE_ONLY_KEY);
  } catch {
    // localStorage write can fail in private mode; toggle just
    // doesn't persist. Acceptable.
  }
  send({ type: "openrouter_free_only_set", enabled: value });
}

/// Ask the server for the canonical flag value and update the
/// localStorage cache when the reply arrives. Returns the unsubscribe
/// function so callers can clean up.
export function refreshOpenRouterFreeOnly(onUpdate: (v: boolean) => void): () => void {
  const unsub = subscribe((msg) => {
    if (msg.type === "openrouter_free_only") {
      const enabled = Boolean((msg as { enabled?: boolean }).enabled);
      try {
        if (enabled) localStorage.setItem(OPENROUTER_FREE_ONLY_KEY, "1");
        else localStorage.removeItem(OPENROUTER_FREE_ONLY_KEY);
      } catch {
        // see setOpenRouterFreeOnly note
      }
      onUpdate(enabled);
    }
  });
  send({ type: "openrouter_free_only_get" });
  return unsub;
}
