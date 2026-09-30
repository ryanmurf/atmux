# Quick Resume rosters

Quick Resume is atmux's host restart recovery. Each owner machine keeps one
roster script; the dashboard's **Quick resume** button asks the owning node to
run it, and the node runs it only after the script passes the safety checks in
the README ("Quick Resume (host restart recovery)"). A rerun is always safe:
every `new` preserves a session that already exists, and a failed unit rolls
back only the session it created.

## Install on a machine

1. Copy `quick-resume.example.sh` to the machine, by default as
   `quick-resume.sh` beside `config.toml` (`~/.config/atmux/quick-resume.sh`),
   or set `[recovery].script` to another absolute path.
2. `chmod 700` it. The file and every directory above it must be owned by the
   atmux user (or root) and must not be group/world writable. Do not symlink it.
3. Replace the example roster after the `ATMUX_QUICK_RESUME_TRANSACTION_END`
   line with the machine's sessions. Do not edit anything between the
   `TRANSACTION_BEGIN` and `TRANSACTION_END` lines.
4. Optionally list the launchers the roster depends on in
   `[recovery].required_commands`, so the action stays unavailable until a
   fresh machine has them installed.
5. Restart the web service on that node and open the dashboard: the topbar
   shows **Quick resume**, and the dialog lists this machine as ready. If it
   says the script fails its safety checks, re-check steps 2 and 3.

## Roster entries

```bash
new  <session> <cwd>          # creates the detached session, preserved if present
send <session> '<command>'    # typed into the new session; $HOME expands at launch
ok   <session>                # only after a wrapper that opens a startup dialog
```

Use absolute launcher paths or `$HOME`; the script runs without your shell
aliases and with a fixed `PATH`. For Claude, resume with
`--resume <id>` where `<id>` is the newest top-level `<id>.jsonl` under
`$CLAUDE_CONFIG_DIR/projects/<escaped-cwd>/` (the escaping replaces both `/`
and `.` with `-`). A top-level transcript starts with a prompt you typed, not
"You are <agent-name>", and a session that compacts forks to a new id. For
Codex, `codex resume <UUID>` with the rollout id from `~/.codex/sessions/`.
Finish the roster with `finish_unit` and `exit "$resume_failures"` as the
example does.

If the machine's tmux server lives on a named socket, define a `tmux` wrapper
before the transaction block (see the commented line in the example).

## Memory-isolated nodes

A Linux node with `[agent_resources].memory_max_bytes` refuses the direct
bridge. Replace the `send()` block (from the
`# ATMUX_QUICK_RESUME_DIRECT_EXEC_V1` line through the closing `}`) with
`deploy/systemd/resume-tron-scoped-exec-block.bash`, then set its
`scoped_exec_command` to this node's atmux executable and the configuration
file the web service runs with, for example:

```bash
  local scoped_exec_command='/home/ryan/.local/bin/atmux --config /home/ryan/.config/atmux/config.toml scoped-exec'
```

The executable must be a regular file owned by you or root under a
non-writable directory chain, and the configuration path must be the daemon's
own. The `atmux-web` service-cap override lines may stay, change their byte
count, or be removed; `scoped-exec` enforces the cap policy at launch either
way. Everything else in the block must stay byte-exact. Tron's live
`~/resume-tron.sh` is the reference for this shape.
