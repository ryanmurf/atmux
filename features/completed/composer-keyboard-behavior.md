# Composer keyboard behavior

Status: completed

## Acceptance criteria

- [x] Plain Enter sends the current message.
- [x] Ctrl+Enter and Command+Enter insert a newline at the selection; Shift+Enter remains an
  additional newline shortcut.
- [x] IME composition and Alt+Enter are not intercepted.
- [x] The dedicated mobile Send button remains unchanged.

## Gates

- [x] Implementation
- [x] Unit/source-contract tests
- [x] Desktop browser integration test: fetch-intercepted key events verified newline versus one
  send, with no message delivered to a real agent
- [x] Final Fable/Claude Max review: SAFE, no blockers
- [x] Final independent security review: SAFE, no blockers
