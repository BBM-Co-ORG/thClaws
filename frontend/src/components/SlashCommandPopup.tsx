import { useEffect, useMemo, useRef } from "react";
import {
  filterCommands,
  groupByCategory,
  type SlashCommandInfo,
} from "./slashCommands";

type Props = {
  query: string;
  commands: SlashCommandInfo[];
  selectedIndex: number;
  onHoverIndex: (i: number) => void;
  onSelect: (cmd: SlashCommandInfo) => void;
};

/// Popup shown above the chat input when the user types a leading "/".
///
/// Filtering is prefix-match on the command name (case-insensitive).
/// Items are grouped by `category` in source order — the parent owns
/// `selectedIndex` so arrow keys/Enter can be handled in ChatView's
/// `onKeyDown` without bubbling synthetic events between components.
export function SlashCommandPopup({
  query,
  commands,
  selectedIndex,
  onHoverIndex,
  onSelect,
}: Props) {
  const filtered = useMemo(() => filterCommands(commands, query), [commands, query]);
  const itemRefs = useRef<Array<HTMLButtonElement | null>>([]);

  useEffect(() => {
    const el = itemRefs.current[selectedIndex];
    if (el) el.scrollIntoView({ block: "nearest" });
  }, [selectedIndex]);

  if (filtered.length === 0) return null;

  const grouped = groupByCategory(filtered);
  // Flat index of each group's first row. The parent owns
  // `selectedIndex` against the flattened list, so a row needs its
  // global position; deriving it from the group offset keeps that a
  // pure function of where the row sits, where the old counter was
  // reassigned from inside a render callback.
  const offsets = grouped.map((_, g) =>
    grouped.slice(0, g).reduce((n, [, items]) => n + items.length, 0),
  );

  return (
    <div
      className="rounded-lg border shadow-xl overflow-y-auto"
      style={{
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
        maxHeight: 320,
      }}
      onMouseDown={(e) => e.preventDefault()}
    >
      {grouped.map(([category, items], g) => (
        <div key={category}>
          <div
            className="px-3 pt-2 pb-1 text-[10px] uppercase tracking-wider"
            style={{ color: "var(--text-secondary)" }}
          >
            {category}
          </div>
          {items.map((cmd, j) => {
            const idx = offsets[g] + j;
            const active = idx === selectedIndex;
            return (
              <button
                key={`${cmd.source}:${cmd.name}`}
                ref={(el) => {
                  itemRefs.current[idx] = el;
                }}
                type="button"
                onMouseEnter={() => onHoverIndex(idx)}
                onClick={() => onSelect(cmd)}
                className="w-full text-left px-3 py-1.5 flex items-baseline gap-2 text-sm"
                style={{
                  background: active ? "var(--accent)" : "transparent",
                  color: active ? "var(--accent-fg)" : "var(--text-primary)",
                  cursor: "pointer",
                  border: "none",
                }}
              >
                <span className="font-mono">/{cmd.name}</span>
                {cmd.usage && (
                  <span
                    className="font-mono text-xs"
                    style={{
                      color: active ? "var(--accent-fg)" : "var(--text-secondary)",
                      opacity: 0.8,
                    }}
                  >
                    {cmd.usage}
                  </span>
                )}
                {cmd.description && (
                  <span
                    className="text-xs truncate ml-auto"
                    style={{
                      color: active ? "var(--accent-fg)" : "var(--text-secondary)",
                      opacity: 0.85,
                    }}
                  >
                    {cmd.description}
                  </span>
                )}
              </button>
            );
          })}
        </div>
      ))}
    </div>
  );
}
