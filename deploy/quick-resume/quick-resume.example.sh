#!/usr/bin/env bash
# quick-resume.sh — recreate this machine's agent tmux sessions after a reboot.
#
# atmux runs this file from the dashboard's "Quick resume" action. It only
# runs when the file is owned by the atmux user, is not group/world writable,
# is not a symlink, keeps the two marker lines below, and carries the exact
# transaction block between ATMUX_QUICK_RESUME_TRANSACTION_BEGIN/END. Edit the
# roster after the block; leave the block itself untouched.
#
# The script runs with an empty environment plus HOME, USER, LOGNAME, LANG and
# a fixed PATH (~/.local/bin, ~/.asdf/shims, /opt/homebrew/bin on macOS, and
# the system directories). Use absolute launcher paths or $HOME; shell aliases
# do not resolve here. Output is discarded, so a rerun must be safe: every
# `new` preserves a session that already exists.
#
# Each pane is recreated in its original cwd and resumed on its real native
# session id. For Claude the id is the newest TOP-LEVEL <id>.jsonl under
# $CLAUDE_CONFIG_DIR/projects/<escaped-cwd>/ (a top-level transcript starts
# with a prompt you typed, not "You are <agent-name>"); a session that compacts
# forks to a NEW id. For Codex use `codex resume <UUID>` with the rollout id
# from ~/.codex/sessions/. Resume does not bring back child jobs that were
# running inside an agent; nudge the session afterwards if needed.
#
# A wrapper that loads development channels opens a confirm dialog at startup;
# `ok <session>` clears it for a newly created session. Plain `claude` and
# `codex` launches need no `ok`.
set -uo pipefail

# If this machine's protected tmux server lives on a named socket, route every
# tmux call there. Define this before the transaction block; the block itself
# must stay byte-exact.
# tmux() { command tmux -L aqua "$@"; }

# ATMUX_QUICK_RESUME_IDEMPOTENT_V1
# ATMUX_QUICK_RESUME_TRANSACTION_BEGIN
# shellcheck shell=bash
# `new` and its following `send`/`ok` form one recovery unit. A rerun must
# never type a launch command into a session that survived or was already
# restored by an earlier partial run. Each unit is committed only after its
# newly-created session still exists following launch/dialog input. A failed
# unit rolls back only the session created by this invocation, while later
# units continue so a subsequent run can repair just the missing sessions.
resume_failures=0
created_session_id=
unit_session=
unit_state=idle

valid_session_id() {
  [[ $1 =~ ^\$[0-9]+$ ]]
}

session_belongs_to_unit() {
  local resolved_id
  local resolved_name
  [ -n "$created_session_id" ] && valid_session_id "$created_session_id" \
    || return 1
  tmux has-session -t "$created_session_id" 2>/dev/null || return 1
  resolved_name=$(tmux display-message -p -t "$created_session_id" '#{session_name}' 2>/dev/null) \
    || return 1
  resolved_id=$(tmux display-message -p -t "=$unit_session:" '#{session_id}' 2>/dev/null) \
    || return 1
  [ "$resolved_name" = "$unit_session" ] && [ "$resolved_id" = "$created_session_id" ]
}

fail_unit() {
  local stage=$1
  local rollback_session_id
  if [ "$unit_state" != failed ]; then
    resume_failures=$((resume_failures + 1))
  fi
  printf '  FAILED: %s (%s)\n' "${unit_session:-unknown session}" "$stage" >&2

  # Never roll back by name. The created session's immutable tmux id must
  # still resolve to this unit's name, and that name must still resolve back
  # to the same id. If another process replaced it, leave the replacement.
  if session_belongs_to_unit; then
    rollback_session_id=$created_session_id
    if ! tmux kill-session -t "$rollback_session_id" 2>/dev/null \
      && session_belongs_to_unit; then
      printf '  FAILED: %s (rollback incomplete)\n' "$unit_session" >&2
    fi
  fi
  created_session_id=
  unit_state=failed
}

