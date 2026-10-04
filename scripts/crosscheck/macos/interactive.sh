#!/usr/bin/env bash
# Runs programs in a Mac's signed-in session for crosscheck.sh and reports on them: the macOS
# counterpart of windows/interactive.ps1.
#
# A program started over SSH doesn't carry the grants (Screen Recording, Accessibility) that the
# signed-in user gave an app, so screen capture and input injection fail. `serve` runs where the
# grants are: in Terminal on a Mac that granted Terminal both, or in a step of a GitHub macOS job,
# whose processes inherit the runner agent's grants. Over SSH, `start` leaves a request in
# ~/dari-check/run and `serve` starts the program as its own child, so the program has serve's
# grants.
#
#   serve [--until FILE] [--timeout SECONDS]   # returns once FILE exists; fails after SECONDS
#   start NAME LOG [--input FILE] -- PROGRAM [ARGUMENT...]
#   wait  NAME SECONDS                         # exits with the program's exit code, 124 on timeout
#   stop  NAME
#
# A PROGRAM ending in .app is launched with `open -n`, so it gets its own grants (the system audio
# recording that needs DariCheck.app, see mac-check-app.sh); it counts as passed when its log has
# a "RESULT pass" line, since `open` doesn't report the app's exit code.
#
# Runs on the macOS /bin/bash (3.2).
set -euo pipefail

run=${DARI_CHECK_DIR:-$HOME/dari-check}/run

valid_name() {
  [[ $1 =~ ^[A-Za-z0-9-]+$ ]] || { echo "bad name: $1" >&2; exit 2; }
}

# Ends the program NAME runs, if any, and forgets it. Its log stays.
stop_program() {
  local name=$1 pid app
  if [[ -f $run/$name.pid ]]; then
    pid=$(cat "$run/$name.pid")
    kill "$pid" 2>/dev/null || true
  fi
  if [[ -f $run/$name.app ]]; then
    # `open` returns once the app runs; the app itself is a separate process.
    app=$(cat "$run/$name.app")
    pkill -f "$app/Contents/MacOS/" 2>/dev/null || true
  fi
  rm -f "$run/$name".{request,running,pid,app,exit}
}

# Runs one request (in the background, from serve) and records its exit code.
run_request() {
  local name=$1 log input program code=0 child
  local -a arguments=()
  {
    IFS= read -r log
    IFS= read -r input
    IFS= read -r program
    while IFS= read -r line; do arguments+=("$line"); done
  } <"$run/$name.running"
  mkdir -p "$(dirname "$log")"
  rm -f "$log"
  if [[ $program == *.app ]]; then
    printf '%s' "$program" >"$run/$name.app"
    open -n -W --stdout "$log" --stderr "$log" ${input:+--stdin "$input"} "$program" \
      --args ${arguments[@]+"${arguments[@]}"} &
  else
    "$program" ${arguments[@]+"${arguments[@]}"} >"$log" 2>&1 <"${input:-/dev/null}" &
  fi
  child=$!
  printf '%s' "$child" >"$run/$name.pid"
  wait "$child" || code=$?
  if [[ $program == *.app ]]; then
    code=1
    grep -q '^RESULT pass' "$log" 2>/dev/null && code=0
  fi
  # A stopped program, or one a newer request with this name replaced, reports nothing.
  [[ $(cat "$run/$name.pid" 2>/dev/null) == "$child" ]] || return 0
  # Renamed into place so `wait` never reads a half-written file.
  printf '%s' "$code" >"$run/$name.exit.tmp"
  mv "$run/$name.exit.tmp" "$run/$name.exit"
}

serve() {
  local until='' timeout=0 started=$SECONDS request name
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --until) until=$2; shift 2 ;;
      --timeout) timeout=$2; shift 2 ;;
      *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
  done
  mkdir -p "$run"
  rm -f "$run"/*.{request,running,pid,app,exit,exit.tmp}
  # The driver checks for this before it starts.
  touch "$run/serving"
  trap 'for pid in $(cat "$run"/*.pid 2>/dev/null); do kill "$pid" 2>/dev/null || true; done; rm -f "$run/serving"' EXIT
  echo "Serving requests in $run"
  while [[ -z $until || ! -e $until ]]; do
    if ((timeout > 0 && SECONDS - started > timeout)); then
      echo "nothing told this Mac to finish within ${timeout}s" >&2
      exit 1
    fi
    for request in "$run"/*.request; do
      [[ -e $request ]] || continue
      name=$(basename "$request" .request)
      mv "$request" "$run/$name.running"
      echo "$(date +%H:%M:%S) start $name"
      run_request "$name" &
    done
    sleep 0.2
  done
}

start() {
  local name=$1 log=$2 input=''
  valid_name "$name"
  shift 2
  if [[ ${1:-} == --input ]]; then input=$2; shift 2; fi
  [[ ${1:-} == -- ]] || { echo "start: expected -- before the program" >&2; exit 2; }
  shift
  [[ $# -gt 0 ]] || { echo "start: no program" >&2; exit 2; }
  [[ -e $run/serving ]] || { echo "interactive.sh serve is not running on this Mac" >&2; exit 1; }
  stop_program "$name"
  {
    printf '%s\n' "$log" "$input"
    printf '%s\n' "$@"
  } >"$run/$name.request.tmp"
  mv "$run/$name.request.tmp" "$run/$name.request"
  local _
  for _ in $(seq 1 50); do
    [[ -e $run/$name.pid || -e $run/$name.exit ]] && return 0
    sleep 0.2
  done
  rm -f "$run/$name.request"
  echo "interactive.sh serve did not pick up $name" >&2
  exit 1
}

wait_program() {
  local name=$1 deadline=$((SECONDS + $2)) code
  valid_name "$name"
  while [[ ! -e $run/$name.exit ]]; do
    if ((SECONDS >= deadline)); then
      stop_program "$name"
      echo "$name still running after ${2}s; stopped"
      exit 124
    fi
    sleep 0.5
  done
  code=$(cat "$run/$name.exit")
  rm -f "$run/$name".{running,pid,app,exit}
  exit "$code"
}

action=${1:-}
shift || true
case "$action" in
  serve) serve "$@" ;;
  start) [[ $# -ge 2 ]] || { echo "usage: start NAME LOG [--input FILE] -- PROGRAM..." >&2; exit 2; }; start "$@" ;;
  wait) [[ $# -eq 2 ]] || { echo "usage: wait NAME SECONDS" >&2; exit 2; }; wait_program "$@" ;;
  stop) [[ $# -eq 1 ]] || { echo "usage: stop NAME" >&2; exit 2; }; valid_name "$1"; stop_program "$1" ;;
  *) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
esac
