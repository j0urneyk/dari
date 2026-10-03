<#
.SYNOPSIS
Lets a Mac drive dari-check on this Windows machine over SSH. Run as an administrator.

.DESCRIPTION
Installs and starts the OpenSSH server with key-only login and PowerShell as its shell,
authorizes -PublicKey for this user, opens UDP 47821 for dari-check host, and creates
C:\dari-check. -AllowFrom limits SSH and UDP 47821 to those remote addresses.

  .\prepare-peer.ps1 -PublicKey 'ssh-ed25519 AAAA... me@mac' -AllowFrom 192.168.64.0/24
#>
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^(ssh-ed25519|ecdsa-sha2-nistp256|ssh-rsa) [A-Za-z0-9+/=]+( [^\r\n]*)?$')]
    [string] $PublicKey,
    [string[]] $AllowFrom = @('Any')
)

$ErrorActionPreference = 'Stop'

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'run this from an elevated PowerShell'
}

if (-not (Get-Service sshd -ErrorAction SilentlyContinue)) {
    Write-Output 'Installing the OpenSSH server...'
    Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 | Out-Null
}
# The first start writes the default sshd_config.
Start-Service sshd
$config = "$env:ProgramData\ssh\sshd_config"
$lines = Get-Content $config | Where-Object { $_ -notmatch '^\s*PasswordAuthentication\s' }
Set-Content -Path $config -Value (@('PasswordAuthentication no') + $lines) -Encoding ascii
New-ItemProperty -Path HKLM:\SOFTWARE\OpenSSH -Name DefaultShell -PropertyType String -Force `
    -Value "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" | Out-Null

# Administrators log in with the shared key file, which must be writable only by them and SYSTEM.
$keys = "$env:ProgramData\ssh\administrators_authorized_keys"
$existing = @(Get-Content $keys -ErrorAction SilentlyContinue)
if ($existing -notcontains $PublicKey) {
    Add-Content -Path $keys -Value $PublicKey -Encoding ascii
}
icacls.exe $keys /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F' | Out-Null
Set-Service sshd -StartupType Automatic
Restart-Service sshd

Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue | Disable-NetFirewallRule
foreach ($rule in 'dari-check-ssh', 'dari-check-udp') {
    Remove-NetFirewallRule -Name $rule -ErrorAction SilentlyContinue
}
New-NetFirewallRule -Name dari-check-ssh -DisplayName 'dari-check SSH' -Direction Inbound `
    -Protocol TCP -LocalPort 22 -RemoteAddress $AllowFrom -Action Allow -Profile Any | Out-Null
New-NetFirewallRule -Name dari-check-udp -DisplayName 'dari-check host' -Direction Inbound `
    -Protocol UDP -LocalPort 47821 -RemoteAddress $AllowFrom -Action Allow -Profile Any | Out-Null

# The first time a program listens, Windows asks the desktop user whether to allow it; unanswered,
# it adds block rules for the program, and block rules beat the allow rule above. A rule for the
# program itself keeps Windows from asking; drop any block rules an earlier run left.
$program = 'C:\dari-check\dari-check.exe'
Get-NetFirewallApplicationFilter -Program $program -ErrorAction SilentlyContinue |
    Get-NetFirewallRule | Where-Object Action -eq 'Block' | Remove-NetFirewallRule
Remove-NetFirewallRule -Name dari-check-program -ErrorAction SilentlyContinue
New-NetFirewallRule -Name dari-check-program -DisplayName 'dari-check' -Direction Inbound `
    -Program $program -RemoteAddress $AllowFrom -Action Allow -Profile Any | Out-Null

New-Item -ItemType Directory -Force -Path C:\dari-check\logs | Out-Null
Write-Output "Ready: ssh $env:USERNAME@<this machine> can drive dari-check from $($AllowFrom -join ', ')"
