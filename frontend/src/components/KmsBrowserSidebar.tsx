import { useEffect, useRef, useState } from "react";
import {
  ChevronRight,
  X,
  BookOpen,
  FileText,
  Link2,
  Network,
  Plus,
  Receipt,
  Search,
  ShieldCheck,
  Trash2,
} from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import { KmsCreateModal, type KmsCreateMode } from "./KmsCreateModal";
import { KmsRunConfirm } from "./KmsRunConfirm";
import { duration, money, type ModeCost } from "./kmsRunCost";
import {
  DEFAULT_INGEST_MODE,
  INGEST_MODES,
  type IngestMode,
} from "./kmsIngestModes";

/// M6.39.9: right-edge KMS browser. Activated by clicking a KMS row's
/// title in the left sidebar. Lists `pages/*.md` and `sources/*.md`
/// files; clicking an entry opens [`KmsViewerOverlay`] over the main
/// pane. Mirrors the layout style of `ResearchSidebar` /
/// `TodoSidebar` — fixed-width right column, dismiss/restore via
/// chevron tab.
///
/// State protocol:
///   parent passes `kmsName` (the KMS being browsed) + `onClose`
///   (clears parent's `browsingKms` state) + `onOpenFile` (parent
///   tracks the file the viewer overlay should display).
///
/// The component subscribes to `kms_browse_result` envelopes for
/// the matching `kmsName` and re-renders when the listing arrives.
/// On mount or `kmsName` change, it sends `kms_browse` to fetch a
/// fresh listing.

type BrowseFile = {
  name: string;
  bytes: number;
  /// On-disk extension. Always "md" for pages; sources carry whatever
  /// they were ingested as, shown as a badge so a `.json` archive is
  /// distinguishable from a `.md` one at a glance. Optional so an
  /// older backend that doesn't send it still renders.
  ext?: string;
  /// What a person calls it: a page's `title:`, a source's catalogued
  /// title. Absent when there is none — the row then shows `name`.
  title?: string;
  /// `researching` | `derived` | `failed` when the page is not a finished
  /// note; absent otherwise.
  status?: string;
};

/// The name a reader looks for.
const labelOf = (f: BrowseFile) => f.title || f.name;

/// Titles are what is read, so titles are what is sorted — with Thai
/// collation, which is not code-point order (leading vowels sort after the
/// consonant they are written before), and digits as numbers.
const byLabel = (a: BrowseFile, b: BrowseFile) =>
  labelOf(a).localeCompare(labelOf(b), ["th", "en"], {
    numeric: true,
    sensitivity: "base",
  });

type FileKind = "page" | "source" | "run";

/// One row of the provenance ledger, as `kms_runs_result` sends it —
/// read from the run log's own frontmatter (dev-plan/64 P5.6).
type RunEntry = {
  /// File stem; `{kind: "run", name}` opens it in the viewer.
  name: string;
  /// "research" | "verify", or "" for a log this build doesn't know.
  kind: string;
  date: string;
  /// The research query. Absent on a verify run.
  title?: string;
  bytes: number;
  cost_usd?: number;
  claims?: number;
  findings?: number;
  elapsed_secs?: number;
  llm_calls?: number;
};

/// One structural issue, as `kms_lint_result` sends it (dev-plan/64 P5.6).
type LintFinding = {
  kind: string;
  detail: string;
  /// "page" | "source", or "" when there is nothing to open.
  target_kind: string;
  target: string;
};

/// One kept version, as `kms_trash_result` sends it (dev-plan/64 P5.6).
type TrashRow = {
  entry: string;
  /// The stamp rendered for a reader.
  when: string;
  /// What put it here: "delete" | "overwrite" | "drop".
  why: string;
  kind: string;
  /// File stem — what a restore takes.
  name: string;
  bytes: number;
};

const runLabel = (r: RunEntry) =>
  r.title || (r.kind === "verify" ? "Verify" : r.name);


/// One row of a content search, as `kms_search_result` sends it.
type SearchHit = {
  kind: FileKind;
  /// What the viewer opens (page stem, or a source's stem).
  name: string;
  /// The file on disk, for display.
  file: string;
  title: string;
  /// The matching line, windowed around the match.
  snippet: string;
};

export type ViewerTarget = {
  kms: string;
  kind: FileKind;
  name: string;
  /// Verbatim text to mark and scroll to once the file is open
  /// (dev-plan/64 P5.1: a citation opens its source at the passage it
  /// cites). Absent for an ordinary open.
  highlight?: string;
};

interface Props {
  kmsName: string;
  onClose: () => void;
  onOpenFile: (target: ViewerTarget) => void;
  onOpenGraph: (kms: string) => void;
  graphActive: boolean;
  /// The file currently open in the viewer overlay. When this row
  /// appears in the listing, it gets accent styling so the user can
  /// see at a glance which entry corresponds to what's on screen.
  /// Null when no file is open.
  selected: ViewerTarget | null;
}

const KMS_SIDEBAR_WIDTH_KEY = "thclaws_kms_sidebar_width";
const KMS_SIDEBAR_WIDTH_MIN = 200;
// Page titles here are whole Thai sentences, not slugs, so the useful
// ceiling is well past the main sidebar's 480.
const KMS_SIDEBAR_WIDTH_MAX = 720;
const KMS_SIDEBAR_WIDTH_DEFAULT = 260;

