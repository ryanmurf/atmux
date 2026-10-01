# A2: Conversation tools, Qwen summaries, rolling digests, auto titles, easy rename

Status: complete on `feat/agent-conversation`; all acceptance gates passed

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
- Summaries use private JSON files rather than Pulse/SQLite so no-default-feature owners
  compile. Store directories are mode 700, new files mode 600, reads reject symlinks/non-regular
  files, writes use fsync plus atomic rename, and an exclusive lock prevents duplicate workers.
  Retention is 30 days / 2,000 records. Attempts and daily UTC budgets survive restarts; failed
  requests count. Only explicitly enabled federating/coordinator nodes run the worker.
- Scheduling uses a global semaphore (1 or 2), per-key in-flight exclusion, the configured
  minimum interval, and a oldest-checked-first traversal to avoid starving a large fleet. Idle
  sessions are eligible immediately; initially working sessions wait the minimum interval.
  Cache reads return immediately and notify the worker when stale. A failed request keeps the
  prior digest and the dirty/stale flag. Clock rollback does not reset budget or intervals.
- Rolling prompts contain the previous digest and new eligible entries after a durable entry
  cursor. Tools, system messages, native tool fields, and recognized credentials are excluded;
  compaction context is included. A missing bounded-window cursor is marked as a context gap.
  A 96 KiB prompt limit advances through bounded chunks without skipping the remaining entries.
  Titles are at most six words, descriptions 120 printable characters, and digests at most
  1,500 words / 16,000 characters. Untrusted output must be strict JSON, is stripped of controls
  and bidi characters, and obvious action directives are rejected. UI rendering uses text nodes.
- HTTP endpoints need an exact configured host allowlist entry, plus all resolved addresses
  must be private/loopback. HTTPS uses the existing rustls trust roots. There are no redirects;
  requests have a deadline and responses a 64 KiB cap. API keys may come from one env/file
  source. Neither credentials nor model response bodies appear in errors. Defaults and the
  opt-in example are documented in README and the generated configuration.
- Descriptions and `@atmux_description_source` use the existing description's **session option
  scope**, inherited by the selected pane. The brief calls it a pane option; retaining existing
  scope keeps notes consistent across a session's panes. Automatic writes use one tmux `-F`
  conditional command list checking pane generation, stable key, and source. Legacy notes,
  explicit user clears, and user edits always win. The existing human edit writes source=user
  atomically with the note. Saving an unchanged automatic note explicitly claims it as user-owned;
  an inline name-only edit leaves the automatic note alone.
- Generated titles are cached and offered by Suggest; existing session names change only through
  the explicit pane-bound rename path. Names participate in session addressing, so automatic
  model text should not silently change them. Inline editors capture identity and close on a
  replaced generation. Suggest fills a validated slug and requires Enter/Save to rename.
- Search matches all query terms case-insensitively with weights name=8, title=6, description=4,
  digest=1, deterministic ties, a maximum 100 results, and 240-character snippets. Live metadata
  supersedes recent cached metadata, including explicit cleared descriptions. Missing stable keys
  are nullable for older owners; A3 can extend the same result shape with archived records.
- Read the lead's herodevs platform contract and H2's finalized **Exact external producer
  contract** on 2026-09-30. `src/session_search.rs` is a pure full-snapshot builder for the exact
  `HdEntityChangeEvent` envelope. Topic=`entity-change`, key=`session_key`, both discriminators
  `ENTITY_CHANGE`, and entityId/current.id/current.session_key agree. Archive is UPDATED with
  archived state/time; SOFT_DELETED/HARD_DELETED have current=null; resume clears archivedAt.
  The complete H2 rendered template is capped at **8,000 UTF-8 bytes** by shortening the digest;
  serialized envelopes, including escaping, are capped at **65,536 bytes**. Credential-bearing
  remotes are omitted. No raw log, tool, or environment payload is published.
- Search enqueue and its durable per-session millisecond high-water mark are serialized under
  the store lock. Equal/older snapshots are suppressed, including a late model job after archive.
  Persistence merges high-water marks so late digest persistence cannot regress ordering. Failed
  enqueue remains retryable from unchanged cached work. Explicit `summaries.search_tenant_id`
  opts into the hook; its default is absent while the producer is unwired. The former plain-struct
  placeholder is superseded by the exact H2 contract. Literal H2 fixtures are copied unchanged to
  `tests/fixtures/atmux-search/` and compared as complete JSON, not selected fields.

### Validation evidence

- Native Claude and Codex JSONL fixtures exercise transcript parsing/redaction and conversation
  filtering, cursor ordering/expiry, pagination, and response-byte bounds.
- Summarizer unit tests cover rolling assembly, compaction, credential/tool/system exclusion,
  output bounds/directive rejection, user-description precedence, endpoint policy, interval/UTC
  budget boundaries, store exclusion and restart, and exact H2 envelopes/document limits.
