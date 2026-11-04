# Register ONLY the daily bulk-refresh task, without the interactive Get-Credential that
# install-task.ps1 -Install uses for the whole estate.
#
# The refresh posts to http://127.0.0.1:8003/refresh and needs no network resource beyond
# localhost, so it needs no stored password. LogonType S4U lets it run whether or not the
# account is logged on, which is the point of a scheduled job on this box.
#
# The account defaults to the one running this script. The Python installer copied it from the
# keep-alive task, but -TakeOver deletes that task, so on a machine where the service is hosted
# by NSSM the copy would fail. Pass -UserId to run the job as a different account.
#
# If this fails on permissions, the task needs an elevated shell -- the script says so rather
# than failing silently.

param(
    [string]$UserId = [Security.Principal.WindowsIdentity]::GetCurrent().Name
)

$ErrorActionPreference = 'Stop'

$Root     = Split-Path -Parent $PSScriptRoot
$TaskName = 'McTrader_Market_Data_Service_KeepAlive_Refresh'
$Script   = Join-Path $Root 'scripts\Refresh-DailyBars.ps1'
$At       = '11:30'

if (-not (Test-Path $Script)) { throw "missing $Script" }

Write-Host "  principal for the refresh task: $UserId"

$principal = New-ScheduledTaskPrincipal -UserId $UserId -LogonType S4U -RunLevel Limited
$action = New-ScheduledTaskAction `
    -Execute 'C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe' `
    -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$Script`"" `
    -WorkingDirectory $Root
$trigger = New-ScheduledTaskTrigger -Daily -At $At
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -ExecutionTimeLimit (New-TimeSpan -Minutes 20)

if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    Write-Host "  removed the previous registration"
}

Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
    -Settings $settings -Principal $principal `
    -Description 'Bulk refresh of the market data service (POST /refresh), daily after the US close has settled' | Out-Null

$task = Get-ScheduledTask -TaskName $TaskName
$info = Get-ScheduledTaskInfo -TaskName $TaskName
Write-Host ""
Write-Host "  registered : $($task.TaskName)"
Write-Host "  state      : $($task.State)"
Write-Host "  principal  : $($task.Principal.UserId)  logon=$($task.Principal.LogonType)  runlevel=$($task.Principal.RunLevel)"
Write-Host "  trigger    : daily at $At"
Write-Host "  action     : $($task.Actions[0].Arguments)"
Write-Host "  next run   : $($info.NextRunTime)"
