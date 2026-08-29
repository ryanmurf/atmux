# Claude Pulse native Rust merge plan

Reviewed planning inputs: two independent GPT-5.6 Sol xhigh agents and Claude Opus 5 at maximum
effort through the Claude Max usage plan.

## Frozen behavioral reference

- Repository: `/home/ryan/IdeaProjects/claude-pulse` (read-only)
- Base commit: `00ef42a9816e680bc35707bdc93426a97bc4902d`
- Tracked dirty diff SHA-256: `e58da3f2ceb0dd2458aef1bf50c71f2aec806821f8b0bb40503ed96cd1be381f`
- Base + tracked diff + untracked manifest SHA-256:
  `7913ee2cf5d476a84b82af2b316a9eb18f56092e4ef0ff8123e069465aca46ad`
- Untracked behavior includes Grok, credential preflight/healing, profile hiding, and gauge health.
- The Pulse suite is timing-sensitive: observed runs range from 287-293 passing, with six skipped
  PostgreSQL tests and stable contract disagreements in rolling-window staleness, per-machine
  quota cards, and child-process agent pushes. The Rust port treats passing behavior and explicit
  source intent as the compatibility baseline, not those defects.

## Architecture decisions

1. Pulse is an embedded `PulseService` in the existing `atmux web` process. There is no second
   collector daemon. This is mandatory on Midnight so collection inherits its Aqua Keychain
   security session.
2. The common domain and scheduler are the only business-logic layer. REST, MCP, federation,
   optional ingest, CLI, and the Usage UI are thin adapters.
3. SQLite and PostgreSQL use separate typed SQL backends behind one conformance-tested store trait.
   SQLite stores epoch milliseconds; PostgreSQL stores `TIMESTAMPTZ`. No SQL string rewriting or
   timestamp string sorting is retained.
4. Subscription quota is account/profile-global. Each card includes per-machine contributor,
   freshness, and reporter-version provenance without duplicating the allowance.
5. Staleness rules are vendor-aware: Anthropic rolling seven-day decreases are legal, while fixed
   Codex/Grok weekly periods and DeepSeek monthly-budget periods reject same-period regressions.
6. One jittered, single-flight scheduler runs usage (15m/profile), context (2m), token tally (30m,
   two-day lookback), Gemini (30m), retention (1h), and completion-triggered reporting.
7. Runtime capability flags are `collect`, `serve`, `receive` (default false), and optional
   `report_to`. Real CLI operations are `pulse doctor`, `pulse push --once [--backfill]`, and a
   non-destructive `pulse import`.
8. Credential refresh defaults to in-memory lock/re-read/adopt behavior. Linux persistence is
   explicit opt-in. macOS Keychain writes are disabled because command-line persistence exposes
   credential material and can damage Keychain ACL behavior.
9. Existing atmux mTLS federation is the default machine transport. Optional push ingest is a
   separately enabled plane for machines federation cannot reach.
10. Atmux never trusts `X-Auth-Request-Email`. Existing host/origin/mTLS/token policy remains the
    request boundary; public signup is not introduced.
11. Pulse source-derived tables/constants are reimplemented under atmux's MIT license with an
    explicit README provenance note identifying Claude Pulse (Apache-2.0, copyright Ryan Murphy).
12. Pulse implementation and deployment leave the existing authenticated `murphytek/atmux`
    Ingress unchanged and introduce no new or bypass route. Any future Ingress mutation requires
    Ryan's separate explicit authorization.

## Feature acceptance matrix

### Providers and local collection

- [x] Anthropic OAuth usage API, scope checking, bounded 429/5xx retries, whole-second resets,
  forced in-memory refresh on 401, and explicit opt-in one-token inference fallback.
- [x] Codex live usage plus bounded rollout fallback, duration-based window classification,
  entry-timestamp ranking, identity stripping, expiry, and fixed-week staleness.
