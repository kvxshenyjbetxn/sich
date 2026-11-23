# Phone chat view — spec

Status: experiment, started 2026-11-23. It is compared against the terminal view, which renders Kitty images. Keep it self-contained so it can be removed cleanly:
- gateway module `chat/`;
- the proto variants below;
- mobile `src/chat/`;
- the toggle button in the terminal header.

Reference: Orca's "native chat" over a terminal tab (transcript-based). Orca's structured-session mode is NOT used: Sich is not an agent harness.

## Idea
For a terminal that runs Claude Code or Codex, the phone can switch "Термінал ⇄ Чат":
- The chat is built **read-only from the agent's local transcript file**. Reading the agent's local session/transcript files is allowed by CLAUDE.md. Read only the needed fields and fail soft.
- Replies still go into the PTY through the existing `SendText`, with images as separate pastes.
- While the chat is shown, the phone does not resize the PTY.

## Transcript lookup (gateway)
- Input: the terminal's `AgentStatus { kind, session_id }` and `cwd` (already in the daemon data; no daemon/sich-proto change).
- Claude: `~/.claude/projects/<cwd bucket>/<session_id>.jsonl`. The cwd bucket is the path with every non-alphanumeric char replaced by `-`. Fallback: scan `~/.claude/projects/*/<session_id>.jsonl`. `CLAUDE_CONFIG_DIR` overrides `~/.claude`. Reuse whatever the desktop already has (see `crates/app/src/agents_panel.rs`, `claude_images.rs`, `crates/daemon/src/model_probe.rs`) by moving pure helpers to a GPUI-free crate if needed.
- Codex: the rollout file for the session id under `$CODEX_HOME` or `~/.codex/sessions/YYYY/MM/DD/rollout-*-<id>.jsonl` (see `crates/daemon/src/codex_rollout.rs`).
- Other agents: chat unavailable.
- Watch: tail the file incrementally from the last byte offset. Use a 300 ms poll (simple, robust on Windows) plus a re-scan when the file shrinks or is replaced. Debounce updates to ≤ 1 per 150 ms.

## Message model (decoders, fail soft)
- `ChatMessage { id: String, order: u64 /* byte offset of the JSONL line */, role: ChatRole, timestamp_ms: Option<u64>, blocks: Vec<ChatBlock> }`
- `ChatRole { User, Assistant, Tool, Reasoning, System }`
- `ChatBlock`:
  - `Text { markdown: String }`, capped at 64 000 chars with "… (обрізано)";
  - `ToolCall { call_id: String, name: String, summary: String }`, where summary is a one-line preview such as the command, file path or pattern, ≤ 300 chars;
  - `ToolResult { call_id: String, output: String, is_error: bool }`, capped at 4 000 chars;
  - `Image { image_ref: String, alt: String }`.
- Unknown or unsupported lines are skipped. Claude `isMeta` lines are skipped unless they carry an image. Codex: dedupe `response_item` vs `event_msg` duplicates.
- Images in transcripts:
  - inline base64 (user-pasted images): the gateway keeps them server-side;
  - file paths.
  - `image_ref` is an opaque string the phone passes back to fetch bytes.

## Protocol (sich-remote-proto) — `REMOTE_PROTO_VERSION = 2`, `MIN_COMPATIBLE = 2`
Variants are appended after the existing ones.
- PhoneMsg:
  - `10 ChatSubscribe { term: TermRef, limit: u32 }` — snapshot of the last `limit` messages, then live updates;
  - `11 ChatUnsubscribe { term: TermRef }`;
  - `12 ChatLoadEarlier { term: TermRef, before_order: u64, limit: u32 }`;
  - `13 ChatImage { req_id: u64, term: TermRef, image_ref: String }`.
