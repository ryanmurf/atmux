# Left-rail session controls and request ledger

Status: completed

## Acceptance criteria

- [x] Every controllable session row has a nearby garbage-can action.
- [x] The action opens the existing named confirmation dialog and deletes the captured session, not
  a session selected later.
- [x] The left rail can collapse/expand, preserves the choice locally, and remains
  keyboard/screen-reader operable.
- [x] Mobile touch targets remain at least 44 px.
- [x] `features/requests.md` records Ryan's requests and completed records move only after all gates
  pass.

## Gates

- [x] Implementation
- [x] Unit/source-contract tests
- [x] Desktop and mobile browser integration test: captured target confirmation, collapse/restore,
  and 44×44 px mobile trash target; delete confirmation was deliberately not submitted
- [x] Final Fable/Claude Max review: SAFE, no blockers
- [x] Final independent security review: SAFE, no blockers