- [x] DeepSeek balance/monthly-budget normalization with externally referenced API keys.
- [x] Grok weekly billing and bounded transcript fallback without config-dir or `PATH` execution.
- [x] Gemini per-model quotas, OAuth refresh/cache/throttle, and graceful disabled state.
- [x] Antigravity bounded SQLite/protobuf tally, checksum validation, model inference, and dedupe.
- [x] Claude context session discovery, bounded tails, compaction boundary, 200k/1M limits, and
  75-percent compact recommendation.
- [x] Claude/Codex/Antigravity coarse and fine token accounting, cache splits, settings, dedupe,
  synthetic exclusion, and full-history/recent backfill.
- [x] Secret-free credential health states and narrow config-dir preflight healing.

### Storage, analytics, and lifecycle

- [x] Account/machine/profile scoped schema, append-only snapshots, typed outcomes, contributor
  provenance, profile hiding, reporter versions, and placeholder states.
- [x] SQLite WAL/foreign keys/busy timeout plus real PostgreSQL parity in CI.
- [x] Current usage, history, pace/context pace, capacity, context sessions, Gemini, and bounded
  daily/weekly token/cost reports with profile/machine/session/model drill-down.
- [x] One authoritative model/settings-aware pricing table, per-account overrides and explicit
  account-scoped revert to seeded defaults, fallback rates, cache costs, and report-time repricing.
- [x] Threshold/auth/context/reset alerts, durable cooldown/deduplication, acknowledgement/reply,
  capability-gated Claude channel events, and opt-in pane delivery (never for auth failures).
- [ ] Retention/downsampling: context 1d, alerts 180d, snapshots hourly after 7d and daily after
  90d; token report grain retained with bounded settings.
- [ ] Non-destructive read-only Pulse SQLite import with provenance, idempotence, dry-run, exact
  reconciliation, secrets externalized, and ingest-token hashes excluded by default.
- [x] Native gauge-health/doctor output that distinguishes dead, null/auth-failing, stale, and
  authenticated-but-unchanged collectors.

### Distributed, API, MCP, and UI

- [ ] Bounded cursor-based Pulse federation over existing machine mTLS/token trust with no mirrored
  re-export loops. The Rust implementation and focused storage/runtime tests are green: pulls use
  only explicit authenticated remotes/accounts, cursor and page application commit atomically,
  remote rows are sanitized and reported-only, SQL keyset paging advances beyond 10,000 local rows,
  offline peers are isolated, and mirrored rows are not exported. This remains unchecked until the
  final native three-host and independent-review gates pass.
- [ ] Optional receiver with separate hashed ingest tokens, authoritative token account/machine,
  IP-first/token throttling, transactional profile/row caps, 1MiB/row bounds, HTTPS reporting,
  chunking, idempotence, version tracking, and bounded backoff.
- [x] Authenticated and origin-checked `/api/v1/pulse/*` routes with pagination/day bounds and no
  unauthenticated forced polling.
- [x] MCP parity for usage, pace, context, Gemini, history, reports, profiles, budgets, visibility,
  polling, alerts, pricing, limits, machines, and ingest tokens; raw secrets are never accepted.
- [ ] Dense safe-DOM Usage UI with account-global gauges, machine provenance, context sessions,
  Gemini, reports, alerts, profile/pricing/receiver settings, SSE, and mobile behavior.
  The management UI and browser coverage are present, including health states, tri-state budgets,
  account/profile collection, delivery capability guards, server-aligned replies, pricing revert,
  stale-response rejection, account-scoped latest-only SSE, hidden-tab/account-switch cleanup,
  bounded reconnect, and mobile controls. Native three-host and frozen security-review gates remain
  pending before the overall work package is checked complete.
- [ ] No copied standalone HTML server, unsafe `innerHTML`, duplicate daemon/pidfile, raw vendor
  bodies, plaintext DB secrets, destructive migration, or Pulse-created/bypass Ingress route.

## Security regressions that must be RED-to-GREEN tests

