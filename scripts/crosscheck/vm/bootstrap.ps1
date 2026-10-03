<#
.SYNOPSIS
First-logon setup of a VM created by create-vm.sh; runs from the setup ISO.

.DESCRIPTION
Keeps the desktop awake and unlocked (dari-check captures it), waits for the network and
winget, runs setup-vm.ps1 with the Mac's public key, and writes C:\dari-check\bootstrap-done.
The log is C:\dari-check\bootstrap.log.
#>
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path C:\dari-check\logs | Out-Null
Start-Transcript -Path C:\dari-check\bootstrap.log -Append

# A sleeping or locked desktop can't be captured.
powercfg /change monitor-timeout-ac 0
powercfg /change standby-timeout-ac 0
powercfg /setacvalueindex SCHEME_CURRENT SUB_NONE CONSOLELOCK 0
powercfg /setactive SCHEME_CURRENT
Set-ItemProperty 'HKCU:\Control Panel\Desktop' -Name ScreenSaveActive -Value 0
New-Item -Force -Path HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization | Out-Null
Set-ItemProperty HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization -Name NoLockScreen -Value 1 -Type DWord

# The SPICE agent shares this VM's clipboard with the Mac's. It would carry clipboard text
# between the two machines outside Dari's session and race the clipboard check.
Stop-Service vdservice -ErrorAction SilentlyContinue
Set-Service vdservice -StartupType Disabled -ErrorAction SilentlyContinue
Get-Process vdagent -ErrorAction SilentlyContinue | Stop-Process -Force

$deadline = (Get-Date).AddMinutes(15)
while (-not (Resolve-DnsName www.microsoft.com -ErrorAction SilentlyContinue)) {
    if ((Get-Date) -gt $deadline) { throw 'no network after 15 minutes' }
    Start-Sleep -Seconds 5
}
Write-Output 'Network is up.'

# winget arrives with App Installer, which registers some time after the first logon.
$deadline = (Get-Date).AddMinutes(15)
while (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
    if ((Get-Date) -gt $deadline) { throw 'winget did not appear after 15 minutes' }
    Add-AppxPackage -RegisterByFamilyName -MainPackage Microsoft.DesktopAppInstaller_8wekyb3d8bbwe -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 15
}
Write-Output 'winget is available.'

$publicKey = (Get-Content -Raw "$PSScriptRoot\id_ed25519.pub").Trim()
& "$PSScriptRoot\setup-vm.ps1" -PublicKey $publicKey -AllowFrom 192.168.64.0/24
Copy-Item "$PSScriptRoot\interactive.ps1" C:\dari-check\interactive.ps1 -Force

New-Item -ItemType File -Force -Path C:\dari-check\bootstrap-done | Out-Null
Write-Output 'Bootstrap done.'
Stop-Transcript
