#!/usr/bin/env bash
# The macOS job of the Cross-device check workflow: waits for the Windows job ($PEER) to join the
# tailnet, runs crosscheck.sh against it, and then tells it to finish. Expects SSH_PRIVATE_KEY
# (base64) from the workflow's ssh-key job.
set -euo pipefail

: "${PEER:?}" "${SSH_PRIVATE_KEY:?}"
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out=$root/target/crosscheck/ci
mkdir -p "$out"
# Outside target/, which is uploaded as an artifact.
keys=${RUNNER_TEMP:-$(mktemp -d)}
(umask 077 && printf '%s' "$SSH_PRIVATE_KEY" | base64 -d >"$keys/id_ed25519")
ssh_options=(-i "$keys/id_ed25519" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10
  -o StrictHostKeyChecking=accept-new -o "UserKnownHostsFile=$keys/known_hosts")

echo "Waiting for $PEER to build dari-check and join the tailnet..."
windows_ip=''
for _ in $(seq 1 180); do
  windows_ip=$(tailscale ip -4 "$PEER" 2>/dev/null | head -n 1) || windows_ip=''
  [[ -n $windows_ip ]] && break
  sleep 15
done
[[ -n $windows_ip ]] || { echo "$PEER did not join the tailnet within 45 minutes" >&2; exit 1; }
user=runneradmin
for _ in $(seq 1 30); do
  ssh "${ssh_options[@]}" "$user@$windows_ip" 'Test-Path C:\dari-check\dari-check.exe' >/dev/null 2>&1 && break
  sleep 5
done

finish() {
  ssh "${ssh_options[@]}" "$user@$windows_ip" 'New-Item -Force -Path C:\dari-check\done | Out-Null' ||
    echo "could not tell $PEER to finish; it stops when its wait runs out" >&2
}
trap finish EXIT

"$root/scripts/crosscheck/crosscheck.sh" --windows "$user@$windows_ip" --windows-ip "$windows_ip" \
  --mac-ip "$(tailscale ip -4 | head -n 1)" --identity "$keys/id_ed25519" \
  --known-hosts "$keys/known_hosts" --expect-windows-displays 1 --out "$out"
