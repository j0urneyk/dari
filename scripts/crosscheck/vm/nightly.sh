#!/usr/bin/env bash
# Runs the cross-device checks of the latest main against the VM from create-vm.sh, for a
# scheduled job. It checks out origin/main in its own clone (~/.dari-check-vm/checkout), starts
# the VM if needed, runs crosscheck.sh --build, shuts the VM down again, and keeps the last two
# weeks of results in ~/.dari-check-vm/nightly. On failure it posts a macOS notification and
# exits non-zero.
#
# The cases where this Mac hosts need an unlocked screen; when it is locked they are skipped and
# the summary says so. Run it from an app that has Screen Recording, Accessibility, and Local
# Network access, as crosscheck.sh needs.
set -euo pipefail

name=dari-win11 ip=192.168.64.5 displays=2
while [[ $# -gt 0 ]]; do
  case "$1" in
    --name) name=$2; shift 2 ;;
    --expect-windows-displays) displays=$2; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

state=$HOME/.dari-check-vm
checkout=$state/checkout
results=$state/nightly
run=$results/$(date +%Y%m%d-%H%M%S)
mkdir -p "$run"
exec > >(tee "$run/run.log") 2>&1

notify() {
  osascript -e "display notification \"$1\" with title \"Dari cross-device check\"" || true
}

if [[ ! -d $checkout/.git ]]; then
  git clone --quiet https://github.com/j0urneyk/dari.git "$checkout"
fi
git -C "$checkout" fetch --quiet origin main
git -C "$checkout" checkout --quiet --detach origin/main
echo "main at $(git -C "$checkout" log -1 --format='%h %s')"

started=0
if [[ $(utmctl status "$name") != started ]]; then
  osascript -e "tell application \"UTM\" to start virtual machine \"$name\""
  started=1
fi
"$checkout/scripts/crosscheck/vm/wait-vm.sh" --name "$name" --timeout-minutes 15 >/dev/null

cases=()
locked=$(ioreg -n Root -d1 -a | plutil -extract IOConsoleUsers.0.CGSSessionScreenIsLocked raw - 2>/dev/null || true)
if [[ $locked == true ]]; then
  echo "This Mac's screen is locked: running only the cases where Windows hosts."
  cases=(--cases windows-host-direct,windows-host-relay,windows-host-view-only)
fi

code=0
"$checkout/scripts/crosscheck/crosscheck.sh" --windows "dari@$ip" --identity "$state/id_ed25519" \
  --known-hosts "$state/known_hosts" --build --expect-windows-displays "$displays" \
  --out "$run" ${cases[@]+"${cases[@]}"} || code=$?

if ((started)); then
  ssh -i "$state/id_ed25519" -o IdentitiesOnly=yes -o BatchMode=yes \
    -o "UserKnownHostsFile=$state/known_hosts" "dari@$ip" 'Stop-Computer -Force' || true
fi
find "$results" -mindepth 1 -maxdepth 1 -type d -mtime +14 -exec rm -rf {} +

if ((code != 0)); then
  notify "Failed on $(git -C "$checkout" log -1 --format=%h): see $run"
  exit "$code"
fi
echo "All cases passed${cases[*]:+ (Mac-hosted cases skipped: screen locked)}."
