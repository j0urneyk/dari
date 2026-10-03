<#
.SYNOPSIS
Runs a program in the signed-in user's desktop session and reports on it.

.DESCRIPTION
Programs started over SSH run outside the interactive desktop, where screen capture returns
nothing and input injection fails. This starts them through a scheduled task that runs only in
the signed-in user's session instead, so a user must be signed in (autologon on a test VM).

  start -Name host -Exe C:\dari-check\dari-check.exe -Arguments 'host','--approve','allow' -Log C:\dari-check\logs\host.log
  start -Name view -Exe ...\dari.exe -Arguments 'connect','10.0.0.2' -InputFile C:\dari-check\password.txt -Log ...
  wait  -Name host -TimeoutSeconds 180   # exits with the program's exit code
  stop  -Name host
#>
param(
    [Parameter(Mandatory, Position = 0)]
    [ValidateSet('start', 'wait', 'stop')]
    [string] $Action,
    [Parameter(Mandatory)]
    [ValidatePattern('^[A-Za-z0-9-]+$')]
    [string] $Name,
    [string] $Exe,
    [string[]] $Arguments = @(),
    [string] $Log,
    # A file the program reads as its standard input.
    [string] $InputFile,
    [int] $TimeoutSeconds = 300
)

$ErrorActionPreference = 'Stop'
# The ScheduledTasks cmdlets live in a module, which in Windows PowerShell 5.1 doesn't see this
# script's $ErrorActionPreference: without this a failed registration reports success.
$PSDefaultParameterValues['*-ScheduledTask*:ErrorAction'] = 'Stop'
$task = "dari-check-$Name"

# Get-ScheduledTaskInfo reports this until the task has run once.
$NotYetRun = 267011

# The signed-in account as Windows knows it; $env:USERDOMAIN is not reliable over SSH.
$account = [Security.Principal.WindowsIdentity]::GetCurrent().Name

function Remove-Task {
    # Stopping the task ends its cmd.exe but not the program cmd.exe started, which would keep
    # holding its port; end the program first. The task's arguments identify its cmd.exe.
    $registered = Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
    if ($registered) {
        $arguments = $registered.Actions[0].Arguments
        $shells = @(Get-CimInstance Win32_Process -Filter "Name = 'cmd.exe'" |
            Where-Object { $_.CommandLine -and $_.CommandLine.EndsWith($arguments) })
        if ($shells) {
            Get-CimInstance Win32_Process | Where-Object { $shells.ProcessId -contains $_.ParentProcessId } |
                ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
        }
    }
    Stop-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
}

switch ($Action) {
    'start' {
        if (-not $Exe -or -not $Log) { throw 'start needs -Exe and -Log' }
        foreach ($argument in $Arguments) {
            # Arguments pass through cmd.exe; keep them to characters it leaves alone.
            if ($argument -notmatch '^[A-Za-z0-9.:\\/_\[\]-]+$') { throw "unsupported argument: $argument" }
        }
        New-Item -ItemType Directory -Force -Path (Split-Path $Log) | Out-Null
        Remove-Item -Force -ErrorAction SilentlyContinue $Log
        Remove-Task
        $command = '"{0}" {1} > "{2}" 2>&1' -f $Exe, ($Arguments -join ' '), $Log
        if ($InputFile) { $command += ' < "{0}"' -f $InputFile }
        $taskAction = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument "/c `"$command`""
        $principal = New-ScheduledTaskPrincipal -UserId $account -LogonType Interactive -RunLevel Highest
        $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 30)
        Register-ScheduledTask -TaskName $task -Action $taskAction -Principal $principal -Settings $settings -Force | Out-Null
        Start-ScheduledTask -TaskName $task
        # A task for an interactive logon only starts while that user is signed in.
        for ($i = 0; $i -lt 50; $i++) {
            $state = (Get-ScheduledTask -TaskName $task).State
            $result = (Get-ScheduledTaskInfo -TaskName $task).LastTaskResult
            if ($state -eq 'Running' -or $result -ne $NotYetRun) { exit 0 }
            Start-Sleep -Milliseconds 200
        }
        Remove-Task
        throw "$task did not start: is $account signed in to the desktop?"
    }
    'wait' {
        $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
        while ((Get-ScheduledTask -TaskName $task).State -eq 'Running') {
            if ((Get-Date) -gt $deadline) {
                Remove-Task
                Write-Output "$task still running after ${TimeoutSeconds}s; stopped"
                exit 124
            }
            Start-Sleep -Milliseconds 500
        }
        $result = (Get-ScheduledTaskInfo -TaskName $task).LastTaskResult
        Unregister-ScheduledTask -TaskName $task -Confirm:$false
        exit $result
    }
    'stop' {
        Remove-Task
    }
}
