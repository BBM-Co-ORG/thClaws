/// dev-plan/64 P4.9: what to do with a document once it is archived.
///
/// Four modes, not four flavours of one thing — they buy different
/// guarantees at different prices, and the choice belongs to whoever
/// knows how much the document matters. The labels say what you get,
/// and the hints say what it costs, because the expensive one is the
/// only one whose page the trust strip and `/kms verify` can actually
/// check.

export type IngestMode = "archive" | "summary" | "cited" | "atomic";

export const INGEST_MODES: {
  id: IngestMode;
  label: string;
  hint: string;
}[] = [
  {
    id: "archive",
    label: "Just archive it",
    hint: "Keeps the source, writes no page. Searchable, and free.",
  },
  {
    id: "summary",
    label: "Summarise it",
    hint: "One page, written in the chat in a single turn. Fast and cheap — but with no claims, quotes or citations, so nothing can check it later.",
  },
  {
    id: "cited",
    label: "Summarise with citations",
    hint: "One page, written by the research pipeline: every statement backed by a quote checked against the archived text. Several minutes and a few cents.",
  },
  {
    id: "atomic",
    label: "Break it into notes",
    hint: "A topic page and one note per idea, all cited. The most thorough and the most expensive.",
  },
];

export const DEFAULT_INGEST_MODE: IngestMode = "summary";
