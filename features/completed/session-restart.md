# In-place agent session restart

Status: deployed and runtime-verified 2026-09-07

## Acceptance criteria

- The web UI offers a confirmed restart action for recognized Claude and Codex panes and explains
  when the selected pane is not safe to restart.
- A restart respawns only the selected pane in its existing tmux session and resumes the exact
  native saved conversation with the current trusted launcher.
- The owning node derives the launcher, config store, session id, profile, model, effort, and
  service tier. The browser cannot supply native session data or replay a raw launch command.
- The execution path rechecks current pane identity and provider-specific readiness under the
  shared process and mutation locks, preserves an allowed per-agent memory cap, and rejects stale
  or ambiguous state. Codex additionally requires its confirmed empty top-level prompt.
- Federated requests execute on the pane's owning machine; offline owners and unsupported agents
  produce explicit errors.
- The legacy Claude-only resume API remains compatible with already-loaded clients.

## Gates

- [x] Implementation
- [x] Focused Rust and browser tests
- [x] Disposable tmux integration test for Claude and Codex
- [x] Midnight and Max live verification
- [x] Fable/Claude Max review
- [x] Independent security review

## Verification evidence

- Control-plane tests verify that Codex reaches only the guarded owner-local mutation seam while
  the legacy Claude route rejects Codex before any tmux operation.
- Route tests cover local Codex dispatch and offline federated restart behavior.
- Browser tests cover Claude/Codex capability gating and confirmation. New clients send only the
  confirmed `instance_id` and `restart_token` to `/restart-instance`. The token includes pane/native
  PIDs and a stable OS process creation stamp. The owner rechecks both under the mutation gate and
  again immediately before respawn. Older owner versions fail closed on the new route.
- Opening Actions refreshes capabilities; a rejected restart clears its stale confirmation so a new
  attempt requires a fresh user confirmation. Independent security and Fable reviews approved.
- The disposable native restart integration verifies real in-place respawn for both providers with
  recorder launchers and exact saved ID/config/cwd/model/effort/Fast arguments. It leaves its canary
  process and server/session/window/pane identities untouched and replaces only the target PID.
- Runtime commit `fbb2bff` is deployed on Tron, Max, Midnight, Clue, and coordinator revision 27.
  Live authenticated checks verify stable native process stamps, captured output, and stale-request
  rejection on all four owners without manually restarting live agents. Deployment details and
  the pre-existing Midnight maintenance observation are in `../dashboard-improvements.md`.
