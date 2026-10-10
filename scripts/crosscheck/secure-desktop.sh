#!/usr/bin/env bash
# The viewer can't answer a prompt before #47, so this script dismisses each UAC prompt over SSH
# by ending consent.exe.
#
# Runs on the macOS /bin/bash (3.2).
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/crosscheck/secure-desktop.sh --b windows:USER@HOST [options]

  --b windows:USER@HOST      The Windows peer (required)
  --b-ip IP                  Address this Mac reaches it at (default: HOST if it is an IPv4 address)
  --identity FILE            SSH private key to log in with
  --known-hosts FILE         SSH known_hosts file to use
  --port N                   UDP port the host listens on (default: 47832)
  --no-uac                   Skip the UAC prompt case
  --lock                     Also lock the peer; a person unlocks it when asked
  --helper-killed            Also end the secure-desktop helper during a UAC prompt; the viewer
                             must get the secure-desktop notice and the session must go on.
                             Runs last: the helper doesn't come back in that session
  --second-display           While each screen is up, also select the peer's second display
                             (vm/add-second-display.sh) and check it shows the secure desktop
  --timeout SECONDS          How long each step may take (default: 120; 600 with --lock)
  --settle-ms N              How long a secure screen must stay still before its frame is
                             saved (default: dari-check's)
  --out DIR                  Where logs and frames go (default: target/crosscheck/secure-<time>)
EOF
}

peer='' ip='' identity='' known_hosts='' port=47832 uac=1 lock=0 helper_killed=0 second_display=0
timeout='' settle_ms='' out=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --b) peer=$2; shift 2 ;;
    --b-ip) ip=$2; shift 2 ;;
    --identity) identity=$2; shift 2 ;;
    --known-hosts) known_hosts=$2; shift 2 ;;
    --port) port=$2; shift 2 ;;
    --no-uac) uac=0; shift ;;
    --lock) lock=1; shift ;;
    --helper-killed) helper_killed=1; shift ;;
    --second-display) second_display=1; shift ;;
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
if [[ -z $timeout ]]; then
  if ((lock)); then timeout=600; else timeout=120; fi
fi
((uac || lock || helper_killed)) || { echo "nothing to check: --no-uac without --lock or --helper-killed" >&2; exit 2; }

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out=${out:-$root/target/crosscheck/secure-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$out/frames"
view_log=$out/view.log
host_log=$out/host.log

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

wait_for_host_line() {
  local key=$1 deadline=$((SECONDS + $2))
  while ((SECONDS < deadline)); do
    fetch "$peer_host_log" "$host_log" 2>/dev/null || true
    if [[ -f $host_log ]] && grep -q "^$key: " "$host_log"; then
      sed -n "s/^$key: //p" "$host_log" | head -n 1 | tr -d '\r'
      return 0
    fi
    sleep 1
  done
  return 1
}

view_pid=''
wait_for_viewer() {
  local line=$1 limit=$((timeout * 4 + 60))
  local deadline=$((SECONDS + limit))
  echo "waiting for the viewer to print $line"
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

cleanup() {
  if [[ -n $view_pid ]]; then kill "$view_pid" 2>/dev/null || true; fi
  rm -f "$out/password"
  secure cancel-uac >/dev/null 2>&1 || true
  for name in secure-uac secure-lock secure-host; do stop_task "$name"; done
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
pass() { summary+=("PASS $1"); }
fail() { summary+=("FAIL $1"); failures=$((failures + 1)); }

script interactive.ps1 start -Name secure-host -RunLevel Limited -Exe "'$dir\\secure-host.cmd'" \
  -Arguments "'host','--port','$port'" -Log "'$peer_host_log'"
password=$(wait_for_host_line 'Access password' 60) || { echo "the installed app did not start hosting" >&2; exit 1; }
(umask 077 && printf '%s\n' "$password" >"$out/password")

screens=()
((uac)) && screens+=(--screen uac)
((lock)) && screens+=(--screen lock)
((helper_killed)) && screens+=(--screen helper-killed:notice)
view_args=(secure-view "$ip:$port" --password-file "$out/password" --out "$out/frames" --timeout "$timeout"
  "${screens[@]}")
[[ -n $settle_ms ]] && view_args+=(--settle-ms "$settle_ms")
((second_display)) && view_args+=(--select-display --expect-displays 2)
"$root/target/debug/dari-check" "${view_args[@]}" >"$view_log" 2>&1 &
view_pid=$!

prompt_case() {
  local label=$1 meanwhile=${2:-}
  start_secure uac || return 1
  wait_for_viewer "SEEN $label" || return 1
  if [[ $meanwhile == kill-helper ]]; then
    secure kill-helper || return 1
    wait_for_viewer "NOTICE $label" || return 1
  fi
  secure cancel-uac
  wait_for_viewer "BACK $label" || return 1
  stop_task secure-uac
}

lock_case() {
  start_secure lock || return 1
  wait_for_viewer 'SEEN lock' || return 1
  echo
  echo ">>> Unlock $peer in its window now (the UTM window for the VM). The viewer waits up to ${timeout}s."
  echo
  wait_for_viewer 'BACK lock' || return 1
  stop_task secure-lock
}

if wait_for_viewer READY; then
  rm -f "$out/password"
  ran=1
  if ((uac)); then
    echo "== uac"
    if prompt_case uac; then pass uac; else fail uac; ran=0; fi
  fi
  if ((ran && lock)); then
    echo "== lock"
    if lock_case; then pass lock; else fail lock; ran=0; fi
  fi
  if ((ran && helper_killed)); then
    echo "== helper-killed"
    if prompt_case helper-killed kill-helper; then pass helper-killed; else fail helper-killed; ran=0; fi
  fi
else
  fail 'the viewer connects and saves a baseline'
  ran=0
fi
((ran)) || kill "$view_pid" 2>/dev/null || true

view_code=0
wait "$view_pid" || view_code=$?
view_pid=''
if ((view_code == 0)); then pass "the viewer's checks"; else fail "the viewer's checks (exit $view_code)"; fi
grep -E '^(FAIL|RESULT)' "$view_log" | sed 's/^/  view: /' || true

stop_task secure-host
fetch "$peer_host_log" "$host_log" || true
desktops=(Winlogon)
((uac || lock)) && desktops+=(Default)
for desktop in "${desktops[@]}"; do
  count=$(grep -c "secure-desktop helper: DesktopChanged($desktop)" "$host_log" 2>/dev/null || true)
  if ((${count:-0} > 0)); then
    pass "the host log shows the helper's DesktopChanged($desktop) (${count}x)"
  else
    fail "the host log shows the helper's DesktopChanged($desktop)"
  fi
done

echo
echo "== Summary ($out)"
printf '%s\n' ${summary[@]+"${summary[@]}"}
((failures == 0))
