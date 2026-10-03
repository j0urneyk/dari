#!/usr/bin/env bash
# Runs crosscheck.sh against an x64 GitHub Windows runner on this Mac's tailnet: dispatches the
# Cross-device check workflow with a fresh SSH key, waits for the runner to join the tailnet,
# drives it, and then tells it to finish. Options after the known ones go to crosscheck.sh.
#
# Needs Tailscale running on this Mac, gh signed in with permission to dispatch workflows, and
# the workflow's Tailscale variables set on the repository (docs/development.md).
set -euo pipefail

ref=$(git -C "$(dirname "$0")" branch --show-current)
wait_minutes=40
while [[ $# -gt 0 ]]; do
  case "$1" in
    --ref) ref=$2; shift 2 ;;
    --wait-minutes) wait_minutes=$2; shift 2 ;;
    -h | --help)
      echo "Usage: scripts/crosscheck/runner.sh [--ref BRANCH] [--wait-minutes N] [crosscheck.sh options]"
      exit 0
      ;;
    *) break ;;
  esac
done

tailscale=$(command -v tailscale || echo /Applications/Tailscale.app/Contents/MacOS/Tailscale)
"$tailscale" status >/dev/null || { echo "Tailscale is not running on this Mac" >&2; exit 1; }
mac_ip=$("$tailscale" ip -4 | head -n 1)

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out=$root/target/crosscheck/runner-$(date +%Y%m%d-%H%M%S)
mkdir -p "$out"
ssh-keygen -q -t ed25519 -N '' -C dari-check-runner -f "$out/id_ed25519"
ssh_options=(-i "$out/id_ed25519" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10
  -o StrictHostKeyChecking=accept-new -o "UserKnownHostsFile=$out/known_hosts")

since=$(date -u -v-30S +%Y-%m-%dT%H:%M:%SZ)
gh workflow run crosscheck.yml --ref "$ref" -f ssh_public_key="$(cat "$out/id_ed25519.pub")"
run_id=''
for _ in $(seq 1 30); do
  # Newest first, so the first match is the run just dispatched.
  run_id=$(gh run list --workflow crosscheck.yml --event workflow_dispatch --branch "$ref" \
    --json databaseId,createdAt --jq "map(select(.createdAt >= \"$since\")) | first | .databaseId // empty")
  [[ -n $run_id ]] && break
  sleep 2
done
[[ -n $run_id ]] || { echo "cannot find the dispatched run" >&2; exit 1; }
echo "Run: $(gh run view "$run_id" --json url --jq .url)"

peer=dari-check-$run_id
windows_ip=''
echo "Waiting for $peer to build dari-check and join the tailnet…"
deadline=$((SECONDS + wait_minutes * 60))
while ((SECONDS < deadline)); do
  windows_ip=$("$tailscale" ip -4 "$peer" 2>/dev/null | head -n 1) || windows_ip=''
  [[ -n $windows_ip ]] && break
  if [[ $(gh run view "$run_id" --json status --jq .status) == completed ]]; then
    echo "the run ended before the runner joined the tailnet" >&2
    exit 1
  fi
  sleep 15
done
[[ -n $windows_ip ]] || { echo "$peer did not join within $wait_minutes minutes" >&2; exit 1; }

user=runneradmin
for _ in $(seq 1 30); do
  ssh "${ssh_options[@]}" "$user@$windows_ip" 'Test-Path C:\dari-check\dari-check.exe' >/dev/null 2>&1 && break
  sleep 5
done

finish() {
  ssh "${ssh_options[@]}" "$user@$windows_ip" 'New-Item -Force -Path C:\dari-check\done | Out-Null' ||
    echo "could not tell the runner to finish; it stops when its hold expires" >&2
  echo "Runner logs and frames: gh run download $run_id -n crosscheck-windows"
}
trap finish EXIT

"$root/scripts/crosscheck/crosscheck.sh" --windows "$user@$windows_ip" --windows-ip "$windows_ip" \
  --mac-ip "$mac_ip" --identity "$out/id_ed25519" --known-hosts "$out/known_hosts" --out "$out" "$@"
