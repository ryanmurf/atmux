# Quick Talk lifecycle reliability

Status: completed

## Acceptance criteria

- Holding Quick Talk continues listening when the browser's speech recognizer ends a segment or
  times out during a longer hold.
- Releasing, cancelling the pointer, hiding the page, or moving focus away stops capture and sends
  at most once to the pane selected when recording began.
- Recognition restarts use bounded backoff and do not loop on permission, device, or service
  failures.
- Repeated Quick Talk holds work without leaving the button stuck in its recording state.
- Desktop mouse and mobile touch behavior remain usable.

## Gates

- [x] Implementation
- [x] Focused lifecycle unit test
- [x] Full browser regression suite
- [x] Desktop/mobile live verification
- [x] Fable/Claude Max review
- [x] Independent security review

## Verification evidence

- All 54 browser unit/regression tests pass.
- `tests/quick_talk_browser.mjs` drove the production UI in headless Chrome with a deterministic
  Speech Recognition implementation: both mouse and touch holds survived a recognizer segment end,
  restarted once while held, stopped on release, sent exactly once to the captured pane, reset the
  button, and worked again on the next hold.
- The same integration delays another composer request to prove dictation queues instead of being
  dropped or duplicating in-flight text. It holds that queued dictation request while beginning a
  second hold to prove the visible prior transcript is not prefixed again, and rejects an oversized
  queued transcript to prove later valid dictation still drains. It then withholds `onend` through
  the stop fallback and fires stale result/end callbacks during the next hold to prove generation
  isolation.
- Fable/Claude Max and the independent security reviewer both returned `SAFE` on the final frozen
  snapshot after tracing queue progress, captured-pane routing, generation isolation, and the
  per-pane owner-node serialization path.
