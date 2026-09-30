# Conversation mapping, collapsed compaction, and Raw pane sizing

Status: implemented and tested locally 2026-09-29; fleet rollout pending

## Requests (2026-09-29)

- "Sometimes my sessions don't map to the conversation can you see about harding that."
  Follow-up: "cve-planner-codex converation isn't working."
- "Can you show compaction as a collapsed box instead of full screen?"
- "Raw Pane sometimes doesn't show much, like the height isn't shown correctly."

## Confirmed causes

A fleet probe of every live Claude/Codex pane found 13 of 44 without a Conversation:

- Midnight and Max `max`-profile panes launched with `--dangerously-load-development-channels`.
  Claude writes `sessions/<pid>.json` only after its startup confirmation is answered, and records
  that moment as `startedAt`. Panes still at the prompt had no metadata; `nes-ecosystem`, answered
  69 minutes after launch, failed atmux's 2-minute start-time window.
- Tron `dispatch`: exact metadata, but no log yet because nothing had been typed.
- Midnight `cve-planner-codex`: Codex 0.159 keeps a resumed rollout closed until its next turn, and
  atmux mapped Codex only through open file descriptors.
- Raw pane: every detached window on Tron and Midnight is tmux's default 80×24, and several agents
  draw full-screen without scrollback.

## Implementation

- Claude identity accepts the exact `procStart` (Linux start ticks, macOS UTC `lstart`) whenever
  `startedAt` is outside its window, and rejects metadata whose `tmux` pane, machine id, or PID
  namespace differs. Rejected or ambiguous metadata never falls back.
- Display-only fallbacks: a still-starting Claude process launched with one explicit resume id
  whose log exists in exactly one config root; a resumed Codex process with no open rollout, unless
  another same-directory user thread was created after it started (UUIDv7 creation times, bounded
  day-directory scan). Mutating callers (restart, maintenance, auto-compact) are unchanged.
- Transcripts carry an owner-written `note` for these states, and an empty mapped session is
  available with zero messages.
- Compaction entries (`kind: "compaction"`) with trigger and pre/post token counts; the browser
  renders them as a collapsed box.
- `POST /api/v1/panes/{id}/size` fits a detached single-pane window to the Raw view and unsets
  `window-size` afterwards; the browser measures its cell grid and debounces.

## Gates

- [x] Implementation
- [x] Focused Rust and browser tests
- [ ] Live verification on every owner after rollout (Tron read-only live checks passed pre-rollout)
- [ ] Fable/Claude Max review
- [ ] Independent security review

## Evidence

- Transcript tests cover exact-start mapping with a 69-minute `startedAt` skew, PID reuse, other
  pane/machine/namespace rejection, empty sessions, argv parsing (picker, `--continue`,
  `--fork-session`, conflicting ids, `--` terminator), rejected metadata never falling back, Codex
  resume-id parsing, UUIDv7 times, the newer-thread guard, the bounded scan, and compaction parsing.
- Disposable-tmux test: resize to 150×50, `window-size` unset afterwards, unchanged and split
  windows skipped, invalid panes rejected. Route test: Origin, bounds, unknown fields, offline owner.
- Tron read-only live checks: `dispatch` (empty, noted), `coord` (240 entries), `projects` Codex
  (112), `atmux-fable` (216). The first run caught that `pidDomain` embeds `/etc/machine-id`, not the
  boot id; the check was corrected before any deployment.
