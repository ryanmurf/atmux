# A3: Session registry and archive on close

Status: implemented on `feat/session-registry`; local acceptance complete; lead-owned release gates pending (2026-09-30)

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
  file fails startup; torn temporary files are removed. Directory creation itself also walks pinned descriptors,
  refusing symlink ancestors before any descendant can be created. This avoids a new database dependency
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
  `bundle.available` reports local descriptor presence/size in browser/MCP history responses;
  SHA-256 is verified when opening/transferring the artifact.
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
- Initial A1 handoff (superseded by the integration follow-up below): call
  `Registry::set_event_sink` once after constructing the control plane. One emission
  call site invokes it after durable `session.closed` / `session.archived` transitions. The
  callback receives a public record and can construct A1's envelope;
  `Registry::set_needs_input_reason` accepts the five contract reasons and waiting scans retain
  that precise reason until work resumes; A3 does not add an event
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
- [x] Focused Rust: `cargo test --all-features --lib registry::tests` — 15 passed (plus a separate actual MCP-tool redaction test).
- [x] Disposable owner: `ATMUX_REQUIRE_TMUX=1 cargo test --all-features --test session_registry`
  — external and dashboard closes both archived the correct fixture native log/key (1 passed).
- [x] `cargo fmt -- --check`; `cargo clippy --all-targets --all-features -- -D warnings` — clean,
  zero warnings on the final implementation and Git regression.
- [x] `node --check web/app.js`; `node --test web/*.test.mjs tests/navigation.test.mjs` — 186 passed.
- [x] `node --test tests/navigation_browser.mjs tests/mobile_viewport_browser.mjs
  tests/web_mobile_pulse_browser.mjs` — 11 passed, including the new history/navigation coverage.
- [x] `node tests/quick_talk_browser.mjs <fixture-url> <debug-port>` — passed with an isolated
  config, a disposable tmux socket, locally spawned test web process and headless Chrome profile,
  and random loopback ports. Cleanup stopped only those spawned test processes/sessions. This
  script is a fixture client, not an independently runnable `node --test` suite.
- [x] Full `cargo test --all-features` on frozen source `2731044` — 824 passed, 7 existing
  ignored, zero failed. Run from private local checkout
  `/home/ryan/atmux-a3-validation-20260930` with `ATMUX_REQUIRE_TMUX=1`,
  `ATMUX_TMUX_SOCKET_NAME=atmux-ci-a3-final-git-20260930`, `RUST_TEST_THREADS=1`, and `TMUX` /
  `TMUX_PANE` removed. Full log: `/home/ryan/atmux-a3-validation-20260930/final-all-features-git.log`.
  The original worktree is 0775, which its existing recovery safety tests reject; a parallel
  self-update test also hit a baseline fork/exec `ETXTBSY` race. A private local clone and serial
  execution satisfy those test prerequisites without weakening safety checks or changing the
  original directory's permissions. Nested filtered subprocess checks are excluded from the
  aggregate above. Registry, federation, external close and dashboard close tests all ran.
- [x] `cargo +1.88 check --all-features --locked`; `cargo check --no-default-features --locked`
  — passed.
- [x] Registry portability checks: `cargo check --manifest-path
  /home/ryan/atmux-a3-portability-20260930/Cargo.toml --target x86_64-apple-darwin` and the same
  command with `--target aarch64-unknown-linux-gnu` — passed. The leaf harness includes the
  actual `src/registry.rs` and real Rust archive/filesystem dependencies, with surrounding atmux
  modules stubbed. This checks platform types and dependency portability, not the full native
  binary or runtime. Linux x86_64 uses the real full build and integration tests above.
- [ ] Platform runtime matrix / independent security and Fable/Claude review: lead-owned release
  gates per `features/README.md`. This record stays active; no running fleet, services, cluster,
  agents, or default tmux server were touched.


### Completed implementation commits

- `2d36953` — durable records, native archives, federation, REST/MCP, Sessions view and tests.
- `8135bbb` — storage/paging hardening, durable precise reasons/digests, and missing-owner-bundle
  recovery. Pages clone only returned records; descriptor-relative mkdir refuses symlink ancestors.
- `2731044` — regression coverage for the final branch, HEAD and dirty state captured at close,
  credential-free project remotes, and the identical Git position in the archive manifest.


### Merge reconciliation and bounded limitations

- A1 must preserve the installed lifecycle bridge and feed precise needs-input reasons; A2
  must feed digest references. Automatic description provenance is now preserved by scans,
  and the integration adapter overlays authoritative descriptions on cached digest context. None of the other workstreams' event spool, summaries, or resume behavior is
  duplicated here.
- A4 must wire the named browser Resume hook, use server-only native identity/verified bundle
  descriptors, and preserve the stable key before scanning a restored pane. Registry imports
  currently arbitrate cross-owner updates by `last_seen_ms`; integrate A4's desired-owner/lease
  decision before relying on clock ordering during ownership transfers.
- Shared merge points are `[registry]` in `Config`, registry initialization and watcher lifecycle
  in `control.rs`, the REST router merge, MCP tools, the returned native log path in
  `transcript.rs`, Cargo dependencies/features, and Sessions navigation in `web/app.js`. New
  impl blocks stay before test modules to satisfy clippy rather than at the literal file end.
- Retention bounds owner records; central records deliberately persist indefinitely. The total
  quota bounds archive artifacts, including staged bytes, rather than durable JSON records.
  Size the coordinator volume for both. A missing exact native binding produces an explicit
  record-only archive; source safety or size failures retain `closed` and retry instead.
