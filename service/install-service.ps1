# Run `matter-peek serve` at boot, or stop doing so.
#
#   .\install-service.ps1 [-Listen ADDR] [-Bin PATH]
#   .\install-service.ps1 -Uninstall
#
# From a PowerShell started as administrator. If Windows refuses to run
# scripts, start it with `powershell -ExecutionPolicy Bypass -File
# .\install-service.ps1`.
#
# It is a scheduled task of the SYSTEM account, started at boot, with its store
# and its access token in C:\ProgramData\matter-peek. It listens on the whole
# network, at 0.0.0.0:17335, unless told otherwise with -Listen (for example
# 127.0.0.1:17335 for this machine alone); Windows Firewall lets the private
# and domain networks reach it. The executable is the one given with -Bin,
# else the matter-peek.exe next to this script (as in the downloaded zip); it
# is copied to C:\Program Files\matter-peek. Running this again updates the
# executable and the task.

param(
    [string]$Listen = "0.0.0.0:17335",
    [string]$Bin,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

$name = "matter-peek"
$dir = Join-Path $env:ProgramFiles "matter-peek"
$exe = Join-Path $dir "matter-peek.exe"
$store = Join-Path $env:ProgramData "matter-peek"

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$admin = ([Security.Principal.WindowsPrincipal]$identity).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Write-Error "Run it from a PowerShell started as administrator."
}

$task = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue

if ($Uninstall) {
    if ($task) {
        Stop-ScheduledTask -TaskName $name
        Unregister-ScheduledTask -TaskName $name -Confirm:$false
    }
    Remove-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $dir
    Write-Output "matter-peek no longer runs at boot. The store in $store stays: it holds"
    Write-Output "the keys of our fabrics, and our devices can only be factory reset without"
    Write-Output "them. Delete it yourself if that is what you want."
    return
}

if (-not $Bin) {
    $Bin = Join-Path $PSScriptRoot "matter-peek.exe"
}
if (-not (Test-Path $Bin)) {
    Write-Error "No matter-peek.exe next to this script: give it with -Bin."
}

# A running task would keep the old executable busy.
if ($task) {
    Stop-ScheduledTask -TaskName $name
    Get-Process -Name "matter-peek" -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $exe } |
        Stop-Process -Force
}
New-Item -ItemType Directory -Force -Path $dir, $store | Out-Null
if ((Resolve-Path $Bin).Path -ne $exe) {
    Copy-Item -Force $Bin $exe
}
# Downloaded: Windows would ask before running it.
Unblock-File $exe

# The store holds the keys of our fabrics: for SYSTEM and the administrators
# alone (ProgramData lets every user read).
icacls $store /inheritance:r /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" | Out-Null

$action = New-ScheduledTaskAction -Execute $exe `
    -Argument "--store-dir `"$store`" serve --listen $Listen"
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
# Tried again every minute if it fails to start, and never stopped for running
# long.
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew `
    -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName $name -Description "matter-peek, health and diagnostics for Matter devices" `
    -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null

$port = $Listen.Substring($Listen.LastIndexOf(":") + 1)
$local = $Listen -match '^(127\.|localhost:|\[::1\]:)'
Remove-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue
if (-not $local) {
    New-NetFirewallRule -DisplayName $name -Direction Inbound -Protocol TCP -LocalPort $port `
        -Action Allow -Profile Private, Domain | Out-Null
}

Start-ScheduledTask -TaskName $name

# The server makes its access token at its first start.
$tokenFile = Join-Path $store "access-token"
$token = $null
for ($i = 0; $i -lt 30 -and -not $token; $i++) {
    if (Test-Path $tokenFile) {
        $token = (Get-Content -Raw $tokenFile).Trim()
    }
    if (-not $token) {
        Start-Sleep -Seconds 1
    }
}

$hostName = $Listen.Substring(0, $Listen.LastIndexOf(":"))
if ($hostName -eq "0.0.0.0" -or $hostName -eq "[::]") {
    $hostName = $env:COMPUTERNAME
}

Write-Output "matter-peek runs now, and at each boot."
if ($token) {
    Write-Output "Open it at: http://${hostName}:$port/?token=$token"
    Write-Output "Its access token is also in $tokenFile."
} else {
    Write-Output "It has not made its access token yet."
}
if (-not $local) {
    Write-Output "There is no TLS yet: the access token and the rest travel unencrypted on the network."
}
