//! Slash-command data and the pure helpers over it, split out of
//! `SlashCommandPopup` so that file exports a component only —
//! `react-refresh` cannot hot-reload a module that mixes the two, and
//! ChatView and TerminalView both filter commands themselves.

export type SlashCommandInfo = {
  name: string;
  description: string;
  category: string;
  usage: string;
  source: "builtin" | "user" | "skill";
};

/// Filter commands by case-insensitive prefix match against `query`.
/// `query` is the text after the leading slash (may be empty when the
/// user has typed only "/").
export function filterCommands(
  commands: SlashCommandInfo[],
  query: string,
): SlashCommandInfo[] {
  const q = query.trim().toLowerCase();
  if (!q) return commands;
  return commands.filter((c) => c.name.toLowerCase().startsWith(q));
}

export function groupByCategory(
  items: SlashCommandInfo[],
): Array<[string, SlashCommandInfo[]]> {
  const map = new Map<string, SlashCommandInfo[]>();
  for (const item of items) {
    const list = map.get(item.category);
    if (list) list.push(item);
    else map.set(item.category, [item]);
  }
  return Array.from(map.entries());
}
