# First-time update of an existing ALXPRGS Workspace installation.
#
# Old installations (for example the 660023c build) do not have the `update`
# command. This script therefore uses the *new* binary from the release package
# to verify the package and to perform the same update engine that later
# `workspace update` calls use.
#
# The script never asks for secrets and never prints them. The updater reads
# them from the environment or from the DPAPI-protected copy written by the
# running server. If neither source is available it stops before changing
# anything.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File deploy\windows\bootstrap-update.ps1 `
#     -Package C:\path\workspace-0.2.0-windows-x64.zip

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Package,
    [string]$InstallDir = "$env:LOCALAPPDATA\ALXPRGS\Workspace",
    [string]$ConfigPath = "",
    [string]$TrustedPublicKey = "",
    [switch]$KeepExtracted
)

$ErrorActionPreference = "Stop"

$wsl = Join-Path $env:SystemRoot "System32\wsl.exe"

if ([string]::IsNullOrWhiteSpace($ConfigPath)) {
    $ConfigPath = Join-Path $InstallDir "config.toml"
}

if (-not (Test-Path -LiteralPath $InstallDir)) {
    throw "installation directory not found: $InstallDir"
}

if (-not (Test-Path -LiteralPath $ConfigPath)) {
    throw "configuration not found: $ConfigPath. A first-time installation needs deploy\windows\install.ps1, not this bootstrap script."
}

$packagePath = (Resolve-Path -LiteralPath $Package).Path
$temporary = $null

try {
    if (Test-Path -LiteralPath $packagePath -PathType Container) {
        $releaseRoot = $packagePath
    }
    else {
        $temporary = Join-Path ([System.IO.Path]::GetTempPath()) ("alxprgs-bootstrap-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Path $temporary | Out-Null
        Expand-Archive -LiteralPath $packagePath -DestinationPath $temporary -Force
        $releaseRoot = $temporary
    }

    $manifest = Join-Path $releaseRoot "workspace-release.json"
    if (-not (Test-Path -LiteralPath $manifest)) {
        throw "workspace-release.json is missing; this is not a release package"
    }

    $newBinary = Join-Path $releaseRoot "workspace.exe"
    if (-not (Test-Path -LiteralPath $newBinary)) {
        throw "workspace.exe is missing from the release package"
    }

    Write-Host "[1/4] Verifying the release signature and file hashes"
    $verifyArguments = @("release", "verify", "--package", $releaseRoot)
    if (-not [string]::IsNullOrWhiteSpace($TrustedPublicKey)) {
        $verifyArguments += @("--public-key", (Resolve-Path -LiteralPath $TrustedPublicKey).Path)
    }

    & $newBinary @verifyArguments
    if ($LASTEXITCODE -ne 0) {
        throw "the release package is not trusted; nothing was changed"
    }

    if (-not [string]::IsNullOrWhiteSpace($env:WORKSPACE_SSO_CLIENT_SECRET) -or
        -not [string]::IsNullOrWhiteSpace($env:WORKSPACE_AUTH_KEY)) {
        if ([string]::IsNullOrWhiteSpace($env:WORKSPACE_SSO_CLIENT_SECRET) -or
            [string]::IsNullOrWhiteSpace($env:WORKSPACE_AUTH_KEY)) {
            throw "only one Workspace secret is set; set both or neither. Nothing was changed."
        }
    }

    Write-Host "[2/4] Checking compatibility with the installed release"
    & $newBinary --config $ConfigPath update --check --package $releaseRoot --install-dir $InstallDir
    if ($LASTEXITCODE -ne 0) {
        throw "the release is not compatible with this installation; nothing was changed"
    }

    Write-Host "[3/4] Running the update engine (backup, migration, install, health checks)"
    & $newBinary --config $ConfigPath update --bootstrap --package $releaseRoot --install-dir $InstallDir --yes
    if ($LASTEXITCODE -ne 0) {
        Write-Host ""
        Write-Warning "The update did not complete. Check the journal:"
        Write-Host "  $InstallDir\state\update\journal.json"
        Write-Host "The engine rolls back automatically when a step fails; review the journal before acting."
        exit $LASTEXITCODE
    }

    Write-Host "[4/4] Done"
    Write-Host ""
    Write-Host "Verify:"
    Write-Host "  & '$InstallDir\workspace.exe' --version"
    Write-Host "  & '$InstallDir\workspace.exe' --config '$ConfigPath' doctor --quick"
    Write-Host "  & '$InstallDir\workspace.exe' --config '$ConfigPath' update --status"
    Write-Host ""
    Write-Host "Rollback (requires the recorded backup):"
    Write-Host "  & '$InstallDir\workspace.exe' --config '$ConfigPath' update --rollback --yes"
}
finally {
    if (-not $KeepExtracted -and $null -ne $temporary -and (Test-Path -LiteralPath $temporary)) {
        Remove-Item -LiteralPath $temporary -Recurse -Force
    }
}
