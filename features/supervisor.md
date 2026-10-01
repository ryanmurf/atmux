# A6: Supervisor (keep agents moving, finish jobs, close and archive)

Status: A6 DONE on branch `feat/supervisor` (2026-10-01); merged lead integration through `ce77c67`

Read `features/agent-control-plane.md` (especially "Phase 2") and `features/intake-router.md`.
A5 (intake) is building the shared clients in `src/herodevs.rs`, `src/github.rs`, and
`src/llm.rs` on branch `feat/intake`; merge that branch into yours as soon as it has them and
whenever it advances, and do not edit those files except through A5 (ask the lead if you need a
change). Until then, build against small traits you define and adapt when you merge.

## Goal

Agents never sit waiting for long: routine prompts are answered automatically, real decisions reach
Ryan quickly with context, finished work is verified and closed, and idle finished sessions are
archived. Autonomy is approved; everything is audited.

## Scope

1. **Event-driven loop** on the coordinator, subscribed to the fleet event log (in-process).
2. **`agent.needs_input`**: gather `agent_summary`, the last turn (`agent_conversation`, tools
   excluded), the pane's job (ledger metadata `session_key`), and the reason. The 27B classifies
   with strict JSON: `continue` (send a short continue/approval message), `answer` (send an answer
   grounded only in the job and conversation), `startup` (leave to the startup-prompt responder),
   or `escalate`. Permission prompts are approved only inside the job's repo/folder and never for
   destructive operations outside it (deleting branches or data, force pushes, production deploys,
   credential changes); those escalate. Escalations go to Ryan through herodevs (the `ryan-tron`
   channel and a Slack message to his private channel via the existing platform tools) with the
   session name, machine, the question, and a link to the pane in the dashboard. Rate-limit per
   session; never answer the same prompt twice.
3. **`agent.turn_completed`**: detect the completion protocol (`JOB DONE` / `JOB BLOCKED`) and
   verify done criteria where possible (PR exists and is open or merged, CI green via GitHub;
   tests reported). On success: complete the job with the outcome, move the GitHub project item
   to the configured status, and after a configurable quiet period close the tmux session (A3
   archives it). On `JOB BLOCKED` or failed verification: escalate.
4. **Stall handling**: sessions working with no output change for a long time, or waiting with an
   open job, get one nudge, then escalate. Sessions with no job and no activity for a long time
   are summarized and archived (configurable, dry-run first).
5. **Daily digest** to Ryan: jobs completed, open, blocked, sessions archived, budget use.
6. **Configuration and safety**: `[supervisor]` off by default, `dry_run`, kill switch, budgets,
   an allowlist of actions, and every decision published as a `supervisor.*` event through the
   fleet append.

## Acceptance

- Unit tests with fake LLM, herodevs, and GitHub servers: classification handling (including
  malformed or adversarial model output and prompt-injection text inside agent output), the
  destructive-permission guard, duplicate suppression, completion verification paths, archive
  timing, stall nudges, digest assembly, dry-run and kill switch.
- An end-to-end test with a disposable tmux socket: a fake agent emits a permission prompt and is
  answered; another reports `JOB DONE` and is closed and archived.
- `cargo fmt`, zero-warning clippy, `cargo test --all-features`, node tests, browser suites.
- No live GitHub, herodevs, or Qwen calls in tests.

## Engineering decisions and evidence

- Supervisor policy is serialized behind small `Agents`, `Platform`, `Model`, and `Audit`
  interfaces. This let A6 build against the merged control-plane contracts while A5 supplied
  the shared clients. A6 never edits `src/herodevs.rs`, `src/github.rs`, or `src/llm.rs`.
- Model output is strict JSON selecting `continue`, `answer` with a ledger fact key, `startup`,
  or `escalate`. No model-generated command, destination, or free-form answer is executable.
  Answers copy the chosen job fact. This deliberately narrows conversation-based answers to
  facts present in the ledger, to prevent an injected agent message from inventing authorization.
