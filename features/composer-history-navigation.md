# Restore composer Up/Down input history

Request (2026-09-09): Up in the input box no longer browses the current session's
previous inputs. Restore the existing behavior.

## Cause and implementation

The successful-send path called `rememberMessage`, then
`finishComposerDraftSubmission` called `forgetComposerDraft`. That cleanup
deleted the same session's message history immediately after recording it.
Draft cleanup now preserves sent-message history; removal/replacement of a pane
still retires both its draft and history.

The keyboard handler also required the caret to be at character zero for Up,
but placed it at the end of every recalled message. Up/Down now traverse
consecutive recalled entries regardless of that caret placement. Outside active
history browsing, arrows remain available within multiline drafts; Up starts
history from the first line, and Down from the last. Selections, modifier
shortcuts, and composition remain untouched. Down past the newest entry restores
the original unsent draft. Browsing never sends text to the native agent.

History keeps its existing in-memory, bounded, machine/pane-generation scope.
This fix does not introduce persistent history storage or import old transcripts.
Pricing work and automatic-compaction settings are unchanged.

## Evidence and gates

- [x] Implementation present in `web/app.js`.
- [x] Focused unit tests and Chrome regression test pass.
- [x] Full local browser/web regression checks recorded.
- [ ] Live integration on every affected platform.
- [ ] Fable/Claude Max and independent security review of the frozen snapshot.

The new Chrome test exercises the actual composer, successful and failed HTTP
sends, native browser arrow-key events, repeated navigation through multiline
messages, draft restoration, session switching, and reuse of a pane ID with a
new generation. It failed against the original implementation and passes with
the fix. It uses a disposable fixture server and does not send anything to live
agents.

Passing verification:

- JavaScript/navigation unit suite: 157 tests.
- `node --test tests/web_mobile_pulse_browser.mjs tests/navigation_browser.mjs tests/mobile_viewport_browser.mjs`:
  8 browser/harness tests, including the new history regression and existing
  draft persistence, send-failure, session-replacement, and mobile checks.
- `cargo test --all-features --lib web::tests`: 45 tests, including revalidation
  of embedded browser assets.
- `node --check web/app.js` and `git diff --check`.

No deployment or service restart has been performed for this fix. Keep this
record active until the remaining project gates are complete.
