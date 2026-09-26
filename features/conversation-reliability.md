# Conversation reliability and refresh efficiency

Request (2026-09-10): optimize Conversation and fix intermittent unavailable
views, specifically `support-issues` on Midnight.

## Confirmed causes

- Midnight's `support-issues` pane `%40` still runs PID 7038, Claude `2.1.251`.
  macOS reports that executable as the process's mapped image, but the version
  file no longer exists. The previous classifier required it to remain on disk,
  dropping the native PID and therefore the exact Conversation mapping.
- A new 2.5-second browser poll invalidated an older request even when that
  request was still pending. Slow but healthy remote reads could never display.
- Every pane patch reset the transcript debounce. Continuous output could
  postpone the read indefinitely. Transcript requests also ran outside the
  Conversation view, and lacked explicit timeout/cancellation handling.

## Implementation

- Accept an unlinked native Claude version only in the current owner's verified
  canonical install tree, when the kernel's live executable path and real and
  effective process owners agree. Do not accept argv-only lookalikes, symlinks,
  unsafe replacements, or missing/untrusted parent directories. Existing exact
  PID/cwd/start-time/profile-root/log validation remains mandatory.
- Classify only descendants of the selected pane, avoiding repeated filesystem
  checks for unrelated processes on every pane scan.
- Serialize Conversation reads. Activity keeps the earliest scheduled refresh
  and coalesces during an in-flight request. Limit active refreshes to at least
  750 ms apart; idle polls start 2.5 seconds after completion. Keep known hashes
  and avoid redraws for unchanged data.
- Abort retired requests on selection, pane replacement, hidden views, and page
  backgrounding. Resume immediately on return. Time out stalled reads after
  15 seconds and retry automatically without clearing the last successful view.
- Do not cache native log identity across `/clear` or `/new`, guess a log from
  its directory, change compaction policy, or restart agents to hide the bug.

## Gates and evidence

- [x] Implementation in the shared worktree
- [x] Focused unit/browser regression verification complete
- [ ] Live/integration verification on every affected platform
- [ ] Frozen-snapshot Fable/Claude Max and independent security review

Evidence:

- `node --test web/*.test.mjs tests/navigation.test.mjs`: 165 unit tests passed, including deterministic
  slow-read, continuous-activity, bounded-follow-up, cancellation, and timeout
  tests for the new poller.
- `node --test tests/web_mobile_pulse_browser.mjs tests/navigation_browser.mjs tests/mobile_viewport_browser.mjs`:
  all 10 browser tests passed. The new mobile test uses 3.2-second responses and
  pane events every 100 ms, verifies hash reuse, cancels retired owners, and
  checks Raw/background pause and immediate return. Its final focused rerun
  additionally verifies cancellation and hash clearing for same-pane process
  generation replacement. Existing scroll, grouping, metrics, composer history,
  and navigation regressions remain covered.
- Full serial `cargo test --all-features -- --test-threads=1` passed, including
  integration suites. The final library rerun passed 645 tests, with 6 explicit
  opt-in/fixture skips. Native classification tests reject forged argv, wrong
  owners, symlinks, and writable parent directories, and accept a provably live
  unlinked image. The same unlinked-image regression passed natively on Midnight.
- All 29 native transcript tests passed on Midnight. The opt-in, read-only
  `tmux::tests::live_conversation_maps_selected_pane_without_mutation` passed
  against `ATMUX_LIVE_CONVERSATION_PANE=%40`: Claude PID 7038 mapped successfully
  and returned 240 bounded entries (`truncated=true`). The final check took
  about 0.26 seconds. It did not expose message text or native session IDs.
- `cargo clippy --all-targets --all-features -- -D warnings`, formatting,
  JavaScript syntax, and diff checks passed. The locked all-features release
  build passed; its embedded `app.js`, `app.css`, and `index.html` were verified
  byte-for-byte against the tested sources. Midnight's test build retained only
  its pre-existing non-Linux unused-import/dead-code warnings.
- Midnight's protected tmux server remains PID 26474 on
  `/private/tmp/tmux-501/default`. No server, agent, or web service was restarted;
  no socket was created. Native test sources were staged in
  `/Users/ryan/IdeaProjects/atmux/.conversation-check-GV9CsG`, with builds launched
  from `/Users/ryan/IdeaProjects/atmux`; original source files were preserved.
- One existing browser assertion expected hidden Conversation polling. It was
  updated to verify deferred refresh and catch-up on return, while retaining its
  Raw scroll-position assertions. Native test fixtures use a copied test binary
  so their identity checks do not depend on system utility dispatch/signing behavior.

Not deployed. Full fleet live verification and external reviewer approval remain
open; keep this record active. Compaction settings and billing data are unchanged.

## Requested rollout preflight — 2026-09-12

Ryan reported the same missing Conversation on Midnight's `cve-planner` and
requested checking in and pushing out the work. Pane `%84` still runs PID 10840,
Claude `2.1.263`, whose version file has been removed. The fixed native read-only
test resolved 240 entries from that exact existing process in about 0.24 seconds.
The complete serial Rust suite, 165 JavaScript unit tests, all 10 browser tests,
Clippy, formatting, and diff checks passed again before check-in.

Restoring native process detection also restores eligibility for the existing
automatic-compaction policy. Its tmux-only idle clock is a separate confirmed
bug, so rollout requires Ryan's choice about temporarily disabling atmux-driven
compaction; no such configuration change is implied by this code fix.

## Deployment — 2026-09-26

Deployed fleet-wide as part of runtime `8d5cd01`. The pending compaction decision was resolved by
fixing the idle clock instead of disabling compaction ([auto-compact-idle-clock.md](auto-compact-idle-clock.md#deployment--2026-09-26)).
Live Conversation is available for 37 of 38 agent sessions, including all 17 on Midnight; Tron's
`dispatch` reports it unavailable and was not restarted. The independent-review gate remains open.

