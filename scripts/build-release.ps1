# Build a signed, reproducible ALXPRGS Workspace release package locally.
#
# No Rust, Node.js, musl toolchain or Linux environment is needed on the target
# machine: this script runs on the maintainer host and produces one zip that the
# updater verifies and installs.
#
# What it does:
#   1. reads the version from Cargo.toml (or -Version);
#   2. builds the web interface, then workspace.exe with the embedded UI;
#   3. builds workspace-helper and workspace-egress for x86_64-unknown-linux-musl
#      inside a Linux container with an explicit musl-gcc linker;
#   4. assembles only the deployment files (never config.toml, secrets, data);
#   5. writes workspace-release.json, signs it with the Ed25519 release key and
#      creates a deterministic zip;
#   6. verifies the produced package with the key embedded in the binary.
#
# The private key is never printed and must stay outside the repository. The
# public half is committed as deploy/release-public-key.txt.

[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$OutputDir = "dist",
    [string]$KeyPath = "$env:USERPROFILE\.alxprgs\release\release-signing-key.pk8",
    [string]$ChangelogPath = "",
    [string]$Channel = "stable",
    [int]$SchemaVersion = 11,
    [int]$MinSchema = 9,
    [string]$LinuxImage = "rust:1.100-bookworm",
    [switch]$SkipFrontend,
    [switch]$SkipWindows,
    [switch]$SkipLinux,
    [switch]$SkipPackage,
    [switch]$AllowDirty
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

function Fail([string]$Message) {
    throw $Message
}

function Get-WorkspaceVersion {
    $cargo = Get-Content -Raw -LiteralPath (Join-Path $repoRoot "Cargo.toml")
    $match = [regex]::Match($cargo, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"')
    if (-not $match.Success) { Fail "cannot read the workspace version from Cargo.toml" }
    return $match.Groups[1].Value
}

if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = Get-WorkspaceVersion
}

if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    Fail "version must be a plain semantic version, got '$Version'"
}

if (-not $SkipPackage -and -not (Test-Path -LiteralPath $KeyPath)) {
    Fail "release signing key not found at $KeyPath. Run: workspace release keygen --out $([System.IO.Path]::GetDirectoryName($KeyPath))"
}

$sha = (& git rev-parse HEAD 2>$null)
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($sha)) {
    Fail "not a git checkout; the release never guesses its commit"
}
$sha = $sha.Trim()

$dirty = (& git status --porcelain --untracked-files=no)
if ($LASTEXITCODE -ne 0) { Fail "git status failed" }

if (-not [string]::IsNullOrWhiteSpace($dirty)) {
    if (-not $AllowDirty) {
        Fail "the working tree has uncommitted changes; commit them or pass -AllowDirty for a test build"
    }

    $sha = "$sha-dirty"
    Write-Warning "building from a dirty tree: $sha"
}

$buildTime = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$output = if ([System.IO.Path]::IsPathRooted($OutputDir)) { $OutputDir } else { Join-Path $repoRoot $OutputDir }
$staging = Join-Path $output "staging"
$linuxOut = Join-Path $staging "linux"

if (Test-Path -LiteralPath $staging) {
    Remove-Item -LiteralPath $staging -Recurse -Force
}

New-Item -ItemType Directory -Force -Path $staging, $linuxOut, (Join-Path $staging "deploy") | Out-Null

Write-Host "== ALXPRGS Workspace $Version ($sha)"

if (-not $SkipFrontend) {
    Write-Host "[1/6] Frontend: npm ci, typecheck, build"
    Push-Location (Join-Path $repoRoot "web")
    try {
        npm ci --no-audit --no-fund
        if ($LASTEXITCODE -ne 0) { Fail "npm ci failed" }
        npm run typecheck
        if ($LASTEXITCODE -ne 0) { Fail "npm run typecheck failed" }
        npm run build
        if ($LASTEXITCODE -ne 0) { Fail "npm run build failed" }
    }
    finally {
        Pop-Location
    }
}
else {
    Write-Host "[1/6] Frontend: skipped (using web/dist)"
}

if (-not (Test-Path -LiteralPath (Join-Path $repoRoot "web\dist\index.html"))) {
    Fail "web/dist is missing; build the frontend first"
}