- Cross-account profile mutation/squatting and unscoped `ensureProfileExists`.
- Plaintext API keys and raw provider responses.
- Forwarded-email trust and unauthenticated poll amplification.
- Anonymous/cross-Host SSE construction, reconnect refresh amplification, and hidden-tab leaks.
- Cross-account pricing override deletion and seeded-default deletion during revert.
- Missing Origin checks on mutations and plain-HTTP bearer upload.
- Non-transactional row caps and invalid-token rate-limit bypass.
- Unbounded/symlinked transcript walks and in-memory 365-day reports.
- Duplicate poll schedules and unsafe template `innerHTML`.
- Incomplete/destructive migrations, unbounded Gemini rows, and mutable snapshot/context coupling.
- Grok config-dir/PATH command execution.
- PostgreSQL alert-cooldown timestamp failure.

## Dependency-ordered agent work packages

1. WP0 foundations: Rust domain/time/error/config, features/dependencies, module skeleton.
2. WP1 store trait, SQLite schema/migrations, conformance suite.
3. WP2 PostgreSQL backend and the same conformance suite against a real server.
4. WP3 Claude/Codex collectors, credential inspection/refresh, bounded fixtures.
5. WP4 DeepSeek/Grok/Gemini/Antigravity collectors and fixtures.
6. WP5 token tally, pricing, SQL reports.
7. WP6 single scheduler and embedded `PulseService`.
8. WP7 alerts, durable reset jobs, channel/pane delivery.
9. WP8 REST and MCP surface parity under existing policy.
10. WP9 optional receiver/reporter and federation pull.
11. WP10 read-only import and reconciliation CLI.
12. WP11 dense responsive Usage UI using safe DOM construction only.
13. WP12 CI, retention, documentation, native gates, two reviews, and safe-disabled deployment.

Packages that touch existing shared files (`Cargo.toml`, `src/lib.rs`, `src/web.rs`, `src/mcp.rs`,
`src/main.rs`, `web/app.js`, `web/index.html`) are serialized; new-file packages run in parallel only
after their dependency contracts are frozen.

## Final verification gates

- Rust: full features, strict Clippy, formatting, Rust 1.88 lock compatibility, mandatory tmux tests.
- Storage: SQLite/PostgreSQL conformance and transaction/cooldown/account-isolation tests.
- Browser: safe DOM, no `innerHTML`, mobile rendering, SSE/reconnect, existing atmux regressions.
- Import: source remains read-only/integrity-clean and exact per-profile/day reconciliation passes.
- Federation: three nodes, same profile names, offline isolation, resync, no loops.
- Native: Tron, Midnight Aqua Keychain, and Max; no Midnight server/session rebuild.
- Review: Fable/Claude Max plus an independent security reviewer review both implementation and fixes.
- Deployment: Helm safe-disabled Pulse render, existing authenticated Ingress unchanged with no new
  or bypass route, and scoped service restarts.

## WP9 federation verification evidence

- Stable export cursor: version 2 encodes the last exported SQL key, not an offset into a rebuilt
  list. Full resyncs can discover rows inserted before the prior terminal key without shifting or
  duplicating a mutable page.
- Transaction boundary: record validation, logical-key/fingerprint replay checks, imported rows,
  record ledger, page counters, completion state, and next cursor commit all-or-none for one
  account/source peer.
- Same-profile behavior: an existing local profile is retained when a remote machine reports the
  same name/vendor; a vendor conflict rejects the complete page. Remote profile rows cannot carry
  local paths, credential references, or persistent-refresh settings.
- Lifecycle bounds: at most 64 configured peers, 8 concurrent peers, 32 pages per scan, and 500
  records per page; scans run every 30-86,400 seconds (300 by default) and isolate offline peers.
- Large-store regression: 10,050 local usage snapshots page to completion alongside 15,000 newer
  mirrored snapshots. The mirrored rows do not consume the local SQL limit or enter an export.
- Current evidence: federation units 7/7; restart/resync/cross-account runtime integration 2/2;
  PostgreSQL 18 conformance 21/21 under `NOSUPERUSER NOBYPASSRLS`, including forced, fail-closed
  account RLS. Final strict lint, native-host, and two-review gates are still pending.
