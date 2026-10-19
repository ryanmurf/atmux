# Machine page: Restart machine

Status: deploying (runtime `TBD`).

## Why

Rebooting a machine meant an SSH session or, for Midnight, a walk to the Mac. The machine page
already shows what the machine is doing; it's the natural place to ask it to restart, with the
same confirmation discipline as the update buttons that restart atmux itself.

## What changed

- **One fixed command per machine.** The owning node runs `[restart].command` from its own
  configuration, or a platform default when the section is unset: `sudo -n systemctl reboot` on
  Linux, a System Events restart on macOS (`src/host_restart.rs`). The browser can only name the
  machine; the request body has no fields, and a body that carries a command is refused. Output
  is discarded, and the status a caller sees holds a phase and a message, never the command line.
  `enabled = false` takes the button off a machine.
- **Availability is probed, not promised.** The node reports `unavailable` when restart is
  disabled or the program is missing or not executable, so the page shows the reason instead of a
  button that would fail. A command the node cannot run with a known user identity is not
  offered either.
- **One attempt at a time, retries allowed.** While the command runs the phase is `restarting`
  and a second request gets 409. Success leaves `requested`; the machine then goes offline on its
  own. A non-zero exit or a 30 s timeout leaves `failed` with the reason, and `sudo -n` exiting 1
  gets the hint that it needs a passwordless sudoers entry. Either settled phase accepts another
  request, because a reboot that didn't happen is worth retrying once the cause is fixed.
- **Federation.** `GET`/`POST /api/v1/machines/{id}/restart`. A coordinator forwards the machine
  id and an empty body to the owning node, exactly like Quick Resume, and never runs the command
  on the owner's behalf. Origin is checked before anything runs.
- **The page.** A Power card below Software holds one **Restart machine…** button. It confirms
  with a dialog that names the machine, says it reboots the whole computer and not just atmux,
  and that every tmux session and agent on it stops. The card shows the node's message, a status
  line while the restart is in flight (polled every 5 s until the node stops answering), and the
  failure reason in amber. The button is disabled while a request is in flight, while the node is
  restarting, and when the machine is offline.

Config (`config.toml`):

```toml
[restart]
# enabled = true
# command = ["/usr/bin/sudo", "-n", "/usr/bin/systemctl", "reboot"]
```

Tron, Max, and Clue have passwordless sudo for the atmux user, so the Linux default works there
without a config change. Midnight's sudo needs a password, so it uses the macOS default, which
runs in the Aqua session the LaunchAgent lives in; the first use may ask for Automation
permission to control System Events on the Mac's screen.

## Gate

fmt and zero-warning clippy (pedantic); `cargo +1.88 check --locked`. New tests:

- `host_restart`: the platform default is absolute; a missing program, a disabled section, and a
  node without an identity all fail closed before anything is spawned; a succeeding command ends
  in `requested`; a failing one reports its exit status and accepts a retry; a stuck one is
  refused while running, then times out and reports it; the sudo hint only applies to sudo; the
  status serializes without the command line.
- `config`: `[restart].command` expands `~`, must start with an absolute path, and must name a
  program; defaults are enabled with no command.
- `web`: the local route reads status, refuses anonymous and cross-origin requests, refuses a body
  that supplies a command, returns 409 when the node cannot run one, 400 for an unknown machine,
  and 503 for an offline owner.
- `tests/federation.rs`: a coordinator forwards status and start to the owner with an empty body,
  and nothing it sends or returns names `sudo` or `reboot`.
- JS: the Power card model offers the button only when the node says it can, moves in-flight and
  requested messages to the status line, treats a failure as an error with the button live, and
  the confirmation names the machine.

The full Rust suite passed on a disposable `ATMUX_TMUX_SOCKET_NAME` (848 lib tests plus the
integration suites; one intake test tripped its own store lock under parallel load and passed
alone, as before). 216 JS tests passed, including the five browser navigation tests.

Three dev webs on Tron, each on a private socket:

- With `command = ["/bin/sh", "-c", "exit 0"]`, headless Chrome at 1280 px opened the machine
  page, saw the card ready, opened the dialog ("Restart Probe?", "Reboot Probe now. This restarts
  the whole computer, not just atmux."), cancelled it, confirmed it, and saw the card move to
  "Restart requested" with the toast. At 390 px there was no horizontal scroll.
- With a fake `sudo` that exits 1, the card showed "Restart command exited with status 1; sudo
  needs a passwordless entry for it" and left the button live.
- With no `[restart]` section, Tron's own default reported `available: true`; nothing was posted
  to it.

No real machine was rebooted during the check.

## Deployment

TBD.