finish_unit() {
  [ "$unit_state" = created ] || return 0

  # Give tmux time to process the queued launch and catch commands that exit
  # immediately rather than leaving behind a false-positive empty session.
  if ! sleep 1 || ! session_belongs_to_unit; then
    fail_unit "verification"
    return 0
  fi

  created_session_id=
  unit_state=committed
}

new() {
  local new_session_output
  local new_session_status
  finish_unit
  created_session_id=
  unit_session=$1
  unit_state=pending
  if tmux has-session -t "=$1" 2>/dev/null; then
    echo "  preserved existing: $1"
    unit_state=existing
    return 0
  fi

  new_session_output=
  if new_session_output=$(tmux new-session -d -P -F '#{session_id}' -s "$1" -c "$2"); then
    new_session_status=0
  else
    new_session_status=$?
  fi
  if valid_session_id "$new_session_output"; then
    created_session_id=$new_session_output
    unit_state=created
  fi
  if [ "$new_session_status" -ne 0 ]; then
    fail_unit "create"
  elif [ "$unit_state" != created ] || ! session_belongs_to_unit; then
    fail_unit "create identity"
  fi
}
# ATMUX_QUICK_RESUME_DIRECT_EXEC_V1
#
# Launch bridge for a machine without per-agent memory isolation, such as
# macOS or a Linux host with no [agent_resources] policy. atmux accepts this
# block only where that policy is absent; a node that enforces MemoryMax must
# use deploy/systemd/resume-tron-scoped-exec-block.bash instead. Keep the
# marker as its own exact line: atmux refuses Quick Resume without it.
# Variables and helper functions are defined by the validated canonical script
# into which this block is inserted.
# shellcheck disable=SC2154
send() {
  [ "$unit_state" = created ] || return 0
  if [ "$unit_session" != "$1" ]; then
    fail_unit "launch target mismatch"
  elif ! session_belongs_to_unit; then
    fail_unit "launch ownership"
  elif ! tmux send-keys -t "$created_session_id" "exec $2" Enter; then
    fail_unit "launch input"
  fi
}
ok() {
  [ "$unit_state" = created ] || return 0
  if ! sleep 8 || ! session_belongs_to_unit \
    || ! tmux send-keys -t "$created_session_id" Enter; then
    fail_unit "dialog input"
    return 0
  fi
  finish_unit
}   # clear the dev-channels dialog only for a newly created session

resume_exit_trap() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [ "$unit_state" = created ]; then
    fail_unit "unexpected exit"
    [ "$status" -ne 0 ] || status=1
  elif [ "$status" -eq 0 ] && [ "$resume_failures" -ne 0 ]; then
    status=$resume_failures
  fi
  exit "$status"
}

resume_signal_trap() {
  local signal_name=$1
  local signal_status=$2
  trap - INT TERM HUP
  if [ "$unit_state" = created ]; then
    fail_unit "interrupted by $signal_name"
  fi
  exit "$signal_status"
}

trap 'resume_signal_trap HUP 129' HUP
trap 'resume_signal_trap INT 130' INT
trap 'resume_signal_trap TERM 143' TERM
trap resume_exit_trap EXIT
# ATMUX_QUICK_RESUME_TRANSACTION_END

# ---- roster ---------------------------------------------------------------
# new  <session> <cwd>
# send <session> '<launch command>'   # quoted once; $HOME expands at launch
# ok   <session>                      # only for wrappers with a startup dialog

new  example-claude "$HOME/IdeaProjects/example"
send example-claude '$HOME/.local/bin/claude --resume 00000000-0000-0000-0000-000000000000'

new  example-codex "$HOME/IdeaProjects/example"
send example-codex 'codex resume 00000000-0000-0000-0000-000000000000'
finish_unit

if [ "$resume_failures" -ne 0 ]; then
  printf 'quick resume finished with %s failed recovery unit(s)\n' "$resume_failures" >&2
fi
exit "$resume_failures"