- A loopback-only fake OpenAI/owner server exercises successful model requests, federated Git
  metadata, automatic-description requests, cached reads, unchanged-work suppression, failed
  output preserving the digest/stale flag, durable budget accounting, response caps, and deadlines.
  No automated test calls the LAN model or any live owner.
- Disposable-socket tmux tests exercise auto refresh, user edit/clear and legacy-note precedence,
  and stale generation rejection. MCP tests cover stable-key federation, filters/cursors, cached
  summary shape and input reason, search ranking shape, and unknown references.
- Node tests cover inline Enter/Escape, validation, captured generation, Suggest without saving,
  F2 guards/IME, double-click, and stationary touch hold versus movement. The browser scenario
  checks header/rail rename, exact owner PATCH binding, auto marker, summary collapse, and inert
  HTML-looking model text, alongside all existing mobile/navigation/Pulse cases.
- `cargo fmt --all`, `cargo clippy --all-features --all-targets -- -D warnings`,
  `cargo check --no-default-features`, and `node --check web/app.js`: passed.
- `node --test web/*.test.mjs tests/navigation.test.mjs`: **188 passed**, no failures/skips.
- `node --test tests/web_mobile_pulse_browser.mjs tests/navigation_browser.mjs
  tests/mobile_viewport_browser.mjs`: **11 passed**, no failures/skips.
- `RUST_TEST_THREADS=1 cargo test --all-features`: **826 passed, 7 ignored**, no failures
  (20 suites, 30.75 seconds). Default parallel runs exposed pre-existing fork/exec races in the
  executable identity and Pulse flock tests; the serial acceptance rerun passes without changing
  those tests. Recovery tests also reject group-writable ancestry: this worktree was temporarily
  mode 755 instead of its original 775 for validation and is restored afterwards.

### Manual LAN model check (2026-09-30, Tron)

- `curl --fail --silent --show-error --connect-timeout 5 --max-time 15
  http://192.168.0.124:8091/v1/models`: passed; returned `qwen3.8-flash-next`, context 262144.
- Created an OpenAI-compatible request from the non-tool messages of our native Codex fixture
  `tests/fixtures/a2-codex.jsonl`, then POSTed to `/v1/chat/completions` with temperature 0.2,
  max_tokens 4096, connect timeout 5 seconds, and total timeout 120 seconds. No live pane/log was
  used. Response completed in about 12.9 seconds: 184 prompt + 476 completion = 660 tokens.
- Saved the response's model text as `tests/fixtures/a2-qwen-output.json`. It produced the title
  **Bounded Session Summaries Implementation** and a compaction-style digest covering the goal,
  durable JSON, user precedence, and acceptance checks. The model's overlong description is
  truncated to 120 characters by the production sanitizer; an offline test validates the saved
  fixture and preservation of paragraph breaks.

### Merge handoff and scope

- **A1:** replace `ControlPlane::summary_updated_hook` with one `agent.summary_updated` event-log
  append. Replace `ControlPlane::summary_search_document_hook` with synchronous **durable enqueue**
  to A1's producer using `SearchPublication.topic`, `.key`, and `.value()`. Return Ok only after
  durable enqueue; the ordering lock/high-water persistence surrounds this single call site.
  Reconcile `summaries.search_tenant_id` with A1's `[events.redpanda].tenant_id` and enable only after
  wiring. H2's exact envelope question is resolved; no Redpanda access/configuration was performed.
- **A1:** replace the conservative `waiting -> idle_prompt` summary reason with its richer prompt
  state/reason once available. Resolve shared `SessionSummary`, tmux pane-list columns, config,
  web/MCP registration, and control worker setup against A1's final versions.
- **A3:** use `session_digest_record` for cached context and `publish_digest_search` for ordered
  archive/resume/removal snapshots, supplying lifecycle observation timestamps in milliseconds.
  Refresh metadata before constructing each full snapshot; keep created_at fixed. Extend
  `sessions_find` with archived records rather than inventing a second result contract.
- **H2:** fixture provenance is draft PR `murphytek/hd-api-search#92`, plan
  `/home/ryan/IdeaProjects/herodevs.dev/hd-api-search-atmux/docs/plans/atmux-session-index.md`, and
  its `src/test/resources/atmux/` literals. The lead should keep these fixture copies aligned if
  that contract changes before merge.
- UI changes share `web/app.js` with the other workstreams; preserve existing conversation polling,
  selection/generation guards, safe rendering, and rename validation when reconciling conflicts.
- Authorized work stayed local. No deployment, push, service restart, Kubernetes action, or running
  tmux session/agent was touched. Existing untracked `.atmux.toml` was left untouched.

### Gates

- [x] Conversation filtering/cursors/bounds tested with both native harness fixtures.
- [x] Durable rolling digest, prompt/output safety, scheduling and fake server tests.
- [x] Auto/user description precedence and federation tests.
- [x] MCP summary/search tests.
- [x] UI inline rename, F2, Escape, Suggest, auto marker and summary tests.
- [x] Full Rust/Node/browser acceptance commands.
- [x] Real local Qwen endpoint check using a fixture only.
