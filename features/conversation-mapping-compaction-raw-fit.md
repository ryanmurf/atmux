# Conversation mapping, collapsed compaction, and Raw pane sizing

Status: deployed fleet-wide 2026-09-30 UTC; review gates pending

## Requests (2026-09-29)

- "Sometimes my sessions don't map to the conversation can you see about harding that."
  Follow-up: "cve-planner-codex converation isn't working."
- "Can you show compaction as a collapsed box instead of full screen?"
- "Raw Pane sometimes doesn't show much, like the height isn't shown correctly."
- "Sometimes I'm seeing a disconnected thing."

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

- Disconnected banner: every browser event stream through atmux.murphytek.com ended together at
  each wall-clock 5-minute mark. The browser is Chrome on Tron. The `herodevs/containerd-watchdog`
  CronJob (`*/5`) starts a pod whose Calico veth appears at :00 and disappears at :03, and Chrome
  drops its connections on that network change. atmux (420 s direct stream), the in-pod gateway
  (400 s), the ingress/hostPort path and pod-network TCP (keep-alive connections) all stayed up
  across the marks, and the event stream had no revision gap.

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
- The dashboard waits 8 seconds before raising the disconnected banner (the pill says
  Reconnecting meanwhile), and both event streams ask browsers to retry after 1 second. The root
  cause is outside atmux: run the watchdog Job with `hostNetwork: true` or less often.
- `POST /api/v1/panes/{id}/size` fits a detached single-pane window to the Raw view and unsets
  `window-size` afterwards; the browser measures its cell grid and debounces.

## Gates

- [x] Implementation
- [x] Focused Rust and browser tests
- [x] Live verification on every owner after rollout
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

## Deployment — 2026-09-30 (UTC)

Runtime commit `38bc395` (merge of the Conversation/compaction/Raw-fit and IDE-viewer work, on top
of per-machine Quick Resume `3e6a7a1`). Each binary was checked to embed the committed `app.js`,
`app.css`, and `index.html`, and the coordinator serves all three byte for byte.

| Machine | Artifact | SHA-256 prefix | Restart |
| --- | --- | --- | --- |
| Tron | local release build | `e2783566c5a68102` | `atmux-web:0.0` respawned in its original `scoped-exec` 56 GiB scope |
| Max | Tron's binary, checksum-verified | `e2783566c5a68102` | user `atmux-web.service` |
| Midnight | native macOS build in `.deploy-38bc395-WN5atA` | `399f54b904b70e78` | Aqua `launchctl kickstart -k gui/$(id -u)/dev.herodevs.atmux-web` |
| Clue | native ARM64 build in `~/atmux-build-38bc395-bQx5XI` | `33beed5e3e8a53c3` | user `atmux-web.service` |
| Coordinator | `localhost:32000/atmux@sha256:3c929925…a3e12` | — | Helm `atmux-web` revision 31 |

- Only web services restarted. Every tmux server kept its pid (Tron 11243, Max 7182, Midnight
  42550, Clue 169912) and every agent pane kept its id, pid, and identity.
- The Helm render differed from the live manifest only in the two image references.
- Through the coordinator: all four owners online with no health error; Conversation available
  for 40 of 41 live agent panes (31 of 44 before). The remaining pane is Clue's Codex, whose open
  rollout was archived and deleted from disk while the process kept running.
- Rollback copies: Tron `target/release/atmux.rollback-3e6a7a1`, Max
  `target/release/atmux.rollback-3e6a7a1`, Midnight `target/release/atmux.rollback-8d5cd01-pre-38bc395`,
  Clue `~/.local/bin/atmux.rollback-8d5cd01-pre-38bc395`, Helm revision 30. Private snapshots and
  values are in `/mnt/data/herodevs-agents/atmux-rollout-38bc395-bBNq2p` on Tron.

Live checks after rollout, through the coordinator:

- `cve-planner-codex` (Midnight) shows 131 entries with the resumed-Codex note; startup-prompt
  `max` panes on Midnight and Max show their resumed conversation with the startup note;
  `nes-ecosystem` maps through `procStart`; Tron `dispatch` reports no messages yet.
- 30 panes include `compaction` entries.
- `POST /api/v1/panes/{id}/size` on a disposable Tron session: 80×24 to 150×45 (`resized`),
  `window-size` unset afterwards, `unchanged` on repeat, 400 for 10 columns; session removed.
