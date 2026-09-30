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
