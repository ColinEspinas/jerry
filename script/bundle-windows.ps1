# Assembles dist/Jerry-windows.zip: the release binary renamed from the crate's internal
# `jerry-app.exe` to the product name `Jerry.exe` (see script/bundle-mac.sh's own comment on the same
# rename for macOS/Linux - "jerry"/"app" is a workspace detail, "Jerry" is what this repo ships).
#
# `Compress-Archive`, not a `zip` CLI: windows-latest GitHub Actions runners have no `zip` on
# PATH (only the Linux/macOS runners do - see .github/workflows/release.yml's own comment next to
# its existing `Compress-Archive` step), but every Windows install ships this cmdlet.
#
# The icon/version resource embedded in jerry-app.exe itself comes from crates/jerry-app/build.rs
# (GitHub issue #447) - this script only stages and archives the already-built binary.

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $RepoRoot

$AppName = "Jerry"
$BinName = "jerry-app.exe"
$CliName = "jerry.exe"
$ExecutableName = "Jerry.exe"
$DistDir = Join-Path $RepoRoot "dist"
$ArchivePath = Join-Path $DistDir "Jerry-windows.zip"

# Single source of truth for the release version, matching how
# .claude/hooks/check-release-version.sh reads it: the `version` under Cargo.toml's
# [workspace.package] table. Logged only, not embedded by this script (crates/jerry-app/build.rs
# already burns it into the .exe's own FileVersion resource).
$CargoToml = Get-Content (Join-Path $RepoRoot "Cargo.toml") -Raw
if ($CargoToml -notmatch '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    Write-Error "could not read [workspace.package] version from Cargo.toml"
    exit 1
}
$Version = $Matches[1]
Write-Host "==> Bundling $AppName $Version for Windows"

$ReleaseBin = Join-Path $RepoRoot "target\release\$BinName"
$ReleaseCli = Join-Path $RepoRoot "target\release\$CliName"
if ((Test-Path $ReleaseBin) -and $env:SKIP_BUILD) {
    Write-Host "==> SKIP_BUILD set and $ReleaseBin exists - reusing it"
} elseif (Test-Path $ReleaseBin) {
    Write-Host "==> $ReleaseBin already exists - reusing it (set SKIP_BUILD=1 to make this explicit, or remove it to force a rebuild)"
} else {
    Write-Host "==> Building $ReleaseBin"
    cargo build --release -p jerry-app -p jerry-cli
    if ($LASTEXITCODE -ne 0) {
        Write-Error "cargo build --release -p jerry-app -p jerry-cli failed with exit code $LASTEXITCODE"
        exit $LASTEXITCODE
    }
}

if (-not (Test-Path $ReleaseBin)) {
    Write-Error "$ReleaseBin not found after build step"
    exit 1
}
if (-not (Test-Path $ReleaseCli)) {
    Write-Error "$ReleaseCli not found after build step"
    exit 1
}

# A case-insensitive filesystem cannot hold Jerry.exe and jerry.exe side by side, so the
# `jerry` command lives under bin (decisions section 17), where jerry-app looks for it.
Write-Host "==> Staging $ExecutableName and bin\$CliName"
$StageDir = Join-Path $DistDir "Jerry-windows"
if (Test-Path $StageDir) {
    Remove-Item $StageDir -Recurse -Force
}
New-Item -ItemType Directory -Force -Path (Join-Path $StageDir "bin") | Out-Null
Copy-Item -Path $ReleaseBin -Destination (Join-Path $StageDir $ExecutableName) -Force
Copy-Item -Path $ReleaseCli -Destination (Join-Path $StageDir "bin\$CliName") -Force

Write-Host "==> Creating $ArchivePath"
if (Test-Path $ArchivePath) {
    Remove-Item $ArchivePath -Force
}
Compress-Archive -Path (Join-Path $StageDir "*") -DestinationPath $ArchivePath

Remove-Item $StageDir -Recurse -Force

Write-Host "==> Done"
Write-Host "    $ArchivePath"
