# ATMUX_QUICK_RESUME_SCOPED_EXEC_V1
#
# Replace the live resume-tron.sh `send()` function with this block. Keep the
# marker as its own exact line: atmux refuses Quick Resume without it. The
# canonical script remains owner-validated and pins every roster command; this
# bridge makes each restored command independently preflight and enter its own
# configured MemoryMax scope before the agent starts.
# Variables and helper functions are defined by the validated canonical script
# into which this replacement block is inserted.
# shellcheck disable=SC2154
send() {
  local scoped_exec_command='/home/ryan/.local/bin/atmux --config /home/ryan/.config/atmux/config.toml scoped-exec'
  [ "$unit_state" = created ] || return 0
  if [ "$unit_session" = atmux-web ]; then
    scoped_exec_command+=' --recovery-service-memory-max-bytes 60129542144'
  fi
  if [ "$unit_session" != "$1" ]; then
    fail_unit "launch target mismatch"
  elif ! session_belongs_to_unit; then
    fail_unit "launch ownership"
  elif ! tmux send-keys -t "$created_session_id" "exec $scoped_exec_command -- $2" Enter; then
    fail_unit "launch input"
  fi
}
