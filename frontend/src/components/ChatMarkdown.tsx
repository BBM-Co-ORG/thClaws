import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { send } from "../hooks/useIPC";

// Assistant text rendered as markdown, for surfaces outside the Chat tab.
//
// The Browser tab's sidebar chat used to render `{m.text}` raw under
// `whitespace-pre-wrap`, so a reply containing a table came out as literal
// `| a | b |` rows and `**bold**` stayed asterisks — the same content the
// Chat tab renders properly, because only Chat ran it through markdown.
//
// SECURITY: `text` is untrusted model output. This is the same safe stack
// the Chat tab uses — remark-gfm for tables/strikethrough/task-lists,
// rehype-highlight as a CSS-class applier over fenced code. No
// allowDangerousHtml, no rehype-raw, no dangerouslySetInnerHTML. Do not add
// an HTML pass-through here without rethinking that threat model.

const THINK_BLOCK = /<think>[\s\S]*?<\/think>/gi;
const ORPHAN_CLOSE = /^[ \t\r\n]*<\/think>\n?/i;

/// Reasoning blocks belong to the Chat tab's collapsible, not to a
/// 12px sidebar — strip them rather than dumping raw `<think>` text.
function stripThinkBlocks(content: string): string {
  return content.replace(THINK_BLOCK, "").replace(ORPHAN_CLOSE, "");
}

export function ChatMarkdown({ text }: { text: string }) {
  return (
    <div className="markdown-body">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeHighlight]}
        components={{
          // A link must never navigate the webview away from the tab;
          // hand it to the OS browser instead.
          a: ({ href, children, ...rest }) => (
            <a
              {...rest}
              href={href}
              onClick={(e) => {
                if (!href) return;
                e.preventDefault();
                send({ type: "open_external", url: href });
              }}
            >
              {children}
            </a>
          ),
        }}
      >
        {stripThinkBlocks(text)}
      </ReactMarkdown>
    </div>
  );
}
