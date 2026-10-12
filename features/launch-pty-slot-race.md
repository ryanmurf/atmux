# Launches on macOS: no more "fork failed: Device not configured"

Status: deployed to Midnight 2026-10-12 (runtime `8c8ba19`).

## Why

On 2026-10-12 every launch on Midnight failed with `respawn pane failed: fork failed: Device not
configured`. The tmux server was healthy: `new-session` worked, and a pty opened from ssh worked.
Only the second step of a launch failed.

atmux starts each session on a placeholder pane (`/bin/sleep 2147483647`) so it can publish the
session's claims before the agent runs, then swaps the agent in with `respawn-pane -k`. That swap
closes the placeholder's pty and opens a new one in the same step, and on macOS that can race the
kernel's pty table. The table grows 16 slots at a time and never shrinks. When it is full, the new
open is handed the slot one past its end. If the placeholder's slot frees while that open is in
flight, the kernel sees a free slot, skips growing the table, and rejects the out-of-range slot
with ENXIO. The mechanism is written up in anthropics/claude-code#93624, which hit it through the
same placeholder-then-respawn sequence.

Midnight matched it: 31 ptys were in use while launches failed, one short of a 32-slot boundary.
A failed open doesn't grow the table, so every retry failed the same way. Launches started working
again as soon as a test created a few extra panes, which grew the table.

The failure also exposed a worse hazard. When a respawn fails, tmux leaves the pane dead with
`pane_pid` -1 and its input state already freed. A second `respawn-pane` on that pane uses the
freed state and ends the whole tmux server. On Tron, a private tmux 3.6 server crashed this way
("server exited unexpectedly"), and tmux 3.7b has the same code. The launch path was safe because
it kills a session whose respawn failed, but the two relaunch paths leave the pane in place. On
Midnight, a later Resume on such a pane would have taken down the protected Aqua tmux server and
every session on it.

## What changed

- Before every `respawn-pane`, atmux on macOS opens 16 pty masters together and closes them.
  When the table is full, that grows it, so tmux's next pty takes a free slot inside the table.
  On Linux this step does nothing; devpts has no such table.
- The two relaunch paths (`resume_claude` and `resume_after_cli_update`) check `#{pane_pid}` first
  and refuse a pane whose last respawn failed. The error asks you to close the pane and launch a
  new session. Closing it is safe: `kill-pane` checks for the freed state.

Midnight's LaunchAgent restarts `atmux-web` with `respawn-pane -k` too (see
`docs/new-machine.md`), so a restart at a 16-slot boundary can fail the same way. A second
kickstart after such a failure would hit the crash above. This change doesn't touch the
LaunchAgent; that's a follow-up.

## Gate

fmt and zero-warning clippy; `cargo +1.88 check --locked`; new tests:

- `a_pane_whose_respawn_failed_is_never_respawned_again` makes a respawn fail on a disposable
  tmux server by leaving the server one spare file descriptor, then checks that the pane reports
  pid -1, that atmux refuses to respawn it, and that the server is still up. With the guard
  disabled, the test fails.
- `spare_ptys_open_together_and_close` opens and closes the 16 spare ptys twice.

The kernel race itself isn't reproducible on demand: an attempt that misses the timing window
grows the table and moves the boundary. A Python test that held ptys up to each boundary and ran
the close-then-open sequence never hit it across seven boundaries, so the evidence for the
mechanism is Midnight's usage count, the identical retries, and the recovery when the table grew.

The full suite passed (954 tests) at 8 threads. Two earlier library runs each failed one test that
writes and then runs a fake executable (`Text file busy`), a load flake seen in earlier rollouts. It
also failed once with the new tests skipped, passed alone three times, and three later full runs
were clean.

## Deployment (2026-10-12)

| Machine | Artifact SHA-256 prefix | Rollback |
| --- | --- | --- |
| Midnight | `12d51f65b3f40de4` (built on Midnight, 4 jobs) | `target/release/atmux.rollback-f7bb4e5` |

Only Midnight needs the pty change; on Linux it does nothing. Tron, Max, Clue, and the coordinator
stay on `f7bb4e5` until the next rollout, which brings them the relaunch guard. The restart used
the LaunchAgent kickstart, after opening and closing 16 ptys so its own `respawn-pane -k` couldn't
hit a full table. All 15 tmux sessions survived. A launch through the coordinator then started
Claude on Midnight and was removed.