- GatewayMsg:
  - `11 ChatSnapshot { term: TermRef, status: ChatStatus, messages: Vec<ChatMessage>, has_more: bool }`;
  - `12 ChatUpdate { term: TermRef, messages: Vec<ChatMessage> }` — new or replaced messages, matched by `id`, ordered by `order`;
  - `13 ChatEarlier { term: TermRef, messages: Vec<ChatMessage>, has_more: bool }`;
  - `14 ChatImageData { req_id: u64, result: Result<ChatImageBytes, String> }`, where `ChatImageBytes { mime: String, data: Vec<u8> }`; the gateway downsizes to ≤ 1600 px and ≤ 2 MiB.
- `ChatStatus { Ready, Unavailable { reason: String }, Pending }`:
  - `Pending` = the agent has no session id or file yet. The gateway re-checks and sends a new snapshot when it appears.
- Capability string `"chat"` in `Welcome.capabilities`.

## Phone UI
- Terminal header: a "Чат / Термінал" toggle. It is shown when the terminal's agent is Claude or Codex and the gateway has the `chat` capability.
- Chat view (desktop look, dark theme):
  - user messages right-aligned in an accent-tinted bubble;
  - assistant messages full-width rendered markdown (code blocks with horizontal scroll, lists, tables, links);
  - reasoning collapsed in muted italic ("Роздуми…");
  - each tool call is a compact one-line row: icon + name + summary, tap to expand result, red when `is_error`;
  - images as thumbnails, tap to open full screen; fetched lazily with at most 2 at a time.
- Auto-scroll to bottom when already at bottom; otherwise a "↓ нові повідомлення" pill.
- "Завантажити раніше" at the top.
- Composer is the same as in the terminal view (text + images + send), plus a "Стоп" (Esc) button while the agent is Working.
- Agent state line on top: Працює / Чекає / Готово.
- Optimistic echo of the sent text until it appears in the transcript.

## Tests
- Gateway:
  - decoders on trimmed real-format fixtures (Claude and Codex lines: text, tool use/result, thinking, image base64, isMeta, unknown types);
  - lookup (bucket name, fallback scan, CLAUDE_CONFIG_DIR / CODEX_HOME in temp dirs);
  - tailer (append, partial last line, truncate/replace);
  - integration: phone ChatSubscribe → snapshot → append a line to the temp transcript → ChatUpdate; image fetch.
- Proto: round-trip of all new variants; variant indexes.
- Mobile:
  - vitest for message list merge (id replace, order sort, earlier pages);
  - markdown sanitizing (no raw HTML), tool summary formatting, image fetch queue (max 2).

## Gateway implementation notes (crates/gateway/src/chat/)
- Modules: `locate` (transcript lookup), `decode` (line decoders), `tail` (incremental reader), `state` (decoded messages, paging), `image` (refs -> bytes), `session` (async watcher per phone+terminal). Wiring: `Hub::chat_*` and `phone.rs`.
- Message ids: Claude = line `uuid`; Codex = `c<offset>`. A line with several roles (e.g. thinking + text + tool_use) yields several messages `id`, `id#1`, ... that share the line's `order`; paging never splits them. Pages are also capped by `limit <= 300` and about 6 MiB.
- Roles: Claude `tool_use`/`tool_result` -> `Tool`; `thinking` -> `Reasoning`. Codex `reasoning` summary -> `Reasoning`; `function_call`, `custom_tool_call`, `local_shell_call`, `web_search_call` and their outputs -> `Tool`. Sidechain (subagent) Claude lines and developer/injected context are skipped.
- Codex dedupe: only `response_item` records are decoded; `event_msg` duplicates (and newer `item_completed`) are ignored.
- `image_ref` = `i<line offset>.<n>`: the n-th image of the transcript line at that byte offset. The gateway keeps no image data: `ChatImage` re-reads and decodes that line (inline base64, or a file path the transcript names, e.g. `[Image: source: ...]`, Codex `input_image` paths), then downsizes (<= 1600 px, <= 2 MiB, PNG/JPEG). Refs that do not resolve to an image are errors.
- Status: the resolver is re-run every 2 s; `Pending`/`Unavailable` snapshots are sent only when the status changes. A shown transcript stays until another one is ready (new session id after /clear). File truncation or replacement sends a fresh `ChatSnapshot`.
- One tailer per (phone, terminal), not shared across phones.