- Permission approval requires a repo-scoped cwd and one simple command in the actual visible
  prompt. Supported commands are bounded Cargo verification/build commands and read-only Git
  status/diff/log; other commands and plan approvals escalate. Repo builds/tests may execute
  repository code, within the job's existing authorization. Broad approval is never granted.
- Completion evidence must be in the latest main-agent turn, use the exact job id, and satisfy
  ledger flags. `TESTS PASSED` reports tests; it is a report, not independently rerun by the
  supervisor. GitHub verifies the PR repository, OPEN/MERGED state, and explicit successful CI.
- Durable private state holds prompt/action tombstones, budgets, cursor, observations and the
  completion saga. Actions are claimed before external effects. Ambiguous completion or delivery
  escalates and keeps the session. Bounded tombstones fail closed at capacity; an operator must
  rotate state rather than silently discard duplicate protection.
- Quiet closes require an unchanged generation and output hash, waiting status, no attached
  human, and a single pane/window. New owner endpoints revalidate these at the tmux mutation
  boundary and fail closed on older owners (404), using existing owner authentication and Origin
  checks. Closing goes through the existing registry observation/archive lifecycle.
- Initial focused gate: six supervisor policy/runtime tests passed, and library all-feature
  clippy with `-D warnings` passed. Full integration gates will be recorded at completion.

### Coordinator integration

- Merged A5 shared clients `ca03a48` through merge `1179541`. Adapters use those clients unchanged:
  fenced GraphQL ledger transitions, bounded GitHub PR/CI queries and project updates, and the
  shared OpenAI-compatible client. Slack uses the platform's existing `slackMutations.sendMessage`
  operation, source-verified in `hd-mcp`'s `SlackToolHandler`, via the shared GraphQL client.
- The in-process fleet long-poll has a durable cursor. A periodic reconciliation catches attention
  missed during retention gaps, rate limiting, or coordinator downtime. Current native attention
  and visible permission evidence override stale discovery `idle_prompt` events. Startup prompts
  remain exclusively owned by the startup responder.
- `JOB BLOCKED` and failed verification perform a fenced ledger `escalateJob` and notify Ryan.
  Successful completion persists its project metadata before the ledger write, updates the
  configured GitHub single-select option, then closes only after the quiet period. Completed ledger
  history is excluded from polling so it cannot crowd out open work.
- Duplicate protection is conservative: one automated answer per job/generation/reason/native
  turn text and latest human text. Terminal echo cannot turn an answered prompt into another prompt.
  Claims are reclaimed only for the matching archived generation. Archive totals count the
  registry's `session.archived` confirmation, rather than a close request.
- The first output change after a nudge is treated as possible input echo; it cannot reset the
  stall clock indefinitely. Further output or a status change establishes fresh activity.
- Merged A5's complete intake implementation through `0cf0041`. H4 ledger metadata is immutable,
  so dispatched assignments are joined from the coordinator's durable intake mirror only when
  message/channel identity, live fence, session key and folder agree. Blocked, undelivered or
  source-completed assignments cannot authorize answers; any outstanding assignment also prevents
  orphan cleanup. Mirror saturation fails closed rather than overlooking an assignment.
- A6 renews dispatched jobs before their leases expire, with a configurable bounded TTL and
  renewal margin. Each live fence/expiry is claimed once; an ambiguous renewal escalates instead
  of being blindly retried. Expired assignments cannot authorize a prompt answer. This fills the
  handoff after intake's initial claim/renewal; intake does not renew already dispatched work.
- Channel notification request IDs use A5's deterministic UUIDv8 helper, matching H4's UUID schema.
  The new assignment and lease paths passed all 20 focused supervisor tests and all-target,
  all-feature clippy with `-D warnings`.
