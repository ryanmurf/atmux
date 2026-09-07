# Dashboard navigation and reliability improvements

Status: deployed 2026-09-07; physical iPhone keyboard retest remains a device acceptance item

## Scope

1. Collapse machine groups independently, persist the preference, and reveal matching search results.
2. Pin favorite agents within their machine without confusing recycled pane identities.
3. Combine status and harness filters with text search and a clear action.
4. Recover the mobile viewport after keyboard dismissal and page restoration without moving a focused composer.
5. Restrict hover styling to hover-capable devices and provide comfortable touch controls.
6. Show connection failures and offer an explicit reconnect without losing drafts or selection.
7. Copy a direct URL to the selected agent.
8. Focus search with a keyboard shortcut while preserving normal text entry and dialogs.
9. Bind restart confirmations to the process identity, reject stale confirmations on the owner, and fail closed on old owners.
10. Download the selected pane's captured raw output as plain text with a safe filename.

Includes the Claude/Codex session restart work in `completed/session-restart.md`.

## Gates

- [x] Implement all ten improvements
- [x] Focused tests and browser integration
- [x] Rust checks and release build
- [x] Fable/Claude Max review
- [x] Independent security review
- [x] Deploy existing coordinator and affected owner services
- [x] Verify production health, assets, federation, and preserved tmux sessions/panes

## Boundaries

The existing OAuth, TLS, ingress, node credentials, and tmux ownership remain the deployment baseline.
Midnight's web service uses its Aqua LaunchAgent; its tmux server and socket are protected.
Real iPhone keyboard behavior requires a device retest; desktop mobile emulation is not equivalent.

## Verification

- 150 web unit tests and five navigation unit tests pass.
- Existing mobile/Pulse browser suite plus connection/link/shortcut/restart/export coverage: five tests pass.
- Dedicated navigation and viewport browser tests pass, including persisted collapse/pins, combined
  filters, captured downloads, stale restart rejection, touch geometry, and keyboard/zoom guards.
- Native restart confirmations include a versioned hash of pane identity, pane PID, native PID, and
  stable OS process creation stamp; owner revalidation prevents same-pane replacement and PID reuse.
- Independent product/security reviews found and fixed owner-scope fallthrough, stale output binding,
  and cached restart capability issues. Final security review approved the runtime snapshot.
- Fable review approved the native restart, navigation, viewport, connection, and export changes.
- Helm render security tests and the Rust 1.88 locked compatibility check pass.
- Final serialized all-features Rust suite: 760 passed, five ignored. The new disposable native
  restart integration is run explicitly because it creates its own isolated tmux server on Linux.
- Final combined Chromium browser suite: seven passed.
- All-targets/all-features Clippy with warnings denied passes. The explicit disposable native
  restart test passes for both Claude and Codex, preserving server/session/window/pane identities,
  replacing only the target process, and retaining exact configuration, cwd, saved ID, model,
  effort, and Fast arguments. Recorder launchers are used instead of credentialed native agents.

## Rollout evidence

- Runtime source: `fbb2bff18a7fb42efb6b6726462746f08bb7bdbd`.
- Coordinator: Helm release `atmux-web`, namespace `murphytek`, revision 27; pod 3/3 Ready.
- Image: `localhost:32000/atmux@sha256:fd7fe5f8269fbb6c5aa926b8f55a098a6368e9d71c0eb3ede533142b13b68553`.
- Reviewed render differs only in the atmux application/init image. Existing OAuth, gateway, TLS,
  ingress, storage, and owner credentials are unchanged; anonymous/spoofed-identity public requests
  still redirect to login (302). No new public route was introduced.
- Release builds passed natively on Linux x86-64, macOS ARM64, and Linux ARM64. Tron and Max use
  the x86-64 build; Midnight and Clue use native builds of the same committed source.
- All four owners are online with no fleet health errors and 48 sessions visible. Authenticated
  production checks verify new assets, captured output, stable native process tokens, and rejected
  stale restart requests on every owner. The coordinator currently wraps owner conflicts as 502.
- All original tmux session/pane identities remain. Tron/Max/Clue pane PIDs are unchanged.
  Three Midnight Claude PIDs changed after service startup, consistent with the already-enabled
  generation-7 maintenance queue's first 30-second pass. Scheduler code/configuration was not
  changed. Exact retrospective attribution lacks a success audit record; no valid manual restart
  requests were sent during deployment.
- Midnight remains on `/private/tmp/tmux-501/default`, server PID `26474`; its web service was
  restarted only through the Aqua LaunchAgent and runs from `/Users/ryan/IdeaProjects/atmux`.
- Prior binaries were retained on each owner; prior coordinator revision is 26. Build, review,
  render, and live-check logs are under `/mnt/data/herodevs-agents/atmux-*`.
