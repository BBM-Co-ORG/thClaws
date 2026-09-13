# Chapter 15 — Subagents

The `Task` tool lets the main agent **delegate** to a sub-agent: a
fresh, isolated copy of the agent with its own tool scope and its own
goal. Useful for branching work (explore multiple approaches in
parallel), protecting the main context (run a noisy exploration in a
child), or specialisation (hand off to a "reviewer" agent with
read-only tools).

Subagents are part of the same process — they run in-memory, not as
separate OS processes. For true parallelism across processes, see
Agent Teams in Chapter 17.

## How it looks

```
❯ are the REST endpoints in this repo consistent with our naming
  convention in AGENTS.md?

[tool: Task: (agent=reviewer, prompt=Check every route under src/api …)] …
  [child:reviewer] Using Glob to find route files…
  [child:reviewer] Found 14 routes; 3 don't match the convention
[tool: Task] ✓

Looking at the sub-agent's findings:
- `src/api/v1/getUsers.ts` should be `get_users.ts` per convention.
- `src/api/v1/FetchOrders.ts` should be `fetch_orders.ts`.
- `src/api/v2/createPost.ts` should be `create_post.ts`.
```

The parent sees only the sub-agent's final text response, keeping the
intermediate tool chatter out of the main context.

## Agent definitions

Specific sub-agent behaviours are configured in
`.thclaws/agents/*.md` (project) or `~/.config/thclaws/agents/*.md`
(user):

```markdown
---
name: reviewer
description: Read-only code review with focus on conventions
model: claude-haiku-4-5
tools: Read, Glob, Grep, Ls
permissionMode: auto
maxTurns: 20
color: cyan
---

You are a code reviewer. Look at the code the parent points you at.
Flag:
- Naming inconsistencies with the project's `AGENTS.md` conventions.
- Missing tests alongside new code.
- Security-sensitive patterns (raw SQL, unsanitised input).

Return a concise bullet list. Don't propose fixes unless asked.
```

Frontmatter fields:

