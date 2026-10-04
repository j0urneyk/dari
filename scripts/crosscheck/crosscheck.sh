#!/usr/bin/env bash
# Cross-device session checks between two peers, A and B, in both directions: any pairing of
# macOS and Windows. A peer is this Mac (`local`), or a Mac or Windows machine reachable over SSH:
# a local Windows 11 VM, a second Mac, or GitHub runners on the tailnet (ci-driver.sh). Each case
# runs `dari-check host` on one peer and `dari-check view` on the other; both must pass. The relay
# for the relay cases runs on the machine running this script, which may be macOS or Linux.
#
# A Windows peer needs scripts/crosscheck/windows/prepare-peer.ps1 (setup-vm.ps1 on a VM) and a
# signed-in desktop session. A Mac peer over SSH needs scripts/crosscheck/macos/prepare-peer.sh
# and `scripts/crosscheck/macos/interactive.sh serve` running in its signed-in session. A Mac
# peer's pointer moves and its clipboard changes during the run; on a local Mac, the terminal
# running this needs Screen Recording and Accessibility.
#
# Runs on the macOS /bin/bash (3.2).
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/crosscheck/crosscheck.sh --a PEER --b PEER [options]

PEER is `local` (this Mac), `macos:USER@HOST`, or `windows:USER@HOST`.

  --a PEER, --b PEER         The two peers (required)
  --a-ip IP, --b-ip IP       Address the other peer reaches it at (default: HOST if it is an
                             IPv4 address; for `local`, this machine's address towards the other)
  --a-displays N, --b-displays N
                             Fail unless that peer offers N displays
  --relay-ip IP              Address the peers reach this machine's relay at (default: this
                             machine's address towards peer A)
  --identity FILE            SSH private key to log in with
  --known-hosts FILE         SSH known_hosts file to use
  --build                    Copy this checkout to every SSH peer and build dari-check there
  --windows-target TRIPLE    Target for --build on Windows (default: x86_64-pc-windows-msvc)
  --cases LIST               Comma-separated subset of the cases below (default: all)
  --no-audio                 Skip the audio cases (for peers without a sound output)
  --out DIR                  Where logs and frames go (default: target/crosscheck/<time>)
  --release TAG              Also install that release on both peers and connect the installed
                             apps to each other

Cases: a-host-direct, b-host-direct, a-host-relay, b-host-relay, a-host-view-only,
       b-host-view-only, a-host-audio, b-host-audio; with --release also a-host-installed,
       b-host-installed
EOF
}

a_peer='' b_peer='' a_ip='' b_ip='' a_displays='' b_displays='' relay_ip='' identity='' known_hosts=''
build=0 windows_target='x86_64-pc-windows-msvc' cases='' audio=1 out='' release=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --a) a_peer=$2; shift 2 ;;
    --b) b_peer=$2; shift 2 ;;
    --a-ip) a_ip=$2; shift 2 ;;
    --b-ip) b_ip=$2; shift 2 ;;
    --a-displays) a_displays=$2; shift 2 ;;
    --b-displays) b_displays=$2; shift 2 ;;
    --relay-ip) relay_ip=$2; shift 2 ;;
    --identity) identity=$2; shift 2 ;;
    --known-hosts) known_hosts=$2; shift 2 ;;
    --build) build=1; shift ;;
    --windows-target) windows_target=$2; shift 2 ;;
    --cases) cases=$2; shift 2 ;;
    --no-audio) audio=0; shift ;;
    --out) out=$2; shift 2 ;;
    --release) release=$2; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done
[[ -n $a_peer && -n $b_peer ]] || { usage >&2; exit 2; }

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out=${out:-$root/target/crosscheck/$(date +%Y%m%d-%H%M%S)}
mkdir -p "$out/frames"

# Each peer's settings live in variables named after it (a_os, b_dir, ...), read with `field`.
for side in a b; do
  for name in os via dest dir bin port hangul app check_app; do eval "${side}_$name=''"; done
done
field() { local name="$1_$2"; printf '%s' "${!name}"; }
set_field() { eval "$1_$2=\$3"; }
other() { if [[ $1 == a ]]; then echo b; else echo a; fi; }

