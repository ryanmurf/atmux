#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
block=$repo_root/deploy/systemd/resume-tron-scoped-exec-block.bash

if [ "$(grep -Fc -- '--recovery-service-memory-max-bytes 60129542144' "$block")" -ne 1 ]; then
  printf 'Tron recovery must set exactly one 56 GiB atmux-web service cap\n' >&2
  exit 1
fi
if grep -Fq 'scoped-exec --memory-max-bytes' "$block"; then
  printf 'Tron recovery must not apply a worker override to the web service\n' >&2
  exit 1
fi

grep -Fxq '# ATMUX_QUICK_RESUME_SCOPED_EXEC_V1' "$block"
# The pattern is intentionally a literal shell fragment from the unsafe legacy
# helper, not an expression for this test shell to expand.
# shellcheck disable=SC2016
if grep -Fq '"exec $2"' "$block"; then
  printf 'Tron recovery template retains a raw agent launch\n' >&2
  exit 1
fi

# These variables are consumed by the dynamically sourced replacement block.
# shellcheck disable=SC2034
unit_state=created
# shellcheck disable=SC2034
unit_session=fixture
# shellcheck disable=SC2016,SC2034
created_session_id='$42'
session_belongs_to_unit() { return 0; }
fail_unit() { printf 'unexpected failure: %s\n' "$1" >&2; return 1; }
tmux() { printf '%s\0' "$@" >"$capture"; }
capture=$(mktemp)
trap 'rm -f -- "$capture"' EXIT

# shellcheck disable=SC1090
source "$block"
send fixture '/usr/bin/env CODEX_HOME=/home/ryan/.codex /home/ryan/.local/bin/codex resume -C /home/ryan/IdeaProjects/qwen-kernel -m gpt-5.6-sol -c model_reasoning_effort="max" 01a03c8d-0826-7561-8a84-c16d95ac7a49'
mapfile -d '' -t actual <"$capture"
[[ ${actual[0]} == send-keys ]]
[[ ${actual[1]} == -t ]]
# This is a literal tmux pane id.
# shellcheck disable=SC2016
[[ ${actual[2]} == '$42' ]]
[[ ${actual[3]} == 'exec /home/ryan/.local/bin/atmux --config /home/ryan/.config/atmux/config.toml scoped-exec -- /usr/bin/env CODEX_HOME=/home/ryan/.codex /home/ryan/.local/bin/codex resume -C /home/ryan/IdeaProjects/qwen-kernel -m gpt-5.6-sol -c model_reasoning_effort="max" 01a03c8d-0826-7561-8a84-c16d95ac7a49' ]]
[[ ${actual[4]} == Enter ]]

: >"$capture"
# shellcheck disable=SC2034
unit_session=atmux-web
send atmux-web './target/release/atmux --config /home/ryan/.config/atmux/config.toml web'
mapfile -d '' -t actual <"$capture"
[[ ${actual[0]} == send-keys ]]
[[ ${actual[1]} == -t ]]
# This is a literal tmux pane id.
# shellcheck disable=SC2016
[[ ${actual[2]} == '$42' ]]
[[ ${actual[3]} == 'exec /home/ryan/.local/bin/atmux --config /home/ryan/.config/atmux/config.toml scoped-exec --recovery-service-memory-max-bytes 60129542144 -- ./target/release/atmux --config /home/ryan/.config/atmux/config.toml web' ]]
[[ ${actual[4]} == Enter ]]

printf 'Tron scoped recovery template tests passed\n'
