# In-place agent session restart

Status: implementation active

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
- [ ] Disposable tmux integration test for Claude and Codex
- [ ] Midnight and Max live verification
- [ ] Fable/Claude Max review
- [ ] Independent security review

## Verification evidence

- Control-plane tests verify that Codex reaches only the guarded owner-local mutation seam while
  the legacy Claude route rejects Codex before any tmux operation.
- Route tests cover local Codex dispatch and offline federated restart behavior.
- Browser tests cover Claude/Codex capability gating and confirmation. New clients send only the
  confirmed pane process identity to `/restart-instance`; the owner rechecks it under the mutation
  gate and again immediately before respawn. Older owner versions fail closed on the new route.
