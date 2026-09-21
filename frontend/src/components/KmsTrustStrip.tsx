import { Check } from "lucide-react";
import type { PageMeta } from "./kmsPageMeta";

/// dev-plan/64 P5.2: what a reader needs in order to decide how far to trust
/// a page, above the page.
///
/// Every number here was already in the page's frontmatter — claims,
/// confidence, sources, `uncited:`, `verified:`, `stale_since:` — and none of
/// it was visible without opening the YAML editor. A note that is 80 % the
/// model's own elaboration looked exactly like one where every paragraph is
/// cited.

function listLength(v: string | undefined): number {
  if (!v) return 0;
  const m = v.match(/^\[(.*)\]$/);
  if (!m) return v.trim() ? 1 : 0;
  return m[1].split(",").filter((x) => x.trim() !== "").length;
}

function daysSince(ymd: string | undefined): number | null {
  if (!ymd || !/^\d{4}-\d{2}-\d{2}$/.test(ymd)) return null;
  const then = new Date(`${ymd}T00:00:00`).getTime();
  if (Number.isNaN(then)) return null;
  return Math.floor((Date.now() - then) / 86_400_000);
}

const KIND_LABEL: Record<string, string> = {
  moc: "topic page",
  entity: "entity",
  concept: "concept",
  claim: "claim",
};

/// Above this share of uncited prose the chip turns into a warning. Same
/// line the research run log draws (`UNCITED_WARN`).
const UNCITED_WARN = 0.35;
const STALE_DAYS = 90;

type Tone = "plain" | "good" | "warn";

function Chip({ label, value, tone = "plain", title }: {
  label: string;
  value: string;
  tone?: Tone;
  title?: string;
}) {
  const color =
    tone === "warn"
      ? "var(--warning, #e5c07b)"
      : tone === "good"
        ? "var(--success, #98c379)"
        : "var(--text-secondary)";
  return (
    <span
      title={title}
      className="inline-flex items-baseline gap-1 rounded px-1.5 py-0.5"
      style={{ border: "1px solid var(--border)", fontSize: "11px", color }}
    >
      <span style={{ opacity: 0.75 }}>{label}</span>
      <span style={{ color: tone === "plain" ? "var(--text-primary)" : color }}>{value}</span>
    </span>
  );
}

function Banner({ tone, children }: { tone: "warn" | "info"; children: React.ReactNode }) {
  return (
    <div
      className="rounded px-3 py-2 mb-2 text-xs"
      style={{
        background: "var(--bg-secondary)",
        borderLeft: `3px solid ${tone === "warn" ? "var(--warning, #e5c07b)" : "var(--accent, #61afef)"}`,
        color: "var(--text-primary)",
      }}
    >
      {children}
    </div>
  );
}

export function KmsTrustStrip({
  meta,
  onMarkReviewed,
  busy = false,
  readOnly = false,
}: {
  meta: PageMeta;
  /// Absent → the button is not offered (sources, truncated pages).
  onMarkReviewed?: () => void;
  busy?: boolean;
  readOnly?: boolean;
}) {
  const status = (meta.status ?? "").toLowerCase();
  const claims = Number.parseInt(meta.claims ?? "", 10);
  const confidence = Number.parseFloat(meta.confidence ?? "");
  const uncited = Number.parseFloat(meta.uncited ?? "");
  const sources = listLength(meta.sources);
  const verifiedAge = daysSince(meta.verified);
  const reviewedAge = daysSince(meta.reviewed);
  const researchWritten = meta.type === "note" || Number.isFinite(claims);
  // ISO dates compare as strings. A review is of the text as it stood: a
  // refresh the day after leaves the stamp and makes it a half-truth.
  const changedSinceReview =
    !!meta.reviewed && !!meta.updated && meta.updated > meta.reviewed;

  // A page with none of this is a hand-written page: say nothing rather
  // than show a row of dashes.
  const hasAnything =
    researchWritten || sources > 0 || meta.verified || meta.reviewed || meta.updated || status;
  if (!hasAnything) return null;

  return (
    <div className="mb-4">
      {status === "researching" && (
        <Banner tone="info">
          This page is being written by <code>/research</code>. What is here is a placeholder.
        </Banner>
      )}
      {status === "failed" && (
        <Banner tone="warn">
          The research run that was writing this page did not finish. The page is incomplete.
        </Banner>
      )}
      {status === "derived" && (
        <Banner tone="info">
          Generated from an ingested source and not yet written up — read the source for the real
          content.
        </Banner>
      )}
      {meta.stale_since && (
        <Banner tone="warn">
          A source this page was built on was re-ingested on {meta.stale_since}. The page has not
          been refreshed since.
        </Banner>
      )}
      <div className="flex flex-wrap items-center gap-1.5">
        {meta.kind && <Chip label="kind" value={KIND_LABEL[meta.kind] ?? meta.kind} />}
        {Number.isFinite(claims) && (
          <Chip
            label="claims"
            value={String(claims)}
            title="Statements checked word for word against a source when this page was written"
          />
        )}
        <Chip
          label="sources"
          value={String(sources)}
          tone={researchWritten && sources === 0 ? "warn" : "plain"}
        />
        {Number.isFinite(uncited) && (
          <Chip
            label="uncited"
            value={`${Math.round(uncited * 100)}%`}
            tone={uncited > UNCITED_WARN ? "warn" : uncited === 0 ? "good" : "plain"}
            title="Share of the prose in paragraphs with no citation. The opening description is not counted."
          />
        )}
        {Number.isFinite(confidence) && (
          <Chip
            label="confidence"
            value={confidence.toFixed(2)}
            title="Mean confidence the digest step gave this page's claims"
          />
        )}
        {meta.verified ? (
          <Chip
            label="checked"
            value={meta.verified}
            tone={verifiedAge !== null && verifiedAge > STALE_DAYS ? "warn" : "plain"}
            title={
              verifiedAge !== null && verifiedAge > STALE_DAYS
                ? `Claims were last checked against their sources ${verifiedAge} days ago`
                : "Claims were checked against their sources on this date"
            }
          />
        ) : (
          researchWritten && <Chip label="checked" value="never" tone="warn" />
        )}
        {meta.updated && <Chip label="updated" value={meta.updated} />}
        {meta.reviewed ? (
          <Chip
            label="reviewed"
            value={changedSinceReview ? `${meta.reviewed} · changed since` : meta.reviewed}
            tone={
              changedSinceReview
                ? "warn"
                : reviewedAge !== null && reviewedAge > STALE_DAYS
                  ? "plain"
                  : "good"
            }
            title={
              changedSinceReview
                ? `A person reviewed this page on ${meta.reviewed}; it was updated on ${meta.updated}, after that`
                : "A person marked this page as read and accepted on this date"
            }
          />
        ) : (
          <Chip label="reviewed" value="no" title="Nobody has marked this page as reviewed" />
        )}
        {onMarkReviewed && !readOnly && (
          <button
            type="button"
            onClick={onMarkReviewed}
            disabled={busy}
            className="inline-flex items-center gap-1 rounded px-1.5 py-0.5"
            style={{
              border: "1px solid var(--border)",
              fontSize: "11px",
              color: "var(--accent, #61afef)",
              cursor: busy ? "default" : "pointer",
              opacity: busy ? 0.5 : 1,
              background: "transparent",
            }}
            title="Stamp reviewed: with today's date. Research never overwrites this key."
          >
            <Check size={11} />
            {meta.reviewed ? "Mark reviewed again" : "Mark reviewed"}
          </button>
        )}
      </div>
    </div>
  );
}
