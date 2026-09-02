#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
helper_fixture="$repo_root/tests/fixtures/resume_tron_transactional_helpers.sh"
canonical_script=${ATMUX_TRON_CANONICAL_SCRIPT:-/home/ryan/resume-tron.sh}
test_root=$(mktemp -d)
socket_name="atmux-test-resume-${BASHPID}-${RANDOM}"
export TMUX_TMPDIR="$test_root/tmux-tmp"
mkdir -m 700 "$TMUX_TMPDIR"

socket_tmux() {
  command tmux -L "$socket_name" "$@"
}

cleanup() {
  command tmux -L "$socket_name" kill-server 2>/dev/null || true
  rm -rf -- "$test_root"
}
trap cleanup EXIT

fail() {
  printf 'not ok - %s\n' "$1" >&2
  exit 1
}

assert_session() {
  socket_tmux has-session -t "=$1" 2>/dev/null \
    || fail "expected disposable session $1"
}

assert_no_session() {
  if socket_tmux has-session -t "=$1" 2>/dev/null; then
    fail "unexpected disposable session $1"
  fi
}

assert_count() {
  local expected=$1
  local pattern=$2
  local actual=0
  if [ -f "$launch_log" ]; then
    actual=$(grep -Fxc "$pattern" "$launch_log" || true)
  fi
  [ "$actual" -eq "$expected" ] \
    || fail "expected $expected '$pattern' launches, got $actual"
}

wait_for_count() {
  local expected=$1
  local pattern=$2
  local attempt
  for ((attempt = 0; attempt < 100; attempt++)); do
    if [ -f "$launch_log" ] \
      && [ "$(grep -Fxc "$pattern" "$launch_log" || true)" -ge "$expected" ]; then
      return 0
    fi
    sleep 0.02
  done
  fail "timed out waiting for $expected '$pattern' launches"
}

wait_for_file() {
  local path=$1
  local attempt
  for ((attempt = 0; attempt < 100; attempt++)); do
    [ -s "$path" ] && return 0
    sleep 0.01
  done
  fail "timed out waiting for fixture state file"
}

# The production helper is read for parity only. This test never invokes the
# canonical script or any of its real roster commands.
if [ -f "$canonical_script" ]; then
  extracted_helper="$test_root/canonical-helper.read-only"
  awk '
    /# ATMUX_QUICK_RESUME_TRANSACTION_BEGIN/ { copying=1; next }
    /# ATMUX_QUICK_RESUME_TRANSACTION_END/ { copying=0 }
    copying { print }
  ' "$canonical_script" >"$extracted_helper"
  cmp -s "$helper_fixture" "$extracted_helper" \
    || fail "checked-in helper fixture differs from canonical helper block"
fi

generated_helper="$test_root/generated-helpers.sh"
generated_runner="$test_root/generated-resume-fixture.sh"
cp "$helper_fixture" "$generated_helper"

cat >"$generated_runner" <<'FIXTURE'
#!/usr/bin/env bash
set -uo pipefail
[ "${TRACE_FIXTURE:-0}" = 1 ] && set -x

if [ "${1:-}" = worker ]; then
  printf '%s\n' "$2" >>"$LAUNCH_LOG"
  exec sleep 300
fi

