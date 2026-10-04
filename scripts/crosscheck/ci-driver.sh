#!/usr/bin/env bash
# The driver job of the Cross-device check workflow: waits for the peer jobs to join the tailnet,
# runs crosscheck.sh between peers A and B, and then tells them to finish.
#
# PEER_A and PEER_B are `local` (this macOS runner is the peer) or OS:NAME, a peer job's tailnet
# name with OS `macos` or `windows`. Expects SSH_PRIVATE_KEY (base64) from the workflow's ssh-key
# job.
set -euo pipefail

: "${PEER_A:?}" "${PEER_B:?}" "${SSH_PRIVATE_KEY:?}"
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out=$root/target/crosscheck/ci
mkdir -p "$out"
# Outside target/, which is uploaded as an artifact.
keys=${RUNNER_TEMP:-$(mktemp -d)}
(umask 077 && printf '%s' "$SSH_PRIVATE_KEY" | base64 -d >"$keys/id_ed25519")
ssh_options=(-i "$keys/id_ed25519" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10
  -o StrictHostKeyChecking=accept-new -o "UserKnownHostsFile=$keys/known_hosts")
this_ip=$(tailscale ip -4 | head -n 1)

arguments=(--relay-ip "$this_ip" --identity "$keys/id_ed25519" --known-hosts "$keys/known_hosts"
  --out "$out" --no-audio)
destinations=()
deadline=$((SECONDS + 45 * 60))
for side in a b; do
  if [[ $side == a ]]; then peer=$PEER_A; else peer=$PEER_B; fi
  if [[ $peer == local ]]; then
    arguments+=(--"$side" local --"$side"-ip "$this_ip")
    continue
  fi
  os=${peer%%:*} name=${peer#*:}
  case "$os" in
    # The account each hosted runner image signs in as.
    windows) user=runneradmin ready='if (Test-Path C:\dari-check\dari-check.exe) { exit 0 } else { exit 1 }' ;;
    macos) user=runner ready='test -e ~/dari-check/run/serving' ;;
    *) echo "unknown peer $peer" >&2; exit 2 ;;
  esac
  echo "Waiting for $name to build dari-check and join the tailnet..."
  ip=''
  while [[ -z $ip ]] && ((SECONDS < deadline)); do
    ip=$(tailscale ip -4 "$name" 2>/dev/null | head -n 1) || ip=''
    [[ -n $ip ]] || sleep 15
  done
  [[ -n $ip ]] || { echo "$name did not join the tailnet within 45 minutes" >&2; exit 1; }
  for _ in $(seq 1 60); do
    ssh "${ssh_options[@]}" "$user@$ip" "$ready" >/dev/null 2>&1 && break
    sleep 5
  done
  destinations+=("$os:$user@$ip")
  arguments+=(--"$side" "$os:$user@$ip" --"$side"-ip "$ip")
  # Hosted Windows runners have one display.
  if [[ $os == windows ]]; then arguments+=(--"$side"-displays 1); fi
done

finish() {
  local destination
  for destination in ${destinations[@]+"${destinations[@]}"}; do
    if [[ $destination == windows:* ]]; then
      ssh "${ssh_options[@]}" "${destination#*:}" 'New-Item -Force -Path C:\dari-check\done | Out-Null'
    else
      ssh "${ssh_options[@]}" "${destination#*:}" 'touch ~/dari-check/done'
    fi || echo "could not tell ${destination#*:} to finish; it stops when its wait runs out" >&2
  done
}
trap finish EXIT

"$root/scripts/crosscheck/crosscheck.sh" "${arguments[@]}"
