import { useEffect, useSyncExternalStore } from "react";
import { send, subscribe } from "./useIPC";

/**
 * Product branding from the backend (`branding` frame): the org policy's
 * name / banner and, on a per-customer build, the customer's logo baked into
 * the binary. Open-core builds get name "thClaws" and no logo, so every
 * consumer renders exactly what it always did.
 *
 * The last frame is cached at module level: BotShell remounts App per bot,
 * and a remount must not flash the stock name before the next frame lands.
 * Each mount also asks (`branding_get`) — a frame sent before this
 * subscriber existed is gone.
 */
export type Branding = {
  name: string;
  support_email?: string;
  about?: string;
  banner?: string | null;
  logo?: string | null;
  logo_dark?: string | null;
  customer_build?: boolean;
  pubkey_fingerprint?: string | null;
  /** Set on an org gateway desktop: sign in here instead of adding API keys. */
  org_cloud_url?: string | null;
  policy?: {
    issuer: string;
    issued_at: string;
    expires_at?: string | null;
    key_source: string;
  } | null;
};

const DEFAULT: Branding = { name: "thClaws" };
let cached: Branding = DEFAULT;
const listeners = new Set<() => void>();

subscribe((msg) => {
  if (msg.type !== "branding") return;
  const next: Branding = {
    ...DEFAULT,
    ...(msg as unknown as Branding),
    name: (typeof msg.name === "string" && msg.name.trim()) || DEFAULT.name,
  };
  cached = next;
  listeners.forEach((l) => l());
});

export function currentBranding(): Branding {
  return cached;
}

function subscribeBranding(onChange: () => void): () => void {
  listeners.add(onChange);
  return () => {
    listeners.delete(onChange);
  };
}

export function useBranding(): Branding {
  const b = useSyncExternalStore(subscribeBranding, currentBranding);
  useEffect(() => {
    send({ type: "branding_get" });
  }, []);
  return b;
}
