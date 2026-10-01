# A6: Supervisor (keep agents moving, finish jobs, close and archive)

Status: assigned to a Sol 6.1 agent on branch `feat/supervisor`

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
