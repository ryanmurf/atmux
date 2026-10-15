# Resizable agent list; one box per prompt in Conversation

Status: deployed fleet-wide 2026-10-14 (runtime `083db5a`, Helm rev 45).

## Why

Two reports from the same afternoon:

- **"How come I can't resize the left bar?"** It was never built. The agent list took
  `minmax(220px, 23vw)` and nothing else, so long session names and descriptions were cut off
  on a wide monitor, and the only control was the collapse arrow.
- **"In Conversation I'm seeing two boxes for everything I type."** Claude Code 2.1.300 added
  new records to its session log. One of them, `api-request-blob`, stores a copy of each message
  that goes into an API request, and it has the same `message.role` and `message.content` as the
  real entry. The Claude reader accepted any record with a role, so each prompt appeared twice:
  once as the real entry and once as a copy with no timestamp and an id like `message-173`.
  Every machine but Clue runs 2.1.300, so every Claude session started since the upgrade was
  affected. A scan of the fleet's transcripts found 27 copies in three Midnight sessions: typed
  prompts and a few "Tool loaded." tool results, each shown a second time.

The same copies had a quieter effect. An assistant copy has no `usage`, and the native context
reader gave up on the whole log when an assistant record had no usage. Auto-compact needs that
count, so it had silently stopped acting on 2.1.300 sessions. It fails closed, so nothing was
compacted wrongly; it just never ran.

## What changed

- **Claude logs.** Only records whose `type` is `user` or `assistant` count as conversation
  entries (`transcript::claude_message_record`). The transcript reader, the native context
  reader, and the Sessions preview all use it. Request copies, `last-prompt`, `api-request`,
  `queue-operation`, and any later bookkeeping record are skipped. Compaction markers are read
  before this check, so they still show.
- **The agent list.** The rail has a drag handle on its right edge. It highlights on hover,
  and dragging sets the width. The width stays at least 180 px and leaves the detail pane at
  least 420 px, so a width saved on a wide monitor shrinks to fit a laptop window instead of
  squeezing the agent view. This browser remembers the width (`atmux.rail-width` in local
  storage). Double-click the edge to go back to the default. The handle is focusable: Left and
  Right move it 16 px (64 px with Shift), and Home and End jump to the limits. It hides when the
  list is collapsed and on phone layouts, where the list is a full-width page.

## Gate

fmt, zero-warning clippy (pedantic), and `cargo +1.88 check --locked`. New tests:

- `claude_request_blobs_are_not_conversation_entries` mixes real 2.1.300 record types (request
  copies of a prompt, an assistant reply and a tool result, plus `last-prompt` and
  `api-request`) with the real entries. The transcript holds one prompt and one reply, and the
  native context count still comes from the real assistant record.
- A JS test covers the width rule: unusable values keep the default, and widths are clamped to
  the minimum and to the window.

The full Rust suite and the JS suites pass. A dev `atmux web` on a private tmux socket was
driven in headless Chrome: dragging resized the list, the width survived a reload, and drags
past either limit stopped at 180 px and at the window minus 420 px. The arrow key moved it
16 px, double-click restored the default and cleared the stored width, and the handle was
hidden at 390 px with no horizontal scroll.

## Deployment (2026-10-14)

| Machine | Artifact SHA-256 prefix | Rollback |
| --- | --- | --- |
| Tron | `c6eedd11bb4dced9` | `target/release/atmux.rollback-23e66ae` |
| Max | `c6eedd11bb4dced9` (Tron's build) | `target/release/atmux.rollback-23e66ae` |
| Midnight | `8f90195d90e8e30d` (built on Midnight, 4 jobs) | `target/release/atmux.rollback-23e66ae` |
| Clue | `6c434114440cf30a` (aarch64, built on Clue) | `~/.local/bin/atmux.rollback-23e66ae` |
| Coordinator | `localhost:32000/atmux:083db5afb948@sha256:1affc1b8…` | Helm rev 44 (`23e66ae`) |

Only `server.image` changed in Helm; the values are in
`/mnt/data/herodevs-agents/atmux-rollout-rail-kKIuR7` on Tron. Tron's web was respawned with
`remain-on-exit` on and came up on the first try. Midnight restarted through the LaunchAgent
kickstart after 16 ptys were opened and closed.

Session counts held: Tron 15 tmux sessions, Max 6, Midnight 20, Clue none (as before). After
the restart, Midnight's own API returned the three affected transcripts with no copies and no
repeated prompts; before, they held 27 copies between them. The coordinator, which forwards to
Midnight, returned the same three transcripts with no copies, and all four machines report
online through it.