ipv4='^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$'
locals=0
for side in a b; do
  spec=$(field "$side" peer)
  case "$spec" in
    local)
      [[ $(uname) == Darwin ]] || { echo "--$side local: only a Mac can be a local peer" >&2; exit 2; }
      set_field "$side" os macos
      set_field "$side" via local
      set_field "$side" dest ''
      locals=$((locals + 1))
      ;;
    macos:*@* | windows:*@*)
      set_field "$side" os "${spec%%:*}"
      set_field "$side" via ssh
      set_field "$side" dest "${spec#*:}"
      if [[ -z $(field "$side" ip) ]]; then
        host=${spec#*@}
        [[ $host =~ $ipv4 ]] || { echo "pass --$side-ip: $host is not an IPv4 address" >&2; exit 2; }
        set_field "$side" ip "$host"
      fi
      ;;
    *) echo "--$side: expected local, macos:USER@HOST, or windows:USER@HOST, not $spec" >&2; exit 2 ;;
  esac
done
((locals < 2)) || { echo "at most one peer can be local" >&2; exit 2; }

# This machine's address towards $1.
address_towards() {
  local interface
  if [[ $(uname) == Darwin ]]; then
    interface=$(route -n get "$1" | awk '/interface:/ { print $2 }')
    ifconfig "$interface" | awk '/inet / { print $2; exit }'
  else
    ip -4 route get "$1" | awk '{ for (i = 1; i < NF; i++) if ($i == "src") { print $(i + 1); exit } }'
  fi
}
for side in a b; do
  if [[ $(field "$side" via) == local && -z $(field "$side" ip) ]]; then
    ip=$(address_towards "$(field "$(other "$side")" ip)")
    [[ -n $ip ]] || { echo "cannot tell this Mac's address; pass --$side-ip" >&2; exit 2; }
    set_field "$side" ip "$ip"
  fi
done
if [[ -z $relay_ip ]]; then
  if [[ $a_via == local ]]; then relay_ip=$a_ip; elif [[ $b_via == local ]]; then relay_ip=$b_ip; else
    relay_ip=$(address_towards "$a_ip")
  fi
  [[ -n $relay_ip ]] || { echo "cannot tell this machine's address; pass --relay-ip" >&2; exit 2; }
fi

ssh_options=(-o BatchMode=yes -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new
  -o ControlMaster=auto -o "ControlPath=/tmp/dari-check-%C" -o ControlPersist=120)
[[ -n $identity ]] && ssh_options+=(-i "$identity" -o IdentitiesOnly=yes)
[[ -n $known_hosts ]] && ssh_options+=(-o "UserKnownHostsFile=$known_hosts")

