# Automatic compaction idle clock

Status: deployed fleet-wide on 2026-09-26 (runtime `8d5cd01`); post-fix threshold observation and reviews pending

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
- [x] macOS runtime check on Midnight (`window_activity` on tmux 3.7b tracks real output; `session_activity` is 20-163 h stale)
- [ ] Fable/Claude Max review
- [ ] Independent security review
- [x] Deploy owners and coordinator
- [ ] Observe an eligible pane compact only after more than 15 quiet minutes

## Boundaries

Drafts typed only in the browser composer never reach the pane, so they do not delay compaction.
Configuration (`enabled`, thresholds) is unchanged.

## Deployment — 2026-09-26

Ryan asked for all work, including this fix, to be shipped. The rollout also carries the
previously undeployed [Conversation fix](conversation-reliability.md) (`3719d28`), whose rollout
had been held only because of this bug. Runtime commit `8d5cd01` was installed in two passes
(`1048562`, then `8d5cd01`, which adds the rename precheck). Before each install, the binary
was checked to embed the exact committed `app.js`, `app.css`, and `index.html`.

| Machine | Artifact | SHA-256 | Restart |
| --- | --- | --- | --- |
| Tron | local release build | `37a04f7a…b77e60e` | `atmux-web:0.0` respawned in its original `scoped-exec` 56 GiB scope |
| Max | Tron's binary, checksum-verified | same | user `atmux-web.service` (`KillMode=process`) |
| Clue | native ARM64 build from `git archive` | `4c3e6008…b5032e9` | user `atmux-web.service` (`KillMode=process`) |
| Midnight | native macOS build in `.deploy-8d5cd01-5QJPlF` | `677bcfd3…a3ae781` | Aqua `launchctl kickstart -k gui/$(id -u)/dev.herodevs.atmux-web` |
| Coordinator | `localhost:32000/atmux@sha256:86617344…9b2f08f`, OCI revision `8d5cd01` | — | Helm `atmux-web` revision 30 |

- Only web services restarted. Every tmux server kept its PID (Tron 16728, Max 23235, Clue
  169912, Midnight 1061), and before/after snapshots show every agent pane id, pid, and pane
  identity unchanged.
- The Helm render differed from the live manifest only in the two image references. The chart
  render/security tests passed. The pod is 3/3 Ready with zero restarts. Anonymous and
  spoofed-identity public requests still get 302 to login.
- All four owners are online with no health error, all 38 previously visible sessions remain, and
  the coordinator serves the committed assets byte for byte.
- Conversation is available for 37 of 38 agent sessions (Clue 1/1, Max 6/6, Midnight 17/17, Tron
  13/14). Tron's `dispatch` reports it unavailable; this was not investigated, and no agent was
  restarted.
- Before the fix, this week's transcripts on Tron and Midnight show `/compact` 1-3 minutes after a
  turn (four times), and once right after a Quick Resume of a conversation idle for 12 days. None
  has fired since deployment. No pane has yet been eligible for a full 15 quiet minutes, so the
  positive threshold observation stays open.

Rollback: the binaries replaced by each pass are kept beside the installed executable as
`atmux.rollback-1048562` (pre-rollout: `3719d28` on Tron and Midnight, `6c9fe8d` on Max and Clue)
and `atmux.rollback-8d5cd01` (the `1048562` build). The installed paths are
`~/IdeaProjects/atmux/target/release` on Tron, Max, and Midnight, and `~/.local/bin` on Clue. Helm
revisions 28 (`6c9fe8d`) and 29 (`1048562`) remain. Roll back only through each machine's
existing restart mechanism, preserving Midnight's protected tmux server. Private deployment values,
renders, and snapshots are under `/mnt/data/herodevs-agents/atmux-rollout-1048562-Cy5LI5` on Tron.

