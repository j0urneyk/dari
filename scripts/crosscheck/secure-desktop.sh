#!/usr/bin/env bash
# Runs on the macOS /bin/bash (3.2).
set -euo pipefail

all_cases=(uac-allow uac-deny secure-second-display drop-mid-prompt helper-killed secure-policy-off
  secure-view-only lock-unlock)

usage() {
  cat <<'EOF'
Usage: scripts/crosscheck/secure-desktop.sh --b windows:USER@HOST [options]

  --b windows:USER@HOST      The Windows peer (required)
  --b-ip IP                  Address this Mac reaches it at (default: HOST if it is an IPv4 address)
  --identity FILE            SSH private key to log in with
  --known-hosts FILE         SSH known_hosts file to use
  --port N                   UDP port the host listens on (default: 47832)
  --cases LIST               Comma-separated subset of the cases below (default: all of them;
                             secure-second-display only with --second-display)
  --second-display           The peer has a second display (vm/add-second-display.sh): every
                             viewer expects two displays and checks both show each screen, and
                             secure-second-display runs
  --vm-password FILE         The peer user's password, which lock-unlock types (default:
                             ~/.dari-check-vm/password)
  --timeout SECONDS          How long each step may take (default: 120)
  --settle-ms N              How long a secure screen must stay still before its frame is
                             saved (default: dari-check's)
  --out DIR                  Where logs and frames go (default: target/crosscheck/secure-<time>)

Cases (in the order they run):
  uac-allow                  The viewer answers a UAC prompt with Alt+Y; result.txt is elevated
  uac-deny                   The viewer answers with Esc; no result.txt
  secure-second-display      The viewer answers with Alt+Y while the second display is selected
  drop-mid-prompt            The viewer holds Alt and disconnects; the helper logs that it
                             released it, and Esc from a new viewer still cancels the prompt
  helper-killed              The viewer holds F20 and the script ends the helper; the viewer
                             gets the notice, and no key is down once the prompt is gone
  secure-policy-off          SecureDesktopControl is off; the viewer sees the prompt, but its
                             Alt+Y does nothing
  secure-view-only           The host grants view-only sessions; the viewer's Alt+Y does nothing
  lock-unlock                The viewer types the peer user's password on the lock screen
EOF
}

peer='' ip='' identity='' known_hosts='' port=47832 cases='' second_display=0
vm_password=$HOME/.dari-check-vm/password timeout=120 settle_ms='' out=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --b) peer=$2; shift 2 ;;
    --b-ip) ip=$2; shift 2 ;;
    --identity) identity=$2; shift 2 ;;
    --known-hosts) known_hosts=$2; shift 2 ;;
    --port) port=$2; shift 2 ;;
    --cases) cases=$2; shift 2 ;;
    --second-display) second_display=1; shift ;;
    --vm-password) vm_password=$2; shift 2 ;;
    --timeout) timeout=$2; shift 2 ;;
    --settle-ms) settle_ms=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done
case "$peer" in
  windows:*@*) dest=${peer#windows:} ;;
  *) usage >&2; exit 2 ;;
