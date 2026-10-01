# A6: Supervisor (keep agents moving, finish jobs, close and archive)

Status: implementing on branch `feat/supervisor`; core policy and durable state in place

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