# Every external tmux invocation in this generated fixture is forced through
# the caller-provided disposable named socket.
tmux() {
  local operation=$1
  local target=
  local target_name=
  local format=
  local previous=
  local argument
  local send_status
  local wait_attempt
  local index
  local worker_prefix='exec /home/ryan/.local/bin/atmux --config /home/ryan/.config/atmux/config.toml scoped-exec -- '
  local service_prefix='exec /home/ryan/.local/bin/atmux --config /home/ryan/.config/atmux/config.toml scoped-exec --recovery-service-memory-max-bytes 60129542144 -- '
  local -a forwarded
  shift
  forwarded=("$@")
  # Exercise the exact production helper while keeping every launched command
  # inside this disposable socket rather than entering a real systemd scope.
  if [ "$operation" = send-keys ]; then
    for index in "${!forwarded[@]}"; do
      if [[ ${forwarded[$index]} == "$service_prefix"* ]]; then
        forwarded[$index]="exec ${forwarded[$index]#"$service_prefix"}"
      elif [[ ${forwarded[$index]} == "$worker_prefix"* ]]; then
        forwarded[$index]="exec ${forwarded[$index]#"$worker_prefix"}"
      fi
    done
    set -- "${forwarded[@]}"
  fi
  for argument in "$@"; do
    if [ "$previous" = -t ] || [ "$previous" = -s ]; then
      target=$argument
    elif [ "$previous" = -F ]; then
      format=$argument
    fi
    if [[ $argument == '#{'* ]]; then
      format=$argument
    fi
    previous=$argument
  done

  target_name=${target#=}
  target_name=${target_name%:}
  if [[ $target == \$* ]]; then
    target_name=$(command tmux -L "$ATMUX_TEST_SOCKET" display-message -p -t "$target" '#{session_name}' 2>/dev/null) \
      || target_name=
  fi

  if [ "$RUN_MODE" = fail ]; then
    if [ "$operation" = new-session ] && [ "$target" = fail-create ]; then
      return 90
    fi
    if [ "$operation" = new-session ] && [ "$target" = partial-create ]; then
      command tmux -L "$ATMUX_TEST_SOCKET" "$operation" "$@"
      return 90
    fi
    if [ "$operation" = send-keys ] && [ "$target_name" = fail-send ]; then
      return 91
    fi
    if [ "$operation" = send-keys ] && [ "$target_name" = fail-dialog ]; then
      fail_dialog_sends=$((fail_dialog_sends + 1))
      if [ "$fail_dialog_sends" -eq 2 ]; then
        return 92
      fi
    fi
    if [ "$operation" = display-message ] && [ "$target_name" = fail-verify ] \
      && [ "$format" = '#{session_id}' ] \
      && [ -e "$FAULT_STATE_DIR/fail-verify-armed" ]; then
      rm -f -- "$FAULT_STATE_DIR/fail-verify-armed"
      return 93
    fi
    if [ "$operation" = send-keys ] && [ "$target_name" = fail-verify ]; then
      command tmux -L "$ATMUX_TEST_SOCKET" "$operation" "$@" || return
      : >"$FAULT_STATE_DIR/fail-verify-armed"
      return 0
    fi
    if [ "$operation" = send-keys ] && [ "$target_name" = race-replacement ]; then
      command tmux -L "$ATMUX_TEST_SOCKET" "$operation" "$@"
      send_status=$?
      [ "$send_status" -eq 0 ] || return "$send_status"
      printf '%s\n' "$target" >"$RACE_OWNED_ID_FILE"
      command tmux -L "$ATMUX_TEST_SOCKET" kill-session -t "$target" || return
      command tmux -L "$ATMUX_TEST_SOCKET" new-session -d -s race-replacement -c "$TEST_CWD" || return
      printf 'replacement-alive\n' >"$RACE_CANARY_FILE"
      return 0
    fi
    if [ "$operation" = send-keys ] && [ "$target_name" = fail-immediate ]; then
      command tmux -L "$ATMUX_TEST_SOCKET" "$operation" "$@" || return
      for ((wait_attempt = 0; wait_attempt < 100; wait_attempt++)); do
        command tmux -L "$ATMUX_TEST_SOCKET" has-session -t "$target" 2>/dev/null \
          || return 0
        command sleep 0.01
      done
      return 94
    fi
  fi

  command tmux -L "$ATMUX_TEST_SOCKET" "$operation" "$@"
}

# Production waits are useful for real programs and dialogs. The fixture uses
# deterministic tmux fault injection, so elide only those helper delays.
sleep() { :; }

fail_dialog_sends=0
# shellcheck source=/dev/null
source "$GENERATED_HELPER"

if [ "$RUN_MODE" = interrupt ] || [ "$RUN_MODE" = exit-active ]; then
  new "$INTERRUPT_SESSION" "$TEST_CWD"
  send "$INTERRUPT_SESSION" 'bash "$GENERATED_RUNNER" worker interrupt-worker'
  printf '%s\n' "$created_session_id" >"$INTERRUPT_ID_FILE"
  if [ "$RUN_MODE" = exit-active ]; then
    exit 0
  fi
  while :; do
    command sleep 0.05
  done
  exit 95
fi

new existing "$TEST_CWD"
send existing 'touch "$CANARY_FILE"'
ok existing

new fail-create "$TEST_CWD"
send fail-create 'bash "$GENERATED_RUNNER" worker fail-create'

new partial-create "$TEST_CWD"
send partial-create 'bash "$GENERATED_RUNNER" worker partial-create'

new fail-send "$TEST_CWD"
send fail-send 'bash "$GENERATED_RUNNER" worker fail-send'

new fail-dialog "$TEST_CWD"
send fail-dialog 'bash "$GENERATED_RUNNER" worker fail-dialog'
ok fail-dialog

new fail-verify "$TEST_CWD"
send fail-verify 'bash "$GENERATED_RUNNER" worker fail-verify'

new race-replacement "$TEST_CWD"
send race-replacement 'bash "$GENERATED_RUNNER" worker RACE_INPUT_SENTINEL'

new fail-immediate "$TEST_CWD"
if [ "$RUN_MODE" = fail ]; then
  send fail-immediate 'false'
else
  send fail-immediate 'bash "$GENERATED_RUNNER" worker fail-immediate'
fi

new success "$TEST_CWD"
send success 'bash "$GENERATED_RUNNER" worker success'
finish_unit

exit "$resume_failures"
FIXTURE
chmod 700 "$generated_runner"

export ATMUX_TEST_SOCKET="$socket_name"
export GENERATED_HELPER="$generated_helper"
export GENERATED_RUNNER="$generated_runner"
export TEST_CWD="$test_root"
export FAULT_STATE_DIR="$test_root/fault-state"
export CANARY_FILE="$test_root/canary-corrupted"
export LAUNCH_LOG="$test_root/launch.log"
export RACE_OWNED_ID_FILE="$test_root/race-owned-id"
export RACE_CANARY_FILE="$test_root/race-canary"
mkdir -m 700 "$FAULT_STATE_DIR"
launch_log=$LAUNCH_LOG

socket_tmux new-session -d -s existing -c "$test_root"
existing_pane_pid=$(socket_tmux display-message -p -t '=existing:' '#{pane_pid}')

set +e
RUN_MODE=fail bash "$generated_runner" >"$test_root/first.stdout" 2>"$test_root/first.stderr"
first_status=$?
set -e

if [ "$first_status" -ne 7 ]; then
  sed 's/^/  fixture: /' "$test_root/first.stderr" >&2
  fail "first fixture run should aggregate seven failures, got $first_status"
fi
[ ! -e "$CANARY_FILE" ] || fail "existing session received launch/dialog keys"
[ "$(socket_tmux display-message -p -t '=existing:' '#{pane_pid}')" = "$existing_pane_pid" ] \
  || fail "existing session was replaced"
assert_session existing
assert_no_session fail-create
assert_no_session partial-create
assert_no_session fail-send
assert_no_session fail-dialog
assert_no_session fail-verify
assert_session race-replacement
assert_no_session fail-immediate
assert_session success
race_owned_id=$(<"$RACE_OWNED_ID_FILE")
race_replacement_id=$(socket_tmux display-message -p -t '=race-replacement:' '#{session_id}')
[[ $race_owned_id =~ ^\$[0-9]+$ ]] || fail "race fixture did not capture owned session id"
[ "$race_owned_id" != "$race_replacement_id" ] \
  || fail "race fixture did not replace the owned session"
if socket_tmux has-session -t "$race_owned_id" 2>/dev/null; then
  fail "failed owned race session still exists"
fi
[ "$(<"$RACE_CANARY_FILE")" = replacement-alive ] \
  || fail "replacement canary did not survive rollback"
race_replacement_pid=$(socket_tmux display-message -p -t '=race-replacement:' '#{pane_pid}')
if socket_tmux capture-pane -p -t '=race-replacement:' | grep -Fq RACE_INPUT_SENTINEL; then
  fail "replacement session received launch keys"
fi
wait_for_count 1 success
assert_count 1 success
grep -Fq 'fail-create (create)' "$test_root/first.stderr" \
  || fail "create failure was not reported"
grep -Fq 'partial-create (create)' "$test_root/first.stderr" \
  || fail "partial create failure was not reported"
grep -Fq 'fail-send (launch input)' "$test_root/first.stderr" \
  || fail "send failure was not reported"
grep -Fq 'fail-dialog (dialog input)' "$test_root/first.stderr" \
  || fail "dialog failure was not reported"
grep -Fq 'fail-verify (verification)' "$test_root/first.stderr" \
  || fail "verification failure was not reported"
grep -Fq 'race-replacement (verification)' "$test_root/first.stderr" \
  || fail "replacement race was not reported"
grep -Fq 'fail-immediate (verification)' "$test_root/first.stderr" \
  || fail "immediate command failure was not reported"

RUN_MODE=repair bash "$generated_runner" >"$test_root/repair.stdout" 2>"$test_root/repair.stderr"

[ ! -s "$test_root/repair.stderr" ] || fail "repair rerun reported a failure"
[ ! -e "$CANARY_FILE" ] || fail "repair rerun typed into existing canary"
[ "$(socket_tmux display-message -p -t '=existing:' '#{pane_pid}')" = "$existing_pane_pid" ] \
  || fail "repair rerun replaced existing canary"
assert_session existing
assert_session fail-create
assert_session partial-create
assert_session fail-send
assert_session fail-dialog
assert_session fail-verify
assert_session race-replacement
assert_session fail-immediate
assert_session success
[ "$(socket_tmux display-message -p -t '=race-replacement:' '#{session_id}')" = "$race_replacement_id" ] \
  || fail "repair rerun replaced the race winner"
[ "$(socket_tmux display-message -p -t '=race-replacement:' '#{pane_pid}')" = "$race_replacement_pid" ] \
  || fail "repair rerun restarted the race winner"
[ "$(<"$RACE_CANARY_FILE")" = replacement-alive ] \
  || fail "repair rerun damaged replacement canary"
if socket_tmux capture-pane -p -t '=race-replacement:' | grep -Fq RACE_INPUT_SENTINEL; then
  fail "repair rerun typed launch keys into the race winner"
fi
wait_for_count 1 fail-create
wait_for_count 1 partial-create
wait_for_count 1 fail-send
wait_for_count 1 fail-dialog
wait_for_count 1 fail-verify
wait_for_count 1 fail-immediate
assert_count 1 success

exit_id_file="$test_root/exit-owned-id"
set +e
RUN_MODE=exit-active \
  INTERRUPT_SESSION=exit-owned \
  INTERRUPT_ID_FILE="$exit_id_file" \
  bash "$generated_runner" >"$test_root/exit.stdout" 2>"$test_root/exit.stderr"
exit_active_status=$?
set -e
[ "$exit_active_status" -eq 1 ] \
  || fail "active-unit EXIT should return 1, got $exit_active_status"
assert_no_session exit-owned
grep -Fq 'exit-owned (unexpected exit)' "$test_root/exit.stderr" \
  || fail "active-unit EXIT was not reported"

interrupt_owned_id_file="$test_root/interrupt-owned-id"
RUN_MODE=interrupt \
  INTERRUPT_SESSION=interrupt-owned \
  INTERRUPT_ID_FILE="$interrupt_owned_id_file" \
  bash "$generated_runner" >"$test_root/interrupt-owned.stdout" 2>"$test_root/interrupt-owned.stderr" &
interrupt_owned_pid=$!
wait_for_file "$interrupt_owned_id_file"
interrupt_owned_id=$(<"$interrupt_owned_id_file")
assert_session interrupt-owned
kill -TERM "$interrupt_owned_pid"
set +e
wait "$interrupt_owned_pid"
interrupt_owned_status=$?
set -e
[ "$interrupt_owned_status" -eq 143 ] \
  || fail "owned-unit TERM should return 143, got $interrupt_owned_status"
assert_no_session interrupt-owned
if socket_tmux has-session -t "$interrupt_owned_id" 2>/dev/null; then
  fail "TERM left the newly-created owned session stranded"
fi
grep -Fq 'interrupt-owned (interrupted by TERM)' "$test_root/interrupt-owned.stderr" \
  || fail "owned-unit TERM was not reported"

interrupt_race_id_file="$test_root/interrupt-race-id"
RUN_MODE=interrupt \
  INTERRUPT_SESSION=interrupt-replacement \
  INTERRUPT_ID_FILE="$interrupt_race_id_file" \
  bash "$generated_runner" >"$test_root/interrupt-race.stdout" 2>"$test_root/interrupt-race.stderr" &
interrupt_race_pid=$!
wait_for_file "$interrupt_race_id_file"
interrupt_race_owned_id=$(<"$interrupt_race_id_file")
socket_tmux kill-session -t "$interrupt_race_owned_id"
interrupt_replacement_id=$(socket_tmux new-session -d -P -F '#{session_id}' -s interrupt-replacement -c "$test_root")
socket_tmux set-option -t "$interrupt_replacement_id" @resume_test_canary replacement-alive
interrupt_replacement_pid=$(socket_tmux display-message -p -t "$interrupt_replacement_id" '#{pane_pid}')
[ "$interrupt_race_owned_id" != "$interrupt_replacement_id" ] \
  || fail "interrupt race did not produce a distinct replacement id"
kill -TERM "$interrupt_race_pid"
set +e
wait "$interrupt_race_pid"
interrupt_race_status=$?
set -e
[ "$interrupt_race_status" -eq 143 ] \
  || fail "replacement-race TERM should return 143, got $interrupt_race_status"
assert_session interrupt-replacement
[ "$(socket_tmux display-message -p -t "$interrupt_replacement_id" '#{session_name}')" = interrupt-replacement ] \
  || fail "TERM rollback replaced the replacement session"
[ "$(socket_tmux display-message -p -t "$interrupt_replacement_id" '#{pane_pid}')" = "$interrupt_replacement_pid" ] \
  || fail "TERM rollback restarted the replacement pane"
[ "$(socket_tmux show-options -v -t "$interrupt_replacement_id" @resume_test_canary)" = replacement-alive ] \
  || fail "TERM rollback damaged the replacement canary"

printf 'ok - transactional resume helper preserves, rolls back, aggregates, repairs, and handles interruption\n'
