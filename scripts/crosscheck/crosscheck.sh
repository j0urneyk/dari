#!/usr/bin/env bash
# Cross-device session checks between this Mac and a Windows machine reachable over SSH: a
# local Windows 11 VM, or a GitHub Windows runner on the tailnet (see runner.sh). Each case runs
# `dari-check host` on one side and `dari-check view` on the other; both must pass.
#
# The Windows side needs scripts/crosscheck/windows/prepare-peer.ps1 (setup-vm.ps1 on a VM) and
# a signed-in desktop session. This Mac's pointer moves and its clipboard changes during the
# run; the terminal running this needs Screen Recording and Accessibility.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/crosscheck/crosscheck.sh --windows USER@HOST [options]

  --windows USER@HOST        SSH destination of the Windows peer (required)
  --windows-ip IP            Address this Mac reaches the peer at (default: HOST if it is an IP)
  --mac-ip IP                Address the peer reaches this Mac at (default: from the route)
  --identity FILE            SSH private key to log in with
  --known-hosts FILE         SSH known_hosts file to use
  --build                    Copy this checkout to the peer and build dari-check there
  --target TRIPLE            Windows target for --build (default: x86_64-pc-windows-msvc)
  --windows-bin PATH         dari-check.exe on the peer (default: C:\dari-check\dari-check.exe)
  --expect-windows-displays N  Fail unless the peer offers N displays
  --expect-mac-displays N    Fail unless this Mac offers N displays
  --cases LIST               Comma-separated subset of the cases below (default: all)
  --out DIR                  Where logs and frames go (default: target/crosscheck/<time>)

Cases: mac-host-direct, windows-host-direct, mac-host-relay, windows-host-relay,
       mac-host-view-only, windows-host-view-only
EOF
}

windows='' windows_ip='' mac_ip='' identity='' known_hosts='' build=0
target='x86_64-pc-windows-msvc' windows_bin='C:\dari-check\dari-check.exe'
expect_windows='' expect_mac='' cases='' out=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --windows) windows=$2; shift 2 ;;
    --windows-ip) windows_ip=$2; shift 2 ;;
    --mac-ip) mac_ip=$2; shift 2 ;;
    --identity) identity=$2; shift 2 ;;
    --known-hosts) known_hosts=$2; shift 2 ;;
    --build) build=1; shift ;;
    --target) target=$2; shift 2 ;;
    --windows-bin) windows_bin=$2; shift 2 ;;
    --expect-windows-displays) expect_windows=$2; shift 2 ;;
    --expect-mac-displays) expect_mac=$2; shift 2 ;;
    --cases) cases=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done
[[ -n $windows ]] || { usage >&2; exit 2; }

