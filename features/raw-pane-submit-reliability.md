# Raw-pane submit reliability

Status: implemented and live-tested; independent review pending

## Request

Pressing Enter/Send from the web raw-pane view pasted the message into Claude or Codex but left it
in the agent input instead of submitting it.

## Cause and implementation

- Authenticated gateway logs proved the affected Claude and Codex message POSTs returned HTTP 200,
  and the browser request already carried `submit: true`.
- The owning atmux process issued bracketed paste and terminal Enter as two immediately adjacent
  tmux writes. Agent TUIs can decode both in one input turn and retain the pasted text.
- A fixed 75ms boundary now lets the TUI finish bracketed-paste decoding before atmux sends exactly
  one Enter. Non-submitting paste behavior is unchanged.

## Completion gate

- [x] Implemented
- [x] Unit/browser tested
- [x] Real tmux integration tested
- [x] Live deployed and route-verified
- [ ] Independently reviewed

## Verification

- A real isolated tmux probe enables bracketed paste and deliberately reports `enter-too-early`
  when Enter arrives within 50ms. It now reports `submitted` and verifies the exact pasted text.
- Existing multiline/literal-paste and exactly-one-Enter tests remain green.
- Full all-feature Rust tests, strict all-target Clippy, Rust 1.88 all-target check, rustfmt,
  diff-check, and the 66-test browser unit suite pass on 2026-08-10.
