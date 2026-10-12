#!/usr/bin/env bash
# Turns UAC on in the VM from create-vm.sh with the Windows defaults. `--off` turns it off again.
#
# With UAC on, the VM user's SSH session (key auth) still gets a High Mandatory Level token and can
# write HKLM, so `--off` works over SSH.
set -euo pipefail

ip=192.168.64.5 on=1
for arg in "$@"; do
  case "$arg" in
    --off) on=0 ;;
    -*) echo "unknown option: $arg" >&2; exit 2 ;;
    *) ip=$arg ;;
  esac
done
if ((on)); then
  want='EnableLUA=1 ConsentPromptBehaviorAdmin=5 PromptOnSecureDesktop=1'
else
  want='EnableLUA=0'
fi

state=$HOME/.dari-check-vm
ssh_options=(-i "$state/id_ed25519" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=5
  -o StrictHostKeyChecking=accept-new -o "UserKnownHostsFile=$state/known_hosts")
# The command reaches PowerShell as one -Command argument, so statements are joined onto one line.
win() { ssh "${ssh_options[@]}" "dari@$ip" "${1//$'\n'/ }"; }

# Read before writing anything, so a failed read leaves the VM unchanged.
# 0x7FFE02F0 is SharedDataFlags in KUSER_SHARED_DATA; bit 1, DbgElevationEnabled, is EnableLUA as of boot.
probe=$(win '$ErrorActionPreference = "Stop"; (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString("o");
  ([Runtime.InteropServices.Marshal]::ReadInt32([IntPtr]0x7FFE02F0) -shr 1) -band 1' | tr -d '\r')
boot=${probe%%$'\n'*} lua_at_boot=${probe##*$'\n'}
changed=$(win "\$want = '$want';"'
  $key = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
  $now = Get-ItemProperty $key;
  foreach ($pair in $want.Split(" ")) {
    $name, $value = $pair.Split("=");
    if ($now.$name -ne [int]$value) {
      Set-ItemProperty -Path $key -Name $name -Value ([int]$value) -Type DWord;
      Write-Output $name
    }
  }' | tr -d '\r')
if [[ -n $changed ]]; then
  echo "Set: ${changed//$'\n'/ }"
else
  echo "Already set: $want"
fi
if ((lua_at_boot == on)); then exit 0; fi

echo "Windows is running with EnableLUA=$lua_at_boot. Restarting..."
win 'Restart-Computer -Force' || true
# SSH answers before autologon has started the desktop, so wait for a new boot with explorer running.
deadline=$((SECONDS + 600))
while ((SECONDS < deadline)); do
  sleep 10
  now=$(win 'if (Get-Process explorer -ErrorAction SilentlyContinue) {
    (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString("o") }' 2>/dev/null | tr -d '\r') || now=''
  if [[ -n $now && $now != "$boot" ]]; then
    echo "Done. The VM is back and signed in."
    exit 0
  fi
done
echo "the VM at $ip was not back and signed in within 10 minutes" >&2
exit 1
