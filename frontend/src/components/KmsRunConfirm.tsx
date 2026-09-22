/// dev-plan/64 P5.4: nothing that spends money starts on one click.
///
/// Every research-spawning control in the KMS — create a page from a
/// selection, refresh a note's references — used to fire straight from
/// a context menu. The owner learned the scope and the price after the
/// fact, in a run log, once the money was gone.
///
/// The price shown is not a token model. It is the median of what runs
/// of the same kind have actually cost in *this* knowledge base, and
/// the line says how many runs that rests on — because "we have never
/// done this here, so it is not known" is a real answer and inventing
/// a number in its place would be worse than saying nothing.

import { priceLine, type ModeCost } from "./kmsRunCost";

export function KmsRunConfirm({
  title,
  scope,
  estimate,
  noun,
  runLabel = "Run it",
  busy = false,
  onRun,
  onCancel,
  children,
}: {
  title: string;
  /// What the run will do, in the owner's terms — one line per effect.
  scope: string[];
  estimate?: ModeCost;
  /// What a run of this kind is called, for the price sentence.
  noun: string;
  runLabel?: string;
  busy?: boolean;
  onRun: () => void;
  onCancel: () => void;
  /// Options that change the scope, rendered above the price.
  children?: React.ReactNode;
}) {
  return (
    <div
      className="fixed inset-0 z-[60] flex items-center justify-center p-4"
      style={{ background: "rgba(0,0,0,0.45)" }}
      onClick={onCancel}
    >
      <div
        className="rounded border shadow-lg w-full"
        style={{
          maxWidth: 440,
          background: "var(--bg-primary)",
          borderColor: "var(--border)",
          color: "var(--text-primary)",
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <div
          className="px-4 py-2 border-b text-sm font-semibold"
          style={{ borderColor: "var(--border)" }}
        >
          {title}
        </div>
        <div className="px-4 py-3 flex flex-col gap-2">
          <ul className="text-xs flex flex-col gap-1" style={{ paddingLeft: 0 }}>
            {scope.map((s) => (
              <li key={s} className="flex gap-2">
                <span style={{ color: "var(--text-secondary)" }}>·</span>
                <span>{s}</span>
              </li>
            ))}
          </ul>
          {children}
          <div
            className="rounded px-3 py-2 text-xs"
            style={{
              background: "var(--bg-secondary)",
              color: "var(--text-secondary)",
            }}
          >
            {priceLine(estimate, noun)}
          </div>
        </div>
        <div
          className="px-4 py-2 border-t flex justify-end gap-2"
          style={{ borderColor: "var(--border)" }}
        >
          <button
            type="button"
            onClick={onCancel}
            className="rounded px-2 py-1"
            style={{
              border: "1px solid var(--border)",
              fontSize: "11px",
              background: "transparent",
              color: "var(--text-primary)",
            }}
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={onRun}
            disabled={busy}
            className="rounded px-2 py-1"
            style={{
              border: "1px solid var(--accent, #61afef)",
              fontSize: "11px",
              background: "transparent",
              color: "var(--accent, #61afef)",
              cursor: busy ? "default" : "pointer",
              opacity: busy ? 0.5 : 1,
            }}
          >
            {busy ? "Starting…" : runLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
