/// dev-plan/64 P5.4: how a run's price is read and said.
///
/// Split from `KmsRunConfirm.tsx` because a component file that also
/// exports helpers breaks fast refresh — the same reason `kmsPageMeta`
/// sits beside `KmsTrustStrip`.

/// Per-mode history, as `kms_cost_result` sends it. `runs` is the size
/// of the sample: `0` means the honest answer is "not known yet".
export type ModeCost = {
  runs: number;
  cost_usd?: number;
  elapsed_secs?: number;
  claims?: number;
};

/// `$0.0114` reads as nothing at a glance; `1.1¢` reads as cheap. Runs
/// span three orders of magnitude, so the unit moves.
export function money(usd: number): string {
  if (usd >= 1) return `$${usd.toFixed(2)}`;
  if (usd >= 0.01) return `${(usd * 100).toFixed(1)}¢`;
  return `${(usd * 100).toFixed(2)}¢`;
}

export function duration(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  return secs % 60 === 0 ? `${m}m` : `${m}m ${secs % 60}s`;
}

/// What this click is likely to cost, in one sentence, with the size of
/// the sample it rests on. Never an invented number: with no history,
/// it says so rather than borrowing another kind of run's price.
export function priceLine(estimate: ModeCost | undefined, noun: string): string {
  const n = estimate?.runs ?? 0;
  const plural = n === 1 ? "" : "s";
  if (n === 0) {
    return `No ${noun} has run in this knowledge base yet, so what it costs is not known in advance.`;
  }
  if (estimate?.cost_usd === undefined) {
    return `${n} previous ${noun}${plural} here, none of which recorded a cost.`;
  }
  const time =
    estimate.elapsed_secs !== undefined
      ? ` and about ${duration(estimate.elapsed_secs)}`
      : "";
  return `About ${money(estimate.cost_usd)}${time} — the median of ${n} ${noun}${plural} in this knowledge base.`;
}
