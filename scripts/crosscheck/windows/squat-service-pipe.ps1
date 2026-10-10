<#
.SYNOPSIS
Checks that DariService fails loudly when another process holds \\.\pipe\dari-service. Run as an
administrator with UAC on and the user signed in.

.DESCRIPTION
Stops DariService, starts a medium-integrity PowerShell in the signed-in session (through
interactive.ps1 -RunLevel Limited) that creates \\.\pipe\dari-service and holds every client
without answering, and starts DariService. It prints the service's state and the Application and
System log entries the start left, keeps the squatter for -HoldSeconds so a Dari session can be
tried meanwhile, then ends the squatter and starts DariService again. Exits 1 if DariService
kept running while the pipe was held or doesn't run again afterwards.

  .\squat-service-pipe.ps1
  .\squat-service-pipe.ps1 -HoldSeconds 120   # time to connect a viewer during the squat
#>
param(
    [int] $HoldSeconds = 0,
    [string] $WorkDir = 'C:\dari-check\squat'
)

$ErrorActionPreference = 'Stop'
$interactive = Join-Path $PSScriptRoot 'interactive.ps1'
$name = 'squat'

function Test-ServicePipe {
    [IO.Directory]::GetFiles('\\.\pipe\') -contains '\\.\pipe\dari-service'
}

New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
$squatter = Join-Path $WorkDir 'squatter.ps1'
$log = Join-Path $WorkDir 'squatter.log'
Set-Content -Path $squatter -Encoding ASCII -Value @'
"squatter " + (whoami /groups | Select-String 'Mandatory Label')
while ($true) {
    $pipe = New-Object System.IO.Pipes.NamedPipeServerStream('dari-service', [System.IO.Pipes.PipeDirection]::InOut, 1)
    "holding the pipe"
    $pipe.WaitForConnection()
    "a client connected; not answering"
    Start-Sleep -Seconds 30
    $pipe.Dispose()
}
'@

Write-Output 'Stopping DariService...'
Stop-Service DariService
(Get-Service DariService).WaitForStatus('Stopped', '00:00:30')

$failed = $false
try {
    & $interactive start -Name $name -RunLevel Limited -Exe powershell.exe -Log $log `
        -Arguments '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $squatter
    $deadline = (Get-Date).AddSeconds(30)
    while (-not (Test-ServicePipe)) {
        if ((Get-Date) -gt $deadline) { throw 'the squatter never created \\.\pipe\dari-service' }
        Start-Sleep -Milliseconds 200
    }
    Get-Content $log

    # Entries carry whole seconds, so mark the logs by index rather than by time.
    $applicationMark = (Get-EventLog -LogName Application -Newest 1).Index
    $systemMark = (Get-EventLog -LogName System -Newest 1).Index
    Write-Output 'Starting DariService while the squatter holds its pipe...'
    try { Start-Service DariService } catch { Write-Output "Start-Service: $($_.Exception.Message)" }
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Service DariService).Status -ne 'Stopped' -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
    }
    $status = (Get-Service DariService).Status
    Write-Output "DariService: $status"
    if ($status -ne 'Stopped') {
        Write-Output 'FAIL: DariService kept running while another process held its pipe'
        $failed = $true
    }
    Start-Sleep -Seconds 2
    # Get-WinEvent can't filter on DariService, which registers no message file.
    Write-Output 'Application log, source DariService:'
    Get-EventLog -LogName Application -Source DariService -Newest 20 -ErrorAction SilentlyContinue |
        Where-Object { $_.Index -gt $applicationMark } |
        Sort-Object Index |
        ForEach-Object { '  {0:HH:mm:ss} {1} {2}' -f $_.TimeGenerated, $_.EntryType, ($_.ReplacementStrings -join ' ') }
    Write-Output 'System log, Service Control Manager:'
    Get-EventLog -LogName System -Source 'Service Control Manager' -Newest 50 -ErrorAction SilentlyContinue |
        Where-Object { $_.Index -gt $systemMark -and ($_.ReplacementStrings -contains 'Dari Service') } |
        Sort-Object Index |
        ForEach-Object { '  {0:HH:mm:ss} {1} {2}: {3}' -f $_.TimeGenerated, $_.EntryType, $_.EventID, $_.Message }

    if ($HoldSeconds -gt 0) {
        Write-Output "Holding the pipe for $HoldSeconds s; a Dari session should still work, with the secure-desktop notice."
        Start-Sleep -Seconds $HoldSeconds
    }
}
finally {
    Write-Output 'Ending the squatter and starting DariService again...'
    & $interactive stop -Name $name
    $deadline = (Get-Date).AddSeconds(30)
    while ((Test-ServicePipe) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    Start-Service DariService
    (Get-Service DariService).WaitForStatus('Running', '00:00:30')
    Write-Output "DariService: $((Get-Service DariService).Status)"
}
if ($failed) { exit 1 }