- Merged the requested lead integration through `ce77c67` (including `08456e1` and A4). Guarded
  closes now persist A3's intentional-close tombstone before killing a pane and release the native
  resume lease, so A4 phone-home restore cannot reopen completed jobs. The owner checks tmux's
  actual pane count, including shell panes omitted from agent discovery. The disposable E2E
  confirms stale-output refusal, neighboring-shell preservation, fenced renewal, both escalation
  destinations, verified completion, project update, close/archive and the restore exclusion.
- Unconfirmed renewal claims remain suspended across restarts until a fresh ledger expiry/fence
  is observed. Focused acceptance passed 21 unit tests plus the configured disposable lifecycle;
  the reconciled full gate passed 935 Rust tests (7 ignored), 196 JavaScript tests and all 13
  browser cases. Formatting and all-target/all-feature clippy passed with zero warnings.
- Prompt delivery re-reads the live assignment after classification and compares its fence, job
  identity, folder, facts and verification flags. Changed/ambiguous assignments or elapsed leases
  withhold the answer. A confirmed lease extension or CLAIMED-to-IN_PROGRESS transition preserves
  authorization. Regression fixtures change the fence, folder, expiry and state during inference.
- Dry run still reads the ledger and uses bounded classification, so its audit reflects actual
  decisions. It suppresses prompt/ledger/project/close/notification effects, with separate durable
  claims so switching to live mode can act on the same evidence. The kill switch also stops model
  requests.
- Intermediate full gate: `cargo fmt`, all-target/all-feature clippy with `-D warnings`, 901 Rust
  tests passed / 7 ignored, 193 JavaScript tests passed, and all 12 browser cases passed across
  the initial run and the isolated 9-case mobile-suite rerun. E2E used loopback OAuth/JWKS,
  herodevs, GitHub, Qwen and a disposable tmux owner; routine permission answered, force-push
  prompt escalated through channel and Slack, PR and CI verified, project updated, session
  closed and archived, and the fleet audit confirmed every action.
- Test accommodations: the worktree root was temporarily changed from 0775 to 0755 and restored
  in `finally`, because existing recovery tests reject group-writable ancestry. Default Rust test
  concurrency exposed existing fork/exec descriptor races (ETXTBSY, summarizer lock inheritance,
  immediate-exit timing); the complete suite passed with `-- --test-threads=1`. A6's store now
  explicitly unlocks on drop. One Chromium target-navigation failure passed on an isolated rerun.
  No production code outside A6 was changed to accommodate these failures.

### Lead configuration

- Enable `[events]` and `[registry]` on the coordinator and owners, with durable private storage,
  and configure federation. Owners must include A6's new guarded endpoints; older owners reject
  automation with 404. Configure the existing coordinator Redpanda/search publication separately.
- Supply the supervisor/shared-auth configuration and mounted secret files in the coordinator's
  deployment configuration. The current Helm chart renders events, registry and summaries but has
  no intake/supervisor or shared-auth values yet; that deployment wiring belongs to the lead.
  Permit the 27B gateway's port and herodevs/GitHub HTTPS destinations through explicit egress rules.
  Enable configured rolling summaries before enabling orphan archiving, which requires a digest.
- Configure the shared `[herodevs]` device-auth provider from A5's record, mount private credentials,
  and perform Ryan's one-time device approval. Ledger job claiming and supervising must use the
  same acting identity and current fence. Supervisor renews dispatched leases; configure
  `lease_ttl_seconds` (default 3600) and `lease_renew_before_seconds` (default 900) together.
- Set `[supervisor]` `enabled`, `dry_run` (start true), an absolute `kill_switch` path on a writable
  mount, `store_dir` on the persistent volume, and the authenticated HTTPS `dashboard_url`.
  A kill-switch file stops model calls and external effects; removing it resumes the loop.
- Set actual ledger channel IDs in `job_channels` and the ID of `ryan-tron` in `ryan_channel`;
  channel names are not resolved implicitly. Set Ryan's private Slack channel and installation IDs.