ipv4='^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$'
if [[ -z $windows_ip ]]; then
  windows_ip=${windows#*@}
  [[ $windows_ip =~ $ipv4 ]] || { echo "pass --windows-ip: $windows_ip is not an IPv4 address" >&2; exit 2; }
fi
if [[ -z $mac_ip ]]; then
  interface=$(route -n get "$windows_ip" | awk '/interface:/ { print $2 }')
  mac_ip=$(ifconfig "$interface" | awk '/inet / { print $2; exit }')
  [[ -n $mac_ip ]] || { echo "cannot tell this Mac's address towards $windows_ip; pass --mac-ip" >&2; exit 2; }
fi

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out=${out:-$root/target/crosscheck/$(date +%Y%m%d-%H%M%S)}
mkdir -p "$out/frames"
bin=$root/target/debug

ssh_options=(-o BatchMode=yes -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new
  -o ControlMaster=auto -o "ControlPath=/tmp/dari-check-%C" -o ControlPersist=120)
[[ -n $identity ]] && ssh_options+=(-i "$identity" -o IdentitiesOnly=yes)
[[ -n $known_hosts ]] && ssh_options+=(-o "UserKnownHostsFile=$known_hosts")

# Runs PowerShell on the peer, whose SSH shell is PowerShell (prepare-peer.ps1). The command
# reaches it as one -Command argument, so statements are joined onto one line.
win() { ssh "${ssh_options[@]}" "$windows" "${1//$'\n'/ }"; }
fetch() { scp -q "${ssh_options[@]}" "$windows:$1" "$2"; }
# interactive.ps1 ACTION -Name NAME [more parameters]; exits with the helper's exit code. Script
# execution is allowed for this process only: Windows client editions refuse scripts by default.
# (`powershell -File` would not do: it passes -Arguments arrays as one string.)
interactive() {
  win "Set-ExecutionPolicy -Scope Process Bypass -Force;
    try { & 'C:\\dari-check\\interactive.ps1' $*; exit \$LASTEXITCODE } catch { Write-Output \$_; exit 1 }"
}
# Quotes words as a PowerShell array literal.
ps_array() {
  local word joined=''
  for word in "$@"; do joined+="${joined:+,}'$word'"; done
  echo "$joined"
}

echo "Windows peer $windows ($windows_ip), this Mac $mac_ip; output in $out"
win "New-Item -ItemType Directory -Force -Path C:\\dari-check\\logs | Out-Null"
scp -q "${ssh_options[@]}" "$root/scripts/crosscheck/windows/interactive.ps1" "$windows:C:/dari-check/interactive.ps1"

echo "Building dari-check and dari-relay for this Mac…"
(cd "$root" && cargo build -p dari-check -p dari-relay --locked)

if [[ $build == 1 ]]; then
  echo "Building dari-check on the peer for $target…"
  (cd "$root" && git ls-files -z --cached --others --exclude-standard |
    while IFS= read -r -d '' file; do [[ -e $file ]] && printf '%s\0' "$file"; done |
    tar --null -T - -cf "$out/src.tar")
  scp -q "${ssh_options[@]}" "$out/src.tar" "$windows:C:/dari-check/src.tar"
  win "\$ErrorActionPreference = 'Stop'; \$env:CARGO_PROFILE_DEV_DEBUG = '0';
    New-Item -ItemType Directory -Force -Path C:\\dari-check\\src | Out-Null;
    tar.exe -xf C:\\dari-check\\src.tar -C C:\\dari-check\\src;
    Set-Location C:\\dari-check\\src;
    rustup target add $target;
    cargo build -p dari-check --locked --target $target;
    if (\$LASTEXITCODE) { exit \$LASTEXITCODE };
    Copy-Item target\\$target\\debug\\dari-check.exe '$windows_bin' -Force"
fi

relay_pid='' mac_host_pid=''
cleanup() {
  [[ -n $mac_host_pid ]] && kill "$mac_host_pid" 2>/dev/null || true
  [[ -n $relay_pid ]] && kill "$relay_pid" 2>/dev/null || true
  interactive stop -Name host >/dev/null 2>&1 || true
  interactive stop -Name view >/dev/null 2>&1 || true
  ssh "${ssh_options[@]}" -O exit "$windows" 2>/dev/null || true
}
trap cleanup EXIT

"$bin/dari-relay" --listen 0.0.0.0:47822 --data-dir "$out/relay-data" >"$out/relay.log" 2>&1 &
relay_pid=$!

# Prints the value of the first "KEY: value" line in a local file, waiting up to $3 seconds.
wait_for_line() {
  local file=$1 key=$2 deadline=$((SECONDS + $3))
  while true; do
    if [[ -f $file ]] && grep -q "^$key: " "$file"; then
      sed -n "s/^$key: //p" "$file" | head -n 1 | tr -d '\r'
      return 0
    fi
    ((SECONDS < deadline)) || return 1
    sleep 1
  done
}

# Like wait_for_line, for a log on the peer.
wait_for_remote_line() {
  local remote=$1 local_copy=$2 key=$3 deadline=$((SECONDS + $4))
  while ((SECONDS < deadline)); do
    fetch "$remote" "$local_copy" 2>/dev/null || true
    if wait_for_line "$local_copy" "$key" 0; then return 0; fi
    sleep 1
  done
  return 1
}

declare -a summary=()
failures=0

# run_case NAME HOST_SIDE(mac|windows) APPROVE(allow|view-only) VIA(direct|relay)
run_case() {
  local name=$1 side=$2 approve=$3 via=$4
  local nonce host_log="$out/$name-host.log" view_log="$out/$name-view.log"
  # Not `tr </dev/urandom | head`: tr dies of SIGPIPE, which pipefail turns into an exit.
  nonce=$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')
  local relay_args=() password relay_id='' address host_code view_code
  [[ $via == relay ]] && relay_args=(--relay "$mac_ip:47822")
  echo
  echo "== $name: $side hosts, $approve, $via"

  if [[ $side == mac ]]; then
    # 47831, so a Dari app running on this Mac (47821) does not get in the way.
    "$bin/dari-check" host --port 47831 --approve "$approve" --nonce "$nonce" ${relay_args[@]+"${relay_args[@]}"} \
      >"$host_log" 2>&1 &
    mac_host_pid=$!
    password=$(wait_for_line "$host_log" password 30) || { echo "the Mac host did not start"; cat "$host_log"; }
    address="$mac_ip:47831"
    if [[ $via == relay && -n ${password:-} ]]; then
      relay_id=$(wait_for_line "$host_log" relay-id 30) || echo "the Mac host did not register with the relay"
      address=$relay_id
    fi
    if [[ -n ${password:-} && ( $via == direct || -n $relay_id ) ]]; then
      win "Set-Content -NoNewline -Path C:\\dari-check\\password.txt -Value '$password'"
      local view_args=(view "$address" --password-file 'C:\dari-check\password.txt' --approve "$approve"
        --nonce "$nonce" --out "C:\\dari-check\\frames\\$name")
      [[ $via == relay ]] && view_args+=(--relay "$mac_ip:47822")
      [[ -n $expect_mac ]] && view_args+=(--expect-displays "$expect_mac")
      view_code=0
      if interactive start -Name view -Exe "'$windows_bin'" -Arguments "$(ps_array "${view_args[@]}")" \
        -Log "'C:\\dari-check\\logs\\$name-view.log'"; then
        interactive wait -Name view -TimeoutSeconds 240 || view_code=$?
      else
        view_code=1
      fi
      fetch "C:/dari-check/logs/$name-view.log" "$view_log" || true
      scp -q -r "${ssh_options[@]}" "$windows:C:/dari-check/frames/$name" "$out/frames/" 2>/dev/null || true
    else
      view_code=1
    fi
    host_code=0
    # The host exits once the session ends; give it a moment, then stop it.
    for _ in $(seq 1 30); do kill -0 "$mac_host_pid" 2>/dev/null || break; sleep 1; done
    kill "$mac_host_pid" 2>/dev/null || true
    wait "$mac_host_pid" || host_code=$?
    mac_host_pid=''
  else
    local host_args=(host --approve "$approve" --nonce "$nonce" ${relay_args[@]+"${relay_args[@]}"})
    interactive start -Name host -Exe "'$windows_bin'" -Arguments "$(ps_array "${host_args[@]}")" \
      -Log "'C:\\dari-check\\logs\\$name-host.log'" || echo "the Windows host task did not start"
    local remote_log="C:/dari-check/logs/$name-host.log"
    password=$(wait_for_remote_line "$remote_log" "$host_log" password 60) || echo "the Windows host did not start"
    address="$windows_ip:47821"
    if [[ $via == relay && -n ${password:-} ]]; then
      relay_id=$(wait_for_remote_line "$remote_log" "$host_log" relay-id 30) ||
        echo "the Windows host did not register with the relay"
      address=$relay_id
    fi
    if [[ -n ${password:-} && ( $via == direct || -n $relay_id ) ]]; then
      printf '%s' "$password" >"$out/$name-password"
      local view_args=(view "$address" --password-file "$out/$name-password" --approve "$approve"
        --nonce "$nonce" --out "$out/frames/$name")
      [[ $via == relay ]] && view_args+=(--relay "$mac_ip:47822")
      [[ -n $expect_windows ]] && view_args+=(--expect-displays "$expect_windows")
      view_code=0
      "$bin/dari-check" "${view_args[@]}" >"$view_log" 2>&1 || view_code=$?
      rm -f "$out/$name-password"
    else
      view_code=1
    fi
    host_code=0
    interactive wait -Name host -TimeoutSeconds 60 || host_code=$?
    fetch "$remote_log" "$host_log" || true
  fi

  for log in "$host_log" "$view_log"; do
    [[ -f $log ]] && { grep -E '^(FAIL|RESULT)' "$log" | sed "s|^|  $(basename "$log" .log): |" || true; }
  done
  if [[ $host_code == 0 && $view_code == 0 ]]; then
    summary+=("PASS $name")
  else
    summary+=("FAIL $name (host exit $host_code, viewer exit $view_code)")
    failures=$((failures + 1))
  fi
}

all_cases=(
  "mac-host-direct mac allow direct"
  "windows-host-direct windows allow direct"
  "mac-host-relay mac allow relay"
  "windows-host-relay windows allow relay"
  "mac-host-view-only mac view-only direct"
  "windows-host-view-only windows view-only direct"
)
for entry in "${all_cases[@]}"; do
  read -r name side approve via <<<"$entry"
  if [[ -z $cases || ",$cases," == *",$name,"* ]]; then
    run_case "$name" "$side" "$approve" "$via"
  fi
done

echo
echo "== Summary ($out)"
printf '%s\n' ${summary[@]+"${summary[@]}"}
((failures == 0))
