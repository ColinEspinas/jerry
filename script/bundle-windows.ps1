# Assembles dist/Jerry-windows.zip: the release binary renamed from the crate's internal
# `app.exe` to the product name `Jerry.exe` (see script/bundle-mac.sh's own comment on the same
# rename for macOS/Linux - "jerry"/"app" is a workspace detail, "Jerry" is what this repo ships).
#
# `Compress-Archive`, not a `zip` CLI: windows-latest GitHub Actions runners have no `zip` on
# PATH (only the Linux/macOS runners do - see .github/workflows/release.yml's own comment next to
# its existing `Compress-Archive` step), but every Windows install ships this cmdlet.
#
# The icon/version resource embedded in app.exe itself comes from crates/app/build.rs
# (GitHub issue #447) - this script only stages and archives the already-built binary.

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $RepoRoot

$AppName = "Jerry"
$BinName = "app.exe"
$ExecutableName = "Jerry.exe"
$DistDir = Join-Path $RepoRoot "dist"
$ArchivePath = Join-Path $DistDir "Jerry-windows.zip"

# Single source of truth for the release version, matching how
# .claude/hooks/check-release-version.sh reads it: the `version` under Cargo.toml's
# [workspace.package] table. Logged only, not embedded by this script (crates/app/build.rs
# already burns it into the .exe's own FileVersion resource).
$CargoToml = Get-Content (Join-Path $RepoRoot "Cargo.toml") -Raw
if ($CargoToml -notmatch '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    Write-Error "could not read [workspace.package] version from Cargo.toml"
    exit 1
}
$Version = $Matches[1]
Write-Host "==> Bundling $AppName $Version for Windows"

$ReleaseBin = Join-Path $RepoRoot "target\release\$BinName"
if ((Test-Path $ReleaseBin) -and $env:SKIP_BUILD) {
    Write-Host "==> SKIP_BUILD set and $ReleaseBin exists - reusing it"
} elseif (Test-Path $ReleaseBin) {
    Write-Host "==> $ReleaseBin already exists - reusing it (set SKIP_BUILD=1 to make this explicit, or remove it to force a rebuild)"
} else {
    Write-Host "==> Building $ReleaseBin"
    cargo build --release -p app
    if ($LASTEXITCODE -ne 0) {
        Write-Error "cargo build --release -p app failed with exit code $LASTEXITCODE"
        exit $LASTEXITCODE
    }
}

if (-not (Test-Path $ReleaseBin)) {
    Write-Error "$ReleaseBin not found after build step"
    exit 1
}

Write-Host "==> Staging $ExecutableName"
New-Item -ItemType Directory -Force -Path $DistDir | Out-Null
$StagedExe = Join-Path $DistDir $ExecutableName
Copy-Item -Path $ReleaseBin -Destination $StagedExe -Force

Write-Host "==> Creating $ArchivePath"
if (Test-Path $ArchivePath) {
    Remove-Item $ArchivePath -Force
}
Compress-Archive -Path $StagedExe -DestinationPath $ArchivePath

Remove-Item $StagedExe -Force

Write-Host "==> Done"
Write-Host "    $ArchivePath"
