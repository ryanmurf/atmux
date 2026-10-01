# A3: Session registry and archive on close

Status: implemented on `feat/session-registry`; final acceptance checks in progress (2026-09-30)

Read `features/agent-control-plane.md` first. This record is your brief; keep it updated with
progress, evidence, and gate checkboxes.

## Goal

A durable record of every agent session and its current state, on each owner and centrally on the
coordinator ("home"). Closing a tmux session archives it: its record, final state, git position,
and native conversation log are preserved so it can be found and resumed later, even from another
computer (A4 builds the resume).

## Scope

1. **Record** (`src/registry.rs`), keyed by `session_key` (already minted in the base commit):
   machine; session name; description and its source; harness; profile; mode, model, effort and
   service tier; cwd; project (credential-free remote URL, branch, HEAD commit, dirty flag);
   native identity (Claude config root and session id, or Codex home and thread id: owner and
   coordinator only, never sent to browsers or MCP); timestamps (created, last active, last seen,
   closed, archived); state (`running`, `exited`, `closed`, `archived`); last status and
   needs-input reason; digest reference (A2). Bounded string sizes; serde with forward-compatible
   defaults.
   - Use the existing native identity lookups in `src/transcript.rs`
     (`native_resume_target`, `claude_resume_target`) rather than new heuristics.
2. **Owner registry.** Updated from pane scans (cheaply: only on change), persisted atomically
   under the state directory (one JSON file per session or a single SQLite file; it must build on
   Linux x86_64, Linux ARM64, and macOS). Bounded count with retention for archived records.
3. **Archive on close.** When a session's pane or tmux session disappears, the owner writes the
   final record and an **archive bundle**: a manifest plus a copy of the native log (Claude
   `projects/<encoded cwd>/<id>.jsonl` and its sibling `<id>/` directory if present; Codex rollout
   file), compressed with a pure-Rust codec if you add one (justify the dependency), size-capped
   (configurable; default 256 MiB per bundle) and checksummed. Copy descriptor-safely without
   following symlinks (see `src/workspace.rs` for the established pattern). A session killed from
   the dashboard archives the same way. Emit `session.closed` and `session.archived` once A1's
   event log exists (single call site the lead can wire up).
4. **Central registry on the coordinator.** The coordinator pulls each owner's registry changes
   (owner endpoint `GET /api/v1/registry?after=<cursor>` long-poll; mTLS + token like existing
   federation) and archive bundles (`GET /api/v1/registry/{session_key}/bundle`, streamed,
   bounded), and stores them under its data directory with a total quota and oldest-first
   eviction of bundles (never of records). Configure with `[registry]` (off by default). The
   Helm chart may need a persistent volume for this; document what the lead must add, but do not
   change the cluster.
5. **APIs.** MCP `sessions_search` (state, machine, project, text over name/description/title;
   extend A2's `sessions_find` shape if it exists, otherwise use the shape documented in A2's
   brief) and `session_get` (record without native identity). REST equivalents for the UI.
6. **UI.** A "Sessions" history view listing running, closed, and archived sessions across all
   machines with filters and text search; each row shows name, description, machine, project,
   last activity, and state. The Resume action is provided by A4; leave a clearly named hook.

## Out of scope

Bundle import/resume and phone-home restore (A4); summaries (A2); events (A1).

## Acceptance

- Unit tests: record updates from scans, change detection, atomic persistence and recovery from a
  torn write, retention, bundle creation (bounds, checksum, symlink refusal, Claude sibling
  directory, Codex rollout), and redaction of native identity from every API/MCP response.
- Disposable-tmux integration test: create a fake agent pane with a fixture Claude log, kill the
  session, and observe an archived record and bundle.
- Federation test with a fixture owner for registry pull and bundle transfer.
- `cargo fmt`, zero-warning clippy, `cargo test --all-features`, `node --check web/app.js`,
  `node --test web/*.test.mjs tests/navigation.test.mjs`, and the browser suites if the UI
  changes.


## A3 implementation decisions and merge contract

- Storage is one private, bounded JSON record per stable key, with descriptor-relative temporary
  writes, file sync, rename, directory sync, and a process ownership lock. A corrupt committed
  file fails startup; torn temporary files are removed. This avoids a new database dependency
  and works independently of the optional Pulse feature.
- `[registry]` is optional and disabled by default. `directory` defaults to the platform state
  directory's `registry/`; `max_owner_records = 10000`, `archived_retention_days = 90`,
  `bundle_max_bytes = 268435456`, and `bundle_quota_bytes = 4294967296`. Directories must be
  owner-private. Active records are never evicted to admit another live pane; if that limit is
  reached the owner defers discovery until space exists. Central records are never evicted.
- The existing strict `native_resume_target` now also returns its already-selected log path.
  Registry scans reuse it, preserve the last exact binding after CLI exit, and clear an ambiguous
  binding while a new/live process is being inspected. No transcript path guessing was added.
  Native/provider roots and ids live in `StoredRecord`, never in the public `SessionRecord`.
- Git position reads have output/deadline limits, disable optional locks and fsmonitor, and strip
  remote userinfo, SSH usernames, query strings, and fragments. Timestamps are Unix milliseconds.
  Expensive native/model/git enrichment and heartbeat persistence are limited to once per 30s
  per session (immediate on first discovery/process replacement); observable metadata changes
  are persisted immediately.
