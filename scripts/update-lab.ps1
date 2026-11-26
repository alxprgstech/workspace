# Isolated update acceptance lab.
#
# Creates a second Workspace instance under %LOCALAPPDATA%\ALXPRGS\Workspace-Lab
# with its own port, database, installation directory and WSL paths. It never
# touches the production instance, its WSL runtime files or its data.
#
# Actions:
#   Reset            recreate the lab from a package (the "old" release)
#   ProvisionRuntime install helper/egress/quota/sudoers into the lab WSL paths
#   SeedSchema9      create a schema-9 database with preserved rows (server stopped)
#   Start / Stop     control the lab server
#   Status           report version, health, journal
#   SeedSession      create a real web session and print the cookie values
#
# Test secrets are fixed, non-production values. The server writes them to its
# DPAPI store, which is what the update tests exercise.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("Reset", "ProvisionRuntime", "SeedSchema9", "Start", "Stop", "Status", "SeedSession")]
    [string]$Action,

    [string]$LabDir = "$env:LOCALAPPDATA\ALXPRGS\Workspace-Lab",
    [string]$BasePackage = "",
    [string]$NewPackage = "",
    [string]$Port = "8799",
    [string]$Distro = "ALXPRGS-Workspace",
    [string]$RuntimeUser = "workspace",
    [string]$DataRoot = "/srv/alxprgs-workspace-lab/data",
    [string]$BinRoot = "/srv/alxprgs-workspace-lab/bin",
    [string]$QuotaHelperPath = "/srv/alxprgs-workspace-lab/system/alxprgs-quota",
    [string]$SudoersPath = "/srv/alxprgs-workspace-lab/system/sudoers/alxprgs-workspace-quota",
    [string]$Subject = "3eaf7a4b-5c6d-4e7f-9a8b-3c4d5e6f7081",
    [string]$Role = "admin",
    [switch]$KeepData
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$production = Join-Path $env:LOCALAPPDATA "ALXPRGS\Workspace"

if ($LabDir.TrimEnd('\') -eq $production.TrimEnd('\')) {
    throw "refusing to use the production installation directory as the lab"
}

$configPath = Join-Path $LabDir "config.toml"
$wsl = Join-Path $env:SystemRoot "System32\wsl.exe"

# Fixed, non-production test secrets.
$labClientSecret = "lab-client-secret-0123456789"
$labAuthKey = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk="

function Invoke-WslRoot {
    param([Parameter(Mandatory)][string[]]$Arguments)

    & $wsl -d $Distro -u root --exec @Arguments

    if ($LASTEXITCODE -ne 0) {
        throw "wsl command failed ($LASTEXITCODE): $($Arguments -join ' ')"
    }
}

function Wait-Health {
    param([int]$TimeoutSeconds = 60)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)

    while ((Get-Date) -lt $deadline) {
        try {
            $response = Invoke-WebRequest -Uri "http://127.0.0.1:$Port/health" -UseBasicParsing -TimeoutSec 2

            if ($response.StatusCode -eq 200) {
                return ($response.Content | ConvertFrom-Json)
            }
        }
        catch {
            Start-Sleep -Milliseconds 500
        }
    }

    throw "lab server did not become healthy on port $Port"
}

