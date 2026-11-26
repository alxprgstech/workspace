# Start the reverse SSH tunnel that publishes the local Workspace listener
# through the existing NGINX on ALX-3.
#
# Topology:
#
#   ChatGPT -> https://workspace.alxprgs.tech (NGINX on ALX-3)
#           -> 127.0.0.1:18765 on ALX-3
#           -> SSH reverse tunnel -> 127.0.0.1:8765 on this Windows host
#
# The tunnel is an ordinary outbound SSH connection: nothing on ALX-3 is
# modified. Keep this process running for as long as the public endpoint must
# stay available; it does not survive a reboot unless it is started again (or
# installed as a scheduled task by the operator).
#
# Usage:
#   pwsh deploy\windows\tunnel.ps1
#   pwsh deploy\windows\tunnel.ps1 -HostName alx@192.168.1.225 -RemotePort 18765 -LocalPort 8765

[CmdletBinding()]
param(
    [string]$SshTarget = "alx@192.168.1.225",
    [int]$RemotePort = 18765,
    [int]$LocalPort = 8765,
    [string]$LogPath = (Join-Path $env:TEMP "alxprgs-workspace-tunnel.log")
)

$ErrorActionPreference = "Stop"

$health = "http://127.0.0.1:$LocalPort/health"

try {
    $response = Invoke-WebRequest -Uri $health -UseBasicParsing -TimeoutSec 3
    if ($response.StatusCode -ne 200) {
        throw "unexpected status $($response.StatusCode)"
    }
}
catch {
    throw "the local Workspace server is not healthy on $health; start it first"
}

$existing = Get-CimInstance Win32_Process -Filter "Name='ssh.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -match "-R\s+127\.0\.0\.1:${RemotePort}:127\.0\.0\.1:${LocalPort}" }

if ($existing) {
    Write-Host "Tunnel is already running (pid $($existing.ProcessId))."
    exit 0
}

$arguments = @(
    "-N", "-T",
    "-o", "BatchMode=yes",
    "-o", "ExitOnForwardFailure=yes",
    "-o", "ServerAliveInterval=30",
    "-o", "ServerAliveCountMax=3",
    "-o", "StrictHostKeyChecking=accept-new",
    "-E", $LogPath,
    "-R", "127.0.0.1:$RemotePort:127.0.0.1:$LocalPort",
    $SshTarget
)

$process = Start-Process -FilePath "ssh.exe" -ArgumentList $arguments -WindowStyle Hidden -PassThru

Start-Sleep -Seconds 5

if ($process.HasExited) {
    $log = if (Test-Path -LiteralPath $LogPath) { Get-Content -LiteralPath $LogPath -Tail 20 } else { @() }
    throw "ssh exited with code $($process.ExitCode): $($log -join ' ')"
}

Write-Host "Tunnel started (pid $($process.Id)): ALX-3 127.0.0.1:$RemotePort -> local 127.0.0.1:$LocalPort"
Write-Host "Log: $LogPath"
Write-Host "Stop with: Stop-Process -Id $($process.Id)"
