#!/usr/bin/env bash
# Waits until the VM from create-vm.sh has finished its first-logon bootstrap, then prints the
# crosscheck.sh command for it.
set -euo pipefail

name=dari-win11 timeout_minutes=120
while [[ $# -gt 0 ]]; do
  case "$1" in
    --name) name=$2; shift 2 ;;
    --timeout-minutes) timeout_minutes=$2; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

state=$HOME/.dari-check-vm
ssh_options=(-i "$state/id_ed25519" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=5
  -o StrictHostKeyChecking=accept-new -o "UserKnownHostsFile=$state/known_hosts")
deadline=$((SECONDS + timeout_minutes * 60))
ip='' stage=''
while ((SECONDS < deadline)); do
  # The guest agent comes with the guest tools, so this answers only after the first logon.
  ip=$(utmctl ip-address "$name" 2>/dev/null | grep -E '^192\.168\.' | head -n 1) || ip=''
  if [[ -z $ip ]]; then
    next='installing Windows'
  else
    reply=$(ssh "${ssh_options[@]}" "dari@$ip" 'Test-Path C:\dari-check\bootstrap-done' 2>&1) || true
    if [[ $reply == *True* ]]; then
      echo "$name is ready at $ip."
      echo "scripts/crosscheck/crosscheck.sh --windows dari@$ip --identity $state/id_ed25519 --known-hosts $state/known_hosts --build"
      exit 0
    fi
    # The guest has an address but this Mac can't reach it at all: retrying won't help.
    if [[ $reply == *'No route to host'* ]]; then
      echo "cannot reach $ip: allow Local Network access for the app running this script" \
        "(System Settings > Privacy & Security > Local Network)" >&2
      exit 1
    fi
    next="bootstrapping at $ip (log: C:\\dari-check\\bootstrap.log)"
  fi
  if [[ $next != "$stage" ]]; then
    echo "$(date +%H:%M) $next"
    stage=$next
  fi
  sleep 30
done
echo "$name was not ready within $timeout_minutes minutes" >&2
exit 1
