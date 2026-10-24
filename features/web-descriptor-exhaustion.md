# Web descriptor exhaustion and the archive that caused it

Status: fixed, rollout pending (branch `dashboard-improvements-20260907`).

## Why

On 2026-10-22 Midnight's `atmux web` stopped answering while it still listened on :7345. Its
pane was full of `atmux archive … failed: Too many open files (os error 24)`, and the process
sat at 0% CPU with one worker on a mutex and one blocking thread in `read()`. It never recovered
on its own. Three separate defects combined to produce it, and all three are fixed here.

## What changed

- **Archiving holds directories, not every file.** `write_bundle` opened and kept open every
  file in a Claude session's tree at once, up to 4,096 descriptors. launchd starts jobs with a
  256-descriptor soft limit, so a session with a few hundred tool results could never be
  archived. The walk now keeps only the tree's directories open (capped at 512) and reopens each
  file through its directory for the manifest pass and again for the copy. A file whose device,
  inode, length, or mtime changes between the two passes still fails the archive, so the
  consistency guarantee is unchanged (`src/registry.rs`).
- **The web raises its own descriptor limit.** `atmux web` now lifts its soft `RLIMIT_NOFILE`
  toward the hard limit at startup, capped at 65,536, and prints the result when it changed. When
  the hard limit is too low to help it says so instead. This covers macOS, where launchd's
  default is 256 and atmux inherits it from the tmux pane (`src/web.rs`).
- **A stuck helper can no longer pin the monitor.** tmux, git, and the platform metric
  collectors are read from blocking threads, often under a lock the request handlers need. A new
  `src/child.rs` drains both pipes from detached reader threads and waits on a deadline alone, so
  a helper that hangs, or a grandchild that keeps a pipe open after its parent exits, becomes a
  timeout rather than a wedged thread. tmux commands get a 30 s ceiling; git keeps its 2 s one;
  the metric collectors keep theirs. The event-hook accept loop now backs off on an accept error
  instead of spinning (`src/events/runtime.rs`).
- **Peer pulls no longer starve local archiving.** A coordinator held the single archive permit
  for the whole duration of a peer bundle download. Those downloads were failing on every node
  with "peer archive size mismatch": the puller trusted the bundle size from the page revision it
  read while the owner served its current, re-archived file. The size/overrun/short-read checks
  are now a typed `PeerArchiveMismatch`, which the puller treats like a missing bundle and skips
  until a later page carries the current record. The permit is taken only for the local finish,
  after the transfer, and a bundle larger than this node's cap is skipped up front
  (`src/registry.rs`, `src/remote.rs`).
- **A rejected archive backs off.** A session over the size cap failed every 30 s forever,
  re-reading its whole tree each time. Retries now start at 30 s and double to just over an hour.

## Gate

fmt and zero-warning clippy (pedantic) on all targets; `cargo check --locked`. New tests:

- `child`: both streams and the exit status are collected; output past the limit keeps an exact
  prefix; a silent child becomes a `TimedOut` error and is killed; a grandchild holding a pipe
  open does not stall the caller.
- `registry`: a 300-file, nested session archives under a 48-descriptor limit (the test re-execs
  itself under `ulimit -n 48`); a rejected archive backs off instead of retrying every interval;
  a bundle the owner no longer serves at the described size does not wedge the pull and leaves no
  staged file behind.
- `web`: the soft descriptor limit ends at the ceiling or the hard limit, and raising twice is a
  no-op.

The full Rust suite passed on a disposable `ATMUX_TMUX_SOCKET_NAME` (856 lib tests plus every
integration suite; the `intake` and `summarizer` parallel-lock flakes each passed alone, as
before). clippy is clean on `--all-targets`.

A dev web on Tron under `ulimit -n 64`, driven to exhaustion by 90 idle loopback connections:
while every descriptor was held it could not accept, as expected, and once the connections
closed it answered again on its own within two seconds. It also logged
`atmux web  open files 64: raise the limit before archiving large sessions` at startup, since its
hard limit left no room to raise. The pre-fix binary in the same situation never recovered.

## Deployment

Pending. Tron/Max in-place and scp, Clue and Midnight native builds, coordinator Helm image swap,
per [[atmux-fleet-rollout-procedure]]. On Midnight the LaunchAgent already carries a
`ulimit -n 65536` that a prior session added by hand; the binary's own raise makes that redundant
but harmless.
