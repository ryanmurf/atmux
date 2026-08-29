#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly repo_root
test_root=$(mktemp -d)
readonly test_root
trap 'rm -rf -- "$test_root"' EXIT
export XDG_RUNTIME_DIR="$test_root/runtime"
export ATMUX_MAX_BOOT_ID_FILE="$test_root/boot-id"
export FAKE_TMUX_STATE="$test_root/tmux"
printf '%s\n' test-boot >"$ATMUX_MAX_BOOT_ID_FILE"

# shellcheck disable=SC1091
source "$repo_root/deploy/systemd/resume-max-at-boot"

max_resume_tmux() { "$repo_root/tests/fixtures/max_recovery_tmux.sh" "$@"; }
max_resume_validate_roster() { return 0; }
fake_now=0
max_resume_now() { printf '%s\n' "$fake_now"; }
max_resume_sleep() {
  # Advance the deterministic clock in ten-second quanta so the production
  # 60-second poll contract is covered without a slow process-heavy fixture.
  ((fake_now += $1 * 10))
  if [[ -n ${FAKE_LATE_EXIT_NAME:-} && $fake_now -ge ${FAKE_LATE_EXIT_AT:-0} ]]; then
    rm -rf -- "$FAKE_TMUX_STATE/sessions/$FAKE_LATE_EXIT_NAME"
    unset FAKE_LATE_EXIT_NAME
  fi
}
max_resume_lan_ready() { return 0; }
max_resume_service_active() {
  printf '%s\n' "$1" >>"$test_root/service-checks"
  [[ $1 != qwen-kernel-qwen27-mtp.service ]]
}
max_resume_http_ready() {
  printf '%s\n' "$1" >>"$test_root/http-checks"
  return 0
}
max_resume_wait_for_split_stack() {
  [[ ${FAKE_POST_QWEN_FAILURE:-0} != 1 ]]
}

reset_fake() {
  rm -rf -- "$FAKE_TMUX_STATE" "$XDG_RUNTIME_DIR"
  mkdir -p "$FAKE_TMUX_STATE" "$XDG_RUNTIME_DIR/atmux-max-resume"
  # shellcheck disable=SC2034
  max_resume_created_ids=()
  # shellcheck disable=SC2034
  max_resume_created_names=()
  # shellcheck disable=SC2034
  max_resume_created_indices=()
  # shellcheck disable=SC2034
  max_resume_created_at=()
  fake_now=0
  unset FAKE_TMUX_FAIL_NAME
  unset FAKE_POST_QWEN_FAILURE
  unset FAKE_LATE_EXIT_NAME FAKE_LATE_EXIT_AT
}

created_count() {
  if [[ -f $FAKE_TMUX_STATE/created ]]; then
    wc -l <"$FAKE_TMUX_STATE/created"
  else
    printf '0\n'
  fi
}

seed_valid() {
  local index=$1 id
  # shellcheck disable=SC2154
  id=$(max_resume_tmux new-session -d -P -F '#{session_id}' \
    -s "${max_resume_names[index]}" -c "${max_resume_directories[index]}" ignored)
  max_resume_set_identity "$index" "$id"
}

# A normal run creates the complete exact roster, then a same-boot rerun is a
# verified no-op rather than trusting the marker alone.
reset_fake
max_resume_main --recover >/dev/null
[[ $(created_count) -eq 9 ]]
# shellcheck disable=SC2154
[[ $(<"$max_resume_marker") == test-boot ]]
max_resume_main --recover >/dev/null
[[ $(created_count) -eq 9 ]]

# The current wrapper owns the split router/Halo/XTX backend. Recovery checks
# every declared service and endpoint, and never starts or requires the
# conflicting obsolete monolithic MTP unit.
for expected in \
  claude-qwen-proxy.service \
  qwen-kernel-prefill-router.service \
  qwen-kernel-decode-halo.service \
  qwen-kernel-prefill-xtx.service; do
  grep -Fxq "$expected" "$test_root/service-checks"
done
if grep -Fq qwen-kernel-qwen27-mtp.service "$test_root/service-checks"; then
  printf 'obsolete conflicting backend was checked\n' >&2
  exit 1
fi
for expected in 8091 8092 8191 8192; do
  grep -Fq "127.0.0.1:${expected}/health" "$test_root/http-checks"
