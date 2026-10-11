# Conversation: no blank reconnects, digests on Tron's Qwen

Status: deployed fleet-wide 2026-10-11 (runtime `4f18199`).

## Why

On a phone, Conversation went blank every time you switched apps and came back. Returning to the
page reconnected the pane, and the reconnect wiped the conversation and its summary and then
downloaded the whole transcript again. Nothing showed until that finished, which over a phone
connection through the coordinator to Midnight took a noticeable while. A pane stream resync did
the same thing.

The Summary heading made it look like the cause. It said "Summary · refreshing" whenever the owner
marked the digest stale, which for a working agent is nearly always. Refreshes weren't running
anyway: every request went to Max's `qwen3.8-flash-next`, which the reach-poc queue kept busy, so
each one hit the 90-second timeout. The coordinator logged "summary refresh failed" with no reason.

## What changed

- A reconnect to the same pane output keeps the conversation and summary on screen and polls with
  the hash it already has. Selecting an agent, or a pane ID reused by a new owner, still starts
  from nothing.
- The Summary heading says when the digest was written ("Summary · updated 8:41 AM"). Status and
  staleness changes no longer rebuild the whole transcript; only a new title, digest, or update
  time redraws it.
- `[summaries]` and the shared LLM client gain `enable_thinking`, sent as Qwen's
  `chat_template_kwargs` switch. Without it, Qwen 27B spends the 4,096-token reply budget
  reasoning before it writes the digest. The chart renders it and `timeout_seconds`.
- A failed refresh logs its outermost reason, such as a timeout or an HTTP status. Inner parse
  errors can quote model output, so they stay out of the log.
- The coordinator's digests now use Tron's Qwen3.8 27B (`127.0.0.1:8195`, one slot, 81,920-token
  context). The pod can't reach loopback, so the user unit `atmux-summaries-qwen-forward.service`
  on Tron forwards `10.1.112.64:18195` (Tron's pod-network address, pod clients only) to it. A
  real 22 KB digest prompt took 27 seconds there.

## Gate

fmt and zero-warning clippy; `cargo +1.88 check --locked`; the full Rust suite passed with a
disposable `ATMUX_TMUX_SOCKET_NAME`. `self_update::a_second_apply_is_refused_while_the_first_is_still_running`
failed once while four browser suites ran beside it, then passed three times alone and in a full
rerun. 202 JS tests passed. All four browser suites passed. The mobile suite now hides and re-shows
the page during a slow transcript poll and checks that no card or the open summary ever leaves the
screen. With the old reset restored, that test fails. The existing same-pane resync test expected
the old rebuild; it now checks that the stale tool-group callback is stopped by its generation
check while the cards stay connected.

## Deployment (2026-10-11)

| Machine | Artifact SHA-256 prefix | Rollback |
| --- | --- | --- |
| Tron | `548331686bbf35db` | `target/release/atmux.rollback-334e6d5` |
| Max | `548331686bbf35db` (Tron's build) | `target/release/atmux.rollback-334e6d5` |
| Midnight | `592529e2d0bef38d` (built on Midnight, 4 jobs) | `target/release/atmux.rollback-334e6d5` |
| Clue | `11022debd79a3438` (aarch64, built on Clue) | `~/.local/bin/atmux.rollback-334e6d5` |
| Coordinator | `localhost:32000/atmux:4f181997a4f1@sha256:0b877de7…` | Helm rev 39 (`334e6d5`, digests on Max) |

The coordinator is Helm rev 40. Its values changed `server.image` and the `server.summaries` block
(endpoint, model, allow list, egress, `timeoutSeconds: 300`, `enableThinking: false`). Before and
release values live in `/mnt/data/herodevs-agents/atmux-rollout-summaries-MeFral` on Tron. Session
counts held across restarts: Tron 15, Max 6, Midnight 14 (LaunchAgent kickstart only), Clue 1.

On the coordinator, hiding and re-showing the page on a Midnight agent kept all 165 cards on screen
with no redraw. The first digest request reached Tron's Qwen through the forwarder within a minute
of the upgrade.
