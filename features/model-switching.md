# Running-agent model switching

Status: implementation active

## Acceptance criteria

- The web UI shows the current model and a compact model picker for every recognized Claude and
  Codex pane.
- Available models are discovered/reported by the owning machine and filtered for the selected
  harness; the coordinator does not invent a machine's capabilities.
- Choosing a model targets the pane captured when the picker action began, even if the selected
  session changes while the request is in flight.
- Switching uses a validated fixed agent-control path, never a caller-provided shell command.
- Success, rejection, unsupported CLI versions, and stale/offline panes are visible in the UI.
- Existing prompt/image/Quick Talk ordering remains intact.

## Gates

- [x] Implementation
- [ ] Claude unit and native integration tests
- [ ] Codex unit and native integration tests
- [x] Browser/mobile integration tests
- [ ] Midnight and Max live verification
- [ ] Fable/Claude Max review
- [ ] Independent security review

## Verification evidence

- Browser behavior is covered by the shared Node suite, including captured-pane routing,
  unsupported/offline feedback, and in-flight state.
- Disposable real-tmux integration fixtures exercise the verified Claude and Codex native picker
  protocols. Live installed-CLI verification on Midnight and Max remains open.