if (-not $SkipWindows) {
    Write-Host "[2/6] Windows: cargo build --release --locked -p workspace-cli --features web-ui"
    cargo build --release --locked -p workspace-cli --features web-ui
    if ($LASTEXITCODE -ne 0) { Fail "Windows release build failed" }

    Copy-Item -LiteralPath (Join-Path $repoRoot "target\release\workspace.exe") `
        -Destination (Join-Path $staging "workspace.exe")
}
else {
    Write-Host "[2/6] Windows: skipped"
    if (-not (Test-Path -LiteralPath (Join-Path $staging "workspace.exe"))) {
        Fail "staging has no workspace.exe; -SkipWindows requires a previously staged binary"
    }
}

if (-not $SkipLinux) {
    Write-Host "[3/6] Linux musl: $LinuxImage"

    if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
        Fail "docker is required for the local musl build. Use scripts/build-linux-musl.sh in WSL/CI, or install Docker Desktop."
    }

    $repoMount = $repoRoot

    docker run --rm `
        -v "${repoMount}:/src" `
        -v "alxprgs-cargo-registry:/usr/local/cargo/registry" `
        -v "alxprgs-cargo-git:/usr/local/cargo/git" `
        -v "alxprgs-musl-target:/target" `
        -w /src `
        -e CARGO_TARGET_DIR=/target `
        -e CARGO_TERM_COLOR=never `
        $LinuxImage `
        sh -c "sh scripts/build-linux-musl.sh"

    if ($LASTEXITCODE -ne 0) { Fail "Linux musl build failed" }

    docker run --rm `
        -v "alxprgs-musl-target:/target" `
        -v "${linuxOut}:/out" `
        $LinuxImage `
        sh -c "cp /target/x86_64-unknown-linux-musl/release/workspace-helper /target/x86_64-unknown-linux-musl/release/workspace-egress /out/ && chmod 0755 /out/workspace-helper /out/workspace-egress"

    if ($LASTEXITCODE -ne 0) { Fail "cannot extract the Linux binaries from the builder" }
}
else {
    Write-Host "[3/6] Linux musl: skipped"
}

Write-Host "[4/6] Deployment files"

$deployFiles = @(
    "install.ps1",
    "provision-runtime.ps1",
    "tunnel.ps1",
    "bootstrap-update.ps1",
    "alxprgs-quota"
)

foreach ($file in $deployFiles) {
    $source = Join-Path $repoRoot "deploy\windows\$file"
    if (-not (Test-Path -LiteralPath $source)) { Fail "deployment file is missing: $source" }
    Copy-Item -LiteralPath $source -Destination (Join-Path $staging "deploy\$file")
}

foreach ($doc in @("docs\UPDATE.md", "docs\RELEASES.md")) {
    $source = Join-Path $repoRoot $doc
    if (Test-Path -LiteralPath $source) {
        Copy-Item -LiteralPath $source -Destination (Join-Path $staging ([System.IO.Path]::GetFileName($doc)))
    }
}

if (-not (Test-Path -LiteralPath (Join-Path $staging "linux\workspace-helper"))) {
    Fail "linux/workspace-helper is missing in the staging directory"
}

if (-not (Test-Path -LiteralPath (Join-Path $staging "linux\workspace-egress"))) {
    Fail "linux/workspace-egress is missing in the staging directory"
}

if ($SkipPackage) {
    Write-Host "[5/6] Packaging: skipped"
    Write-Host "Staging: $staging"
    exit 0
}

Write-Host "[5/6] Sign and package"

$packagePath = Join-Path $output "workspace-$Version-windows-x64.zip"

$arguments = @(
    "release", "package",
    "--files", $staging,
    "--output", $packagePath,
    "--release-version", $Version,
    "--git-sha", $sha,
    "--build-time-unix", "$buildTime",
    "--schema-version", "$SchemaVersion",
    "--min-schema", "$MinSchema",
    "--max-schema", "$SchemaVersion",
    "--channel", $Channel,
    "--key", $KeyPath
)

if (-not [string]::IsNullOrWhiteSpace($ChangelogPath)) {
    $arguments += @("--changelog", (Resolve-Path -LiteralPath $ChangelogPath).Path)
}

& (Join-Path $staging "workspace.exe") @arguments
if ($LASTEXITCODE -ne 0) { Fail "release packaging failed" }

Write-Host "[6/6] Verify the package with the embedded public key"

& (Join-Path $staging "workspace.exe") release verify --package $packagePath
if ($LASTEXITCODE -ne 0) { Fail "the produced package does not verify" }

$hash = (Get-FileHash -LiteralPath $packagePath -Algorithm SHA256).Hash.ToLowerInvariant()
$hashFile = "$packagePath.sha256"
"$hash  $(Split-Path -Leaf $packagePath)" | Set-Content -LiteralPath $hashFile -Encoding ASCII

$summary = [ordered]@{
    version = $Version
    git_sha = $sha
    channel = $Channel
    schema_version = $SchemaVersion
    min_schema = $MinSchema
    package = [System.IO.Path]::GetFileName($packagePath)
    package_sha256 = $hash
    built_at_unix = $buildTime
    linux_image = $LinuxImage
}

$summary | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $output "release.json") -Encoding UTF8

Write-Host ""
Write-Host "Package: $packagePath"
Write-Host "SHA-256: $hash"
Write-Host "Summary: $(Join-Path $output 'release.json')"
Write-Host ""
Write-Host "First installation on the target machine:"
Write-Host "  powershell -ExecutionPolicy Bypass -File deploy\windows\bootstrap-update.ps1 -Package '$packagePath'"
Write-Host "Subsequent updates:"
Write-Host "  workspace update --check"
Write-Host "  workspace update --package '$packagePath'"
