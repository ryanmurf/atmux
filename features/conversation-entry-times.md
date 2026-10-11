# Conversation: when each entry finished

Status: implemented 2026-10-11; deploying.

## Why

Conversation showed who said what but not when. On a quiet agent there was no way to tell
whether its last reply came a minute ago or yesterday without opening the raw pane.

## What changed

- Each card's header shows when the entry finished and how long ago: "2:46 PM · 5m ago" today,
  "Thu 2:46 PM · 2d ago" this week, and only the date after that. Hovering shows the full date
  and time. Times sit on the right edge, beside Copy, so they line up down the conversation.
- A tool call counts from when its result arrived. Owners now send that as `finished_at` beside
  the call's `timestamp`, for both Claude and Codex. A call with no result yet reads
  "started 2:41 PM · 5m ago". An empty result still finishes the call; Codex writes one for
  every `send_message` and `followup_task`. A collapsed run of calls shows its newest call.
- The "ago" part is refreshed every 30 seconds and when the page comes back into view. The
  refresh only rewrites labels whose text changed and never redraws the cards.
- Message headers align on the first text line, so a long subagent name that wraps on a narrow
  phone keeps the time on its first line.

An older owner doesn't send `finished_at`, so its tool calls read "started" with the call time
until it's updated.

## Gate

fmt and zero-warning clippy; `cargo +1.88 check --locked`. The full Rust suite passed with a
disposable `ATMUX_TMUX_SOCKET_NAME` and `--test-threads=8`. Two earlier full runs at default
parallelism, made while Tron's load average was above 30, failed unrelated tests. The first failed
`self_update` ("Text file busy"), `intake` budgets, and `summarizer` enqueue; the second failed a
different `self_update` install test. Each passed alone, and none touches transcripts. 203 JS
tests passed, and all four browser suites passed. The new browser test runs at 360 px with a long, 400-day-old subagent header. It checks every label, right alignment, the
first-line placement, no horizontal overflow, and that the label moves to "10m ago" on return
without a redraw. With the old centered header, the first-line check fails.