export function KmsBrowserSidebar({
  kmsName,
  onClose,
  onOpenFile,
  onOpenGraph,
  graphActive,
  selected,
}: Props) {
  /// Compare a file row against the active viewer target. Limited to
  /// rows in the currently-browsed KMS — opening a file from KMS-A
  /// while browsing KMS-B should NOT highlight a same-named entry in
  /// KMS-B.
  const isSelected = (kind: FileKind, name: string) =>
    selected !== null &&
    selected.kms === kmsName &&
    selected.kind === kind &&
    selected.name === name;
  // A KMS opens on its entry page — the topic page for a research
  // vault, the most linked-to page otherwise (the engine decides; see
  // `kms::entry_page`). Once per open: a later `kms_browse_result`,
  // fired when a research job finishes or a page is renamed, must not
  // yank the reader off whatever they are reading.
  const autoOpened = useRef(false);
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  const [pages, setPages] = useState<BrowseFile[] | null>(null);
  const [sources, setSources] = useState<BrowseFile[]>([]);
  /// The provenance ledger. Collapsed by default: it is what you open
  /// when you want to know where a page came from or what a run cost,
  /// not something to scroll past on the way to a page.
  const [runs, setRuns] = useState<RunEntry[]>([]);
  const [runsOpen, setRunsOpen] = useState(false);
  /// What the trash is holding. Collapsed like Runs — it matters on the
  /// day something is lost, and never otherwise.
  const [trash, setTrash] = useState<TrashRow[]>([]);
  const [trashOpen, setTrashOpen] = useState(false);
  const [keepDays, setKeepDays] = useState(30);
  /// The page a restore is in flight for, and the last thing a restore
  /// said — success or refusal, shown in place rather than swallowed.
  const [restoring, setRestoring] = useState<string | null>(null);
  const [restoreNote, setRestoreNote] = useState<{
    ok: boolean;
    text: string;
  } | null>(null);
  /// The structural audit. `null` = not run yet, which is different
  /// from "run and found nothing" — the section says which.
  const [lint, setLint] = useState<LintFinding[] | null>(null);
  const [linting, setLinting] = useState(false);
  const [lintError, setLintError] = useState<string | null>(null);
  const [auditOpen, setAuditOpen] = useState(false);
  /// What runs of each kind have cost in this knowledge base, for
  /// pricing a click before it is made (dev-plan/64 P5.4).
  const [modes, setModes] = useState<Record<string, ModeCost>>({});
  /// The page a Refresh is waiting on confirmation for.
  const [confirmRefresh, setConfirmRefresh] = useState<string | null>(null);
  /// Placeholders a dead research run left behind, cleared on the last
  /// browse (dev-plan/64 P5.5). Shown rather than done silently: a page
  /// disappearing on its own needs to say why.
  const [reconciled, setReconciled] = useState<string[]>([]);
  /// dev-plan/64 P5.3: a URL or a path to take into this knowledge
  /// base. The Files tab could add a `.md` file and nothing else, and
  /// a researcher's real inputs are PDFs and web pages.
  const [ingestWhat, setIngestWhat] = useState("");
  const [ingesting, setIngesting] = useState(false);
  /// What to do with it once archived (dev-plan/64 P4.9). Sticky for
  /// the session: someone taking a folder of papers in one at a time
  /// should not re-choose every time.
  const [ingestMode, setIngestMode] =
    useState<IngestMode>(DEFAULT_INGEST_MODE);
  const [ingestNote, setIngestNote] = useState<{
    ok: boolean;
    text: string;
  } | null>(null);
  /// Name filter. A KMS of any real size is unbrowsable by scrolling,
  /// and the sidebar had no way to narrow the list at all.
  const [filter, setFilter] = useState("");
  /// What the engine found *inside* pages and sources for `filter`. The
  /// box used to match file names only — and a research-built vault has
  /// English slugs over Thai pages, so a Thai reader typing Thai found
  /// nothing, with ranked full-text search sitting behind a slash command.
  /// `null` = no search has been asked for.
  const [hits, setHits] = useState<SearchHit[] | null>(null);
  const [searching, setSearching] = useState(false);
  const [searchNote, setSearchNote] = useState<string | null>(null);
  const searchSeq = useRef(0);
  const [error, setError] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState(false);
  // Persisted, user-resizable width — same gesture as the main
  // sidebar, mirrored: this column is on the right, so the gutter is
  // on its left edge and the width grows as the pointer moves left.
  const [width, setWidth] = useState<number>(() => {
    if (typeof window === "undefined") return KMS_SIDEBAR_WIDTH_DEFAULT;
    const n = Number(localStorage.getItem(KMS_SIDEBAR_WIDTH_KEY));
    if (
      !Number.isFinite(n) ||
      n < KMS_SIDEBAR_WIDTH_MIN ||
      n > KMS_SIDEBAR_WIDTH_MAX
    ) {
      return KMS_SIDEBAR_WIDTH_DEFAULT;
    }
    return Math.round(n);
  });
  const [resizing, setResizing] = useState(false);
  useEffect(() => {
    if (!resizing) return;
    const onMove = (e: MouseEvent) => {
      setWidth(
        Math.max(
          KMS_SIDEBAR_WIDTH_MIN,
          Math.min(
            KMS_SIDEBAR_WIDTH_MAX,
            Math.round(window.innerWidth - e.clientX),
          ),
        ),
      );
    };
    const onUp = () => setResizing(false);
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, [resizing]);
  useEffect(() => {
    if (typeof window !== "undefined") {
      localStorage.setItem(KMS_SIDEBAR_WIDTH_KEY, String(width));
    }
  }, [width]);
  // Page create / rename / delete modal (null = closed). The backend
  // re-emits kms_browse_result on success, so the existing subscription
  // refreshes the list.
  const [modal, setModal] = useState<KmsCreateMode | null>(null);
  // Right-click context menu on a page row (null = closed). Anchored to
  // the cursor; its right edge pins to the click x so it never spills
  // off the right-edge panel.
  const [pageMenu, setPageMenu] = useState<{
    name: string;
    x: number;
    y: number;
  } | null>(null);

  useEffect(() => {
    setPages(null);
    setSources([]);
    setRuns([]);
    setTrash([]);
    setModes({});
    setReconciled([]);
    setIngestWhat("");
    setIngesting(false);
    setIngestNote(null);
    setRestoreNote(null);
    setRestoring(null);
    setLint(null);
    setLintError(null);
    setLinting(false);
    setError(null);
    setDismissed(false);
    autoOpened.current = false;
    const unsub = subscribe((msg) => {
      if (
        msg.type === "kms_ingest_result" &&
        (msg.kms as string) === kmsName &&
        msg.id === "sidebar"
      ) {
        setIngesting(false);
        if (msg.ok) {
          setIngestWhat("");
          const bulk = msg.bulk as string | null | undefined;
          // Say what happens next, because three of the four modes
          // leave work running somewhere the reader is not looking.
          const next =
            msg.research === "pending"
              ? " Writing it up now — see the Research sidebar."
              : msg.research_error
                ? ` Could not start the write-up: ${String(msg.research_error)}`
                : msg.summarize_prompt
                  ? " Writing it up in the chat."
                  : "";
          setIngestNote({
            ok: true,
            text: bulk
              ? `Added ${bulk}.`
              : `Added “${String(msg.alias ?? "")}”.${
                  msg.overwrote ? " It replaced what was there." : ""
                }${next}`,
          });
          // The stub is bare until something writes it up; hand the
          // engine's own prompt to the agent, exactly as the Files tab
          // does, rather than leaving a source nobody has read.
          const prompt = String(msg.summarize_prompt ?? "");
          if (prompt) send({ type: "shell_input", text: prompt });
        } else {
          setIngestNote({
            ok: false,
            text: (msg.error as string) ?? "ingest failed",
          });
        }
      } else if (
        msg.type === "kms_reconciled" &&
        (msg.kms as string) === kmsName
      ) {
        setReconciled((msg.cleared as string[]) ?? []);
      } else if (msg.type === "kms_runs_result" && (msg.kms as string) === kmsName) {
        setRuns(msg.ok ? ((msg.runs as RunEntry[]) ?? []) : []);
      } else if (
        msg.type === "kms_cost_result" &&
        (msg.kms as string) === kmsName
      ) {
        setModes((msg.modes as Record<string, ModeCost>) ?? {});
      } else if (
        msg.type === "kms_trash_result" &&
        (msg.kms as string) === kmsName
      ) {
        setTrash(msg.ok ? ((msg.items as TrashRow[]) ?? []) : []);
        if (typeof msg.keep_days === "number") setKeepDays(msg.keep_days);
      } else if (
        msg.type === "kms_lint_result" &&
        (msg.kms as string) === kmsName
      ) {
        setLinting(false);
        setLint(msg.ok ? ((msg.findings as LintFinding[]) ?? []) : []);
        setLintError(msg.ok ? null : ((msg.error as string) ?? "lint failed"));
      } else if (
        msg.type === "kms_restore_result" &&
        (msg.kms as string) === kmsName
      ) {
        setRestoring(null);
        setRestoreNote({
          ok: msg.ok === true,
          text: ((msg.ok ? msg.note : msg.error) as string) ?? "",
        });
      } else if (
        msg.type === "kms_browse_result" &&
        (msg.kms as string) === kmsName
      ) {
        if (msg.ok) {
          setPages((msg.pages as BrowseFile[]) ?? []);
          setSources((msg.sources as BrowseFile[]) ?? []);
          setError(null);
          const entry = typeof msg.entry === "string" ? msg.entry : null;
          if (entry && !autoOpened.current && selectedRef.current === null) {
            autoOpened.current = true;
            onOpenFile({ kms: kmsName, kind: "page", name: entry });
          }
        } else {
          setError((msg.error as string) ?? "browse failed");
          setPages([]);
        }
      } else if (msg.type === "kms_update") {
        // Backend fires this when a research job finishes (and when
        // any KMS is created / activated / deactivated). The envelope
        // carries the KMS list, not page-level deltas, so we don't
        // know whether OUR kms gained pages — just re-fetch
        // unconditionally. Browse is cheap (reads a directory) and
        // this only fires on real changes. Without this, a research
        // run finishes, the agent has clearly written pages (the LLM
        // can reference them), but this sidebar keeps showing the
        // stale page list from the moment it was opened.
        send({ type: "kms_browse", name: kmsName });
        send({ type: "kms_runs", name: kmsName });
        send({ type: "kms_trash", name: kmsName });
        send({ type: "kms_cost", name: kmsName });
      }
    });
    send({ type: "kms_browse", name: kmsName });
    send({ type: "kms_runs", name: kmsName });
    send({ type: "kms_trash", name: kmsName });
    send({ type: "kms_cost", name: kmsName });
    return unsub;
  }, [kmsName]);

  // Content search: debounced, and each request numbered so a slow reply
  // to an earlier query cannot overwrite the results of the current one.
  useEffect(() => {
    const query = filter.trim();
    if ([...query].length < 2) {
      setHits(null);
      setSearching(false);
      setSearchNote(null);
      return;
    }
    const id = ++searchSeq.current;
    setSearching(true);
    const unsub = subscribe((msg) => {
      if (msg.type !== "kms_search_result" || msg.id !== id) return;
      setSearching(false);
      if (msg.ok) {
        setHits((msg.hits as SearchHit[]) ?? []);
        setSearchNote(typeof msg.note === "string" ? msg.note : null);
      } else {
        setHits([]);
        setSearchNote((msg.error as string) ?? "search failed");
      }
    });
    const timer = setTimeout(
      () => send({ type: "kms_search", kms: kmsName, query, id }),
      250,
    );
    return () => {
      clearTimeout(timer);
      unsub();
    };
  }, [filter, kmsName]);

  const needle = filter.trim().toLowerCase();
  const matches = (f: BrowseFile) =>
    needle === "" ||
    f.name.toLowerCase().includes(needle) ||
    (f.title ?? "").toLowerCase().includes(needle) ||
    (f.ext ?? "").toLowerCase().includes(needle);
  const shownPages = (pages ?? []).filter(matches).sort(byLabel);
  const shownSources = sources.filter(matches).sort(byLabel);
  const shownRuns = runs.filter(
    (r) =>
      needle === "" ||
      runLabel(r).toLowerCase().includes(needle) ||
      r.name.toLowerCase().includes(needle),
  );
  // What this vault has cost in LLM calls, as far as the logs record it.
  // A run written before its writer stamped a cost contributes nothing
  // and is not counted, so the total is a floor — say so in the tooltip.
  const priced = runs.filter((r) => typeof r.cost_usd === "number");
  const totalCost = priced.reduce((sum, r) => sum + (r.cost_usd ?? 0), 0);
  const startIngest = () => {
    const what = ingestWhat.trim();
    if (!what || ingesting) return;
    setIngestNote(null);
    setIngesting(true);
    send({
      type: "kms_ingest",
      id: "sidebar",
      path: what,
      kms: kmsName,
      mode: ingestMode,
    });
  };
  const shownTrash = trash.filter(
    (t) => needle === "" || t.name.toLowerCase().includes(needle),
  );

  if (dismissed) {
    return (
      <button
        type="button"
        onClick={() => setDismissed(false)}
        className="flex items-center justify-center shrink-0 border-l"
        style={{
          width: "20px",
          background: "var(--bg-secondary)",
          borderColor: "var(--border)",
          color: "var(--text-secondary)",
          cursor: "pointer",
        }}
        title={`Browse KMS: ${kmsName}`}
      >
        <ChevronRight size={14} style={{ transform: "rotate(180deg)" }} />
      </button>
    );
  }

  return (
    <div
      className="flex flex-col shrink-0 border-l"
      style={{
        width,
        position: "relative",
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
        cursor: resizing ? "col-resize" : undefined,
      }}
    >
      {/* Drag handle — thin gutter on the LEFT edge, mirroring the main
          sidebar's right-edge one. Double-click resets. */}
      <div
        onMouseDown={(e) => {
          e.preventDefault();
          setResizing(true);
        }}
        onDoubleClick={() => setWidth(KMS_SIDEBAR_WIDTH_DEFAULT)}
        title="Drag to resize · double-click to reset"
        style={{
          position: "absolute",
          left: 0,
          top: 0,
          bottom: 0,
          width: 4,
          zIndex: 30,
          cursor: "col-resize",
          background: resizing ? "var(--accent)" : "transparent",
          transition: resizing ? undefined : "background 0.15s",
        }}
        onMouseEnter={(e) => {
          if (!resizing) {
            (e.currentTarget as HTMLDivElement).style.background =
              "var(--border-strong, var(--border))";
          }
        }}
        onMouseLeave={(e) => {
          if (!resizing) {
            (e.currentTarget as HTMLDivElement).style.background = "transparent";
          }
        }}
      />
      <div
        className="flex items-center justify-between px-3 py-2 border-b shrink-0"
        style={{ borderColor: "var(--border)" }}
      >
        <div
          className="text-[10px] uppercase tracking-wider flex items-center gap-2 truncate"
          style={{ color: "var(--text-secondary)" }}
        >
          <BookOpen size={11} />
          <span className="truncate" title={kmsName}>
            KMS: {kmsName}
          </span>
        </div>
        <div className="flex items-center gap-1 shrink-0">
          <button
            type="button"
            onClick={() => setModal({ kind: "page", kms: kmsName })}
            className="p-0.5 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="New blank page in this KMS"
          >
            <Plus size={14} />
          </button>
          <button
            type="button"
            onClick={() => setDismissed(true)}
            className="p-0.5 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="Hide (chevron tab restores)"
          >
            <ChevronRight size={14} />
          </button>
          <button
            type="button"
            onClick={onClose}
            className="p-0.5 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="Close browser"
          >
            <X size={14} />
          </button>
        </div>
      </div>

      <div className="px-2 py-1.5 border-b" style={{ borderColor: "var(--border)" }}>
        <div className="relative">
          <Search
            size={11}
            className="absolute left-2 top-1/2 -translate-y-1/2 pointer-events-none"
            style={{ color: "var(--text-secondary)" }}
          />
          <input
            type="text"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setFilter("");
            }}
            placeholder="Search this knowledge base…"
            spellCheck={false}
            className="w-full rounded pl-6 pr-2 py-1 text-xs outline-none"
            style={{
              background: "var(--bg-secondary, rgba(255,255,255,0.05))",
              color: "var(--text-primary)",
              border: "1px solid var(--border)",
            }}
          />
        </div>
        <div className="flex gap-1 mt-1">
          <input
            type="text"
            value={ingestWhat}
            onChange={(e) => setIngestWhat(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setIngestWhat("");
              if (e.key === "Enter" && ingestWhat.trim() && !ingesting) {
                startIngest();
              }
            }}
            placeholder="Add a URL, file or folder…"
            spellCheck={false}
            className="flex-1 min-w-0 rounded px-2 py-1 text-xs outline-none"
            style={{
              background: "var(--bg-secondary, rgba(255,255,255,0.05))",
              color: "var(--text-primary)",
              border: "1px solid var(--border)",
            }}
            title="A web page, a PDF, a .md/.txt/.html/.csv file, or a folder of them. The source is archived here and written up."
          />
          <button
            type="button"
            onClick={startIngest}
            disabled={!ingestWhat.trim() || ingesting}
            className="shrink-0 rounded px-2 py-1"
            style={{
              border: "1px solid var(--border)",
              fontSize: "11px",
              background: "transparent",
              color: "var(--accent, #61afef)",
              cursor: !ingestWhat.trim() || ingesting ? "default" : "pointer",
              opacity: !ingestWhat.trim() || ingesting ? 0.5 : 1,
            }}
          >
            {ingesting ? "Adding…" : "Add"}
          </button>
        </div>
        <select
          value={ingestMode}
          onChange={(e) => setIngestMode(e.target.value as IngestMode)}
          className="mt-1 w-full rounded px-1 py-0.5 outline-none"
          style={{
            background: "var(--bg-secondary, rgba(255,255,255,0.05))",
            color: "var(--text-secondary)",
            border: "1px solid var(--border)",
            fontSize: "10px",
          }}
          title={INGEST_MODES.find((m) => m.id === ingestMode)?.hint}
        >
          {INGEST_MODES.map((m) => (
            <option key={m.id} value={m.id}>
              {m.label}
            </option>
          ))}
        </select>
        <div
          className="mt-1"
          style={{ fontSize: "10px", color: "var(--text-secondary)" }}
        >
          {INGEST_MODES.find((m) => m.id === ingestMode)?.hint}
        </div>
        {ingestNote && (
          <div
            className="mt-1 text-xs"
            style={{
              color: ingestNote.ok
                ? "var(--success, #98c379)"
                : "var(--danger, #e06c75)",
            }}
          >
            {ingestNote.text}
          </div>
        )}
      </div>

      <div className="flex-1 overflow-auto">
        {error && (
          <div
            className="px-3 py-3 text-xs"
            style={{ color: "var(--danger, #e06c75)" }}
          >
            {error}
          </div>
        )}
        {pages === null && !error && (
          <div
            className="px-3 py-3 text-xs italic"
            style={{ color: "var(--text-secondary)" }}
          >
            Loading…
          </div>
        )}
        {pages !== null && (
          <>
            <button
              type="button"
              onClick={() => onOpenGraph(kmsName)}
              className="flex items-center gap-2 w-full px-3 py-2 text-xs font-medium border-b transition-colors"
              style={{
                color: graphActive
                  ? "var(--accent, #61afef)"
                  : "var(--text-primary)",
                background: graphActive
                  ? "color-mix(in srgb, var(--accent, #61afef) 12%, transparent)"
                  : "transparent",
                borderColor: "var(--border)",
              }}
              onMouseEnter={(e) => {
                if (!graphActive)
                  (e.currentTarget as HTMLButtonElement).style.background =
                    "rgba(255,255,255,0.04)";
              }}
              onMouseLeave={(e) => {
                if (!graphActive)
                  (e.currentTarget as HTMLButtonElement).style.background =
                    "transparent";
              }}
              title="Open Obsidian-style graph view"
            >
              <Network size={13} />
              <span>Graph View</span>
              {graphActive && (
                <span
                  className="ml-auto text-[9px] uppercase tracking-wider"
                  style={{ opacity: 0.7 }}
                >
                  open
                </span>
              )}
            </button>
            {(hits !== null || searching) && (
              <Section
                icon={<Search size={11} />}
                title={
                  hits === null
                    ? "In content…"
                    : `In content (${hits.length})`
                }
              >
                {searchNote && (
                  <div
                    className="px-3 pb-1 text-[10px] italic"
                    style={{ color: "var(--text-secondary)" }}
                  >
                    {searchNote}
                  </div>
                )}
                {hits !== null && hits.length === 0 && !searching && (
                  <div
                    className="px-3 py-1 text-xs italic"
                    style={{ color: "var(--text-secondary)" }}
                  >
                    Nothing in this knowledge base mentions that
                  </div>
                )}
                {(hits ?? []).map((h) => (
                  <button
                    key={`${h.kind}/${h.file}`}
                    type="button"
                    onClick={() =>
                      onOpenFile({ kms: kmsName, kind: h.kind, name: h.name })
                    }
                    className="block w-full text-left px-3 py-1.5 transition-colors"
                    style={{
                      background: isSelected(h.kind, h.name)
                        ? "color-mix(in srgb, var(--accent, #61afef) 12%, transparent)"
                        : "transparent",
                    }}
                    onMouseEnter={(e) => {
                      if (!isSelected(h.kind, h.name))
                        e.currentTarget.style.background = "rgba(255,255,255,0.04)";
                    }}
                    onMouseLeave={(e) => {
                      if (!isSelected(h.kind, h.name))
                        e.currentTarget.style.background = "transparent";
                    }}
                    title={h.kind === "source" ? `source · ${h.file}` : h.name}
                  >
                    <div
                      className="flex items-center gap-1.5 text-xs"
                      style={{ color: "var(--text-primary)" }}
                    >
                      {h.kind === "source" ? (
                        <Link2 size={10} className="shrink-0" />
                      ) : (
                        <FileText size={10} className="shrink-0" />
                      )}
                      <span className="truncate">{h.title}</span>
                    </div>
                    {h.snippet && (
                      <div
                        className="text-[10px] mt-0.5"
                        style={{
                          color: "var(--text-secondary)",
                          display: "-webkit-box",
                          WebkitLineClamp: 2,
                          WebkitBoxOrient: "vertical",
                          overflow: "hidden",
                          // Thai has no spaces to break on.
                          overflowWrap: "anywhere",
                        }}
                      >
                        {h.snippet}
                      </div>
                    )}
                  </button>
                ))}
              </Section>
            )}
            {reconciled.length > 0 && (
              <div
                className="mx-3 mb-2 rounded px-3 py-2 text-xs flex items-start gap-2"
                style={{
                  background: "var(--bg-secondary)",
                  borderLeft: "3px solid var(--warning, #e5c07b)",
                  color: "var(--text-primary)",
                }}
              >
                <span className="flex-1">
                  {reconciled.length === 1
                    ? `“${reconciled[0]}” was a placeholder left by a research run that never finished — removed.`
                    : `${reconciled.length} placeholders left by research runs that never finished were removed.`}{" "}
                  <span style={{ color: "var(--text-secondary)" }}>
                    They are in the trash if the work is worth recovering.
                  </span>
                </span>
                <button
                  type="button"
                  onClick={() => setReconciled([])}
                  title="Dismiss"
                  style={{
                    background: "transparent",
                    border: "none",
                    color: "var(--text-secondary)",
                    cursor: "pointer",
                  }}
                >
                  <X size={11} />
                </button>
              </div>
            )}
            <Section
              icon={<FileText size={11} />}
              title={
                filter
                  ? `Pages (${shownPages.length}/${pages.length})`
                  : `Pages (${pages.length})`
              }
            >
              {shownPages.length === 0 ? (
                <div
                  className="px-3 py-1 text-xs italic"
                  style={{ color: "var(--text-secondary)" }}
                >
                  {pages.length === 0 ? "No pages yet" : "No matching pages"}
                </div>
              ) : (
                shownPages.map((p) => (
                  <FileRow
                    key={p.name}
                    file={p}
                    active={isSelected("page", p.name)}
                    onClick={() =>
                      onOpenFile({ kms: kmsName, kind: "page", name: p.name })
                    }
                    onContextMenu={(e) => {
                      e.preventDefault();
                      setPageMenu({ name: p.name, x: e.clientX, y: e.clientY });
                    }}
                  />
                ))
              )}
            </Section>
            <Section
              icon={<Link2 size={11} />}
              title={
                filter
                  ? `Sources (${shownSources.length}/${sources.length})`
                  : `Sources (${sources.length})`
              }
            >
              {shownSources.length === 0 ? (
                <div
                  className="px-3 py-1 text-xs italic"
                  style={{ color: "var(--text-secondary)" }}
                >
                  {sources.length === 0
                    ? "No cached sources"
                    : "No matching sources"}
                </div>
              ) : (
                shownSources.map((s) => (
                  <FileRow
                    key={s.name}
                    file={s}
                    active={isSelected("source", s.name)}
                    onClick={() =>
                      onOpenFile({
                        kms: kmsName,
                        kind: "source",
                        name: s.name,
                      })
                    }
                  />
                ))
              )}
            </Section>
            <Section
              icon={<ShieldCheck size={11} />}
              title={
                lint === null
                  ? "Audit"
                  : lint.length === 0
                    ? "Audit (clean)"
                    : `Audit (${lint.length})`
              }
              open={auditOpen}
              onToggle={() => setAuditOpen((o) => !o)}
            >
              <div className="flex flex-wrap gap-1.5 px-3 pb-1.5">
                <button
                  type="button"
                  onClick={() => {
                    setLintError(null);
                    setLinting(true);
                    send({ type: "kms_lint", name: kmsName });
                  }}
                  disabled={linting}
                  className="rounded px-1.5 py-0.5"
                  style={{
                    border: "1px solid var(--border)",
                    fontSize: "10px",
                    color: "var(--accent, #61afef)",
                    background: "transparent",
                    cursor: linting ? "default" : "pointer",
                    opacity: linting ? 0.5 : 1,
                  }}
                  title="Read the vault and report broken links, uncited archives, unfinished pages. No model call, nothing written."
                >
                  {linting ? "Checking…" : "Check structure"}
                </button>
                <button
                  type="button"
                  onClick={() =>
                    send({
                      type: "chat_prompt",
                      text: `/kms verify ${JSON.stringify(kmsName)} --llm`,
                    })
                  }
                  className="rounded px-1.5 py-0.5"
                  style={{
                    border: "1px solid var(--border)",
                    fontSize: "10px",
                    color: "var(--text-secondary)",
                    background: "transparent",
                    cursor: "pointer",
                  }}
                  title="Re-check every claim against its archived source, then ask the model whether each sentence is supported. One model call per page — it costs money and takes minutes, so it runs in the chat, where you can watch it and stop it."
                >
                  Audit claims (model) →
                </button>
              </div>
              {lintError && (
                <div
                  className="px-3 pb-1 text-xs"
                  style={{ color: "var(--danger, #e06c75)" }}
                >
                  {lintError}
                </div>
              )}
              {lint !== null && lint.length === 0 && !lintError && (
                <div
                  className="px-3 pb-1 text-xs italic"
                  style={{ color: "var(--text-secondary)" }}
                >
                  Links resolve, every archive is cited, no page is
                  half-finished.
                </div>
              )}
              {lint !== null &&
                lint.map((f, i) => (
                  <LintRow
                    key={`${f.kind}-${f.target}-${i}`}
                    finding={f}
                    onOpen={
                      f.target && f.target_kind
                        ? () =>
                            onOpenFile({
                              kms: kmsName,
                              kind: f.target_kind as FileKind,
                              name: f.target,
                            })
                        : undefined
                    }
                  />
                ))}
            </Section>
            <Section
              icon={<Receipt size={11} />}
              title={
                filter
                  ? `Runs (${shownRuns.length}/${runs.length})`
                  : `Runs (${runs.length})`
              }
              right={
                priced.length > 0 ? (
                  <span
                    style={{ fontVariantNumeric: "tabular-nums" }}
                    title={
                      priced.length === runs.length
                        ? "What every run in this vault cost in LLM calls"
                        : `What ${priced.length} of ${runs.length} runs cost — the rest were written before their cost was recorded`
                    }
                  >
                    {money(totalCost)}
                  </span>
                ) : null
              }
              open={runsOpen}
              onToggle={() => setRunsOpen((o) => !o)}
            >
              {shownRuns.length === 0 ? (
                <div
                  className="px-3 py-1 text-xs italic"
                  style={{ color: "var(--text-secondary)" }}
                >
                  {runs.length === 0
                    ? "No research or verify runs yet"
                    : "No matching runs"}
                </div>
              ) : (
                shownRuns.map((r) => (
                  <RunRow
                    key={r.name}
                    run={r}
                    active={isSelected("run", r.name)}
                    onClick={() =>
                      onOpenFile({ kms: kmsName, kind: "run", name: r.name })
                    }
                  />
                ))
              )}
            </Section>
            <Section
              icon={<Trash2 size={11} />}
              title={
                filter
                  ? `Trash (${shownTrash.length}/${trash.length})`
                  : `Trash (${trash.length})`
              }
              open={trashOpen}
              onToggle={() => setTrashOpen((o) => !o)}
            >
              <div
                className="px-3 pb-1 text-xs italic"
                style={{ color: "var(--text-secondary)" }}
              >
                {trash.length === 0
                  ? `Nothing kept. A page this KMS overwrites or deletes is kept here for ${keepDays} days.`
                  : `Kept for ${keepDays} days. A restore is itself undoable.`}
              </div>
              {restoreNote && (
                <div
                  className="px-3 pb-1 text-xs"
                  style={{
                    color: restoreNote.ok
                      ? "var(--success, #98c379)"
                      : "var(--danger, #e06c75)",
                  }}
                >
                  {restoreNote.text}
                </div>
              )}
              {shownTrash.length > 0 &&
                shownTrash.map((t) => (
                  <TrashRowView
                    key={`${t.entry}/${t.kind}/${t.name}`}
                    row={t}
                    busy={restoring === t.name}
                    onRestore={() => {
                      setRestoreNote(null);
                      setRestoring(t.name);
                      send({
                        type: "kms_restore_page",
                        name: kmsName,
                        page: t.name,
                      });
                    }}
                  />
                ))}
            </Section>
          </>
        )}
      </div>
      {pageMenu && (
        <>
          <div
            className="fixed inset-0 z-[55]"
            onClick={() => setPageMenu(null)}
            onContextMenu={(e) => {
              e.preventDefault();
              setPageMenu(null);
            }}
          />
          <div
            className="fixed z-[56] rounded border shadow-lg text-xs py-1"
            style={{
              right: Math.max(8, window.innerWidth - pageMenu.x),
              top: pageMenu.y,
              minWidth: "150px",
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
          >
            <button
              type="button"
              className="block w-full text-left px-3 py-1.5 hover:bg-white/10"
              title="Re-research this note with fresh sources and merge what is new"
              onClick={() => {
                setConfirmRefresh(pageMenu.name);
                setPageMenu(null);
              }}
            >
              Refresh references…
            </button>
            <button
              type="button"
              className="block w-full text-left px-3 py-1.5 hover:bg-white/10"
              onClick={() => {
                setModal({ kind: "rename", kms: kmsName, name: pageMenu.name });
                setPageMenu(null);
              }}
            >
              Rename…
            </button>
            <button
              type="button"
              className="block w-full text-left px-3 py-1.5 hover:bg-white/10"
              style={{ color: "var(--danger, #e06c75)" }}
              onClick={() => {
                setModal({ kind: "delete", kms: kmsName, name: pageMenu.name });
                setPageMenu(null);
              }}
            >
              Delete…
            </button>
          </div>
        </>
      )}
      {modal && (
        <KmsCreateModal mode={modal} onClose={() => setModal(null)} />
      )}
      {confirmRefresh !== null && (
        <KmsRunConfirm
          title={`Refresh references on “${confirmRefresh}”`}
          noun="refresh"
          estimate={modes.refresh}
          runLabel="Refresh it"
          scope={[
            "Searches the web for sources on what this page already says.",
            "Keeps the page's text and adds references and citations to it — no new pages.",
            "The version before is kept in the trash, so this is undoable.",
          ]}
          onCancel={() => setConfirmRefresh(null)}
          onRun={() => {
            send({
              type: "chat_prompt",
              text: `/research refresh ${JSON.stringify(kmsName)} ${confirmRefresh}`,
            });
            setConfirmRefresh(null);
          }}
        />
      )}
    </div>
  );
}

function Section({
  icon,
  title,
  children,
  right,
  open,
  onToggle,
}: {
  icon: React.ReactNode;
  title: string;
  children: React.ReactNode;
  /// Anything that belongs on the header's right edge — a total, a count.
  right?: React.ReactNode;
  /// Omit both to get a section that is always open (every section but
  /// Runs). Pass them together to make the header a disclosure button.
  open?: boolean;
  onToggle?: () => void;
}) {
  const collapsible = onToggle !== undefined;
  const expanded = !collapsible || open === true;
  const header = (
    <>
      {collapsible && (
        <ChevronRight
          size={10}
          className="shrink-0 transition-transform"
          style={{ transform: expanded ? "rotate(90deg)" : "none" }}
        />
      )}
      {icon}
      <span className="flex-1 truncate">{title}</span>
      {right}
    </>
  );
  const headerStyle = {
    color: "var(--text-secondary)",
    fontSize: "10px",
    borderBottom: "1px solid var(--border)",
  } as const;
  const headerClass =
    "flex items-center gap-1.5 px-3 py-1.5 font-semibold uppercase tracking-wider";
  return (
    <div className="mb-2">
      {collapsible ? (
        <button
          type="button"
          onClick={onToggle}
          aria-expanded={expanded}
          className={`${headerClass} w-full text-left hover:bg-white/5`}
          style={{ ...headerStyle, background: "transparent" }}
        >
          {header}
        </button>
      ) : (
        <div className={headerClass} style={headerStyle}>
          {header}
        </div>
      )}
      {expanded && <div className="py-1">{children}</div>}
    </div>
  );
}

/// One structural finding. The name comes first and is the click
/// target, because the answer to "what do I do about this" is always
/// "open that file".
function LintRow({
  finding,
  onOpen,
}: {
  finding: LintFinding;
  onOpen?: () => void;
}) {
  const body = (
    <>
      {finding.target && (
        <span style={{ color: "var(--text-primary)" }}>{finding.target}</span>
      )}{" "}
      <span style={{ color: "var(--text-secondary)" }}>{finding.detail}</span>
    </>
  );
  const style = { fontSize: "11px", lineHeight: 1.5 } as const;
  return onOpen ? (
    <button
      type="button"
      onClick={onOpen}
      className="block w-full text-left px-3 py-0.5 hover:bg-white/5"
      style={{ ...style, background: "transparent" }}
      title={`${finding.kind} — open ${finding.target}`}
    >
      {body}
    </button>
  ) : (
    <div className="px-3 py-0.5" style={style} title={finding.kind}>
      {body}
    </div>
  );
}

/// One kept version. Restore is offered for pages only: `restore_page`
/// goes through `write_page`, which is what makes the restore itself
/// undoable, and there is no equivalent for a source archive.
function TrashRowView({
  row,
  onRestore,
  busy,
}: {
  row: TrashRow;
  onRestore: () => void;
  busy: boolean;
}) {
  return (
    <div className="flex items-baseline gap-2 px-3 py-1">
      <div className="flex-1 min-w-0">
        <div
          className="truncate text-xs"
          style={{ color: "var(--text-primary)" }}
          title={`${row.kind} · ${row.name}`}
        >
          {row.name}
        </div>
        <div
          className="truncate"
          style={{ fontSize: "10px", color: "var(--text-secondary)" }}
        >
          {[row.why, row.when].filter(Boolean).join(" · ")}
        </div>
      </div>
      {row.kind === "page" && (
        <button
          type="button"
          onClick={onRestore}
          disabled={busy}
          className="shrink-0 rounded px-1.5 py-0.5"
          style={{
            border: "1px solid var(--border)",
            fontSize: "10px",
            color: "var(--accent, #61afef)",
            background: "transparent",
            cursor: busy ? "default" : "pointer",
            opacity: busy ? 0.5 : 1,
          }}
          title="Bring back the newest kept version that differs from the page as it stands now. What it replaces is kept too."
        >
          {busy ? "…" : "Restore"}
        </button>
      )}
    </div>
  );
}

/// One run in the ledger. The label answers "what was this", the meta
/// line "what did it do", and the right edge "what did it cost" — the
/// three things a person asks of a run they did not watch happen.
function RunRow({
  run,
  onClick,
  active = false,
}: {
  run: RunEntry;
  onClick: () => void;
  active?: boolean;
}) {
  const meta = [run.date];
  if (run.claims !== undefined) meta.push(`${run.claims} claims`);
  if (run.findings !== undefined)
    meta.push(run.findings === 1 ? "1 finding" : `${run.findings} findings`);
  if (run.elapsed_secs !== undefined) meta.push(duration(run.elapsed_secs));
  return (
    <button
      type="button"
      onClick={onClick}
      title={run.name}
      className="w-full text-left px-3 py-1 hover:bg-white/5"
      style={{
        background: active ? "var(--bg-secondary)" : "transparent",
        borderLeft: `2px solid ${active ? "var(--accent, #61afef)" : "transparent"}`,
      }}
    >
      <div className="flex items-baseline gap-2">
        <span
          className="flex-1 truncate text-xs"
          style={{
            color: active ? "var(--accent, #61afef)" : "var(--text-primary)",
          }}
        >
          {runLabel(run)}
        </span>
        {run.cost_usd !== undefined && (
          <span
            className="shrink-0"
            style={{
              fontSize: "10px",
              color: "var(--text-secondary)",
              fontVariantNumeric: "tabular-nums",
            }}
          >
            {money(run.cost_usd)}
          </span>
        )}
      </div>
      <div
        className="truncate"
        style={{ fontSize: "10px", color: "var(--text-secondary)" }}
      >
        {meta.join(" · ")}
      </div>
    </button>
  );
}

function FileRow({
  file,
  onClick,
  active = false,
  onContextMenu,
}: {
  file: BrowseFile;
  onClick: () => void;
  active?: boolean;
  onContextMenu?: (e: React.MouseEvent) => void;
}) {
  /// Active row styling: 2px accent left-border + tinted bg + accent
  /// text + slightly heavier weight. The chosen tint (`color-mix`
  /// with the accent) reads as a soft highlight on both light and
  /// dark themes — same visual rhythm as the "Graph View" active
  /// button above.
  const activeBg = active
    ? "color-mix(in srgb, var(--accent, #61afef) 14%, transparent)"
    : "transparent";
  const textColor = active
    ? "var(--accent, #61afef)"
    : "var(--text-primary)";
  return (
    <button
      type="button"
      onClick={onClick}
      onContextMenu={onContextMenu}
      className="flex items-baseline justify-between w-full text-left px-3 py-1"
      style={{
        color: textColor,
        background: activeBg,
        borderLeft: active
          ? "2px solid var(--accent, #61afef)"
          : "2px solid transparent",
        fontWeight: active ? 600 : 400,
        cursor: "pointer",
      }}
      onMouseEnter={(e) => {
        if (!active)
          (e.currentTarget as HTMLButtonElement).style.background =
            "rgba(255,255,255,0.05)";
      }}
      onMouseLeave={(e) => {
        if (!active)
          (e.currentTarget as HTMLButtonElement).style.background = activeBg;
      }}
      title={
        (file.title ? `${file.title}\n` : "") +
        `${file.name}${file.ext ? "." + file.ext : ""}` +
        (file.status ? ` — ${file.status}` : "") +
        (active ? " (currently viewing)" : "")
      }
    >
      <span
        className="truncate flex-1 text-xs"
        style={{
          fontStyle: file.status ? "italic" : "normal",
          opacity: file.status ? 0.7 : 1,
        }}
      >
        {labelOf(file)}
      </span>
      {file.status && (
        <span
          className="ml-2 shrink-0 rounded px-1"
          style={{
            fontSize: "9px",
            color: file.status === "failed" ? "var(--danger, #e06c75)" : "var(--text-secondary)",
            border: "1px solid var(--border)",
          }}
        >
          {file.status === "researching" ? "writing…" : file.status}
        </span>
      )}
      {file.ext && file.ext !== "md" && (
        <span
          className="ml-2 shrink-0 rounded px-1"
          style={{
            fontSize: "9px",
            color: "var(--text-secondary)",
            border: "1px solid var(--border)",
            textTransform: "uppercase",
          }}
        >
          {file.ext}
        </span>
      )}
      <span
        className="ml-2 shrink-0"
        style={{
          color: active
            ? "var(--accent, #61afef)"
            : "var(--text-secondary)",
          fontSize: "9px",
          opacity: active ? 0.85 : 1,
        }}
      >
        {formatBytes(file.bytes)}
      </span>
    </button>
  );
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n}B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)}KB`;
  return `${(n / (1024 * 1024)).toFixed(1)}MB`;
}
