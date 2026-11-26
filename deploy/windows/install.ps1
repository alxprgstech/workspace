# Deploy or update ALXPRGS Workspace on Windows.
#
# The service needs two secrets that are never stored in the repository:
#
#   WORKSPACE_SSO_CLIENT_SECRET   ALXPRGS SSO client secret
#   WORKSPACE_AUTH_KEY            32-byte AES key, base64/hex as accepted by the
#                                 existing deployment
#
# Set them in the environment (or paste them when prompted) before running this
# script. The script never prints them.
#
# What it does:
#   1. stops the running server (if any);
#   2. backs up the binary, configuration and a consistent SQLite snapshot;
#   3. installs the new binary and configuration next to the old ones;
#   4. starts the server and waits for /health;
#   5. prints the rollback command.
#
# Rollback is a single command: restore the previous binary from the backup
# directory and start it again. The database is migrated forward only, so the
# old binary needs its pre-migration database copy as well.

[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$BinarySource = "",
    [string]$InstallDir = "$env:LOCALAPPDATA\ALXPRGS\Workspace",
    [string]$ConfigSource = "",
    [switch]$Start
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

if ([string]::IsNullOrWhiteSpace($BinarySource)) {
    $BinarySource = Join-Path $repoRoot "target\release\workspace.exe"
}

if ([string]::IsNullOrWhiteSpace($ConfigSource)) {
    $ConfigSource = Join-Path $repoRoot "config.toml"
}

foreach ($required in @($BinarySource, $ConfigSource)) {
    if (-not (Test-Path -LiteralPath $required)) {
        throw "missing required file: $required"
    }
}

if ($Start -and ([string]::IsNullOrWhiteSpace($env:WORKSPACE_SSO_CLIENT_SECRET) -or
    [string]::IsNullOrWhiteSpace($env:WORKSPACE_AUTH_KEY))) {
    throw "WORKSPACE_SSO_CLIENT_SECRET and WORKSPACE_AUTH_KEY must be set before starting the server"
}

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$backup = Join-Path $InstallDir "backups\release-$stamp"
New-Item -ItemType Directory -Force -Path $backup | Out-Null

Write-Host "[1/5] Stopping the running server"
Get-Process -Name "workspace" -ErrorAction SilentlyContinue | ForEach-Object {
    if ($PSCmdlet.ShouldProcess($_.Id, "Stop workspace server")) {
        Stop-Process -Id $_.Id -Force
        $_.WaitForExit(10000) | Out-Null
    }
}

Write-Host "[2/5] Backing up the current installation"
Copy-Item -LiteralPath $ConfigSource -Destination (Join-Path $backup "config.toml")
Copy-Item -LiteralPath (Join-Path $InstallDir "workspace.exe") -Destination $backup -ErrorAction SilentlyContinue

$stateDir = Join-Path $InstallDir "state"
if (Test-Path -LiteralPath (Join-Path $stateDir "workspace.db")) {
    # Consistent snapshot while no process holds the database.
    $dbCopy = Join-Path $backup "state"
    New-Item -ItemType Directory -Force -Path $dbCopy | Out-Null
    Copy-Item -LiteralPath (Join-Path $stateDir "workspace.db") -Destination $dbCopy
    Copy-Item -LiteralPath (Join-Path $stateDir "workspace.db-wal") -Destination $dbCopy -ErrorAction SilentlyContinue
    Copy-Item -LiteralPath (Join-Path $stateDir "workspace.db-shm") -Destination $dbCopy -ErrorAction SilentlyContinue
}

Write-Host "[3/5] Installing the new binary and configuration"
if ($PSCmdlet.ShouldProcess($InstallDir, "Install workspace.exe")) {
    Copy-Item -LiteralPath $BinarySource -Destination (Join-Path $InstallDir "workspace.exe") -Force
}

# The server cannot start without its configuration. Always install the
# validated configuration next to the binary instead of assuming it is there.
$installedConfig = Join-Path $InstallDir "config.toml"
if ((Resolve-Path -LiteralPath $ConfigSource).Path -ne $installedConfig) {
    if ($PSCmdlet.ShouldProcess($installedConfig, "Install config.toml")) {
        Copy-Item -LiteralPath $ConfigSource -Destination $installedConfig -Force
    }
}

Write-Host "[4/5] Starting the server"
if ($Start) {
    $logDir = Join-Path $InstallDir "logs"
    New-Item -ItemType Directory -Force -Path $logDir | Out-Null
    $outLog = Join-Path $logDir "server.out.log"
    $errLog = Join-Path $logDir "server.err.log"

    $process = Start-Process -FilePath (Join-Path $InstallDir "workspace.exe") `
        -ArgumentList @("--config", $installedConfig, "serve") `
        -WorkingDirectory $InstallDir `
        -WindowStyle Hidden `
        -RedirectStandardOutput $outLog `
        -RedirectStandardError $errLog `
        -PassThru

    $port = 8765
    $listen = Select-String -LiteralPath $installedConfig -Pattern '^\s*listen\s*=\s*"127\.0\.0\.1:(\d+)"' |
        Select-Object -First 1
    if ($listen -and $listen.Matches.Count -gt 0) {
        $port = [int]$listen.Matches[0].Groups[1].Value
    }

    $deadline = (Get-Date).AddSeconds(90)
    $healthy = $false
    $exited = $false

    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 2

        if ($process.HasExited) {
            $exited = $true
            break
        }

        try {
            $response = Invoke-WebRequest -Uri "http://127.0.0.1:$port/health" -UseBasicParsing -TimeoutSec 3
            if ($response.StatusCode -eq 200) { $healthy = $true; break }
        }
        catch { }
    }

    if (-not $healthy) {
        if ($exited) {
            Write-Warning "The server exited during startup (exit code $($process.ExitCode))."
        }
        else {
            Write-Warning "The server did not report healthy on port $port."
        }

        foreach ($log in @($errLog, $outLog)) {
            if ((Test-Path -LiteralPath $log) -and (Get-Item -LiteralPath $log).Length -gt 0) {
                Write-Host ""
                Write-Host "--- $log (tail) ---"
                Get-Content -LiteralPath $log -Tail 40 | ForEach-Object { Write-Host $_ }
            }
        }

        Write-Host ""
        Write-Warning "Roll back with the commands printed below if the previous build was working."
    }
    else {
        Set-Content -LiteralPath (Join-Path $InstallDir "state\server.pid") -Value $process.Id
        Write-Host "Server is healthy (pid $($process.Id), port $port)."
    }
}

Write-Host "[5/5] Done"
Write-Host ""
Write-Host "Backup:   $backup"
Write-Host "Rollback: Stop-Process -Name workspace -Force"
Write-Host "          Copy-Item '$backup\workspace.exe' '$InstallDir\workspace.exe' -Force"
Write-Host "          Copy-Item '$backup\state\*' '$InstallDir\state\' -Force"
Write-Host "          Start-Process '$InstallDir\workspace.exe' -ArgumentList '--config','$InstallDir\config.toml','serve'"
Write-Host ""
Write-Host "Runtime check: workspace.exe --config config.toml doctor"
