import { useEffect, useMemo, useRef, useState } from "react";
import { send } from "../hooks/useIPC";
import { FusionConfigModal } from "./FusionConfigModal";
import {
  isOpenRouterFreeOnly,
  refreshOpenRouterFreeOnly,
  setOpenRouterFreeOnly,
} from "./openrouterFreeOnly";

/// One row from the catalogue, as the backend ships it.
export type PickerModel = {
  id: string;
  context?: number | null;
  /// `true` when `context` is the provider's blanket default rather than
  /// a figure they published. Rendered as `200k?` — a floor shown as a
  /// specification is what made a 1M model advertise 131k (#190).
  context_unverified?: boolean | null;
  max_output?: number | null;
  /// `true` when the upstream provider lists this model as free
  /// (both prompt + completion token prices = 0). Only populated
  /// for OpenRouter today; other providers ship `null`.
  free?: boolean | null;
};

type Props = {
  provider: string;
  current: string;
  models: PickerModel[];
  onClose: () => void;
};

/// Strip the active provider's routing prefix from a model id for
/// display. Keeps the underlying `m.id` intact so the value sent to
/// the backend on `model_set` still routes correctly. Only strips
/// the exact `${provider}/` prefix — incidental matches inside the
/// rest of the id (e.g. `openrouter/anthropic/claude-...`) are
/// preserved.
function displayId(id: string, provider: string): string {
  const prefix = `${provider}/`;
  return id.startsWith(prefix) ? id.slice(prefix.length) : id;
}

/// Format a context window in a tight human form: 200_000 → "200k", etc.
function formatCtx(n: number | null | undefined): string {
  if (!n || n <= 0) return "";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
  if (n >= 1_000) return `${Math.round(n / 1000)}k`;
  return String(n);
}

