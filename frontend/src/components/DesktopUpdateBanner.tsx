import { useEffect, useState } from "react";
import { send, subscribe } from "../hooks/useIPC";

/**
 * Org-enforced desktop updates (`desktop_update_status` frame). The backend
 * decides the level — from the org's version policy and the gateway's
 * `X-Thclaws-Update` / 426 answers — so this only renders it:
 *
 * - available: info, dismissible; the backend remembers the dismissal per
 *   release, so the next release shows again.
 * - warn: required by a date, dismissible for this session only.
 * - blocked: not dismissible.
 *
 * Asked for on mount and after every turn; pushed when the gateway says
 * something new.
 */
type Status = {
  level: "ok" | "available" | "warn" | "blocked";
  name: string;
  url?: string;
  version?: string;
  by_date?: string;
  key?: string;
  dismissed?: boolean;
};

let sessionDismissedWarn = false;

export function DesktopUpdateBanner() {
  const [status, setStatus] = useState<Status | null>(null);
  const [hidden, setHidden] = useState(false);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "desktop_update_status") {
        const level = msg.level as Status["level"];
        if (!level) return;
        setStatus(msg as unknown as Status);
        setHidden(
          level === "ok" ||
            (level === "available" && msg.dismissed === true) ||
            (level === "warn" && sessionDismissedWarn),
        );
      } else if (msg.type === "chat_done") {
        send({ type: "desktop_update_status" });
      }
    });
    send({ type: "desktop_update_status" });
    return unsub;
  }, []);

  if (!status || hidden || status.level === "ok") return null;

  const { level, name, url } = status;
  const text =
    level === "blocked"
      ? `This version of ${name} is no longer allowed`
      : level === "warn"
        ? status.by_date
          ? `An update to ${name} is required by ${status.by_date}`
          : `An update to ${name} is required`
        : `A new version of ${name} is available (${status.version})`;
  const linkLabel = level === "blocked" ? "Download the update" : "Download";
  const accent =
    level === "blocked"
      ? "var(--danger, #b91c1c)"
      : level === "warn"
        ? "var(--warning, #b45309)"
        : "var(--accent, #2563eb)";

  return (
    <div
      role={level === "available" ? "status" : "alert"}
      className="flex items-center justify-center gap-3 px-3 py-1.5 text-xs"
      style={{
        background: "var(--bg-secondary)",
        color: "var(--text-primary)",
        borderBottom: `2px solid ${accent}`,
      }}
    >
      <span className="truncate">{text}</span>
      {url && (
        <button
          type="button"
          className="rounded px-2 py-0.5 font-medium underline"
          style={{ color: accent }}
          // Through `open_external`, never an <a href>: a plain link
          // navigates the webview itself and strands the user.
          onClick={() => send({ type: "open_external", url })}
        >
          {linkLabel}
        </button>
      )}
      {level !== "blocked" && (
        <button
          type="button"
          className="rounded px-2 py-0.5 opacity-60 hover:opacity-100"
          aria-label="Dismiss update notice"
          onClick={() => {
            if (level === "available" && status.key) {
              send({ type: "desktop_update_dismiss", key: status.key });
            }
            if (level === "warn") sessionDismissedWarn = true;
            setHidden(true);
          }}
        >
          ✕
        </button>
      )}
    </div>
  );
}
