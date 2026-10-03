<#
.SYNOPSIS
One-time setup of a Windows 11 VM as a dari-check peer. Run as an administrator.

.DESCRIPTION
Prepares SSH (see prepare-peer.ps1), installs the Rust toolchain and the Visual Studio Build
Tools so crosscheck.sh --build can compile dari-check here, and with -AutoLogonPassword signs
this user in at boot, because dari-check runs in the signed-in desktop session.

On an Arm VM the build targets x64 by default, like the released app, and runs emulated.

  .\setup-vm.ps1 -PublicKey (Get-Content .\id_ed25519.pub) -AllowFrom 192.168.64.0/24
#>
param(
    [Parameter(Mandatory)]
    [string] $PublicKey,
    [string[]] $AllowFrom = @('Any'),
    # Stored in the registry in plain text, which is acceptable only on a disposable test VM.
    [securestring] $AutoLogonPassword
)

$ErrorActionPreference = 'Stop'

& "$PSScriptRoot\prepare-peer.ps1" -PublicKey $PublicKey -AllowFrom $AllowFrom

if (-not (Get-Command cargo -ErrorAction SilentlyContinue) -and -not (Test-Path "$env:USERPROFILE\.cargo\bin\cargo.exe")) {
    Write-Output 'Installing rustup...'
    winget install --id Rustlang.Rustup --exact --silent --accept-package-agreements --accept-source-agreements
}

$vsComponents = @(
    'Microsoft.VisualStudio.Workload.VCTools',
    'Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
    'Microsoft.VisualStudio.Component.Windows11SDK.22621'
)
if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
    $vsComponents += 'Microsoft.VisualStudio.Component.VC.Tools.ARM64'
}
$override = '--quiet --wait --norestart ' + (($vsComponents | ForEach-Object { "--add $_" }) -join ' ')
Write-Output 'Installing the Visual Studio Build Tools (this takes a while)...'
winget install --id Microsoft.VisualStudio.2022.BuildTools --exact --silent --accept-package-agreements --accept-source-agreements --override $override

$rustup = "$env:USERPROFILE\.cargo\bin\rustup.exe"
if (Test-Path $rustup) {
    & $rustup target add x86_64-pc-windows-msvc
}

if ($AutoLogonPassword) {
    $plain = [Runtime.InteropServices.Marshal]::PtrToStringBSTR(
        [Runtime.InteropServices.Marshal]::SecureStringToBSTR($AutoLogonPassword))
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon -Name AutoAdminLogon -Value '1'
    Set-ItemProperty $winlogon -Name DefaultUserName -Value $env:USERNAME
    Set-ItemProperty $winlogon -Name DefaultDomainName -Value $env:USERDOMAIN
    Set-ItemProperty $winlogon -Name DefaultPassword -Value $plain
    Write-Output "$env:USERNAME now signs in automatically at boot."
}

Write-Output 'Done. Open a new PowerShell so PATH includes cargo.'