# Runs a command on a peer: PowerShell on Windows (prepare-peer.ps1 makes it the SSH shell; the
# command reaches it as one -Command argument, so statements are joined onto one line), the login
# shell on a Mac over SSH, and bash here.
on() {
  local side=$1 command=$2
  case "$(field "$side" os)/$(field "$side" via)" in
    windows/ssh) ssh "${ssh_options[@]}" "$(field "$side" dest)" "${command//$'\n'/ }" ;;
    macos/ssh) ssh "${ssh_options[@]}" "$(field "$side" dest)" "$command" ;;
    macos/local) bash -c "$command" ;;
  esac
}
# A path on a peer for scp: Windows paths with forward slashes, and DEST: for SSH peers.
scp_path() {
  local side=$1 path=$2
  [[ $(field "$side" os) == windows ]] && path=${path//\\//}
  if [[ $(field "$side" via) == ssh ]]; then printf '%s:%s' "$(field "$side" dest)" "$path"; else printf '%s' "$path"; fi
}
put() {
  if [[ $(field "$1" via) == local ]]; then cp "$2" "$3"; else scp -q "${ssh_options[@]}" "$2" "$(scp_path "$1" "$3")"; fi
}
fetch() {
  if [[ $(field "$1" via) == local ]]; then cp "$2" "$3"; else scp -q "${ssh_options[@]}" "$(scp_path "$1" "$2")" "$3"; fi
}
fetch_dir() {
  if [[ $(field "$1" via) == local ]]; then cp -R "$2" "$3"; else scp -q -r "${ssh_options[@]}" "$(scp_path "$1" "$2")" "$3"; fi
}
# A file under the peer's work directory, in the peer's own path syntax.
path() {
  local side=$1 relative=$2
  if [[ $(field "$side" os) == windows ]]; then
    printf '%s\\%s' "$(field "$side" dir)" "${relative//\//\\}"
  else
    printf '%s/%s' "$(field "$side" dir)" "$relative"
  fi
}
# Quotes words for the peer's shell: a PowerShell array literal on Windows, words for sh on a Mac.
quote() {
  local side=$1 word joined=''
  shift
  if [[ $(field "$side" os) == windows ]]; then
    for word in "$@"; do joined+="${joined:+,}'$word'"; done
  else
    for word in "$@"; do joined+="${joined:+ }$(printf '%q' "$word")"; done
  fi
  printf '%s' "$joined"
}
# write_file SIDE FILE TEXT [line]: with `line`, TEXT ends with a newline.
write_file() {
  local side=$1 file=$2 content=$3 line=${4:-}
  if [[ $(field "$side" os) == windows ]]; then
    local newline='-NoNewline'
    [[ -n $line ]] && newline=''
    on "$side" "Set-Content $newline -Path '$file' -Value '$content'"
  else
    on "$side" "printf '%s${line:+\\n}' $(quote "$side" "$content") >$(quote "$side" "$file")"
  fi
}
remove_file() {
  if [[ $(field "$1" os) == windows ]]; then
    on "$1" "Remove-Item -Force -ErrorAction SilentlyContinue '$2'" || true
  else
    on "$1" "rm -f $(quote "$1" "$2")" || true
  fi
}

# Starts a program in the peer's signed-in session, where it can capture the screen and inject
# input, through windows/interactive.ps1 or macos/interactive.sh.
#   start SIDE NAME LOG INPUT PROGRAM [ARGUMENT...]   (INPUT may be empty)
start() {
  local side=$1 name=$2 log=$3 input=$4 program=$5
  shift 5
  if [[ $(field "$side" os) == windows ]]; then
    interactive "$side" start -Name "$name" -Exe "'$program'" -Arguments "$(quote "$side" "$@")" \
      -Log "'$log'" ${input:+-InputFile "'$input'"}
  else
    interactive "$side" start "$name" "$(quote "$side" "$log")" ${input:+--input "$(quote "$side" "$input")"} \
      -- "$(quote "$side" "$program" "$@")"
  fi
}
# Waits up to $3 seconds for a started program; returns its exit code (124 when it timed out).
wait_for() {
  if [[ $(field "$1" os) == windows ]]; then
    interactive "$1" wait -Name "$2" -TimeoutSeconds "$3"
  else
    interactive "$1" wait "$2" "$3"
  fi
}
stop() {
  if [[ $(field "$1" os) == windows ]]; then
    interactive "$1" stop -Name "$2" >/dev/null 2>&1 || true
  else
    interactive "$1" stop "$2" >/dev/null 2>&1 || true
  fi
}
# Runs the peer's interactive helper with ACTION and the rest, exiting with its exit code. On
# Windows, script execution is allowed for this process only: Windows client editions refuse
# scripts by default. (`powershell -File` would not do: it passes -Arguments arrays as one string.)
interactive() {
  local side=$1
  shift
  if [[ $(field "$side" os) == windows ]]; then
    on "$side" "Set-ExecutionPolicy -Scope Process Bypass -Force;
      try { & '$(path "$side" interactive.ps1)' $*; exit \$LASTEXITCODE } catch { Write-Output \$_; exit 1 }"
  else
    on "$side" "DARI_CHECK_DIR=$(quote "$side" "$(field "$side" dir)") /bin/bash $(quote "$side" "$(path "$side" interactive.sh)") $*"
  fi
}

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
# Like wait_for_line, for a log on a peer, copied to $3.
wait_for_peer_line() {
  local side=$1 log=$2 local_copy=$3 key=$4 deadline=$((SECONDS + $5))
  while ((SECONDS < deadline)); do
    fetch "$side" "$log" "$local_copy" 2>/dev/null || true
    if wait_for_line "$local_copy" "$key" 0; then return 0; fi
    sleep 1
  done
  return 1
}

selected() { [[ -z $cases || ",$cases," == *",$1,"* ]]; }
wants_audio=0
if ((audio)) && { selected a-host-audio || selected b-host-audio; }; then wants_audio=1; fi

relay_pid='' local_serve_pid=''
cleanup() {
  local side name
  for side in a b; do
    [[ -n $(field "$side" dir) ]] || continue
    for name in host view; do stop "$side" "$name"; done
    if [[ $(field "$side" via) == ssh ]]; then
      ssh "${ssh_options[@]}" -O exit "$(field "$side" dest)" 2>/dev/null || true
    fi
  done
  if [[ -n $local_serve_pid ]]; then kill "$local_serve_pid" 2>/dev/null || true; fi
  if [[ -n $relay_pid ]]; then kill "$relay_pid" 2>/dev/null || true; fi
}
trap cleanup EXIT

echo "A: $a_peer ($a_ip), B: $b_peer ($b_ip), relay on this machine at $relay_ip; output in $out"
echo "Building on this machine..."
if ((locals)); then
  (cd "$root" && cargo build -p dari-check -p dari-relay --locked)
else
  (cd "$root" && cargo build -p dari-relay --locked)
fi
bin=$root/target/debug

if [[ $build == 1 ]]; then
  (cd "$root" && git ls-files -z --cached --others --exclude-standard |
    while IFS= read -r -d '' file; do [[ -e $file ]] && printf '%s\0' "$file"; done |
    tar --null -T - -cf "$out/src.tar")
fi

# Work directories, programs, and helpers on each peer.
for side in a b; do
  case "$(field "$side" os)/$(field "$side" via)" in
    windows/ssh)
      set_field "$side" dir 'C:\dari-check'
      set_field "$side" bin 'C:\dari-check\dari-check.exe'
      set_field "$side" port 47821
      on "$side" "New-Item -ItemType Directory -Force -Path C:\\dari-check\\logs | Out-Null"
      put "$side" "$root/scripts/crosscheck/windows/interactive.ps1" "$(path "$side" interactive.ps1)"
      # With a Korean input method on the peer, its host also checks the viewer's keys compose Hangul.
      set_field "$side" hangul ''
      if on "$side" "if ((Get-WinUserLanguageList).LanguageTag -contains 'ko') { 'korean' }" | grep -q korean; then
        set_field "$side" hangul --expect-hangul
        echo "$side has a Korean input method; its host checks Hangul input too."
      fi
      if [[ $build == 1 ]]; then
        echo "Building dari-check on $side for $windows_target..."
        put "$side" "$out/src.tar" 'C:\dari-check\src.tar'
        on "$side" "\$ErrorActionPreference = 'Stop'; \$env:CARGO_PROFILE_DEV_DEBUG = '0';
          New-Item -ItemType Directory -Force -Path C:\\dari-check\\src | Out-Null;
          tar.exe -xf C:\\dari-check\\src.tar -C C:\\dari-check\\src;
          Set-Location C:\\dari-check\\src;
          rustup target add $windows_target;
          cargo build -p dari-check --locked --target $windows_target;
          if (\$LASTEXITCODE) { exit \$LASTEXITCODE };
          Copy-Item target\\$windows_target\\debug\\dari-check.exe C:\\dari-check\\dari-check.exe -Force"
      fi
      ;;
    macos/ssh)
      set_field "$side" dir "$(on "$side" 'printf %s "$HOME"')/dari-check"
      set_field "$side" bin "$(path "$side" dari-check)"
      # Not 47821, so a Dari app running on the Mac does not get in the way.
      set_field "$side" port 47831
      set_field "$side" hangul ''
      on "$side" "mkdir -p $(quote "$side" "$(path "$side" logs)")"
      put "$side" "$root/scripts/crosscheck/macos/interactive.sh" "$(path "$side" interactive.sh)"
      on "$side" "test -e $(quote "$side" "$(path "$side" run/serving)")" ||
        { echo "$side: run scripts/crosscheck/macos/interactive.sh serve in its signed-in session first" >&2; exit 1; }
      set_field "$side" check_app "$(path "$side" DariCheck.app)"
      if [[ $build == 1 ]]; then
        echo "Building dari-check on $side..."
        put "$side" "$out/src.tar" "$(path "$side" src.tar)"
        on "$side" "set -e; [ -f ~/.cargo/env ] && . ~/.cargo/env; export CARGO_PROFILE_DEV_DEBUG=0;
          mkdir -p $(quote "$side" "$(path "$side" src)"); cd $(quote "$side" "$(path "$side" src)");
          tar -xf ../src.tar; cargo build -p dari-check --locked; cp target/debug/dari-check ../dari-check"
      fi
      ;;
    macos/local)
      set_field "$side" dir "$out/peer-$side"
      set_field "$side" bin "$bin/dari-check"
      # Where mac-check-app.sh puts it by default. macOS remembers the system audio permission
      # for the bundle, so it stays out of the run's own directory.
      set_field "$side" check_app "$root/target/crosscheck/DariCheck.app"
      set_field "$side" port 47831
      set_field "$side" hangul ''
      mkdir -p "$out/peer-$side/logs"
      cp "$root/scripts/crosscheck/macos/interactive.sh" "$out/peer-$side/interactive.sh"
      # Programs started through the helper here have this terminal's grants, as on a Mac peer.
      DARI_CHECK_DIR="$out/peer-$side" bash "$out/peer-$side/interactive.sh" serve >"$out/peer-$side/serve.log" 2>&1 &
      local_serve_pid=$!
      for _ in $(seq 1 25); do [[ -e $out/peer-$side/run/serving ]] && break; sleep 0.2; done
      ;;
  esac
  if ((wants_audio)) && [[ $(field "$side" os) == macos ]]; then
    # macOS only records system sound for an app bundle that declares why (mac-check-app.sh).
    if [[ $(field "$side" via) == local ]]; then
      "$root/scripts/crosscheck/mac-check-app.sh" "$bin/dari-check" "$(dirname "$(field "$side" check_app)")" >/dev/null
    else
      put "$side" "$root/scripts/crosscheck/mac-check-app.sh" "$(path "$side" mac-check-app.sh)"
      on "$side" "/bin/bash $(quote "$side" "$(path "$side" mac-check-app.sh)" "$(field "$side" bin)" "$(field "$side" dir)") >/dev/null"
    fi
  fi