- Operational actions remain with the lead: enable owner/coordinator config, supply a private
  writable persistent directory/PVC, and run the native platform/runtime and independent
  security/Fable release gates. This active record is not moved to `features/completed/` until
  those separate gates pass. No deploy, push, service restart, cluster change, or operation on a
  running fleet tmux session/agent was performed; the original untracked `.atmux.toml` remains
  untouched.


## Lead integration follow-up (base `13fe19a`)

Status: event/search wiring complete and locally accepted on `a52d968` (2026-09-30).

- `registry_integration.rs` installs a weak control-plane callback before the first owner scan.
  Every durable registry commit, including authenticated remote imports, carries a public
  previous/current projection plus pane generation and process id. Native identity is absent.
  This extends the original terminal-only callback so search also sees discovery, rename,
  description/provenance, Git/project changes, process exit and resume. Heartbeat-only writes
  and bundle-availability changes do not produce redundant search documents.
- Local transitions emit `agent.exited`, `session.closed` and `session.archived` through
  `ControlPlane::emit_agent_event` -> `EventService::emit`. The complete envelope carries the
  stable key, machine-qualified pane, generation, harness/profile/model, cwd and credential-free
  final project position. Detail carries `state`, process id, close/archive timestamps and
  `archive_bundle_id` on archive. PID replacement also closes the previous process generation.
- Native SessionEnd and status-derived exits carry the same process id. The existing bounded
  durable event spool deduplicates exits by key/generation/PID and terminal session events by
  key/generation/transition timestamp, so registry-first, native-first, scan-first and retry
  delivery cannot append duplicate lifecycle events within the retained spool; distinct relaunches still emit exits.
- Search uses `ControlPlane::session_digest_record` and `publish_digest_search`. Registry
  metadata overlays cached model title/digest and cursors; without a digest the title is the
  session name, description comes from the registry, and digest is empty. Explicitly cleared
  descriptions remain empty, and scan-derived automatic provenance remains `auto`. Archival
  uses `SearchChange::Updated`, state `archived`, and a non-null archive timestamp in the full
  H2 snapshot. Publication times advance beyond the durable search high-water mark so close
  and archive within one wall-clock millisecond both publish and late model jobs stay stale.
- Accepted lifecycle metadata is cached alongside the freshest model output; no-digest cache
  entries obey A2's existing count/retention limits. A search tenant and enabled summaries
  service remain required; absence preserves existing opt-in behavior. Remote snapshots only
  publish search, never another owner's lifecycle events; ownership transfer likewise cannot
  emit the previous owner's process exit. A1 owns transport/fleet append.
- The callback completes durable publication before the next mutation. A failure retains one
  pending projection for retry before the next scan/import/commit, preserving bounded memory
  and ordering; the registry write itself remains durable. Restart replays existing snapshots
  to search without inventing historical events. This callback must not call back into the
  registry while its transaction lock is held.
- The A1-owned `summary_search_document_hook` and coordinator fleet-append hook bodies were
  left unchanged. No UI edits, deployment, push, service restart, cluster change or running
  tmux/agent operation is included in this follow-up.

Follow-up acceptance evidence on frozen implementation `a52d968`:

- [x] `cargo fmt -- --check` — passed.
- [x] `cargo clippy --all-targets --all-features -- -D warnings` — zero warnings.
- [x] `cargo test --all-features --lib registry` — 23 passed, including 7 integration tests
  and bounded retry coverage. Focused `events::` — 12 passed, `summarizer::` — 9 passed,
  `session_search::` — 4 passed; all are also included in the final complete suite.
- [x] `ATMUX_REQUIRE_TMUX=1 cargo test --all-features --test session_registry` — passed,
  with both fixture close paths and their real owner event feeds verified.
- [x] `cargo test --all-features` — 867 passed, 7 existing ignored, zero failures. The same
  private validation checkout and serial-test prerequisites documented above were used,
  with `ATMUX_REQUIRE_TMUX=1`, `ATMUX_TMUX_SOCKET_NAME=atmux-ci-a3-wired-20260930`,
  `RUST_TEST_THREADS=1`, and `TMUX`/`TMUX_PANE` removed. Full log:
  `/home/ryan/atmux-a3-validation-20260930/a3-wired-all-features.log`.
- [x] `node --check web/app.js` — passed;
  `node --test web/*.test.mjs tests/navigation.test.mjs` — 193 passed.
- [x] Protected A1 hook comparison with `13fe19a` — unchanged. No UI changes were made, so
  the conditional browser-suite rerun was unnecessary; original browser evidence remains above.
- [x] Commit `a52d968` — `Wire registry lifecycle events and complete search snapshots`.

Lead merge points for this follow-up: preserve the ordered callback installation before the
first scan and its projection-only contract; merge the new DigestRecord adapter and accepted
snapshot metadata/high-water logic in `summarizer.rs`; retain the exit PID in A1 native/status
emission and lifecycle deduplication in the event spool. A1 still supplies the search transport
and remote fleet-append behavior through its own hook bodies. The existing platform/runtime
and independent release reviews remain lead-owned.

The disposable owner integration also enables its own events store and private runtime directory
(`XDG_RUNTIME_DIR`/`TMPDIR`), then checks the real HTTP owner feed for exactly one exit, close and
archive per fixture session, including the archived bundle id, for both external and dashboard
closes. Hook sockets and all native/tmux fixtures remain isolated from running agents.
