# Provision the dedicated WSL2 runtime for ALXPRGS Workspace.
#
# Run from an elevated-free PowerShell as the Windows user that owns the
# ``ALXPRGS-Workspace`` distribution. The script performs the administrative
# preparation that the service itself is not allowed to do:
#
#   * creates the quota storage layout owned by the runtime user;
#   * installs the static Linux helper (trusted, no shell execution);
#   * installs the narrowly scoped quota helper and its sudoers rule;
#   * verifies the result with ``workspace runtime status``.
#
# It never touches other WSL distributions, production services or global WSL
# configuration.

[CmdletBinding()]
param(
    [string]$Distro = "ALXPRGS-Workspace",
    [string]$RuntimeUser = "workspace",
    [string]$DataRoot = "/srv/alxprgs-workspace/data",
    [string]$HelperSource = "",
    [string]$WorkspaceBinary = "",
    [switch]$SkipVerify
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$wsl = Join-Path $env:SystemRoot "System32\wsl.exe"

if (-not (Test-Path $wsl)) {
    throw "wsl.exe is not available"
}

if ([string]::IsNullOrWhiteSpace($HelperSource)) {
    $HelperSource = Join-Path $repoRoot "target\x86_64-unknown-linux-musl\release\workspace-helper"
}

if (-not (Test-Path $HelperSource)) {
    throw "helper binary not found at $HelperSource; build it with 'cargo build --release -p workspace-helper --target x86_64-unknown-linux-musl'"
}

$quotaHelper = Join-Path $PSScriptRoot "alxprgs-quota"

if (-not (Test-Path $quotaHelper)) {
    throw "quota helper not found at $quotaHelper"
}

function Invoke-Wsl {
    param(
        [Parameter(Mandatory)][string]$User,
        [Parameter(Mandatory)][string[]]$Arguments
    )

    & $wsl -d $Distro -u $User --cd / --exec @Arguments

    if ($LASTEXITCODE -ne 0) {
        throw "wsl command failed ($LASTEXITCODE): $($Arguments -join ' ')"
    }
}

Write-Host "[1/6] Preparing directories as root"
Invoke-Wsl -User "root" -Arguments @(
    "/usr/bin/install", "-d", "-o", "1000", "-g", "1000", "-m", "0755",
    "/srv/alxprgs-workspace/bin"
)
Invoke-Wsl -User "root" -Arguments @(
    "/usr/bin/install", "-d", "-o", "1000", "-g", "1000", "-m", "0750",
    "$DataRoot", "$DataRoot/workspaces", "$DataRoot/artifacts", "$DataRoot/staging", "$DataRoot/logs"
)

$egressSource = Join-Path $repoRoot "target\x86_64-unknown-linux-musl\release\workspace-egress"

if (-not (Test-Path $egressSource)) {
    throw "egress proxy not found at $egressSource; build it with 'cargo build --release -p workspace-egress --target x86_64-unknown-linux-musl'"
}

Write-Host "[2/6] Installing the trusted filesystem helper"
cmd /c "`"$wsl`" -d $Distro -u $RuntimeUser --exec /usr/bin/tee /srv/alxprgs-workspace/bin/workspace-helper < `"$HelperSource`" > NUL"

if ($LASTEXITCODE -ne 0) {
    throw "cannot copy the helper binary"
}

Invoke-Wsl -User $RuntimeUser -Arguments @(
    "/usr/bin/chmod", "0755", "/srv/alxprgs-workspace/bin/workspace-helper"
Write-Host "[2/6] Installing the trusted egress proxy"
cmd /c "`"$wsl`" -d $Distro -u $RuntimeUser --exec /usr/bin/tee /srv/alxprgs-workspace/bin/workspace-egress < `"$egressSource`" > NUL"

if ($LASTEXITCODE -ne 0) {
    throw "cannot copy the egress proxy"
}

& $wsl -d $Distro -u $RuntimeUser --exec @(
    "/usr/bin/chmod", "0755", "/srv/alxprgs-workspace/bin/workspace-egress"
)
)

Write-Host "[3/6] Installing the quota helper"
$temporary = [System.IO.Path]::GetTempFileName()
try {
    $content = Get-Content -Raw -LiteralPath $quotaHelper
    [System.IO.File]::WriteAllText($temporary, $content.Replace("`r`n", "`n"))

    cmd /c "`"$wsl`" -d $Distro -u root --exec /usr/bin/tee /tmp/alxprgs-quota < `"$temporary`" > NUL"

    if ($LASTEXITCODE -ne 0) {
        throw "cannot copy /usr/sbin/alxprgs-quota"
    }

    Invoke-Wsl -User "root" -Arguments @(
        "/usr/bin/install", "-o", "root", "-g", "root", "-m", "0755",
        "/tmp/alxprgs-quota", "/usr/sbin/alxprgs-quota"
    )
    Invoke-Wsl -User "root" -Arguments @("/usr/bin/rm", "-f", "/tmp/alxprgs-quota")
}
finally {
    Remove-Item -LiteralPath $temporary -ErrorAction SilentlyContinue
}

Write-Host "[4/6] Installing the sudoers rule"
$sudoers = @"
Defaults:$RuntimeUser !requiretty
$RuntimeUser ALL=(root) NOPASSWD: /usr/sbin/alxprgs-quota
"@

$temporary = [System.IO.Path]::GetTempFileName()
try {
    [System.IO.File]::WriteAllText($temporary, $sudoers)

    cmd /c "`"$wsl`" -d $Distro -u root --exec /usr/bin/tee /tmp/alxprgs-sudoers < `"$temporary`" > NUL"

    if ($LASTEXITCODE -ne 0) {
        throw "cannot copy the sudoers rule"
    }

    Invoke-Wsl -User "root" -Arguments @(
        "/usr/bin/install", "-o", "root", "-g", "root", "-m", "0440",
        "/tmp/alxprgs-sudoers", "/etc/sudoers.d/alxprgs-workspace-quota"
    )
    Invoke-Wsl -User "root" -Arguments @("/usr/bin/rm", "-f", "/tmp/alxprgs-sudoers")
}
finally {
    Remove-Item -LiteralPath $temporary -ErrorAction SilentlyContinue
}

Invoke-Wsl -User "root" -Arguments @("/usr/sbin/visudo", "-c", "-f", "/etc/sudoers.d/alxprgs-workspace-quota")

Write-Host "[5/6] Verifying the installed files"
Invoke-Wsl -User "root" -Arguments @("/usr/bin/ls", "-l", "/usr/sbin/alxprgs-quota", "/etc/sudoers.d/alxprgs-workspace-quota")
Invoke-Wsl -User $RuntimeUser -Arguments @("/usr/bin/ls", "-l", "/srv/alxprgs-workspace/bin/workspace-helper")

Write-Host "[6/6] Done"
Write-Host ""
Write-Host "Runtime preparation finished. Next:"
Write-Host "  workspace runtime init"
Write-Host "  workspace runtime accept"
Write-Host ""

if (-not $SkipVerify -and -not [string]::IsNullOrWhiteSpace($WorkspaceBinary)) {
    & $WorkspaceBinary runtime status
}