done

"$bin/dari-relay" --listen 0.0.0.0:47822 --data-dir "$out/relay-data" >"$out/relay.log" 2>&1 &
relay_pid=$!

declare -a summary=()
failures=0

record() {
  local name=$1 host_code=$2 view_code=$3 host_log=$4 view_log=$5 log
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

# run_case NAME HOST(a|b) APPROVE(allow|view-only) VIA(direct|relay)
run_case() {
  local name=$1 host=$2 approve=$3 via=$4
  local viewer nonce host_log="$out/$name-host.log" view_log="$out/$name-view.log"
  local peer_host_log peer_view_log password_file relay_args=() password relay_id='' address
  local host_code=0 view_code=0
  viewer=$(other "$host")
  peer_host_log=$(path "$host" "logs/$name-host.log")
  peer_view_log=$(path "$viewer" "logs/$name-view.log")
  password_file=$(path "$viewer" "$name-password")
  # Not `tr </dev/urandom | head`: tr dies of SIGPIPE, which pipefail turns into an exit.
  nonce=$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')
  [[ $via == relay ]] && relay_args=(--relay "$relay_ip:47822")
  echo
  echo "== $name: $host ($(field "$host" os)) hosts, $(field "$viewer" os) views, $approve, $via"

  start "$host" host "$peer_host_log" '' "$(field "$host" bin)" host --port "$(field "$host" port)" \
    --approve "$approve" --nonce "$nonce" ${relay_args[@]+"${relay_args[@]}"} $(field "$host" hangul) ||
    echo "the host did not start"
  password=$(wait_for_peer_line "$host" "$peer_host_log" "$host_log" password 60) || echo "the host did not start"
  address="$(field "$host" ip):$(field "$host" port)"
  if [[ $via == relay && -n ${password:-} ]]; then
    relay_id=$(wait_for_peer_line "$host" "$peer_host_log" "$host_log" relay-id 30) ||
      echo "the host did not register with the relay"
    address=$relay_id
  fi
  if [[ -n ${password:-} && ($via == direct || -n $relay_id) ]]; then
    write_file "$viewer" "$password_file" "$password"
    local view_args=(view "$address" --password-file "$password_file" --approve "$approve" --nonce "$nonce"
      --out "$(path "$viewer" "frames/$name")")
    [[ $via == relay ]] && view_args+=(--relay "$relay_ip:47822")
    [[ -n $(field "$host" displays) ]] && view_args+=(--expect-displays "$(field "$host" displays)")
    if start "$viewer" view "$peer_view_log" '' "$(field "$viewer" bin)" "${view_args[@]}"; then
      wait_for "$viewer" view 240 || view_code=$?
    else
      view_code=1
    fi
    remove_file "$viewer" "$password_file"
    fetch "$viewer" "$peer_view_log" "$view_log" || true
    fetch_dir "$viewer" "$(path "$viewer" "frames/$name")" "$out/frames/" 2>/dev/null || true
  else
    view_code=1
  fi
  # The host exits once the session ends.
  wait_for "$host" host 60 || host_code=$?
  fetch "$host" "$peer_host_log" "$host_log" || true
  record "$name" "$host_code" "$view_code" "$host_log" "$view_log"
}

# The host plays a tone and shares its sound; the viewer records what arrives.
run_audio_case() {
  local name=$1 host=$2 viewer program password host_code=0 view_code=0
  local host_log="$out/$name-host.log" view_log="$out/$name-view.log"
  local peer_host_log peer_view_log password_file
  viewer=$(other "$host")
  echo
  echo "== $name: $host ($(field "$host" os)) plays a tone, $(field "$viewer" os) listens"
  if ((!audio)); then
    summary+=("SKIP $name (--no-audio)")
    return
  fi
  peer_host_log=$(path "$host" "logs/$name-host.log")
  peer_view_log=$(path "$viewer" "logs/$name-view.log")
  password_file=$(path "$viewer" "$name-password")
  program=$(field "$host" bin)
  [[ $(field "$host" os) == macos ]] && program=$(field "$host" check_app)
  start "$host" host "$peer_host_log" '' "$program" audio-host --port "$(field "$host" port)" ||
    echo "the audio host did not start"
  if password=$(wait_for_peer_line "$host" "$peer_host_log" "$host_log" password 60); then
    write_file "$viewer" "$password_file" "$password"
    if start "$viewer" view "$peer_view_log" '' "$(field "$viewer" bin)" audio-view \
      "$(field "$host" ip):$(field "$host" port)" --password-file "$password_file"; then
      wait_for "$viewer" view 120 || view_code=$?
    else
      view_code=1
    fi
    remove_file "$viewer" "$password_file"
    fetch "$viewer" "$peer_view_log" "$view_log" || true
  else
    echo "the audio host did not start"
    view_code=1
  fi
  wait_for "$host" host 60 || host_code=$?
  fetch "$host" "$peer_host_log" "$host_log" || true
  record "$name" "$host_code" "$view_code" "$host_log" "$view_log"
}

all_cases=(
  "a-host-direct a allow direct"
  "b-host-direct b allow direct"
  "a-host-relay a allow relay"
  "b-host-relay b allow relay"
  "a-host-view-only a view-only direct"
  "b-host-view-only b view-only direct"
)
for entry in "${all_cases[@]}"; do
  read -r name host approve via <<<"$entry"
  if selected "$name"; then run_case "$name" "$host" "$approve" "$via"; fi
done
for host in a b; do
  if selected "$host-host-audio"; then run_audio_case "$host-host-audio" "$host"; fi
done

# The release as users get it: each peer installs its own OS's package, and the installed apps
# connect to each other with their own headless `host` and `connect` commands. This catches
# packaging problems the source build can't.
installed_ok() {
  grep -q 'Connected to' "$1" && grep -q 'host screen: Available, host input: Available' "$1" &&
    grep -Eq 'frame Some\(\([0-9]+, [0-9]+\)\)' "$1"
}

# Installs the release on a peer and sets SIDE_app to its executable.
install_release() {
  local side=$1 dir=$2
  if [[ $(field "$side" os) == windows ]]; then
    put "$side" "$(ls "$dir"/*-setup.exe)" 'C:\dari-check\dari-setup.exe'
    # The installer is per-user, so it installs for the account the checks run as. The release
    # app gets the firewall treatment dari-check gets (prepare-peer.ps1): allowed up front, so
    # Windows never asks and never adds block rules.
    set_field "$side" app "$(on "$side" "\$ErrorActionPreference = 'Stop';
      Start-Process C:\\dari-check\\dari-setup.exe -ArgumentList '/S' -Wait;
      \$exe = (Get-ChildItem -Path \$env:LOCALAPPDATA -Filter dari.exe -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1).FullName;
      Get-NetFirewallApplicationFilter -Program \$exe -ErrorAction SilentlyContinue | Get-NetFirewallRule | Where-Object Action -eq 'Block' | Remove-NetFirewallRule;
      Remove-NetFirewallRule -Name dari-release -ErrorAction SilentlyContinue;
      New-NetFirewallRule -Name dari-release -DisplayName 'dari (release)' -Direction Inbound -Program \$exe -Action Allow -Profile Any | Out-Null;
      \$exe" | tr -d '\r' | tail -n 1)"
  else
    put "$side" "$(ls "$dir"/*.dmg)" "$(path "$side" dari.dmg)"
    on "$side" "set -e; cd $(quote "$side" "$(field "$side" dir)"); mount=\$(mktemp -d);
      hdiutil attach -quiet -nobrowse -readonly -mountpoint \"\$mount\" dari.dmg;
      rm -rf Dari.app; cp -R \"\$mount/Dari.app\" .; hdiutil detach -quiet \"\$mount\""
    set_field "$side" app "$(path "$side" Dari.app/Contents/MacOS/dari)"
  fi
  echo "$side installed: $(field "$side" app)"
}

# The release's Windows app is a GUI program: cmd.exe doesn't wait for it, so it is stopped by
# name as well as through its task.
stop_installed() {
  if [[ $(field "$1" os) == windows ]]; then
    on "$1" "Get-Process dari -ErrorAction SilentlyContinue | Stop-Process -Force" >/dev/null 2>&1 || true
  fi
  stop "$1" "$2"
}

run_installed_case() {
  local name=$1 host=$2 viewer password port=47832
  local host_log="$out/$name-host.log" view_log="$out/$name-view.log" peer_host_log peer_view_log password_file
  viewer=$(other "$host")
  peer_host_log=$(path "$host" "logs/$name-host.log")
  peer_view_log=$(path "$viewer" "logs/$name-view.log")
  password_file=$(path "$viewer" "$name-password")
  echo
  echo "== $name: the installed app on $host ($(field "$host" os)) hosts, the one on $viewer connects"
  stop_installed "$host" host
  start "$host" host "$peer_host_log" '' "$(field "$host" app)" host --port "$port" || true
  if password=$(wait_for_peer_line "$host" "$peer_host_log" "$host_log" 'Access password' 60); then
    # `connect` reads the password as a line.
    write_file "$viewer" "$password_file" "$password" line
    start "$viewer" view "$peer_view_log" "$password_file" "$(field "$viewer" app)" \
      connect "$(field "$host" ip):$port" || true
    sleep 15
    stop_installed "$viewer" view
    remove_file "$viewer" "$password_file"
    fetch "$viewer" "$peer_view_log" "$view_log" || true
  else
    echo "the installed app on $host did not start hosting" >"$view_log"
  fi
  stop_installed "$host" host
  fetch "$host" "$peer_host_log" "$host_log" || true
  if installed_ok "$view_log"; then
    summary+=("PASS $name")
  else
    summary+=("FAIL $name (see $view_log)")
    failures=$((failures + 1))
    tail -n 5 "$view_log" 2>/dev/null | sed 's/^/  /'
  fi
}

if [[ -n $release ]]; then
  release_dir="$out/release"
  mkdir -p "$release_dir"
  patterns=()
  [[ $a_os == macos || $b_os == macos ]] && patterns+=(--pattern '*_macos_aarch64.dmg')
  [[ $a_os == windows || $b_os == windows ]] && patterns+=(--pattern '*-setup.exe')
  echo
  echo "== installed $release: downloading and installing"
  gh release download "$release" --repo "$(cd "$root" && gh repo view --json nameWithOwner --jq .nameWithOwner)" \
    "${patterns[@]}" --dir "$release_dir" --clobber
  for side in a b; do install_release "$side" "$release_dir"; done
  for host in a b; do run_installed_case "$host-host-installed" "$host"; done
fi

echo
echo "== Summary ($out)"
printf '%s\n' ${summary[@]+"${summary[@]}"}
((failures == 0))
