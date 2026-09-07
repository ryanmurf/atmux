# Dashboard navigation and reliability improvements

Status: implementation reviewed; final build and rollout active

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

Includes the previously implemented Claude/Codex session restart work in `session-restart.md`.

## Gates

- [x] Implement all ten improvements
- [x] Focused tests and browser integration
- [ ] Rust checks and release build
- [x] Fable/Claude Max review
- [x] Independent security review
- [ ] Deploy existing coordinator and affected owner services
- [ ] Verify production health, assets, federation, and preserved agent sessions

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
