#!/usr/bin/env bash
# Gives the VM from create-vm.sh a second monitor at 150%, next to its 100% primary display, so the
# cross-device checks cover switching displays and pointer coordinates on a scaled display.
#
# UTM can't add one: a second virtio-gpu device stops Windows on Arm from booting. Instead this
# installs the Virtual Display Driver (windows/install-virtual-display.ps1). Windows 11 24H2 and
# later refuse that Arm64 driver unless test signing is on, so this turns test signing on in the VM
# (it has no Secure Boot); `bcdedit /set testsigning off` undoes it.
set -euo pipefail

ip=${1:-192.168.64.5}
state=$HOME/.dari-check-vm
here=$(cd "$(dirname "$0")" && pwd)
ssh_options=(-i "$state/id_ed25519" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=5
  -o StrictHostKeyChecking=accept-new -o "UserKnownHostsFile=$state/known_hosts")
# The command reaches PowerShell as one -Command argument, so statements are joined onto one line.
win() { ssh "${ssh_options[@]}" "dari@$ip" "${1//$'\n'/ }"; }

# Restarts the VM and waits until it is back and signed in.
restart() {
  win 'Restart-Computer -Force' || true
  sleep 15
  until nc -z -G 2 "$ip" 22 2>/dev/null; do sleep 5; done
  # SSH answers before autologon has started the desktop session.
  sleep 20
}

scp -q "${ssh_options[@]}" "$here/../windows/install-virtual-display.ps1" "dari@$ip:C:/dari-check/"
if ! win 'bcdedit /enum "{current}"' | grep -qi 'testsigning.*Yes'; then
  echo "Turning on test signing and restarting..."
  win 'bcdedit /set testsigning on' >/dev/null
  restart
fi

echo "Installing the virtual display..."
win "Set-ExecutionPolicy -Scope Process Bypass -Force; & C:\\dari-check\\install-virtual-display.ps1 -Width 1920 -Height 1080"

# Per-monitor scaling, as Settings > Display stores it: a step relative to the recommended scale
# (100% for this monitor), so 2 is 150%. It applies at the next sign-in.
echo "Scaling the virtual display to 150% and restarting..."
win '$id = $null;
  for ($i = 0; $i -lt 30 -and -not $id; $i++) {
    $id = (Get-ChildItem HKLM:\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\ScaleFactors |
      Where-Object PSChildName -like "MTT*").PSChildName;
    if (-not $id) { Start-Sleep 1 }
  };
  if (-not $id) { throw "the virtual display never registered" };
  $key = "HKCU:\Control Panel\Desktop\PerMonitorSettings\$id";
  New-Item -Force -Path $key | Out-Null;
  Set-ItemProperty -Path $key -Name DpiValue -Value 2 -Type DWord'
restart
echo "Done. Pass --expect-windows-displays 2 to crosscheck.sh."