esac
if [[ -z $ip ]]; then
  ip=${dest#*@}
  [[ $ip =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "pass --b-ip: $ip is not an IPv4 address" >&2; exit 2; }
fi
if [[ -n $cases ]]; then
  for name in ${cases//,/ }; do
    [[ " ${all_cases[*]} " == *" $name "* ]] || { echo "unknown case: $name" >&2; exit 2; }
  done
  if [[ ",$cases," == *,secure-second-display,* ]] && ((!second_display)); then
    echo "secure-second-display needs --second-display" >&2
    exit 2
  fi
fi
selected() {
  if [[ -n $cases ]]; then [[ ",$cases," == *",$1,"* ]]; else [[ $1 != secure-second-display ]] || ((second_display)); fi
}
if selected lock-unlock && [[ ! -r $vm_password ]]; then
  echo "lock-unlock types the peer user's password: cannot read $vm_password (pass --vm-password)" >&2
  exit 2
fi

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out=${out:-$root/target/crosscheck/secure-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$out/frames"

ssh_options=(-o BatchMode=yes -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new
  -o ControlMaster=auto -o "ControlPath=/tmp/dari-secure-%C" -o ControlPersist=120)
[[ -n $identity ]] && ssh_options+=(-i "$identity" -o IdentitiesOnly=yes)
[[ -n $known_hosts ]] && ssh_options+=(-o "UserKnownHostsFile=$known_hosts")

dir='C:\dari-check'
peer_host_log="$dir\\logs\\secure-host.log"
# Runs PowerShell on the peer (prepare-peer.ps1 makes it the SSH shell). The command reaches it as
# one -Command argument, so statements are joined onto one line.
on() { ssh "${ssh_options[@]}" "$dest" "${1//$'\n'/ }"; }
put() { scp -q "${ssh_options[@]}" "$1" "$dest:${2//\\//}"; }
fetch() { scp -q "${ssh_options[@]}" "$dest:${1//\\//}" "$2"; }
# Windows client editions refuse scripts by default, so execution is allowed for this process
# only.
script() {
  local name=$1
  shift
  on "Set-ExecutionPolicy -Scope Process Bypass -Force;
    try { & '$dir\\$name' $*; exit \$LASTEXITCODE } catch { Write-Output \$_.Exception.Message; exit 1 }"
}
secure() { script secure-desktop.ps1 "$@"; }
start_secure() {
  script interactive.ps1 start -Name "secure-$1" -RunLevel Limited \
    -Exe "'C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe'" \
    -Arguments "'-NoProfile','-ExecutionPolicy','Bypass','-WindowStyle','Hidden','-File','$dir\\secure-desktop.ps1','$1'" \
    -Log "'$dir\\logs\\secure-$1.log'"
}
stop_task() { script interactive.ps1 stop -Name "$1" >/dev/null 2>&1 || true; }
value_of() { tr -d '\r' | sed -n "s/^$1: //p" | head -n 1; }

host_mode='' host_runs=0 host_log='' passwords_used=0
wait_for_host_line() {
  local key=$1 nth=$2 deadline=$((SECONDS + $3)) value
  while ((SECONDS < deadline)); do
    fetch "$peer_host_log" "$host_log" 2>/dev/null || true
    value=$(sed -n "s/^$key: //p" "$host_log" 2>/dev/null | sed -n "${nth}p" | tr -d '\r')
    if [[ -n $value ]]; then
      printf '%s\n' "$value"
      return 0
    fi
    sleep 1
  done
  return 1
}

ensure_host() {
  local mode=$1 arguments="'host','--port','$port'"
  [[ $host_mode == "$mode" ]] && return 0
  stop_task secure-host
  host_mode=''
  host_runs=$((host_runs + 1))
  host_log=$out/host-$host_runs-$mode.log
  passwords_used=0
  [[ $mode == view-only ]] && arguments+=",'--view-only'"
  script interactive.ps1 start -Name secure-host -RunLevel Limited -Exe "'$dir\\secure-host.cmd'" \
    -Arguments "$arguments" -Log "'$peer_host_log'" || return 1
  wait_for_host_line 'Access password' 1 60 >/dev/null || { echo "the installed app did not start hosting"; return 1; }
  host_mode=$mode
}

next_password() {
  local password
  password=$(wait_for_host_line 'Access password' $((passwords_used + 1)) 60) ||
    { echo "the host did not issue password $((passwords_used + 1))"; return 1; }
  passwords_used=$((passwords_used + 1))
  (umask 077 && printf '%s\n' "$password" >"$out/password")
}

log_mark() {
  fetch "$peer_host_log" "$host_log" 2>/dev/null || true
  wc -l <"$host_log" | tr -d ' '
}
winlogon_then_default() {
  local mark=$1 deadline=$((SECONDS + 30))
  while ((SECONDS < deadline)); do
    fetch "$peer_host_log" "$host_log" 2>/dev/null || true
    awk -v mark="$mark" 'NR <= mark { next }
      /secure-desktop helper: DesktopChanged\(Winlogon\)/ { winlogon = 1 }
      winlogon && /secure-desktop helper: DesktopChanged\(Default\)/ { found = 1 }
      END { exit !found }' "$host_log" && return 0
    sleep 1
  done
  echo "the host log shows no DesktopChanged(Winlogon) followed by DesktopChanged(Default)"
  return 1
}

view_pid='' view_log=''
start_viewer() {
  local name=$1
  shift
  next_password || return 1
  view_log=$out/$name-view.log
  local view_args=(secure-view "$ip:$port" --password-file "$out/password" --out "$out/frames/$name"
    --timeout "$timeout" "$@")
  [[ -n $settle_ms ]] && view_args+=(--settle-ms "$settle_ms")
  ((second_display)) && view_args+=(--select-display --expect-displays 2)
  "$root/target/debug/dari-check" "${view_args[@]}" >"$view_log" 2>&1 &
  view_pid=$!
  wait_for_viewer READY || return 1
  rm -f "$out/password"
}

wait_for_viewer() {
  local line=$1 limit=$((timeout * 4 + 60))
  local deadline=$((SECONDS + limit))
  echo "  waiting for the viewer to print $line"
  while ! grep -qx "$line" "$view_log" 2>/dev/null; do
    if ! kill -0 "$view_pid" 2>/dev/null; then
      grep -qx "$line" "$view_log" 2>/dev/null && return 0
      echo "the viewer exited before printing $line"
      return 1
    fi
    ((SECONDS < deadline)) || { echo "the viewer did not print $line within ${limit}s"; return 1; }
    sleep 0.5
  done
}

finish_viewer() {
  local code=0
  wait "$view_pid" || code=$?
  view_pid=''
  grep -E '^(FAIL|RESULT)' "$view_log" | sed 's/^/  view: /' || true
  ((code == 0)) || { echo "the viewer's checks failed (exit $code)"; return 1; }
}

show_uac() { secure clear-result >/dev/null && start_secure uac; }
uac_result() {
  script interactive.ps1 wait -Name secure-uac -TimeoutSeconds 60 >/dev/null || true
  secure result | value_of result
}
expect_result() {
  local result
  result=$(uac_result)
  [[ $result == "$1" ]] || { echo "result.txt: ${result:-unknown}, expected $1"; return 1; }
}

answer_case() {
  local name=$1 keys=$2 expected=$3 mark
  shift 3
  ensure_host control || return 1
  mark=$(log_mark)
  start_viewer "$name" --screen "$name=$keys" "$@" || return 1
  show_uac || return 1
  wait_for_viewer "SEEN $name" || return 1
  wait_for_viewer "SENT $name" || return 1
  wait_for_viewer "BACK $name" || return 1
  finish_viewer || return 1
  expect_result "$expected" || return 1
  winlogon_then_default "$mark"
}

answer_grace=5

unanswered_case() {
  local name=$1 mode=$2
  shift 2
  ensure_host "$mode" || return 1
  start_viewer "$name" --screen "$name=alt-y" "$@" || return 1
  show_uac || return 1
  wait_for_viewer "SEEN $name" || return 1
  wait_for_viewer "SENT $name" || return 1
  sleep "$answer_grace"
  secure cancel-uac || return 1
  wait_for_viewer "BACK $name" || return 1
  finish_viewer || return 1
  expect_result missing
}

policy_off=0
policy_off_case() {
  local code=0
  echo "  turning SecureDesktopControl off"
  policy_off=1
  secure policy -State off || return 1
  unanswered_case secure-policy-off control || code=$?
  echo "  turning SecureDesktopControl on"
  secure policy -State on && policy_off=0
  return "$code"
}

helper_release() {
  local mark=$1 events=$2 deadline=$((SECONDS + 30)) released
  while ((SECONDS < deadline)); do
    secure events -After "$mark" | tr -d '\r' >"$events" || true
    released=$(sed -n 's/.*helper: released \([0-9][0-9]*\) held inputs because.*/\1/p' "$events" | sort -n | tail -n 1)
    if [[ -n $released ]]; then
      echo "$released"
      return 0
    fi
    sleep 1
  done
  return 1
}

drop_case() {
  local name=drop-mid-prompt mark log released
  ensure_host control || return 1
  log=$(log_mark)
  mark=$(secure event-mark | value_of mark)
  start_viewer "$name" --screen "$name=hold-alt-leave" || return 1
  show_uac || return 1
  wait_for_viewer "SEEN $name" || return 1
  wait_for_viewer "SENT $name" || return 1
  finish_viewer || return 1
  released=$(helper_release "$mark" "$out/$name-events.log") ||
    { echo "DariService's log shows no 'helper: released N held inputs'"; return 1; }
  echo "  the helper released $released held inputs"
  ((released >= 1)) || { echo "the helper released $released held inputs, not at least 1"; return 1; }
  start_viewer "$name-esc" --screen "$name-esc:present=esc" || return 1
  wait_for_viewer "SEEN $name-esc" || return 1
  wait_for_viewer "SENT $name-esc" || return 1
  wait_for_viewer "BACK $name-esc" || return 1
  finish_viewer || return 1
  expect_result missing || return 1
  winlogon_then_default "$log"
}

keys_down() {
  start_secure keys-down >/dev/null || return 1
  script interactive.ps1 wait -Name secure-keys-down -TimeoutSeconds 60 >/dev/null || return 1
  fetch "$dir\\logs\\secure-keys-down.log" "$out/keys-down.log" || return 1
  value_of 'keys down' <"$out/keys-down.log"
}

helper_killed_case() {
  local name=helper-killed finish=$out/helper-killed.finish down
  ensure_host control || return 1
  rm -f "$finish"
  start_viewer "$name" --screen "$name:notice=hold-f20" --finish-when "$finish" || return 1
  show_uac || return 1
  wait_for_viewer "SEEN $name" || return 1
  wait_for_viewer "SENT $name" || return 1
  secure kill-helper || return 1
  wait_for_viewer "NOTICE $name" || return 1
  secure cancel-uac || return 1
  wait_for_viewer "BACK $name" || return 1
  down=$(keys_down) || { echo "could not read the keys down"; return 1; }
  touch "$finish"
  finish_viewer || return 1
  expect_result missing || return 1
  [[ $down == none ]] || { echo "keys down on Default after the prompt: $down"; return 1; }
}

unlock() {
  local name=lock-unlock mark
  ensure_host control || return 1
  mark=$(log_mark)
  start_viewer "$name" --screen "$name=password" --secret-file "$vm_password" || return 1
  start_secure lock || return 1
  wait_for_viewer "SEEN $name" || return 1
  wait_for_viewer "SENT $name" || return 1
  wait_for_viewer "BACK $name" || return 1
  finish_viewer || return 1
  winlogon_then_default "$mark" || return 1
  script interactive.ps1 running -Name secure-host || { echo "the host task stopped"; return 1; }
}

lock_case() {
  local code=0 leaked
  unlock || code=$?
  fetch "$peer_host_log" "$host_log" || true
  leaked=$(grep -lF -f "$vm_password" "$out"/*.log || true)
  [[ -z $leaked ]] || { echo "the password appears in: $leaked"; return 1; }
  return "$code"
}

run_case() {
  case "$1" in
    uac-allow) answer_case uac-allow alt-y elevated ;;
    uac-deny) answer_case uac-deny esc missing ;;
    secure-second-display) answer_case secure-second-display alt-y elevated --answer-on-other-display ;;
    drop-mid-prompt) drop_case ;;
    helper-killed) helper_killed_case ;;
    secure-policy-off) policy_off_case ;;
    secure-view-only) unanswered_case secure-view-only view-only --view-only ;;
    lock-unlock) lock_case ;;
  esac
}

recover() {
  local task
  if [[ -n $view_pid ]]; then
    kill "$view_pid" 2>/dev/null || true
    wait "$view_pid" 2>/dev/null || true
    view_pid=''
  fi
  rm -f "$out/password"
  secure cancel-uac >/dev/null 2>&1 || true
  for task in secure-uac secure-lock secure-keys-down; do stop_task "$task"; done
}

cleanup() {
  recover
  if ((policy_off)); then secure policy -State on >/dev/null 2>&1 || echo "turn SecureDesktopControl on again" >&2; fi
  stop_task secure-host
  ssh "${ssh_options[@]}" -O exit "$dest" 2>/dev/null || true
}
trap cleanup EXIT

echo "Viewing $peer ($ip:$port) from this Mac; output in $out"
(cd "$root" && cargo build -p dari-check --locked)
on "New-Item -ItemType Directory -Force -Path $dir\\logs | Out-Null"
put "$root/scripts/crosscheck/windows/interactive.ps1" "$dir\\interactive.ps1"
put "$root/scripts/crosscheck/windows/secure-desktop.ps1" "$dir\\secure-desktop.ps1"
secure prepare

declare -a summary=()
failures=0
for name in "${all_cases[@]}"; do
  selected "$name" || continue
  echo
  echo "== $name"
  if run_case "$name"; then
    summary+=("PASS $name")
  else
    summary+=("FAIL $name")
    failures=$((failures + 1))
    recover
  fi
done

echo
echo "== Summary ($out)"
printf '%s\n' ${summary[@]+"${summary[@]}"}
((failures == 0))
