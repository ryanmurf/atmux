# A2: Conversation tools, Qwen summaries, rolling digests, auto titles, easy rename

Status: implementation in progress on `feat/agent-conversation`

Read `features/agent-control-plane.md` first. This record is your brief; keep it updated with
progress, evidence, and gate checkboxes.

## Goal

Any agent or human can load a conversation with filters, get a short summary, and find every
session by meaning, for both Claude Code and Codex. Each session keeps an up-to-date tool-free
digest (a compaction-style summary) produced by free local Qwen, and the UI shows an automatic
title/description with very easy renaming.

## Existing code to build on

- `src/transcript.rs`: bounded parsing for both CLIs, kinds `message`, `tool`, `compaction`,
  roles `user`, `assistant`, `subagent`, `system`; `read()` already maps a pane to its log.
- `src/control.rs`: federated transcript reads (`/api/v1/panes/{id}/transcript`), session rename and
  description (`update_session`, the `@atmux_description` pane option), `SessionSummary`
  (now includes `session_key`).
- `src/summary.rs`: the existing headless summary used by "Duplicate with summary".
- `src/mcp.rs`: the stateless MCP server (rmcp).
- `web/app.js`: the rail, rename/describe UI from commit `1048562`, `drawConversation`.

## Scope

1. **MCP `agent_conversation`.** Arguments: `id` (pane or session key), `include` (any of `human`,
   `agent`, `subagent`, `tools`, `compaction`; default human+agent+subagent+compaction),
   `after` cursor (entry id), `limit`, and `max_bytes`. Returns entries plus `next` and
   `truncated`. Works across federation through the coordinator. Tool inputs/outputs only when
   `tools` is requested, keeping the existing redaction.
2. **MCP `agent_summary`.** Returns `{title, description, digest, digest_updated_at, status,
   needs_input_reason?}`; if the digest is stale it schedules a refresh and returns the cached
   value with `stale: true` rather than blocking.
3. **Summarizer** (`src/summarizer.rs`), enabled by `[summaries]` (off by default):
   - OpenAI-compatible chat completions client using the existing hyper/rustls stack: endpoint
     `http://192.168.0.124:8091/v1`, model `qwen3.8-flash-next` (262K context), optional API key
     from env/file, request timeout, global concurrency 1-2, per-session minimum interval, and a
     daily request budget. Plain HTTP is allowed only for explicitly configured private/LAN
     hosts; document that.
   - Runs on the node that federates (intended: the coordinator), reading conversations through
     the existing federated transcript path, so owners need no new access.
   - **Rolling digest:** when a session's transcript hash changes and it is idle (or after the
     minimum interval), summarize only the new non-tool entries together with the previous digest
     into a new digest (bounded, about 1,500 words), plus a title (at most 6 words) and a
     description (at most 120 characters, the same rules as `valid_session_description`). Include
     compaction summaries as context; never include tool output, secrets, or system prompts.
     Treat model output as untrusted text: strip control characters, enforce bounds, reject
     instructions to change anything.
   - Persist digests keyed by `session_key` in a small store under the data directory (JSON files
     or SQLite behind the existing `pulse` feature gate; pick one that builds on all owners).
   - Emit `agent.summary_updated` once A1's event log exists; until then expose an internal hook
     the lead can wire up (keep it a single call site).
4. **Automatic description.** When a session has no user-set description, set its
   `@atmux_description` from the generated description and mark the source as automatic (add a
   `@atmux_description_source` pane option `auto|user`). A user edit always wins and is never
   overwritten.
5. **UI.**
   - Show the automatic description in the rail with a subtle "auto" marker, and the digest in a
     collapsible "Summary" block at the top of Conversation.
   - Easy rename: double-click (or long-press on touch) the session name in the rail or header to
     edit inline; Enter saves, Escape cancels; `F2` renames the selected session. Offer a
     "Suggest" action that fills in the generated title. Keep the existing rename validation and
     federation path.
6. **Search groundwork.** Add an MCP `sessions_find` that ranks live and recently seen sessions by
   simple term matching over name, description, title, and digest. A3 adds archived sessions and
   H2 adds full herodevs search; keep the result shape `{session_key, machine, pane, name, title,
   description, score, snippet}` so they can extend it.

## Acceptance

- Unit tests: filter and cursor semantics for both harness fixtures, digest prompt assembly
  (tool entries excluded, compaction included), output sanitization and bounds, auto-versus-user
  description precedence, and budget/interval scheduling.
- A summarizer test against a local fake OpenAI-compatible server (no network in tests).
- MCP tool tests; browser unit tests for inline rename, `F2`, Escape, and Suggest; browser suite
  still green.
- `cargo fmt`, zero-warning clippy, `cargo test --all-features`, `node --check web/app.js`,
  `node --test web/*.test.mjs tests/navigation.test.mjs`, and the three browser suites.
- A manual check against the real endpoint (`curl http://192.168.0.124:8091/v1/models` works from
  Tron) recorded in this file, summarizing one real transcript fixture you create (not a live
  pane).


## Implementation decisions and evidence

- Conversation pagination operates on the owner-redacted bounded transcript window. Cursors
  are applied before filtering; an expired cursor returns a conflict so clients explicitly reload.
  `next` is the last returned entry even on a final page, allowing polling for later entries.
  A too-small byte ceiling returns an empty truncated page, and callers can increase it.
- Summaries will use private JSON files rather than Pulse/SQLite, keeping no-default-feature
  owners supported. Only configured federating/coordinator nodes run the worker.
- Read the lead's herodevs platform contract on 2026-09-30. The search builder will return a
  plain `ATMUX_SESSION` document keyed by `session_key`, bounded below 8 KiB. The lead must
  reconcile the H2 `HdEntityChangeEvent` shape and wire its publication to A1's producer on
  `entity-change`; this workstream does not configure or contact Redpanda.

### Gates

- [x] Conversation filtering/cursors/bounds tested with both native harness fixtures.
- [ ] Durable rolling digest, prompt/output safety, scheduling and fake server tests.
- [ ] Auto/user description precedence and federation tests.
- [ ] MCP summary/search tests.
- [ ] UI inline rename, F2, Escape, Suggest, auto marker and summary tests.
- [ ] Full Rust/Node/browser acceptance commands.
- [ ] Real local Qwen endpoint check using a fixture only.