- Configure `[[supervisor.projects]]` with `channel_id`, `project_id`, `field_id`, and
  `done_option_id` for boards #40/#51. The channel binding supplies project identity when jobs
  carry only the brief's `project_item_id`; optional job `project_id` must agree with the mapping.
- Configure `[supervisor.llm]` for Max's 27B gateway: endpoint, actual gateway model alias,
  API-key file, explicit LAN HTTP allowlist, timeout and response token cap. Configure
  `[supervisor.github]` with a mounted token file and the GraphQL endpoint. No live probes were run.
- Intake jobs carry `completion_criteria` and optional booleans `require_pr`, `require_ci`,
  `require_tests` (all default true) and `require_merged` (default false). Set false explicitly for
  noncoding work. Kickoffs should teach `JOB DONE <job_id>: TESTS PASSED <summary> <PR URL>`;
  tests are reported with the literal marker, GitHub evidence is independently checked. Missing
  tests, absent/pending CI, a closed unmerged PR, or a different repository escalates.
- Tune `allow_actions`, quiet/rate/stall/grace intervals, machine session cap, and hourly model/action
  budgets alongside intake's caps. Orphan archiving is disabled unless `orphan_archive_seconds` is
  set, requires a native conversation and cached digest, and should first be observed in dry run.
  Keep each configured nonterminal ledger scan under 100 rows (H4's bounded API); saturation and
  state-capacity exhaustion fail closed. Inspect `supervisor.error` events for ambiguous effects
  or failed notifications; no blind retries can double-submit a prompt or close an uncertain job.

## Final acceptance

- [x] Event subscription and periodic attention reconciliation on configured coordinators
- [x] Strict classification, repo-scoped permission guard, startup handoff, grounded facts,
  dual-destination escalation, persistent duplicate suppression and assignment revalidation
- [x] Fenced lease renewal, completion/blocking, GitHub PR/CI verification and project status saga
- [x] Quiet guarded close, intentional-close restore exclusion and A3 archive confirmation
- [x] Stall nudge/escalation, opt-in summarized orphan cleanup and durable daily digest
- [x] Disabled defaults, dry run, kill switch, action allowlist, budgets and fleet audit
- [x] Merge shared clients unchanged from A5 and merge requested lead branch containing `08456e1`
- [x] Final Rust, JavaScript and browser acceptance gates

| Gate | Final result |
| --- | --- |
| `cargo fmt --check` and `git diff --check` | Passed |
| `cargo clippy --all-targets --all-features -- -D warnings` | Passed, zero warnings |
| `cargo test --all-features -- --test-threads=1` | 936 passed, zero failed; 7 declared ignored |
| Focused supervisor tests (included in full gate) | 22 unit tests and configured disposable E2E passed |
| `node --check web/app.js` | Passed |
| `node --test web/*.test.mjs tests/navigation.test.mjs` | 196 passed, zero failed/skipped |
| Four browser files: mobile viewport, navigation, Quick Talk, mobile/Pulse | 13 passed, zero failed/skipped |

All commands used RTK. The final Rust gate set `ATMUX_REQUIRE_TMUX=1` and a unique
`ATMUX_TMUX_SOCKET_NAME=atmux-ci-a6-final-<uuid>`, removed inherited TMUX variables, and restored
the worktree's original 0775 mode in `finally`. Browser files ran separately to avoid cross-suite
Chromium target races. Only loopback platform/model fixtures and explicitly disposable tmux
sockets were used. No live probes, deploy, push, service restart, cluster/Keycloak changes or
mutation of existing tmux sessions/agents occurred. The user's untracked `.atmux.toml` is preserved.

Implementation commits: `0bc5caf` (core), `07156d1` (configured adapters/E2E), `d1bf29d`
(intake overlay and lease handoff), and the final assignment-revalidation/acceptance commit.
Integration merges: `1179541` and `0cf0041` (A5), `72aee1a` (lead through `ce77c67`, A4 reconciliation).
The feature remains off by default. No implementation work is pending; deployment configuration
and the one-time shared device login above remain the lead's handoff.