/// Post-key-entry model picker. Opens when the backend broadcasts
/// `model_picker_open` after a successful api_key_set for a provider with
/// a non-trivial catalogue. The user picks a default; we send model_set;
/// the modal closes. Skipping leaves auto_fallback_model's choice in place.
export function ModelPickerModal({ provider, current, models, onClose }: Props) {
  const [query, setQuery] = useState("");
  // Local mirror of the server-side flag, cached in localStorage for
  // first-paint. Only honoured for OpenRouter (other providers don't
  // ship `free` data). On mount we ask the server for the canonical
  // value to correct any drift between cache and ~/.thclaws/settings.
  const [freeOnly, setFreeOnly] = useState<boolean>(() =>
    provider === "openrouter" && isOpenRouterFreeOnly()
  );
  const inputRef = useRef<HTMLInputElement>(null);
  // `openrouter/fusion+` opens the Fusion config modal instead of
  // switching directly — same behaviour as the sidebar ModelPickerDropdown
  // so both pickers are consistent (issue #167).
  const [fusionOpen, setFusionOpen] = useState(false);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  useEffect(() => {
    if (provider !== "openrouter") return;
    return refreshOpenRouterFreeOnly(setFreeOnly);
  }, [provider]);

  // Esc to skip.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    let rows = models;
    if (provider === "openrouter" && freeOnly) {
      rows = rows.filter((m) => m.free === true);
    }
    if (q) {
      // Match against both the canonical id and its display form so a
      // search for `gemma-3-27b` hits `openrouter/google/gemma-3-27b-it:free`.
      rows = rows.filter((m) => {
        const idLower = m.id.toLowerCase();
        return idLower.includes(q) || displayId(idLower, provider).includes(q);
      });
    }
    return rows;
  }, [models, query, provider, freeOnly]);

  const pick = (id: string) => {
    if (id === "openrouter/fusion+") {
      // Defer the switch: the modal saves the config and sends model_set
      // itself on Save. Keep this picker mounted underneath.
      setFusionOpen(true);
      return;
    }
    send({ type: "model_set", model: id });
    onClose();
  };

  if (fusionOpen) {
    return (
      <FusionConfigModal
        onApplied={onClose}
        onCancel={() => setFusionOpen(false)}
      />
    );
  }

  return (
    <div
      className="fixed inset-0 flex items-center justify-center z-50"
      style={{ background: "var(--modal-backdrop)" }}
      onClick={onClose}
    >
      <div
        className="rounded-lg shadow-2xl w-full max-w-xl mx-4 flex flex-col"
        style={{
          background: "var(--bg-secondary)",
          border: "1px solid var(--border)",
          maxHeight: "80vh",
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b" style={{ borderColor: "var(--border)" }}>
          <h2 className="text-sm font-semibold" style={{ color: "var(--text-primary)" }}>
            Pick a default model for {provider}
          </h2>
          <p className="text-xs mt-1" style={{ color: "var(--text-secondary)" }}>
            Your API key is saved. Choose the model thClaws should default
            to. You can switch any time with <code className="font-mono">/model</code>.
          </p>
        </div>

        <div className="px-5 py-3 border-b" style={{ borderColor: "var(--border)" }}>
          {provider === "openrouter" && (
            <label
              className="flex items-center gap-2 text-xs mb-2 select-none cursor-pointer"
              style={{ color: "var(--text-secondary)" }}
              title="Filter the list to OpenRouter models marked as $0 prompt + $0 completion."
            >
              <input
                type="checkbox"
                checked={freeOnly}
                onChange={(e) => {
                  setFreeOnly(e.target.checked);
                  setOpenRouterFreeOnly(e.target.checked);
                }}
              />
              <span>
                Free only{" "}
                <span style={{ opacity: 0.7 }}>
                  ({models.filter((m) => m.free === true).length} of {models.length})
                </span>
              </span>
            </label>
          )}
          <input
            ref={inputRef}
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={`Search ${models.length} model${models.length === 1 ? "" : "s"}…`}
            // Disable autocorrect / autocapitalize / spellcheck so model
            // names like "gpt-4.1-nano" or "claude-sonnet-4-6" aren't
            // silently rewritten by the browser's IME helpers.
            autoCorrect="off"
            autoCapitalize="off"
            autoComplete="off"
            spellCheck={false}
            className="w-full px-3 py-2 rounded text-sm outline-none"
            style={{
              background: "var(--bg-tertiary)",
              border: "1px solid var(--border)",
              color: "var(--text-primary)",
            }}
          />
        </div>

        <div className="flex-1 overflow-y-auto py-1">
          {filtered.length === 0 ? (
            <div
              className="px-5 py-4 text-xs text-center"
              style={{ color: "var(--text-secondary)" }}
            >
              No models match "{query}".
            </div>
          ) : (
            filtered.map((m) => {
              const isCurrent = m.id === current;
              const ctx = formatCtx(m.context);
              return (
                <button
                  key={m.id}
                  type="button"
                  onClick={() => pick(m.id)}
                  className="w-full px-5 py-2 text-left text-sm flex items-center justify-between"
                  style={{
                    background: isCurrent ? "var(--bg-tertiary)" : "transparent",
                    color: "var(--text-primary)",
                    cursor: "pointer",
                    borderLeft: isCurrent
                      ? "2px solid var(--accent)"
                      : "2px solid transparent",
                  }}
                  onMouseEnter={(e) =>
                    (e.currentTarget.style.background = "var(--bg-tertiary)")
                  }
                  onMouseLeave={(e) =>
                    (e.currentTarget.style.background = isCurrent
                      ? "var(--bg-tertiary)"
                      : "transparent")
                  }
                >
                  <span
                    className="font-mono truncate"
                    title={m.id}
                  >
                    {displayId(m.id, provider)}
                  </span>
                  <span className="flex items-center gap-2 ml-3 shrink-0">
                    {m.free === true && (
                      <span
                        className="text-[10px] px-1.5 py-0.5 rounded font-medium"
                        style={{
                          background: "color-mix(in srgb, #3fb950 20%, transparent)",
                          color: "#3fb950",
                          border: "1px solid color-mix(in srgb, #3fb950 40%, transparent)",
                        }}
                        title="OpenRouter lists this model at $0 prompt + $0 completion."
                      >
                        FREE
                      </span>
                    )}
                    {ctx && (
                      <span
                        className="text-xs"
                        style={{ color: "var(--text-secondary)" }}
                        title={
                          m.context_unverified
                            ? `${ctx} is this provider's default, not a published figure — treat it as a lower bound.`
                            : undefined
                        }
                      >
                        {ctx}
                        {m.context_unverified ? "?" : ""} ctx
                      </span>
                    )}
                  </span>
                </button>
              );
            })
          )}
        </div>

        <div
          className="px-5 py-3 border-t flex items-center justify-between"
          style={{ borderColor: "var(--border)" }}
        >
          <span className="text-xs" style={{ color: "var(--text-secondary)" }}>
            Currently: <code className="font-mono">{current || "(none)"}</code>
          </span>
          <button
            type="button"
            onClick={onClose}
            className="px-3 py-1.5 text-xs rounded"
            style={{
              background: "transparent",
              color: "var(--text-secondary)",
              border: "1px solid var(--border)",
              cursor: "pointer",
            }}
          >
            Skip
          </button>
        </div>
      </div>
    </div>
  );
}
