<#
.SYNOPSIS
The Windows side of scripts/crosscheck/secure-desktop.sh: checks the per-machine install, and
shows and dismisses secure screens.

.DESCRIPTION
  status        Exits 1 unless Dari is installed per machine, DariService runs, and UAC is on
  prepare       status, then writes secure-host.cmd and allows the installed app through the firewall
  uac           Asks to elevate whoami.exe, which shows a UAC prompt. Run it at medium integrity
                in the signed-in session (interactive.ps1 start -RunLevel Limited)
  lock          Locks the workstation. Run it in the signed-in session
  cancel-uac    Cancels an open UAC prompt by ending consent.exe. Needs an elevated shell
  kill-helper   Ends DariService's secure-desktop helper. Needs an elevated shell
#>
param(
    [Parameter(Mandatory, Position = 0)]
    [ValidateSet('status', 'prepare', 'uac', 'lock', 'cancel-uac', 'kill-helper')]
    [string] $Action,
    [string] $WorkDir = 'C:\dari-check'
)

$ErrorActionPreference = 'Stop'
$app = Join-Path $env:ProgramFiles 'Dari\dari.exe'

function Assert-Ready {
    if (-not (Test-Path $app)) { throw "Dari is not installed per machine: no $app" }
    $service = Get-Service DariService -ErrorAction SilentlyContinue
    if (-not $service) { throw 'DariService is not registered' }
    if ($service.Status -ne 'Running') { throw "DariService is $($service.Status), not Running" }
    # With UAC off there is no prompt, and a Limited task runs elevated.
    $policy = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
    if ($policy.EnableLUA -ne 1) { throw 'UAC is off: run scripts/crosscheck/vm/enable-uac.sh' }
    Write-Output "installed: $app; DariService: $($service.Status); UAC on"
}

switch ($Action) {
    'status' {
        Assert-Ready
    }
    'prepare' {
        Assert-Ready
        # A batch file runs in the scheduled task's own cmd.exe, so the app stays its child (which
        # interactive.ps1 stop ends) and cmd.exe waits for it although it is a GUI program.
        Set-Content -Path (Join-Path $WorkDir 'secure-host.cmd') -Encoding ASCII -Value @(
            '@set RUST_LOG=info',
            "@`"$app`" %*"
        )
        # Unanswered, Windows' first-listen prompt adds block rules, which beat an allow rule.
        Get-NetFirewallApplicationFilter -Program $app -ErrorAction SilentlyContinue |
            Get-NetFirewallRule | Where-Object Action -eq 'Block' | Remove-NetFirewallRule
        Remove-NetFirewallRule -Name dari-release -ErrorAction SilentlyContinue
        New-NetFirewallRule -Name dari-release -DisplayName 'dari (release)' -Direction Inbound `
            -Program $app -Action Allow -Profile Any | Out-Null
    }
    'uac' {
        try {
            Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\whoami.exe') -Verb RunAs -Wait
            Write-Output 'the prompt was answered: elevated'
        } catch {
            Write-Output "the prompt was dismissed: $($_.Exception.Message)"
        }
    }
    'lock' {
        rundll32.exe 'user32.dll,LockWorkStation'
    }
    'cancel-uac' {
        $consent = @(Get-Process consent -ErrorAction SilentlyContinue)
        $consent | Stop-Process -Force
        Write-Output "ended $($consent.Count) consent.exe"
    }
    'kill-helper' {
        # The helper runs as SYSTEM; ending it takes the debug privilege an elevated shell holds.
        [Diagnostics.Process]::EnterDebugMode()
        $helpers = @(Get-CimInstance Win32_Process -Filter "Name = 'dari-service.exe'" |
            Where-Object { $_.CommandLine -match '\shelper\s' })
        if (-not $helpers) { throw 'no secure-desktop helper is running' }
        foreach ($helper in $helpers) {
            Stop-Process -Id $helper.ProcessId -Force
            Write-Output "ended the helper, process $($helper.ProcessId)"
        }
    }
}
