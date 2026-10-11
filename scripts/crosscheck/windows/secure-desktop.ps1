<#
.SYNOPSIS
The Windows side of scripts/crosscheck/secure-desktop.sh: checks the per-machine install, shows
and dismisses secure screens, and reports what they left behind.

.DESCRIPTION
  status        Exits 1 unless Dari is installed per machine, DariService runs, and UAC is on
  prepare       status, then writes secure-host.cmd and allows the installed app through the firewall
  uac           Asks to elevate "cmd.exe /c whoami /groups > result.txt", which shows a UAC prompt.
                Run it at medium integrity in the signed-in session (interactive.ps1 start
                -RunLevel Limited). It returns once the prompt is answered and cmd.exe is done
  clear-result  Deletes result.txt
  result        Prints "result: elevated", "result: not elevated", or "result: missing"
  lock          Locks the workstation. Run it in the signed-in session
  keys-down     Prints the virtual-key codes GetAsyncKeyState reports down, or "keys down: none".
                Run it in the signed-in session while Default is the input desktop
  cancel-uac    Cancels an open UAC prompt by ending consent.exe. Needs an elevated shell
  kill-helper   Ends DariService's secure-desktop helper. Needs an elevated shell
  policy        Runs "dari-service.exe policy -State" and prints the stored value. Needs an
                elevated shell
  event-mark    Prints "mark: N", the newest Application log record
  events        Prints DariService's Application log entries newer than record -After
#>
param(
    [Parameter(Mandatory, Position = 0)]
    [ValidateSet('status', 'prepare', 'uac', 'clear-result', 'result', 'lock', 'keys-down',
        'cancel-uac', 'kill-helper', 'policy', 'event-mark', 'events')]
    [string] $Action,
    [string] $WorkDir = 'C:\dari-check',
    [ValidateSet('on', 'off')]
    [string] $State,
    [long] $After = 0
)

$ErrorActionPreference = 'Stop'
$app = Join-Path $env:ProgramFiles 'Dari\dari.exe'
$service = Join-Path $env:ProgramFiles 'Dari\dari-service.exe'
$result = Join-Path $WorkDir 'result.txt'

function Assert-Ready {
    if (-not (Test-Path $app)) { throw "Dari is not installed per machine: no $app" }
    $dariService = Get-Service DariService -ErrorAction SilentlyContinue
    if (-not $dariService) { throw 'DariService is not registered' }
    if ($dariService.Status -ne 'Running') { throw "DariService is $($dariService.Status), not Running" }
    # With UAC off there is no prompt, and a Limited task runs elevated.
    $policy = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
    if ($policy.EnableLUA -ne 1) { throw 'UAC is off: run scripts/crosscheck/vm/enable-uac.sh' }
    Write-Output "installed: $app; DariService: $($dariService.Status); UAC on"
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
            Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\cmd.exe') `
                -ArgumentList "/c whoami /groups > `"$result`"" -Verb RunAs -Wait
            Write-Output 'the prompt was answered: elevated'
        } catch {
            Write-Output "the prompt was dismissed: $($_.Exception.Message)"
        }
    }
    'clear-result' {
        Remove-Item -Force -ErrorAction SilentlyContinue $result
    }
    'result' {
        # Windows names the High Mandatory Level label in the display language; its SID is fixed.
        if (-not (Test-Path $result)) {
            Write-Output 'result: missing'
        } elseif (Select-String -Path $result -SimpleMatch 'S-1-16-12288' -Quiet) {
            Write-Output 'result: elevated'
        } else {
            Write-Output 'result: not elevated'
        }
    }
    'lock' {
        rundll32.exe 'user32.dll,LockWorkStation'
    }
    'keys-down' {
        Add-Type -Namespace DariCheck -Name Keys -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern short GetAsyncKeyState(int key);
'@
        # The high bit, which makes the short negative, means the key is down now.
        $down = @(1..254 | Where-Object { [DariCheck.Keys]::GetAsyncKeyState($_) -lt 0 } |
            ForEach-Object { '0x{0:X2}' -f $_ })
        if ($down) { Write-Output "keys down: $($down -join ' ')" } else { Write-Output 'keys down: none' }
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
    'policy' {
        if (-not $State) { throw 'policy needs -State on or -State off' }
        # dari-service.exe is a GUI program, so the call operator neither waits for it nor sets
        # $LASTEXITCODE.
        $change = Start-Process -FilePath $service -ArgumentList 'policy', $State -Wait -PassThru -NoNewWindow
        if ($change.ExitCode) { throw "dari-service.exe policy $State exited with $($change.ExitCode)" }
        $stored = Get-ItemProperty 'HKLM:\SOFTWARE\Policies\Dari' -Name SecureDesktopControl -ErrorAction SilentlyContinue
        Write-Output "SecureDesktopControl: $($stored.SecureDesktopControl)"
    }
    'event-mark' {
        $newest = Get-WinEvent -LogName Application -MaxEvents 1 -ErrorAction SilentlyContinue
        Write-Output "mark: $(if ($newest) { $newest.RecordId } else { 0 })"
    }
    'events' {
        $entries = @(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; ProviderName = 'DariService' } `
            -MaxEvents 500 -ErrorAction SilentlyContinue | Where-Object RecordId -gt $After |
            Sort-Object RecordId)
        foreach ($entry in $entries) {
            # The insertion string is there even when the source's message file is missing.
            $text = if ($entry.Properties.Count) { $entry.Properties[0].Value } else { $entry.Message }
            Write-Output "event $($entry.RecordId) id $($entry.Id): $text"
        }
    }
}