done
grep -Fq 'Wants=atmux-web.service claude-qwen-proxy.service qwen-kernel-prefill-router.service qwen-kernel-decode-halo.service qwen-kernel-prefill-xtx.service' \
  "$repo_root/deploy/systemd/atmux-max-resume.service"
if grep -Eq '^Requires=' "$repo_root/deploy/systemd/atmux-max-resume.service"; then
  printf 'readiness-gated service must not have cancellation dependencies\n' >&2
  exit 1
fi
if grep -Fq qwen-kernel-qwen27-mtp.service "$repo_root/deploy/systemd/atmux-max-resume.service"; then
  printf 'resume unit still requires obsolete conflicting backend\n' >&2
  exit 1
fi

# A same-boot marker cannot suppress repair when one verified session is gone.
rm -rf -- "$FAKE_TMUX_STATE/sessions/kernel"
max_resume_main --recover >/dev/null
[[ $(created_count) -eq 10 ]]
max_resume_all_sessions_valid

# Preflight rejects an identity/profile/mode collision before creating anything.
reset_fake
seed_valid 0
# shellcheck disable=SC2154
printf '%s\n' wrong-effort >"$FAKE_TMUX_STATE/sessions/${max_resume_names[0]}/pane-option-atmux_effort"
if max_resume_main --recover >/dev/null 2>&1; then
  printf 'identity collision unexpectedly passed\n' >&2
  exit 1
fi
[[ $(created_count) -eq 1 ]]

# A mid-roster failure preserves a verified pre-existing session, rolls back
# only this attempt's creation, and leaves no marker claiming success.
reset_fake
seed_valid 0
export FAKE_TMUX_FAIL_NAME=clay-hodge-max
if max_resume_main --recover >/dev/null 2>&1; then
  printf 'injected launch failure unexpectedly passed\n' >&2
  exit 1
fi
[[ ! -e $max_resume_marker ]]
[[ -d $FAKE_TMUX_STATE/sessions/clay-p-vs-np-max ]]
[[ ! -d $FAKE_TMUX_STATE/sessions/clay-navier-stokes-max ]]
[[ $(wc -l <"$FAKE_TMUX_STATE/killed") -eq 1 ]]

# A split-stack failure detected immediately after Qwen launch is handled by
# the script, not a systemd dependency cancellation. It rolls back every new
# session from this attempt while preserving a verified pre-existing session.
reset_fake
seed_valid 0
export FAKE_POST_QWEN_FAILURE=1
if max_resume_main --recover >/dev/null 2>&1; then
  printf 'post-Qwen backend failure unexpectedly passed\n' >&2
  exit 1
fi
[[ -d $FAKE_TMUX_STATE/sessions/clay-p-vs-np-max ]]
for session in \
  clay-navier-stokes-max clay-hodge-max clay-bsd-max clay-yang-mills-max \
  riemann-aristotle riemann-fable qwen-cve-59270 kernel; do
  [[ ! -d $FAKE_TMUX_STATE/sessions/$session ]]
done
[[ $(wc -l <"$FAKE_TMUX_STATE/killed") -eq 7 ]]
[[ ! -e $max_resume_marker ]]

# A pane that survives the old two-second check but exits at 42 seconds cannot
# produce a successful marker. Eight exact pre-existing sessions are preserved
# while the one newly created late-exiting Aristotle pane is detected.
reset_fake
for index in 0 1 2 3 4 6 7 8; do
  seed_valid "$index"
done
export FAKE_LATE_EXIT_NAME=riemann-aristotle
export FAKE_LATE_EXIT_AT=42
if max_resume_main --recover >/dev/null 2>&1; then
  printf '42-second late exit unexpectedly passed stability verification\n' >&2
  exit 1
fi
for session in \
  clay-p-vs-np-max clay-navier-stokes-max clay-hodge-max clay-bsd-max clay-yang-mills-max \
  riemann-fable qwen-cve-59270 kernel; do
  [[ -d $FAKE_TMUX_STATE/sessions/$session ]]
done
[[ ! -d $FAKE_TMUX_STATE/sessions/riemann-aristotle ]]
[[ ! -e $max_resume_marker ]]

printf 'Max transactional recovery tests passed\n'
