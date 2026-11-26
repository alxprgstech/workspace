# Provision the dedicated WSL2 runtime for ALXPRGS Workspace.
#
# Run from an elevated-free PowerShell as the Windows user that owns the
# ``ALXPRGS-Workspace`` distribution. The script performs the administrative
# preparation that the service itself is not allowed to do:
#
#   * creates the quota storage layout owned by the runtime user;
#   * installs the static Linux helper and egress proxy (trusted, no shell);
#   * installs the narrowly scoped quota helper and its sudoers rule;
#   * verifies the result with ``workspace runtime status``.
#
# It never touches other WSL distributions, production services or global WSL
# configuration. The binary and system paths can be overridden to provision an
# isolated test environment.

[CmdletBinding()]
param(
    [string]$Distro = "ALXPRGS-Workspace",
    [string]$RuntimeUser = "workspace",
    [string]$DataRoot = "/srv/alxprgs-workspace/data",
    [string]$BinRoot = "/srv/alxprgs-workspace/bin",
    [string]$QuotaHelperPath = "/usr/sbin/alxprgs-quota",
    [string]$SudoersPath = "/etc/sudoers.d/alxprgs-workspace-quota",
    [string]$HelperSource = "",
    [string]$EgressSource = "",
    [string]$WorkspaceBinary = "",
    [switch]$SkipVerify
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$wsl = Join-Path $env:SystemRoot "System32\wsl.exe"

if (-not (Test-Path $wsl)) {
    throw "wsl.exe is not available"
}

$target = "x86_64-unknown-linux-musl"

if ([string]::IsNullOrWhiteSpace($HelperSource)) {
    $HelperSource = Join-Path $repoRoot "target\$target\release\workspace-helper"
}

if ([string]::IsNullOrWhiteSpace($EgressSource)) {
    $EgressSource = Join-Path $repoRoot "target\$target\release\workspace-egress"
}

foreach ($binary in @($HelperSource, $EgressSource)) {
    if (-not (Test-Path $binary)) {
        throw "binary not found at $binary; build it with 'sh scripts/build-linux-musl.sh' inside a Linux/WSL builder"
    }
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

function Copy-IntoRuntime {
    param(
        [Parameter(Mandatory)][string]$Source,
        [Parameter(Mandatory)][string]$Destination,
        [Parameter(Mandatory)][string]$Temporary
    )

    cmd /c "`"$wsl`" -d $Distro -u root --exec /usr/bin/tee $Temporary < `"$Source`" > NUL"

    if ($LASTEXITCODE -ne 0) {
        throw "cannot copy $Source into the runtime"
    }

    Invoke-Wsl -User "root" -Arguments @(
        "/usr/bin/install", "-o", "root", "-g", "root", "-m", "0755",
        $Temporary, $Destination
    )
    Invoke-Wsl -User "root" -Arguments @("/usr/bin/rm", "-f", $Temporary)
}

Write-Host "[1/6] Preparing directories as root"
Invoke-Wsl -User "root" -Arguments @(
    "/usr/bin/install", "-d", "-o", "1000", "-g", "1000", "-m", "0755", $BinRoot
)
Invoke-Wsl -User "root" -Arguments @(
    "/usr/bin/install", "-d", "-o", "1000", "-g", "1000", "-m", "0750",
    $DataRoot, "$DataRoot/workspaces", "$DataRoot/artifacts", "$DataRoot/staging", "$DataRoot/logs"
)

Write-Host "[2/6] Installing the trusted helper and egress proxy"
Copy-IntoRuntime -Source $HelperSource -Destination "$BinRoot/workspace-helper" -Temporary "/tmp/alxprgs-helper"
Copy-IntoRuntime -Source $EgressSource -Destination "$BinRoot/workspace-egress" -Temporary "/tmp/alxprgs-egress"

# The binaries run as the unprivileged runtime user.
Invoke-Wsl -User "root" -Arguments @(
    "/usr/bin/chown", "$RuntimeUser`:$RuntimeUser",
    "$BinRoot/workspace-helper", "$BinRoot/workspace-egress"
)
Invoke-Wsl -User "root" -Arguments @(
    "/usr/bin/chmod", "0755", "$BinRoot/workspace-helper", "$BinRoot/workspace-egress"
)

Write-Host "[3/6] Installing the quota helper to $QuotaHelperPath"
$temporary = [System.IO.Path]::GetTempFileName()
try {
    $content = Get-Content -Raw -LiteralPath $quotaHelper
    [System.IO.File]::WriteAllText($temporary, $content.Replace("`r`n", "`n"))

    cmd /c "`"$wsl`" -d $Distro -u root --exec /usr/bin/tee /tmp/alxprgs-quota < `"$temporary`" > NUL"

    if ($LASTEXITCODE -ne 0) {
        throw "cannot copy the quota helper"
    }

    Invoke-Wsl -User "root" -Arguments @(
        "/usr/bin/install", "-o", "root", "-g", "root", "-m", "0755",
        "/tmp/alxprgs-quota", $QuotaHelperPath
    )
    Invoke-Wsl -User "root" -Arguments @("/usr/bin/rm", "-f", "/tmp/alxprgs-quota")
}
finally {
    Remove-Item -LiteralPath $temporary -ErrorAction SilentlyContinue
}

Write-Host "[4/6] Installing the sudoers rule to $SudoersPath"
$sudoers = @"
Defaults:$RuntimeUser !requiretty
$RuntimeUser ALL=(root) NOPASSWD: $QuotaHelperPath
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
        "/tmp/alxprgs-sudoers", $SudoersPath
    )
    Invoke-Wsl -User "root" -Arguments @("/usr/bin/rm", "-f", "/tmp/alxprgs-sudoers")
}
finally {
    Remove-Item -LiteralPath $temporary -ErrorAction SilentlyContinue
}

Invoke-Wsl -User "root" -Arguments @("/usr/sbin/visudo", "-c", "-f", $SudoersPath)

Write-Host "[5/6] Verifying the installed files"
Invoke-Wsl -User "root" -Arguments @("/usr/bin/ls", "-l", $QuotaHelperPath, $SudoersPath)
Invoke-Wsl -User $RuntimeUser -Arguments @("/usr/bin/ls", "-l", "$BinRoot/workspace-helper", "$BinRoot/workspace-egress")

Write-Host "[6/6] Done"
Write-Host ""
Write-Host "Runtime preparation finished. Next:"
Write-Host "  workspace runtime init"
Write-Host "  workspace runtime accept"
Write-Host ""

if (-not $SkipVerify -and -not [string]::IsNullOrWhiteSpace($WorkspaceBinary)) {
    & $WorkspaceBinary runtime status
}
