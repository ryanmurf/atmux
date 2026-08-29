# Native Claude Pulse feature merge

Status: implementation active

## Request

Port every supported Claude Pulse capability from the current local
`/home/ryan/IdeaProjects/claude-pulse` working tree into atmux as native Rust functionality. The
result must integrate with atmux rather than embedding or depending on the TypeScript service.

## Planning gates

- [x] Source-verified Claude Pulse feature inventory
- [x] Sol xhigh Rust/atmux architecture plan
- [x] Opus 5 independent plan review and synthesis on the Claude Max usage plan
- [x] Acceptance matrix covering every supported feature and deliberate exclusion

## Completion gates

- [ ] Native Rust implementation
- [ ] Focused unit tests for every migrated behavior
- [ ] Integration and compatibility tests against representative Claude Pulse fixtures
- [ ] Native verification on Tron, Midnight, and Max where platform behavior differs
- [ ] Fable/Claude Max adversarial review
- [ ] Independent security review
- [ ] Deployment verification that leaves the existing authenticated Ingress unchanged and adds no
  new or bypass route

## Management/UI slice status

- [x] Account-scoped gauge health distinguishes dead, null, authentication-failed, stale,
  unchanged, and healthy collectors without returning credential values or local paths.
- [x] Profile management changes only bounded poll intervals and monthly budgets. JSON omission
  preserves a budget, explicit `null` clears it, and neither REST nor MCP accepts credential/path
  fields.
- [x] Account-wide and per-profile collection requests coalesce through the one embedded scheduler;
  the API rejects unknown, cross-account, and non-local profiles before enqueueing work.
- [x] Alert acknowledgement/reply remains explicit-id-only. Pull, controllable-pane, and negotiated
  channel delivery choices are capability-gated; authentication failures are rejected from panes
  at validation and delivery time.
- [x] The safe-DOM web UI exposes the management controls on mobile, uses the server reply limit,
  and keeps stale account responses from replacing current state.
- [x] Account-scoped Pulse SSE emits an authoritative initial revision, latest-only committed
  invalidations, and keepalives behind the existing auth/Host policy. The client closes on account
  switch or hidden tabs, reconnects with bounded backoff, treats gaps as full-refresh signals, and
  debounces bursts without adding a collector or unbounded polling loop. Standalone CLI import has
  no live runtime; the next startup/stream initial event performs its authoritative refresh instead
  of inventing cross-process IPC.
- [x] Account pricing overrides can be reverted through REST, MCP, and the compact UI. Deletion is
  explicitly account/key scoped in SQLite and PostgreSQL, preserves seeded defaults, and returns the
  same not-found envelope for missing and cross-account rules.
- [x] Operator documentation covers safe-disabled configuration, explicit accounts/profiles,
  external secret references, doctor/push/backfill/import, receiver/reporter/federation, retention,
  SSE lifecycle, forward migrations/backups, deliberate security differences, the Midnight Aqua
  rule, and the standing no-Ingress-mutation constraint.
- [ ] Fable/Claude Max review and the independent security review of the frozen combined snapshot
  remain required before this slice or the overall merge may be called complete.

## Standing constraints

- Preserve the dirty Claude Pulse working tree and use it read-only as the behavioral reference.
- Preserve existing Claude and Codex support, machine federation, transcript safety, image delivery,
  and the Midnight Aqua tmux/Keychain procedure.
- Pulse work must not create, disable, re-enable, or otherwise modify the existing authenticated
  `murphytek/atmux` Ingress. Any future Ingress change requires Ryan's separate explicit
  authorization.

## Verification evidence

- The reviewed source baseline, decisions, security corrections, work packages, and feature matrix
  are recorded in `claude-pulse-rust-plan.md`.
- Pulse federation implementation evidence is recorded separately from the overall merge gate:
  - [x] Pull clients are built only from explicitly configured, authenticated atmux remotes and
    explicitly configured Pulse accounts; the local node and credential-less peers are rejected.
  - [x] Versioned last-key cursors and per-account/per-peer resync state are durable. A page and its
    next cursor commit in the same store transaction, including after restart and replay.
  - [x] SQLite and PostgreSQL export local-machine rows directly with bounded SQL keyset pages.
    Mirrored rows are excluded before `LIMIT` and are never re-exported.
  - [x] The large-store regression exports all 10,050 local usage rows in resumable pages while
    15,000 mirrored rows are present, without the former whole-history/10,000-row cap.
  - [x] The embedded web-process lifecycle uses one bounded pull loop (30-86,400 seconds; five
    minutes by default), bounded peer/page concurrency, offline-peer isolation, and awaited
    shutdown.
  - [x] Focused federation unit tests pass 7/7, restart/resync integration tests pass 2/2, and the
    PostgreSQL 18 store/RLS conformance suite passes 25/25 under a role with neither superuser nor
    `BYPASSRLS`.
  - [x] Push reporting uses bounded local-only SQL pages and a durable per-account/machine/
    destination/stream outbox. Exact secret-free envelopes and cursor transitions persist before
    send; restart replays identical bytes and request IDs after source retention/mutation, then
    atomically commits the cursor and removes the page. Destination state is capped at 64/account.
  - [ ] Native three-host verification and both independent reviews remain part of the overall
    merge completion gate. Current all-target strict Clippy, formatting, and Rust 1.88 checks pass.
- `cargo test --all-features --locked --test pulse_api_mcp`: 11 passed, including authenticated
  account-scoped SSE/reconnect bounds and pricing set/revert parity across REST/MCP with
  indistinguishable IDOR misses and seeded-default preservation.
- Focused real-router, StoreSink, ingest, federation-consumer, and invalidation-hub tests pass,
  covering bearer/Host/Origin boundaries, post-commit publication, retention, latest-only slow
  clients, successful federation publication, and zero publication after failed page application.
- `cargo test --all-features --locked --lib force_`: 2 passed, covering tuple coalescing and native
  in-flight refusal/account scoping.
- `cargo test --all-features --locked --lib pulse::health::tests`: 3 passed.
- `cargo test --all-features --locked --lib profile_budget_patch_distinguishes_missing_clear_and_set`:
  1 passed.
- `node --check web/app.js && node --test web/app.test.mjs`: 63 passed, including safe DOM,
  account/path validation, per-profile/account force actions, alert capability guards, reply limit,
  stale-response protection, pricing revert, monotonic/gap-safe invalidations, hidden-tab and
  account-switch cleanup, bounded reconnect, and mobile layout assertions.
- `cargo +1.88.0 check --all-features --all-targets --locked`: passed.
- `cargo clippy --all-features --all-targets --locked -- -D warnings` and
  `cargo fmt --all -- --check`: passed after the corrected reporter outbox and request-ID gates.
- The raw PostgreSQL pricing DELETE RLS probe passed under `NOSUPERUSER`/`NOBYPASSRLS`: an
  account-1 transaction targeting account 2 affected zero rows and account 2's override survived.
  The same real-PostgreSQL run verified forced/fail-closed RLS for reporter cursors, pending pages,
  and pending chunks, plus a read-only doctor that preserved schema and all table counts.
- Repository-wide gates, three-host verification, and the frozen combined-snapshot reviews remain
  pending active shared runtime work.
