#!/usr/bin/env bash
set -euo pipefail

readonly state=${FAKE_TMUX_STATE:?FAKE_TMUX_STATE is required}
mkdir -p "$state/sessions"

resolve() {
  local target=${1%:}
  local session entry
  if [[ $target == =* ]]; then
    session=${target#=}
    [[ -d $state/sessions/$session ]] || return 1
    printf '%s\n' "$state/sessions/$session"
    return
  fi
  for entry in "$state"/sessions/*; do
    [[ -d $entry ]] || continue
    if [[ $(<"$entry/id") == "$target" || $(<"$entry/pane") == "$target" ]]; then
      printf '%s\n' "$entry"
      return
    fi
  done
  return 1
}

target_argument() {
  local previous='' argument
  for argument in "$@"; do
    if [[ $previous == -t ]]; then
      printf '%s\n' "$argument"
      return
    fi
    previous=$argument
  done
  return 1
}

operation=${1:?operation required}
shift
case "$operation" in
  has-session)
    target=$(target_argument "$@")
    resolve "$target" >/dev/null
    ;;
  display-message)
    target=$(target_argument "$@")
    entry=$(resolve "$target")
    format=${*: -1}
    case "$format" in
      '#{session_id}') cat "$entry/id" ;;
      '#{session_name}') basename "$entry" ;;
      '#{pane_current_path}') cat "$entry/path" ;;
      *) exit 2 ;;
    esac
    ;;
  list-panes)
    target=$(target_argument "$@")
    entry=$(resolve "$target")
    cat "$entry/pane"
    ;;
  show-options)
    target=$(target_argument "$@")
    entry=$(resolve "$target")
    option=${*: -1}
    if [[ " $* " == *' -p '* ]]; then
      file="$entry/pane-option-${option#@}"
    else
      file="$entry/session-option-${option#@}"
    fi
    [[ -f $file ]] && cat "$file"
    ;;
  set-option)
    target=$(target_argument "$@")
    entry=$(resolve "$target")
    option=${*: -2:1}
    value=${*: -1}
    if [[ " $* " == *' -p '* ]]; then
      file="$entry/pane-option-${option#@}"
    else
      file="$entry/session-option-${option#@}"
    fi
    printf '%s\n' "$value" >"$file"
    ;;
  new-session)
    name='' directory=''
    previous=
    for argument in "$@"; do
      case "$previous" in
        -s) name=$argument ;;
        -c) directory=$argument ;;
      esac
      previous=$argument
    done
    [[ -n $name && -n $directory && ! -e $state/sessions/$name ]] || exit 1
    [[ ${FAKE_TMUX_FAIL_NAME:-} != "$name" ]] || exit 1
    next=1
    [[ ! -f $state/next ]] || next=$(<"$state/next")
    printf '%s\n' "$((next + 1))" >"$state/next"
    mkdir "$state/sessions/$name"
    printf '$%s\n' "$next" >"$state/sessions/$name/id"
    printf '%%%s\n' "$next" >"$state/sessions/$name/pane"
    printf '%s\n' "$directory" >"$state/sessions/$name/path"
    printf '%s\n' "$name" >>"$state/created"
    printf '$%s\n' "$next"
    ;;
  kill-session)
    target=$(target_argument "$@")
    entry=$(resolve "$target")
    basename "$entry" >>"$state/killed"
    rm -rf -- "$entry"
    ;;
  *)
    printf 'unsupported fake tmux operation: %s\n' "$operation" >&2
    exit 2
    ;;
esac
