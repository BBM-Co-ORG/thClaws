// Shared markdown ↔ HTML round-trip for the TipTap editors
// (MarkdownEditor — Files tab + KMS viewer — and
// InstructionsEditorModal). TipTap works in HTML natively while the
// files on disk stay markdown, so every editor converts on load and on
// save. Keeping one implementation here is the point: the three editors
// used to carry near-identical copies of this and drifted apart, so a
// `.md` opened in the Files tab and the same file opened via the
// instructions modal serialized differently.
//
// `async: false` forces `marked.parse()` to return a string instead of a
// Promise — otherwise TipTap receives `[object Promise]` and renders it
// as plain text.

import { Node } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import Image from "@tiptap/extension-image";
import { TableKit } from "@tiptap/extension-table";
import { BulletList, OrderedList } from "@tiptap/extension-list";
import { marked } from "marked";
import TurndownService from "turndown";
import { assetUrl } from "./assetUrl";

marked.setOptions({ gfm: true, breaks: false, async: false });

function escapeAttr(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/"/g, "&quot;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

// ── Raw HTML comments ────────────────────────────────────────────────
// ProseMirror's DOM parser silently DROPS comment nodes (`<!-- … -->`),
// so wrapper markers like the tutorial's `<!-- GEN-IMAGE … -->` were
// lost on every save. Each comment becomes a `<div data-html-comment>`
// placeholder that survives DOM parsing, is held as an atom node (shown
// as a muted chip via CSS), and turns back into a real comment on
// serialize.
const HtmlComment = Node.create({
  name: "htmlComment",
  group: "block",
  atom: true,
  selectable: true,
  addAttributes() {
    return {
      text: {
        default: "",
        parseHTML: (el: HTMLElement) => el.getAttribute("data-html-comment") || "",
        renderHTML: (attrs: Record<string, unknown>) => ({
          "data-html-comment": String(attrs.text ?? ""),
        }),
      },
    };
  },
  parseHTML() {
    return [{ tag: "div[data-html-comment]" }];
  },
  // The marker text is rendered as real child text, NOT as a CSS
  // ::before. Turndown short-circuits empty elements through its
  // `blankRule` before any user rule gets a look, so an empty
  // placeholder div was dropped on save even with the rule below
  // registered — comments survived editing but not saving. Text content
  // makes the node non-blank so the rule actually fires. `data-md-src`
  // stays the source of truth on the way back out.
  renderHTML({
    node,
    HTMLAttributes,
  }: {
    node: { attrs: Record<string, unknown> };
    HTMLAttributes: Record<string, unknown>;
  }) {
    return [
      "div",
      { ...HTMLAttributes, class: "md-html-comment" },
      `<!--${String(node.attrs.text ?? "")}-->`,
    ];
  },
});

// Comments inside code blocks are already entity-escaped by marked
// (`&lt;!--`), so this only matches real, block-level comments.
function commentsToPlaceholders(html: string): string {
  return html.replace(
    /<!--([\s\S]*?)-->/g,
    (_m, inner: string) => `<div data-html-comment="${escapeAttr(inner)}"></div>`,
  );
}

// ── Images ───────────────────────────────────────────────────────────
// A relative `![alt](img/foo.png)` resolves against the app document
// URL, not the .md file's directory, so it renders broken in the editor
// (the read-only paths dodge this with a `<base href>` / a display-only
// rewrite). We point `src` at the /file-asset handler for display and
// stash the ORIGINAL relative path in `data-md-src`, which the turndown
// rule below writes back on save. Both halves are required: rewriting
// on load alone would bake absolute `…/file-asset/…` URLs into the
// user's markdown on the next save.
const MarkdownImage = Image.extend({
  addAttributes() {
    return {
      ...this.parent?.(),
      mdSrc: {
        default: null,
        parseHTML: (el: HTMLElement) => el.getAttribute("data-md-src"),
        renderHTML: (attrs: Record<string, unknown>) =>
          attrs.mdSrc ? { "data-md-src": String(attrs.mdSrc) } : {},
      },
    };
  },
});

// ── Loose vs tight lists ─────────────────────────────────────────────
// Markdown distinguishes a tight list (`- a\n- b`) from a loose one
// (blank line between items, which renders each item wrapped in <p>).
// ProseMirror's listItem content is `paragraph block*`, so it ALWAYS
// wraps — the distinction is erased by the schema and every round-trip
// had to pick one, rewriting whichever lists used the other form. Detect
// it from marked's output (a loose item has a <p> child, a tight one has
// bare text) and carry it as an attribute the serializer can read back.
const looseAttribute = {
  loose: {
    default: false,
    parseHTML: (el: HTMLElement) =>
      Array.from(el.children).some((li) => !!li.querySelector(":scope > p")),
    renderHTML: (attrs: Record<string, unknown>) =>
      attrs.loose ? { "data-loose": "true" } : {},
  },
};

const LooseBulletList = BulletList.extend({
  addAttributes() {
    return { ...this.parent?.(), ...looseAttribute };
  },
});

const LooseOrderedList = OrderedList.extend({
  addAttributes() {
    return { ...this.parent?.(), ...looseAttribute };
  },
});

function isAbsoluteSrc(src: string): boolean {
  return /^(https?:|data:|blob:|thclaws:|file:|\/\/)/i.test(src) || src.startsWith("/");
}

function rewriteImageSrcs(html: string, baseDir: string): string {
  const base = baseDir.replace(/\/+$/, "");
  if (!base) return html;
  return html.replace(
    /(<img\b[^>]*?\ssrc=)("|')(.*?)\2/gi,
    (match, pre: string, quote: string, src: string) => {
      const s = src.trim();
      if (!s || isAbsoluteSrc(s)) return match;
      const rel = s.replace(/^\.\//, "");
      return `${pre}${quote}${assetUrl(`${base}/${rel}`)}${quote} data-md-src="${escapeAttr(s)}"`;
    },
  );
}

// ── Markdown serialization (turndown) ────────────────────────────────
const turndownService = new TurndownService({
  headingStyle: "atx",
  bulletListMarker: "-",
  codeBlockStyle: "fenced",
  emDelimiter: "_",
  // `* * *` is turndown's default and nothing else in the ecosystem
  // writes it; every file we round-trip uses `---`.
  hr: "---",
});

// GFM autolinks a bare URL into an <a>, and turndown writes every <a>
// back as an explicit `[text](href)` — so `https://arxiv.org/abs/…`
// came back as `[https://arxiv.org/abs/…](https://arxiv.org/abs/…)`,
// doubling the line for no change in meaning. Emit the bare URL when
// the link text IS its target.
turndownService.addRule("bareUrl", {
  filter: (node) =>
    node.nodeName === "A" &&
    !!node.getAttribute("href") &&
    !node.getAttribute("title") &&
    node.textContent === node.getAttribute("href")!.replace(/^mailto:/, ""),
  replacement: (content) => content,
});

// Turndown indents list content by 4 spaces and writes `-` + 3 spaces,
// then separates items with a blank line whenever ProseMirror wraps the
// item text in a <p> (which it always does) — turning every tight list
// loose and leaving whitespace-only lines behind. Emit the conventional
// `- ` / `1. ` with continuation aligned to the marker width.
turndownService.addRule("compactListItem", {
  filter: "li",
  replacement: (content, node, options) => {
    const el = node as HTMLElement;
    const parent = el.parentNode as HTMLElement | null;
    let prefix = `${options.bulletListMarker} `;
    if (parent?.nodeName === "OL") {
      const start = Number(parent.getAttribute("start") || 1);
      const index = Array.prototype.indexOf.call(parent.children, el);
      prefix = `${start + index}. `;
    }
    const body = content
      .replace(/^\n+/, "")
      .replace(/\n+$/, "")
      // A single-paragraph item shouldn't keep the paragraph's blank
      // line; a genuinely multi-block item still needs one.
      .replace(/\n{2,}(?=[^\n])/g, (m, offset: number, whole: string) =>
        /\n\s*(?:[-*+]|\d+\.)\s/.test(whole.slice(offset)) ? "\n" : m,
      )
      .replace(/\n/g, `\n${" ".repeat(prefix.length)}`);
    const separator = parent?.getAttribute("data-loose") === "true" ? "\n\n" : "\n";
    return `${prefix}${body}${el.nextSibling ? separator : ""}`;
  },
});

// ── Agent placeholder tokens ─────────────────────────────────────────
// Agents embed `{{…}}` tokens in generated markdown — the book pipeline
// writes `{{screenshot:ch01/x.png|คำอธิบาย}}`, provider configs carry
// `{{env:NAME}}`. To markdown they're ordinary text, so turndown escapes
// every special character inside them on save: `start_url` became
// `start\_url` and `[docs]` became `\[docs\]`, silently breaking the
// downstream parsers that consume these files. Escape normally, then
// undo it inside token runs.
const TOKEN_RE = /\{\{[^{}\n]*\}\}/g;
const baseEscape = turndownService.escape.bind(turndownService);

// Turndown escapes every `[` and `]` in prose, so plain text like
// `[UNK]` came back as `\[UNK\]`. Brackets only need escaping when they
// could actually open a link or reference — i.e. when followed by `(`,
// `[` or `:`. Anything else is literal text either way.
function unescapeInertBrackets(s: string): string {
  return s.replace(/\\\[([^\]\n]*)\\\]/g, (match, inner: string, offset: number) => {
    const after = s.slice(offset + match.length);
    return /^\s*[([:]/.test(after) ? match : `[${inner}]`;
  });
}

// Same idea for `*`: emphasis needs a pair, so a lone asterisk in a run
// of text is literal no matter what — `return x*2` was coming back as
// `return x\*2`. Only unescape when it can't open a list either, i.e.
// when something non-space precedes it on the line.
function unescapeLoneAsterisk(s: string): string {
  const stars = s.match(/\\\*/g);
  if (!stars || stars.length !== 1) return s;
  return s.replace(/(\S)\\\*/g, "$1*");
}

turndownService.escape = (str: string) =>
  unescapeLoneAsterisk(unescapeInertBrackets(baseEscape(str))).replace(
    TOKEN_RE,
    (token) => token.replace(/\\([\\`*_[\]#+\-=>~])/g, "$1"),
  );

// Block-level markers need their own blank lines: the tutorial pipeline
// matches `<!-- GEN-IMAGE … -->` / `<!-- SCREENSHOT … -->` per line (see
// thclaws-tutorial-th/FORMAT.md), and a marker glued to the end of a
// preceding image line stops being recognized.
turndownService.addRule("htmlComment", {
  filter: (node) =>
    node.nodeName === "DIV" && node.getAttribute("data-html-comment") !== null,
  replacement: (_content, node) =>
    "\n\n<!--" +
    ((node as HTMLElement).getAttribute("data-html-comment") || "") +
    "-->\n\n",
});

// Images normally live inside a paragraph (see `inline: true` below) and
// must NOT get blank lines — that would split `see ![x](y) here` across
// three blocks. An image that lands as a direct block-level child (a
// paste, a table cell holding only an image) still needs the separation,
// or it serializes onto the same line as whatever follows.
const BLOCK_IMG_PARENTS = ["BODY", "DIV", "BLOCKQUOTE", "FIGURE", "SECTION", "ARTICLE"];

// Prefer the original relative path over the display URL so the saved
// markdown matches what was loaded.
turndownService.addRule("mdImage", {
  filter: "img",
  replacement: (_content, node) => {
    const el = node as HTMLElement;
    const src = el.getAttribute("data-md-src") || el.getAttribute("src") || "";
    if (!src) return "";
    const alt = el.getAttribute("alt") || "";
    const title = el.getAttribute("title");
    const target = /\s/.test(src) ? `<${src}>` : src;
    // Escape the backslashes BEFORE the quotes: escaping only `"` turns a
    // title of `a\"b` into `a\\"b`, which markdown reads as an escaped
    // backslash followed by a live quote — the title string ends early and
    // the rest of the line leaks into the document.
    const mdTitle = title?.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
    const md = `![${alt}](${target}${title ? ` "${mdTitle}"` : ""})`;
    const parent = el.parentNode?.nodeName ?? "";
    return BLOCK_IMG_PARENTS.includes(parent) ? `\n\n${md}\n\n` : md;
  },
});

// ── GFM pipe tables ──────────────────────────────────────────────────
// Turndown ships no table rule at all: without this a `<table>` falls
// through to the default block handling and each cell comes back as a
// loose paragraph, so saving a `.md` in Edit mode flattened the table
// permanently. TipTap renders headers as `<th>` in the first row of
// `<tbody>` (no `<thead>`) while marked emits a real `<thead>` — take
// the first row as the header either way.
const ALIGN_SEPARATOR: Record<string, string> = {
  left: ":---",
  right: "---:",
  center: ":---:",
};

function cellAlign(cell: HTMLElement): string {
  const raw = (cell.style?.textAlign || cell.getAttribute("align") || "")
    .trim()
    .toLowerCase();
  return raw in ALIGN_SEPARATOR ? raw : "";
}

// `|` would split the cell and a newline would end the row, so both have
// to go: escape the pipe, fold hard breaks to `<br>` (GFM's only way to
// carry multi-block cell content). Trim BEFORE folding — a block-level
// image inside a cell arrives wrapped in blank lines, which would
// otherwise become leading/trailing `<br>`s.
function cellMarkdown(cell: HTMLElement): string {
  return turndownService
    .turndown(cell.innerHTML)
    .trim()
    // Escape the pipe along with any backslash run in front of it. A bare
    // `.replace(/\|/g, "\\|")` turns `a\|b` into `a\\|b` — an escaped
    // backslash and then a LIVE cell separator, which splits the row. Only
    // the run touching the pipe is doubled; turndown's own escapes
    // (`\*`, `\_`) elsewhere in the cell must be left exactly as they are.
    .replace(/(\\*)\|/g, (_m, slashes: string) => `${slashes}${slashes}\\|`)
    .replace(/\s*\n+\s*/g, "<br>");
}

turndownService.addRule("gfmTable", {
  filter: "table",
  replacement: (_content, node) => {
    const table = node as HTMLElement;
    // Nested tables aren't expressible in GFM and can't be built in the
    // editor, but scope the query anyway so one can't steal the other's
    // rows if a paste ever produces one.
    const rows = Array.from(table.querySelectorAll("tr")).filter(
      (tr) => tr.closest("table") === table,
    );
    if (!rows.length) return "";

    // colspan has no GFM equivalent — widen the cell into N columns and
    // leave the extras blank rather than dropping the row's alignment.
    const grid = rows.map((row) =>
      Array.from(row.children).flatMap((child) => {
        const cell = child as HTMLElement;
        const span = Math.max(1, parseInt(cell.getAttribute("colspan") || "1", 10) || 1);
        const text = cellMarkdown(cell);
        return [
          { text, align: cellAlign(cell) },
          ...Array.from({ length: span - 1 }, () => ({ text: "", align: "" })),
        ];
      }),
    );

    const width = Math.max(...grid.map((row) => row.length));
    if (!width) return "";
    const line = (cells: { text: string }[]) =>
      "| " +
      Array.from({ length: width }, (_v, i) => cells[i]?.text || " ").join(" | ") +
      " |";

    const [header, ...body] = grid;
    const separator =
      "| " +
      Array.from(
        { length: width },
        (_v, i) => ALIGN_SEPARATOR[header[i]?.align || body[0]?.[i]?.align || ""] || "---",
      ).join(" | ") +
      " |";

    return "\n\n" + [line(header), separator, ...body.map(line)].join("\n") + "\n\n";
  },
});

// ── Public API ───────────────────────────────────────────────────────

/// Node/mark set every markdown editor shares. StarterKit has no table
/// and no image node, so without these two the ProseMirror DOM parser
/// has nothing to map `<table>` / `<img>` onto and drops them to plain
/// paragraphs. `resizable` needs the column-resize plugin's drag
/// handles, which we don't style; column widths aren't representable in
/// markdown anyway.
export const markdownExtensions = [
  StarterKit.configure({ bulletList: false, orderedList: false }),
  LooseBulletList,
  LooseOrderedList,
  // `inline: true` keeps images as paragraph content. As block nodes
  // they'd split any sentence containing one, since ProseMirror has to
  // break the paragraph around a block child — `see ![x](y) here` came
  // back as three separate blocks.
  MarkdownImage.configure({ inline: true, allowBase64: true }),
  TableKit.configure({ table: { resizable: false } }),
  HtmlComment,
];

/// Info string marking a code block that came from a `---` YAML block,
/// so `yamlBlock` below can write it back out with its dashes instead of
/// as a fence. Kept out of the `language-yaml` namespace on purpose —
/// a hand-written ```yaml fence must stay a fence.
const YAML_RULE_LANG = "yaml-md-rule";

/// Index of the `---` closing a YAML block opened at `open`, or -1. The
/// author's signal is contiguity: no blank line between the dashes and
/// their contents (a blank line means two separate rules), and a first
/// line that reads as YAML.
function yamlBlockEnd(lines: string[], open: number): number {
  const first = (lines[open + 1] ?? "").trimEnd();
  if (!/^[\w.-]+\s*:/.test(first) && !first.startsWith("- ")) return -1;
  for (let i = open + 1; i < lines.length; i++) {
    if (lines[i].trim() === "") return -1;
    if (/^-{3,}\s*$/.test(lines[i])) return i;
  }
  return -1;
}

/// Rewrite `---` into what the author meant before handing markdown to
/// the converter — the same two CommonMark traps `file_preview.rs`
/// fixes for the read-only preview, except here they're *destructive*:
/// whatever the parse yields is what the save writes back.
///
/// - A paragraph line followed directly by `---` is a setext H2, so a
///   section separator ate the line above it and saved it as `## …`.
///   Insert the blank line that makes it a plain rule.
/// - A `---` pair with nothing blank between it and its contents is
///   YAML (frontmatter, metadata blocks), and the closing `---`
///   underlined the last key. Park it in a code block tagged
///   `YAML_RULE_LANG`, which `yamlBlock` unwraps back to `---` on save.
///
/// Fenced code is skipped, and so are list / quote / table lines
/// (already a rule there — a blank line would only loosen the list).
function normalizeRules(md: string): string {
  const lines = md.split("\n");
  const out: string[] = [];
  let fence: string | null = null;
  let prevBlank = true;
  let prevParagraph = false;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const trimmed = line.replace(/\r$/, "").trimStart();
    const indent = line.length - line.trimStart().length;
    const opener = indent <= 3 ? /^(```+|~~~+)/.exec(trimmed)?.[1] : undefined;
    if (fence) {
      if (opener && opener[0] === fence[0] && opener.length >= fence.length) fence = null;
    } else if (opener) {
      fence = opener;
    } else if (indent === 0 && /^-{3,}\s*$/.test(trimmed)) {
      const end = prevBlank ? yamlBlockEnd(lines, i) : -1;
      if (end > 0) {
        out.push("```" + YAML_RULE_LANG, ...lines.slice(i + 1, end), "```", "");
        i = end;
        prevBlank = true;
        prevParagraph = false;
        continue;
      }
      if (prevParagraph) out.push("");
    }
    out.push(line);
    prevBlank = trimmed === "";
    prevParagraph =
      !fence && indent === 0 && trimmed !== "" && !/^([-*+>#|=]|\d+[.)])/.test(trimmed);
  }
  return out.join("\n");
}

// Undo the parking above: a `YAML_RULE_LANG` block is written back with
// its original `---` delimiters, byte for byte. Without this the save
// would turn every frontmatter block in the file into a fenced code
// block.
turndownService.addRule("yamlBlock", {
  filter: (node) =>
    node.nodeName === "PRE" &&
    !!node.firstChild &&
    (node.firstChild as HTMLElement).className?.includes(YAML_RULE_LANG),
  replacement: (_content, node) =>
    `\n\n---\n${(node.textContent ?? "").replace(/\n+$/, "")}\n---\n\n`,
});

/// Markdown → HTML for `editor.commands.setContent`. `baseDir` is the
/// directory of the file being edited (workspace-relative or absolute);
/// pass it so relative image references resolve, omit it when the
/// content has no place on disk.
export function markdownToEditorHtml(md: string, baseDir?: string): string {
  if (!md) return "";
  const parsed = marked.parse(normalizeRules(md));
  const html = commentsToPlaceholders(typeof parsed === "string" ? parsed : "");
  return baseDir ? rewriteImageSrcs(html, baseDir) : html;
}

/// HTML from `editor.getHTML()` → markdown, normalized to a single
/// trailing newline so `git diff` stays quiet.
export function editorHtmlToMarkdown(html: string): string {
  return (
    turndownService
      .turndown(html)
      // `### 1. หัวข้อ` — turndown escapes a leading `N.` because at the
      // start of a text node it looks like an ordered-list marker. Inside
      // a heading it can't be one, so the backslash is pure noise.
      .replace(/^(#{1,6} .*)$/gm, (line) => line.replace(/(\d)\\\./g, "$1."))
      // Whitespace-only lines (list indentation artifacts) trip
      // markdownlint and show up as trailing-space diffs.
      .replace(/[ \t]+$/gm, "")
      .replace(/\n+$/, "") + "\n"
  );
}

/// Split leading YAML frontmatter off a markdown document. Agent
/// definitions (`.thclaws/agents/*.md`) and generated chapters put
/// their name / model / tool config there, and it must never reach the
/// converter: marked reads the closing `---` as a setext heading and
/// the opening one as a thematic break, so a round-trip turned the
/// whole block into `* * *` + `## title: …`, destroying the config.
/// The returned `frontmatter` keeps its trailing newline so
/// `frontmatter + body` reconstructs the original exactly.
export function splitFrontmatter(md: string): { frontmatter: string; body: string } {
  if (!md.startsWith("---\n")) return { frontmatter: "", body: md };
  const end = md.indexOf("\n---\n", 3);
  if (end < 0) return { frontmatter: "", body: md };
  const cut = end + "\n---\n".length;
  return { frontmatter: md.slice(0, cut), body: md.slice(cut) };
}

/// Re-attach frontmatter to freshly-serialized body markdown. The
/// converter always trims leading whitespace, so the blank line that
/// conventionally separates the two has to be re-inserted here.
export function joinFrontmatter(frontmatter: string, body: string): string {
  if (!frontmatter) return body;
  return `${frontmatter}\n${body.replace(/^\n+/, "")}`;
}

/// Directory portion of a file path, for `baseDir` above.
export function parentDir(filePath: string): string {
  const normalized = filePath.replace(/\\/g, "/");
  const lastSlash = normalized.lastIndexOf("/");
  return lastSlash >= 0 ? normalized.slice(0, lastSlash) : "";
}

/// Styling for the nodes above. Tailwind 4's preflight strips table
/// borders and image sizing, so each editor's inline <style> has to
/// carry these explicitly — they're shared here to stop the editors
/// from looking different from one another.
export const MARKDOWN_NODE_CSS = `
  .tiptap-compact img { max-width: 100%; height: auto; border-radius: 4px; margin: 0.4em 0; }
  .tiptap-compact img.ProseMirror-selectednode { outline: 2px solid var(--accent, #61afef); }
  .tiptap-compact table {
    border-collapse: collapse;
    margin: 0.6em 0;
    table-layout: auto;
    width: auto;
    max-width: 100%;
  }
  .tiptap-compact th, .tiptap-compact td {
    border: 1px solid var(--border);
    padding: 4px 10px;
    font-size: 13px;
    vertical-align: top;
    text-align: left;
    position: relative;
    min-width: 3em;
  }
  .tiptap-compact th { background: rgba(127,127,127,0.12); font-weight: 600; }
  .tiptap-compact th > p, .tiptap-compact td > p { margin: 0; }
  /* prosemirror-tables paints the multi-cell selection through this
     overlay; without it a drag-select across cells is invisible. */
  .tiptap-compact .selectedCell::after {
    content: "";
    position: absolute;
    inset: 0;
    background: var(--accent, #61afef);
    opacity: 0.18;
    pointer-events: none;
  }
  .tiptap-compact .md-html-comment {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11px;
    color: var(--text-secondary);
    opacity: 0.65;
    margin: 0.25em 0;
    white-space: pre-wrap;
    user-select: none;
  }
  .tiptap-compact .md-html-comment.ProseMirror-selectednode { outline: 2px solid var(--accent, #61afef); border-radius: 3px; opacity: 1; }
`;
