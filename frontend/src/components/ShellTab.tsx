import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { send, subscribe, type IPCMessage } from "../hooks/useIPC";
import { useTheme } from "../hooks/useTheme";

// PTY-backed Shell tab — spawns `$SHELL` (or fallback) under a real
// pseudo-tty in the Rust backend and pipes stdio through xterm.js.
// Distinct from:
//   - `Terminal` tab: agent-loop REPL rendered as ANSI by the backend
//   - `UI` tab: iframe-loaded installable GUI shells (dev-plan/33)
//
// IPC contract (see `crates/core/src/shell_pty.rs` + ipc.rs handlers):
//   send: pty_open {cols, rows} | pty_input {data: b64} | pty_resize {cols, rows} | pty_close
//   recv: pty_open_result {ok, cmd?, error?} | pty_data {data: b64} | pty_exit

interface Props {
  active: boolean;
}

// Palettes mirror TerminalView so the two tabs feel cohesive.
const PALETTES = {
  dark: {
    background: "#0a0a0a",
    foreground: "#e6e6e6",
    cursor: "#e6e6e6",
    selectionBackground: "#3a4858",
    selectionInactiveBackground: "#2a3440",
  },
  light: {
    background: "#fafafa",
    foreground: "#1a1a1a",
    cursor: "#1a1a1a",
    selectionBackground: "#b4d5fe",
    selectionInactiveBackground: "#d4e4fa",
  },
} as const;

function b64decode(s: string): Uint8Array {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function b64encode(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}

const utf8 = new TextEncoder();

export function ShellTab({ active }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const openedRef = useRef(false);
  const [status, setStatus] = useState<"opening" | "ready" | "exited" | "error">("opening");
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const { resolved: themeMode } = useTheme();

  // Mount xterm + open the PTY exactly once. `active` is intentionally
  // not a dep — we want a single long-lived session that survives tab
  // switches, the same way TerminalView keeps state across switches.
  useEffect(() => {
    if (!ref.current) return;
    const term = new Terminal({
      fontFamily: 'Menlo, Monaco, "Courier New", monospace',
      fontSize: 12,
      cursorBlink: true,
      convertEol: false,
      theme: PALETTES[themeMode === "light" ? "light" : "dark"],
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(ref.current);
    fit.fit();
    termRef.current = term;
    fitRef.current = fit;

    // Open the PTY with whatever xterm sized itself to. The backend
    // picks `$SHELL` (fallback /bin/sh, or powershell.exe on Windows)
    // when no `cmd` is supplied.
    const cols = term.cols || 80;
    const rows = term.rows || 24;
    send({ type: "pty_open", cols, rows });
    openedRef.current = true;

    const onData = term.onData((data) => {
      const bytes = utf8.encode(data);
      send({ type: "pty_input", data: b64encode(bytes) });
    });

    // ResizeObserver instead of window resize: the chat-tab sidebar
    // can grow/shrink without a window event, and we want the PTY's
    // TIOCSWINSZ to follow.
    let lastCols = term.cols;
    let lastRows = term.rows;
    const ro = new ResizeObserver(() => {
      try {
        fit.fit();
      } catch {
        // xterm throws if the container is 0x0 (tab not visible);
        // ignore — we'll refit when the tab becomes active.
      }
      const c = term.cols || 80;
      const r = term.rows || 24;
      if (c !== lastCols || r !== lastRows) {
        lastCols = c;
        lastRows = r;
        send({ type: "pty_resize", cols: c, rows: r });
      }
    });
    ro.observe(ref.current);

    const unsub = subscribe((msg: IPCMessage) => {
      if (msg?.type === "pty_open_result") {
        if (msg.ok) {
          setStatus("ready");
        } else {
          setStatus("error");
          setErrorMsg(String(msg.error || "spawn failed"));
        }
        return;
      }
      if (msg?.type === "pty_data" && typeof msg.data === "string") {
        const bytes = b64decode(msg.data);
        term.write(bytes);
        return;
      }
      if (msg?.type === "pty_exit") {
        setStatus("exited");
        term.write("\r\n\x1b[2m[shell exited]\x1b[0m\r\n");
      }
    });

    return () => {
      onData.dispose();
      ro.disconnect();
      unsub();
      send({ type: "pty_close" });
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
      openedRef.current = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Re-fit + refocus when becoming active (xterm's fit is no-op when
  // the container is hidden, so we deferred it until now).
  //
  // The focus() is deferred one frame via requestAnimationFrame: this
  // panel transitions from `visibility:hidden` to visible (see the tab
  // wrapper in App.tsx) in the same commit that flips `active`, and a
  // just-unhidden element isn't reliably focusable in the same frame —
  // under the wry/Chromium webview the synchronous focus() is silently
  // dropped, so keystrokes go nowhere until the user clicks (issue #166).
  // Focusing on the next frame, once layout/visibility have settled,
  // lets the terminal accept input immediately on tab switch.
  useEffect(() => {
    if (!active || !termRef.current || !fitRef.current) return;
    try {
      fitRef.current.fit();
    } catch {
      // ignore
    }
    const raf = requestAnimationFrame(() => termRef.current?.focus());
    return () => cancelAnimationFrame(raf);
  }, [active]);

  // Theme switch — repaint xterm without rebuilding it.
  useEffect(() => {
    if (!termRef.current) return;
    termRef.current.options.theme = PALETTES[themeMode === "light" ? "light" : "dark"];
  }, [themeMode]);

  return (
    <div className="w-full h-full flex flex-col" style={{ background: "var(--bg-primary)" }}>
      {status === "error" && errorMsg && (
        <div
          className="text-xs px-3 py-1.5 border-b"
          style={{
            background: "var(--bg-secondary)",
            borderColor: "var(--border)",
            color: "var(--text-secondary)",
          }}
        >
          shell: {errorMsg}
        </div>
      )}
      {/* Explicit click-to-focus as a belt-and-suspenders alongside the
          deferred programmatic focus above: a physical click reliably
          hands keyboard capture to xterm in the wry webview (issue #166).
          Focusing an already-focused terminal is a no-op. */}
      <div ref={ref} className="flex-1 min-h-0" onClick={() => termRef.current?.focus()} />
    </div>
  );
}
