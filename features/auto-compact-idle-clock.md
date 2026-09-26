# Automatic compaction idle clock

Status: implemented and tested on Linux; macOS runtime, reviews, and deployment pending

## Request

"The timing on the compact is horrible. It does it right after I start using it not when it's been
idle."

## Cause

The `[auto_compact]` inactivity gate read tmux `#{session_activity}`, which moves only on
attached-client input. For panes driven through the dashboard (`send-keys`) it stays at the tmux
server's start time, so every eligible pane looked idle for days and `/compact` fired on the first
30-second poll after a turn ended. On 2026-09-22 this session's answer finished at 21:48:42Z and
atmux sent `/compact` at 21:49:01Z (276,751 tokens). Every pane on the Linux host showed a
`session_activity` about 122 hours old, while `#{window_activity}` matched each pane's real last
output (idle Claude panes stay quiet for days).

## Implementation

- The pane record carries `#{window_activity}` as `Session.output_activity`, next to
  `session_activity`, before any free-text field.
- Auto-compact idle time now starts at the later of the two, so a finished turn, a resumed CLI's
  redraw, or echoed keys each restart the 15-minute clock. A missing or future output time fails
  closed. Rail ordering and the API's `activity` field are unchanged.

## Gates

- [x] Implementation
- [x] Focused tests: parser column, regression for a stale session clock with recent output,
  recent attached input, unknown and future output times; existing threshold/marker tests
- [x] Linux integration: full serialized all-features suite on a disposable tmux socket, including
  the real-tmux smoke test
- [ ] macOS runtime check on Midnight (`window_activity` on its tmux)
- [ ] Fable/Claude Max review
- [ ] Independent security review
- [ ] Deploy owners and confirm no compaction inside 15 minutes of a turn

## Boundaries

Drafts typed only in the browser composer never reach the pane, so they do not delay compaction.
Configuration (`enabled`, thresholds) is unchanged.