| Field | Purpose |
|---|---|
| `name` | Unique id (defaults to filename stem) |
| `description` | When-to-use text the parent sees |
| `model` | Model override for this agent |
| `tools` | Comma-separated tool allowlist |
| `disallowedTools` | Tool denylist |
| `skills` | Comma-separated skill allowlist this agent may load/list/search (subset of the parent's). Empty = inherit all |
| `mcp` | Comma-separated MCP **server** names this agent may use (subset). Empty = inherit all; otherwise MCP tools from other servers are dropped |
| `writePaths` | Glob allowlist confining this agent's file writes (Write/Edit/office tools), e.g. `.thclaws/state/kms/**`. Empty = inherit. Mechanical write-scoping — a writer can't scribble outside its lane. Does **not** cover `Bash` |
| `output_schema` | JSON Schema for the agent's output — single-line inline JSON, or a path (relative to the `.md`) to a `.json` file. A workflow `thclaws.subagent({agent})` call that omits a per-call `schema` validates against this (one source of truth instead of duplicating the schema in the workflow). See [ch25](ch25-workflows.md) |
| `input_schema` | JSON Schema documenting the agent's expected input (same encoding as `output_schema`) — used by `thclaws agent validate` |
| `permissionMode` | `auto` or `ask` (useful for "read-only" agents) |
| `maxTurns` | Max iterations (default 200) |
| `color` | Terminal colour for child output |
| `isolation` | `worktree` — give this agent its own git worktree (teams only) |

> **Scaffold, validate, run.** `thclaws agent new <dir> --pattern
> static-pipeline|batch-fanout|dynamic` scaffolds a best-practice skeleton
> (planner / worker / read-only verifier subagents + schemas + a workflow) that
> validates green out of the box. `thclaws agent validate <dir>` then lints
> offline — frontmatter parses, tool names known, `output_schema` /
> `input_schema` valid, `writePaths` globs compile, `.thclaws/scripts/*.py`
> compile, declared MCP/skills present, workflow avoids stripped globals, manifest
> fuses. `thclaws agent run <dir> --args '{…}'` *runs* the agent's workflow
> headlessly (Task + MCP wired) to behaviorally smoke-test it; `--dry-tools`
> skips real generation. `thclaws agent pack <dir>` builds the publish tarball
> (identical to what `/cloud publish` ships).

### Authoring in the GUI — `/agent new` / `/agent edit`

In the desktop GUI you don't have to hand-edit the `.md`. Two GUI-only
commands open an editor modal with separate panes for the YAML
frontmatter and the system prompt:

- **`/agent new <name>`** — opens a starter definition for a brand-new
  agent.
- **`/agent edit <name>`** — opens an existing agent (loaded from disk,
  or reconstructed from a built-in like `translator`) for editing.

Save writes a **project override** at `.thclaws/agents/<name>.md`. The
new/edited def is immediately spawnable via `/agent <name>`; the
model-driven `Task` tool picks it up on the next session. In the CLI
these commands just point you at the file to edit by hand (there's no
terminal modal).

## Invoking

There are **two surfaces** for spawning a subagent:

### Model-driven — the `Task` tool

The parent agent invokes via `Task`:

```
Task(agent: "reviewer", prompt: "Check src/api for naming violations")
```

Typically you don't call this directly — you ask the parent a question in English and it decides. The model sees the list of available agents in its system prompt (rendered from the agent defs).

The `Task` tool **blocks the parent's turn** until the child finishes — the parent then sees the result as a tool result and continues. The child's intermediate reasoning isn't echoed into the parent's context (that's the whole point — keep main context clean), but the parent does pay for one full child run before its next move.

### User-driven — the `/agent` slash command (GUI)

In the desktop GUI, you can spawn a subagent **yourself** without going through the main agent's reasoning:

```
/agent translator แปลไฟล์ src/foo.md เป็นภาษาไทย
```

The chat surface confirms: `✓ spawned background agent 'translator' (id: side-a1b2c3d4)` — the id is `side-` followed by the first 8 hex digits of a UUID. While the translator runs:

- **Main agent keeps accepting input** — you can keep working with it. Side-channel agents run on their own tokio task, concurrent with main.
- **Main's history is unaffected.** The prompt + result never enter the main conversation. Progress shows in a separate **Background Agents** sidebar (green = running, grey = done, red = error), and a one-line system message lands in the chat when it finishes (`✓ /translator done in Ns …`).
- **Cancel is independent.** Pressing the main agent's stop button does NOT kill side-channels. To cancel a side-channel use `/agent cancel <id>`.
- **Permission requests stay distinguishable.** If the side-channel asks for `Bash` approval while main is also doing tool calls, the approval modal labels each request with its source ("translator (background) wants to run Bash" vs "Main wants to run Bash") so you don't accidentally approve the wrong one.

```
/agents                       # list active background agents
/agent cancel side-a1b2c3d4   # signal cancel to a specific channel
```

Side-channel agents share the same AgentDef registry — the named agents available via `Task` are the same ones available via `/agent`. Permissions, sandbox, MCP servers, and KMS access all behave identically.

`/agent` is the right surface when you know specifically what you want done, the work is well-scoped, and you want to keep doing other things in the main conversation while it runs. The model-driven `Task` tool stays the right choice when the parent agent's reasoning should decide whether/when to delegate.

## Recursion

A sub-agent can spawn further sub-agents up to `max_depth = 3` by
default. Each level is more scoped:

```
parent (depth 0)
 ├─ reviewer (depth 1) — "look at auth routes"
 │   └─ specialist (depth 2) — "audit JWT signing"
 └─ tester (depth 1) — "write integration tests"
```

The Task tool at depth 3 disables recursion to prevent runaway chains.

## Load order

Built-in (binary) → `~/.config/thclaws/agents.json` → `~/.claude/agents/*.md` →
`~/.config/thclaws/agents/*.md` → `.claude/agents/*.md` →
`.thclaws/agents/*.md`. Later wins by name.

## Installing subagents from the marketplace

Subagents are one of the four types in the thClaws marketplace (see
[Chapter 12](ch12-skills.md)). A catalog subagent is packaged as a
single `.md` (frontmatter + system prompt) and installed with the same
commands as skills:

```
❯ /subagent marketplace            # list catalog subagents
❯ /subagent search review          # search
❯ /subagent info reviewer          # detail view
❯ /subagent install reviewer       # install by catalog name
```

`/subagent install` accepts a catalog name, a git URL (with the
`#<branch>:<path/to/agent.md>` subpath syntax), or a `.zip`. It lands
the file at `.thclaws/agents/<name>.md` (project) by default, or
`~/.config/thclaws/agents/` with `--user`. The name is taken from the
file's frontmatter `name:` (or pass an override:
`/subagent install <url> <name>`). `linked-only` entries point you at
the upstream repo instead of installing.

In the GUI, the **Subagents** tab of the `/marketplace` browser does
the same with one click. A freshly installed subagent is spawnable via
`/agent <name>` right away; the model-driven `Task` tool sees it on the
next session.

## Built-in subagents

thClaws ships a curated set of subagents compiled into the binary —
no install step required. They appear alongside any user-defined
agents in `Task(agent: "...")` and `/agent <name>` invocations. A
disk-resident agent at `.thclaws/agents/<name>.md` of the same name
overrides the built-in.

| Name | Default model | What it does |
|---|---|---|
| `dream` | session model | Consolidate the project's KMS by mining recent sessions, deduping pages, surfacing insights. Invoked via the `/dream` slash command. See [Chapter 9](ch09-knowledge-bases-kms.md). |
| `translator` | session model | Translate text or files between languages while preserving structure (markdown headings, lists, code blocks, frontmatter). Invoked via `/agent translator <prompt>` or `Task(agent: "translator")`. |
| `folder-indexer` | session model | Catalogue a folder into `<folder>/index.md` — one row per file, described from its content (text, documents, and images alike). Incremental: the `FolderIndex` tool fingerprints every file, so only what changed since the last run is re-read. Invoked via `/index <folder>`, the Files tab's folder right-click → **Index folder…**, or `Task(agent: "folder-indexer")`. |
| `kms-linker` | session model | Fix broken page links, refresh stale pages, and patch missing index entries in a KMS. Dispatched as a side-channel by `/kms wrap-up --fix`. |
| `kms-reconcile` | session model | Find and resolve contradictions across KMS pages — rewrites outdated pages with History sections, flags ambiguous cases as Conflict pages. Dispatched by `/kms reconcile <name> [--apply]`. |
| `kms-maintain` | session model | The one-shot maintenance umbrella — structural fixes, source reconciliation against your live sessions, stale refresh, and contradiction reconciliation in one staged pass. Dispatched by `/kms maintain <name> [--apply]`. See [Chapter 9](ch09-knowledge-bases-kms.md). |
| `summarizer` | session model | Summarize text, a file, or a URL into a shorter faithful digest — key points, decisions, takeaways — optionally in another language. Invoked via `/agent summarizer <prompt>` or `Task(agent: "summarizer")`. |
| `content-extractor` | session model | Turn a webpage, a local file, or pasted text into a clean self-contained markdown article with images downloaded locally. Useful as the step before `/kms ingest`. |

Neither built-in pins a `model:` in its frontmatter — both inherit the
session's active model so cross-provider users don't hit "model not
found" errors. Override per-agent via `settings.json` (below).

### Override the model via `settings.json`

Each built-in subagent's recommended model can be overridden from
`settings.json` without forking the embedded AgentDef body. Single
string only (AgentDef.model is `Option<String>`, no priority list):

```json
// .thclaws/settings.json (project) or ~/.config/thclaws/settings.json (user)
{
  "translator_subagent_model": "claude-sonnet-4-6"
}
```

Resolution chain:

1. Disk-resident `<scope>/.thclaws/agents/translator.md` — full override (replaces the entire AgentDef including instructions). Use this when you want to customize the prose, not just the model.
2. `settings.json` field (e.g. `translator_subagent_model`) — model-only override that leaves the embedded body intact.
3. The session's active model — fallback when neither is configured (the embedded built-ins ship with no `model:` frontmatter).

Each future built-in subagent that needs settings tunability gets its
own field (`<name>_subagent_model`). Same per-agent named-field
convention as the skill side (`extract_save_skill_models` etc.) — more
discoverable in `settings.json` than a generic map.

### Plugin-contributed agents

Plugins (Chapter 16) can ship agent defs via an `agents` entry in
their manifest. Those dirs are walked **after** the standard ones and
merged **additively** — a plugin agent cannot override a user's or
project's existing agent with the same name. That means:

- You can install a plugin that ships a `reviewer` + `tester` +
  `architect` and all three become available via `Task(agent: "…")`
  and team spawns.
- If you later add your own `.thclaws/agents/reviewer.md`, it wins —
  the plugin's is ignored until you remove yours.
- `/plugin show <name>` lists the `agent dirs` the plugin contributes.

## Subagents vs Side-channel agents vs Teams

| | Task subagent | `/agent` side-channel | Teams |
|---|---|---|---|
| **Trigger** | Model decides via `Task` tool | User types `/agent` | Model uses `SpawnTeammate` |
| **Process model** | In-process, blocks parent's turn | In-process tokio task, concurrent with parent | Multiple `thclaws --team-agent` processes, tmux-orchestrated |
| **Parallelism** | Serial (recursion depth, not concurrency) | Concurrent with main, but each side-channel sequential | Truly concurrent |
| **Main's history** | Tool result lands in parent's context | Untouched — result is a separate side bubble | Untouched — teammates have their own sessions |
| **Isolation** | Shared sandbox | Shared sandbox | Optional git worktree per teammate |
| **Cancel** | Inherits parent's cancel | Independent — own cancel via `/agent cancel <id>` | Independent — `kill` the teammate process |
| **Messaging** | None — child returns a string | None — final result delivered as event | Filesystem mailbox + task queue |
| **Overhead** | Negligible | Negligible | High — spins up 1+ extra processes |
| **Use for** | Model-driven sub-problem reduction | User-driven side errands while main works | Parallel streams of long-running work |

Rule of thumb:

- **Default to model-driven `Task`** — let the parent agent decide when to delegate. Lowest ceremony.
- **Reach for `/agent`** when *you* (the user) know specifically what you want a specialist to do AND want to keep working in main while it runs. "Translate this file to Thai while I keep coding."
- **Reach for teams** when the work genuinely fans out across long-running parallel streams (build backend + frontend + ops in three parallel processes).