function Get-LabConfig {
    $dataDirToml = $LabDir.Replace('\', '\\')

    return @"
[server]
listen = "127.0.0.1:$Port"

[storage]
data_dir = "$dataDirToml"
max_import_mib = 32

[resources]
cpu_millis = 2000
memory_mib = 4096
storage_mib = 40960
max_active_workspaces = 3
max_concurrent_jobs = 1
max_processes_per_workspace = 256
max_job_seconds = 3600

[workspace]
default_mode = "ephemeral"
idle_timeout_seconds = 300
ttl_seconds = 86400
artifact_ttl_seconds = 86400

[network]
default_mode = "none"
allowlist = []

[logging]
level = "info"

[sso]
issuer = "https://auth.alxprgs.tech"
client_id = "client_lab_update_0001"
redirect_uri = "https://workspace-lab.alxprgs.tech/auth/callback"

[runtime]
backend = "wsl2"
distro = "$Distro"
user = "$RuntimeUser"
docker_path = "/usr/bin/docker"
engine_name = "alxprgs-workspace"
socket = "unix:///run/user/1000/docker.sock"
data_root = "$DataRoot"
helper_path = "$BinRoot/workspace-helper"
quota_helper_path = "$QuotaHelperPath"
sudoers_path = "$SudoersPath"
startup_timeout_seconds = 120
command_timeout_seconds = 60
workspace_storage_mib = 8192
workspace_files = 200000
minimum_free_mib = 1024
base_image = "debian@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0"

[update]
package_dir = "$($LabDir.Replace('\', '\\'))\\updates\\package"
drain_timeout_seconds = 60
health_timeout_seconds = 20
reserve_free_mib = 256
"@
}

function Expand-Package {
    param(
        [Parameter(Mandatory)][string]$Package,
        [Parameter(Mandatory)][string]$Target
    )

    $resolved = (Resolve-Path -LiteralPath $Package).Path

    if (Test-Path -LiteralPath $resolved -PathType Container) {
        $releaseRoot = $resolved
    }
    else {
        if (Test-Path -LiteralPath $Target) {
            Remove-Item -LiteralPath $Target -Recurse -Force
        }

        New-Item -ItemType Directory -Force -Path $Target | Out-Null
        Expand-Archive -LiteralPath $resolved -DestinationPath $Target -Force
        $releaseRoot = $Target
    }

    if (-not (Test-Path -LiteralPath (Join-Path $releaseRoot "workspace-release.json"))) {
        throw "package $resolved has no workspace-release.json"
    }

    return $releaseRoot
}

function Stop-LabServer {
    $pidPath = Join-Path $LabDir "state\server.pid"

    if (-not (Test-Path -LiteralPath $pidPath)) {
        return
    }

    $serverPid = [int](Get-Content -LiteralPath $pidPath -Raw).Trim()
    $process = Get-Process -Id $serverPid -ErrorAction SilentlyContinue

    if ($process) {
        # Ask the current build to shut down, then terminate if it does not.
        Set-Content -LiteralPath (Join-Path $LabDir "state\shutdown.request") -Value ([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())

        for ($i = 0; $i -lt 60; $i++) {
            Start-Sleep -Milliseconds 250
            if (-not (Get-Process -Id $serverPid -ErrorAction SilentlyContinue)) { break }
        }

        if (Get-Process -Id $serverPid -ErrorAction SilentlyContinue) {
            Stop-Process -Id $serverPid -Force
        }
    }

    Remove-Item -LiteralPath $pidPath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $LabDir "state\shutdown.request") -Force -ErrorAction SilentlyContinue
}

function Start-LabServer {
    if (-not (Test-Path -LiteralPath (Join-Path $LabDir "workspace.exe"))) {
        throw "lab has no workspace.exe; run -Action Reset first"
    }

    New-Item -ItemType Directory -Force -Path (Join-Path $LabDir "logs") | Out-Null

    $env:WORKSPACE_SSO_CLIENT_SECRET = $labClientSecret
    $env:WORKSPACE_AUTH_KEY = $labAuthKey

    $process = Start-Process -FilePath (Join-Path $LabDir "workspace.exe") `
        -ArgumentList @("--config", $configPath, "serve") `
        -WorkingDirectory $LabDir `
        -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $LabDir "logs\server.out.log") `
        -RedirectStandardError (Join-Path $LabDir "logs\server.err.log") `
        -PassThru

    Set-Content -LiteralPath (Join-Path $LabDir "state\server.pid") -Value $process.Id
    $health = Wait-Health

    Write-Host "Lab server pid $($process.Id), version $($health.version), build $($health.build_sha), schema $($health.schema_version)"
}

switch ($Action) {
    "Reset" {
        if ([string]::IsNullOrWhiteSpace($BasePackage)) {
            throw "-BasePackage is required for Reset"
        }

        Stop-LabServer

        if (-not $KeepData -and (Test-Path -LiteralPath $LabDir)) {
            Remove-Item -LiteralPath $LabDir -Recurse -Force
        }

        New-Item -ItemType Directory -Force -Path $LabDir, (Join-Path $LabDir "updates"), (Join-Path $LabDir "state"), (Join-Path $LabDir "logs") | Out-Null

        $baseRoot = Expand-Package -Package $BasePackage -Target (Join-Path $LabDir "updates\package-base")

        Copy-Item -LiteralPath (Join-Path $baseRoot "workspace.exe") -Destination (Join-Path $LabDir "workspace.exe") -Force

        if (-not [string]::IsNullOrWhiteSpace($NewPackage)) {
            $newRoot = Expand-Package -Package $NewPackage -Target (Join-Path $LabDir "updates\package")
            Write-Host "New package: $newRoot"
        }

        Set-Content -LiteralPath $configPath -Value (Get-LabConfig) -Encoding UTF8

        Write-Host "Lab reset at $LabDir"
        Write-Host "  old release: $baseRoot"
        Write-Host "  port: $Port"
        Write-Host "  WSL paths: $DataRoot / $BinRoot"
    }

    "ProvisionRuntime" {
        if ([string]::IsNullOrWhiteSpace($BasePackage)) {
            throw "-BasePackage is required for ProvisionRuntime"
        }

        $baseRoot = Expand-Package -Package $BasePackage -Target (Join-Path $LabDir "updates\package-base")

        Invoke-WslRoot -Arguments @("/usr/bin/install", "-d", "-o", "1000", "-g", "1000", "-m", "0755", $BinRoot)
        Invoke-WslRoot -Arguments @(
            "/usr/bin/install", "-d", "-o", "1000", "-g", "1000", "-m", "0750",
            $DataRoot, "$DataRoot/workspaces", "$DataRoot/artifacts", "$DataRoot/staging", "$DataRoot/logs"
        )
        Invoke-WslRoot -Arguments @("/usr/bin/install", "-d", "-o", "root", "-g", "root", "-m", "0755", "/srv/alxprgs-workspace-lab/system")
        Invoke-WslRoot -Arguments @("/usr/bin/install", "-d", "-o", "root", "-g", "root", "-m", "0755", "/srv/alxprgs-workspace-lab/system/sudoers")

        & (Join-Path $repoRoot "deploy\windows\provision-runtime.ps1") `
            -Distro $Distro `
            -RuntimeUser $RuntimeUser `
            -DataRoot $DataRoot `
            -BinRoot $BinRoot `
            -QuotaHelperPath $QuotaHelperPath `
            -SudoersPath $SudoersPath `
            -HelperSource (Join-Path $baseRoot "linux\workspace-helper") `
            -EgressSource (Join-Path $baseRoot "linux\workspace-egress") `
            -SkipVerify

        Write-Host "Lab runtime provisioned"
    }

    "SeedSchema9" {
        $seed = Join-Path $repoRoot "target\debug\examples\seed_v9.exe"

        if (-not (Test-Path -LiteralPath $seed)) {
            throw "build the example first: cargo build -p workspace-server --example seed_v9"
        }

        $env:WORKSPACE_SSO_CLIENT_SECRET = $labClientSecret
        $env:WORKSPACE_AUTH_KEY = $labAuthKey

        & $seed $configPath
        if ($LASTEXITCODE -ne 0) { throw "schema-9 seed failed" }
    }

    "Start" {
        Start-LabServer
    }

    "Stop" {
        Stop-LabServer
        Write-Host "Lab server stopped"
    }

    "Status" {
        $health = $null

        try {
            $health = Invoke-WebRequest -Uri "http://127.0.0.1:$Port/health" -UseBasicParsing -TimeoutSec 2 | Select-Object -ExpandProperty Content | ConvertFrom-Json
        }
        catch {
            Write-Host "health: unavailable"
        }

        if ($health) {
            Write-Host "health: $($health.status), version $($health.version), build $($health.build_sha), schema $($health.schema_version)"
        }

        $journal = Join-Path $LabDir "state\update\journal.json"
        if (Test-Path -LiteralPath $journal) {
            $state = Get-Content -LiteralPath $journal -Raw | ConvertFrom-Json
            Write-Host "journal: $($state.status) stage=$($state.stage) progress=$($state.progress) $($state.from_version) -> $($state.to_version)"
            if ($state.error) { Write-Host "error: $($state.error)" }
        }
        else {
            Write-Host "journal: none"
        }

        if (Test-Path -LiteralPath (Join-Path $LabDir "state\installed.json")) {
            Get-Content -LiteralPath (Join-Path $LabDir "state\installed.json") -Raw
        }
    }

    "SeedSession" {
        $seed = Join-Path $repoRoot "target\debug\examples\seed_session.exe"

        if (-not (Test-Path -LiteralPath $seed)) {
            throw "build the example first: cargo build -p workspace-server --example seed_session"
        }

        $env:WORKSPACE_SSO_CLIENT_SECRET = $labClientSecret
        $env:WORKSPACE_AUTH_KEY = $labAuthKey

        & $seed $configPath $Subject $Role
        if ($LASTEXITCODE -ne 0) { throw "session seed failed" }
    }
}
