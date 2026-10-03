#!/usr/bin/env bash
# Gives the VM from create-vm.sh a sound card (intel-hda), so `dari-check audio-host` on Windows
# has an output to play its tone through and WASAPI has something to record. Without one, the
# host reports audio Unavailable.
#
# UTM's scripting interface can't add sound devices, and UTM keeps a VM's configuration in memory,
# so this shuts the VM down, quits UTM, edits the VM's config.plist, and starts both again.
# Windows on Arm finds the High Definition Audio device with its own driver.
set -euo pipefail

name=${1:-dari-win11}
ip=${2:-192.168.64.5}
state=$HOME/.dari-check-vm
config=$HOME/Library/Containers/com.utmapp.UTM/Data/Documents/$name.utm/config.plist
ssh_options=(-i "$state/id_ed25519" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=5
  -o StrictHostKeyChecking=accept-new -o "UserKnownHostsFile=$state/known_hosts")

has_sound() {
  plutil -convert json -o - "$config" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("Sound") else 1)'
}
if has_sound; then
  echo "$name already has a sound card."
  exit 0
fi

if utmctl list | grep -q "started *$name"; then
  echo "Shutting $name down..."
  # An ACPI shutdown request can go unanswered by Windows; ask Windows itself.
  ssh "${ssh_options[@]}" "dari@$ip" 'Stop-Computer -Force' || true
  until utmctl list | grep -q "stopped *$name"; do sleep 3; done
fi
if utmctl list | grep -q "started"; then
  echo "Another VM is running; stop it so UTM can be restarted." >&2
  exit 1
fi

echo "Restarting UTM with a sound card in $name..."
# UTM's own quit can be cancelled by a dialog; with no VM running, terminating it loses nothing.
pkill -TERM -x UTM || true
while pgrep -x UTM >/dev/null; do sleep 1; done
python3 - "$config" <<'PY'
import plistlib, sys
path = sys.argv[1]
with open(path, "rb") as file:
    config = plistlib.load(file)
config["Sound"] = [{"Hardware": "intel-hda"}]
with open(path, "wb") as file:
    plistlib.dump(config, file)
PY
open -a /Applications/UTM.app
until utmctl list >/dev/null 2>&1; do sleep 1; done
utmctl start "$name"
until nc -z -G 2 "$ip" 22 2>/dev/null; do sleep 5; done
# SSH answers before autologon has started the desktop session.
sleep 20
ssh "${ssh_options[@]}" "dari@$ip" 'Get-CimInstance Win32_SoundDevice | Select-Object -ExpandProperty Name'