- Archives are standard `.tar.gz` files, containing `manifest.json` and `native/<path relative to
  provider root>`. Claude's sibling directory includes empty directories and nested logs. The
  manifest schema is `atmux.session.archive/v1`, with the final stored record and per-entry
  SHA-256/size/directory metadata. The bundle's SHA-256 is its id. `tar` supplies a standard,
  portable container; `flate2` explicitly uses its pure Rust backend (`miniz_oxide`), avoiding
  native codec toolchains on Linux ARM64/macOS. Source traversal uses pinned descriptors and
  rejects symlinks, hardlinks, special files, excessive depth/count, and concurrent file changes.
  Both native/manifest bytes and compressed output have the configured size cap.
- Closing is detected only from a successful owner scan, including dashboard kills and recovery
  after an owner restart. Bundle errors leave a durable `closed` record with a pathless retry
  notice, retried every 30s; no partial bundle is published. An unmapped/unsupported native log
  yields a valid record-only archive with `bundle.native_log = false`, making the limitation
  explicit without substituting another conversation.
- Federation pages use an epoch/revision cursor and at most 16 records. Restarting an owner
  causes a complete replay. Registry workers share the existing mTLS/bearer transport, follow
  discovered peers' lifecycle, and stream bundles with byte limits and SHA-256 verification.
  Only the configured node token can export private records/bundles; proxy credentials,
  unauthenticated loopback, Origin headers, and browser Sec-Fetch headers cannot cross that
  boundary. A missing/evicted owner bundle does not block later records.
- Bundle quota eviction is oldest first and leaves records intact. Artifact creation/transfers
  serialize and reserve space before writing, keeping staged bytes within the total quota.
  Local archiving reserves the configured maximum, a conservative choice that can evict a
  bundle earlier than strictly necessary; this avoids exceeding quota during compression.
  `bundle.available` reports actual artifact availability in browser/MCP history responses.
- REST history: `GET /api/v1/session-history` and `GET /api/v1/session-history/{session_key}`.
  MCP: `sessions_search` and `session_get`. Search parameters are `state`, `machine`, `project`
  (remote/root substring), `text` (name/description/title substring), `cursor` (last stable key),
  and `limit` (default 50, capped at 100). Search is case-insensitive, machine/state are exact,
  and responses are `{ sessions, next_cursor }`. A2's separate brief/`sessions_find` did not
  exist in this worktree; these explicit bounded parameters are the reconciliation contract.
- UI: Sessions view at `?view=sessions`, bounded filters/pagination, current public metadata,
  30s refresh, Open for a live pane, and a disabled Resume until A4 installs the clearly named
  `window.atmuxSessionResume({ session_key, machine })` hook. Rendering uses text nodes and
  navigation preserves the existing draft/editor guards and mobile Back behavior.
- A1: call `Registry::set_event_sink` once after constructing the control plane. One emission
  call site invokes it after durable `session.closed` / `session.archived` transitions. The
  callback receives a public record and can construct A1's envelope; A3 does not add an event
  spool or Redpanda sink. This is the explicitly deferred wiring in the brief.
- A2: `Registry::set_digest_reference` persists a bounded digest id/version; scans retain it.
  Reconcile `[summaries]`/`[events]` config additions and MCP/history naming at integration.
- A4: use `Registry::native_identity` on the server, `Registry::bundle_file` for a verified
  descriptor, and the public `BundleManifest` / `ManifestFile` types. Native bundle layout is
  provider-root-relative for path translation. Keep setting `@atmux_session_key` before scan
  when importing/restoring; wire the Resume hook without exposing native ids to the browser.
- Lead deployment work (not performed): add a writable persistent volume mount and configure
  `[registry].directory` on the coordinator; size PVC for bundle quota plus durable records and
  filesystem overhead, enforce private permissions, enable owners separately, and retain the
  existing mTLS/node credentials. No Helm/cluster change is required to merge this implementation.

## Gates and evidence

- [x] Implementation: registry, archive, federation, REST/MCP, and Sessions history are present.
- [x] Focused Rust: `cargo test --all-features --lib registry::tests` — 12 passed.
- [x] Disposable owner: `ATMUX_REQUIRE_TMUX=1 cargo test --all-features --test session_registry`
  — external and dashboard closes both archived the correct fixture native log/key (1 passed).
- [x] `cargo fmt`; `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- [x] `node --check web/app.js`; `node --test web/*.test.mjs tests/navigation.test.mjs` — 186 passed.
- [x] `node --test tests/navigation_browser.mjs` — 2 passed, including new history coverage.
- [x] Mobile viewport and mobile/Pulse browser suites — 9 passed combined. Quick Talk was then
  run with its required disposable web/tmux/Chrome fixture and passed. Its script is a fixture
  client, not an independently runnable `node --test` suite.
- [ ] Full `cargo test --all-features`: the first run in this worktree reached 683 passing tests,
  6 ignored, and exposed 10 pre-existing recovery failures because this worktree root is 0775;
  one parallel self-update test hit a fork/exec `ETXTBSY` race. Final verification will use a
  private local checkout and `RUST_TEST_THREADS=1`, with disposable sockets required.
- [ ] Platform runtime matrix / independent security and Fable/Claude review: lead-owned release
  gates per `features/README.md`. This record stays active; no running fleet, services, cluster,
  agents, or default tmux server were touched.
