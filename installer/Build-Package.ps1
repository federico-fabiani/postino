# Builds the portable Postino payload for x86_64 Windows.
# Stages the three release binaries, the runtime DLLs the GNU host needs (none on MSVC, whose
# release uses a static CRT), licences and the icon, then writes SHA256SUMS.txt and a ZIP.
# No installer, no admin and no network.
[CmdletBinding()]
param(
    [ValidateSet('Release')][string]$Configuration = 'Release',
    [ValidateSet('x86_64-pc-windows-gnu', 'x86_64-pc-windows-msvc')][string]$Target = 'x86_64-pc-windows-gnu',
    [string]$StageDir,
    [string]$DistRoot,
    [switch]$SkipBuild,
    [switch]$SkipZip,
    [switch]$Sign,
    [string]$TimestampUrl = 'http://timestamp.digicert.com',
    [switch]$DryRun
)

. (Join-Path $PSScriptRoot 'lib\common.ps1')

$repo = Get-PostinoRepoRoot
$version = Get-PostinoVersion -RepoRoot $repo
if (-not $DistRoot) { $DistRoot = Join-Path $repo 'dist' }
if (-not $StageDir) { $StageDir = Join-Path $DistRoot "Postino-$version-windows-x64" }
$StageDir = Assert-PostinoStagePath -StageDir $StageDir -DistRoot $DistRoot
$zipPath = Join-Path $DistRoot "Postino-$version-windows-x64.zip"

$hostLine = (& rustc -vV | Select-String '^host:').Line
if (-not $hostLine) { throw 'rustc -vV did not report a host triple.' }
$hostTriple = $hostLine.Split(':')[1].Trim()
if ($hostTriple -ne $Target) {
    throw "This package targets $Target, but rustc hosts $hostTriple. Build on a host that matches the target: GNU locally, MSVC on the release runner."
}

# The MSVC release links the CRT statically (`.cargo/config.toml`), so it imports no non-system
# DLL and there is nothing to stage beside the three executables. The GNU payload must carry its
# MinGW runtime DLLs, and that is the only reason this script needs DLL tools at all.
$copyRuntimeDlls = $Target -like '*-gnu'

$targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target' }
if (-not [System.IO.Path]::IsPathRooted($targetRoot)) { $targetRoot = Join-Path $repo $targetRoot }
$releaseDir = Join-Path $targetRoot 'release'
$stampPath = Join-Path $targetRoot 'postino-build-stamp.json'

if ($DryRun) {
    Write-Output "Postino $version portable build (dry run)"
    Write-Output "  target: $Target"
    Write-Output "  stage:  $StageDir"
    Write-Output "  zip:    $zipPath"
    Write-Output "  build:  cargo build --locked --release -p postino-app -p postino-cli -p postino-daemon"
    Write-Output '  payload: postino-app.exe, postino.exe, postino-daemon.exe, required non-system DLLs, licences, icon, SHA256SUMS.txt'
    Write-Output "  provenance: $stampPath; -SkipBuild is refused without a stamp that matches these binaries and a clean source commit"
    Write-Output "  signing: $(if ($Sign) { 'external signtool, SHA-256 + timestamp' } else { 'unsigned' })"
    Write-Output 'No file was written.'
    return
}

if (-not $SkipBuild) {
    if ($copyRuntimeDlls) {
        if (-not (Get-Command dlltool.exe -ErrorAction SilentlyContinue)) {
            throw 'dlltool.exe is missing. Put C:\msys64\mingw64\bin and C:\msys64\usr\bin first on PATH (see AGENTS.md).'
        }
        if (-not (Get-Command make.exe -ErrorAction SilentlyContinue)) {
            Write-Warning 'make.exe is missing; a cold SQLCipher/OpenSSL build will fail. Install MSYS2 make.'
        }
    }
    Write-Output "Building Postino $version for $Target..."
    & cargo build --locked --release -p postino-app -p postino-cli -p postino-daemon
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed.' }
}

$binaries = @('postino-app.exe', 'postino.exe', 'postino-daemon.exe')
$candidates = @($releaseDir, (Join-Path $targetRoot "$Target\release"))
$sourceDir = $null
foreach ($candidate in $candidates) {
    if (Test-Path -LiteralPath (Join-Path $candidate 'postino-app.exe')) { $sourceDir = $candidate; break }
}
if (-not $sourceDir) {
    throw "Release binaries were not found under $releaseDir. Run without -SkipBuild, or build with cargo build --locked --release."
}
foreach ($binary in $binaries) {
    if (-not (Test-Path -LiteralPath (Join-Path $sourceDir $binary))) { throw "Missing release binary: $binary" }
}

# Provenance. A fresh build records a stamp that ties these exact binaries to the source commit;
# -SkipBuild is refused unless that stamp exists and still matches.
if ($SkipBuild) {
    $provenance = Test-PostinoProvenance -RepoRoot $repo -ReleaseDir $sourceDir -Binaries $binaries -StampPath $stampPath
    if (-not $provenance.Ok) {
        throw "Refusing -SkipBuild: $($provenance.Reason). A pre-existing release build is only accepted when $stampPath ties these binaries to a clean source commit; rebuild without -SkipBuild."
    }
} else {
    # Sign the build output before the stamp is written, so the stamp, BUILD-INFO.txt and the
    # staged payload all describe the same bytes. Signing the staged copies afterwards would leave
    # Test-Package.ps1 comparing staged hashes against unsigned ones and failing every signed release.
    if ($Sign) {
        $signFiles = @($binaries | ForEach-Object { Join-Path $sourceDir $_ })
        Invoke-PostinoSign -Files $signFiles -TimestampUrl $TimestampUrl
    }
    $stampedCommit = Get-PostinoGitCommit -RepoRoot $repo
    $stampedDirty = Test-PostinoGitDirty -RepoRoot $repo
    if ($stampedCommit -match '^[0-9a-f]{40}$' -and $stampedDirty -ne $null) {
        Write-PostinoBuildStamp -StampPath $stampPath -Commit $stampedCommit -Target $Target -Version $version `
            -ReleaseDir $sourceDir -Binaries $binaries -Dirty $stampedDirty | Out-Null
        $state = if ($stampedDirty) { 'dirty' } else { 'verified' }
        $provenance = [pscustomobject]@{ Ok = (-not $stampedDirty); Commit = $stampedCommit; Dirty = $stampedDirty; Provenance = $state; Reason = '' }
    } else {
        $provenance = [pscustomobject]@{ Ok = $false; Commit = $stampedCommit; Dirty = $null; Provenance = 'unknown'; Reason = 'git did not report a source commit and a clean tree' }
    }
}
if (-not $provenance.Ok) {
    Write-Warning "Provenance is $($provenance.Provenance): $($provenance.Reason). Test-Package.ps1 will fail the release check for this payload."
}

if (Test-Path -LiteralPath $StageDir) {
    Remove-Item -LiteralPath $StageDir -Recurse -Force
}
New-Item -ItemType Directory -Path $StageDir -Force | Out-Null

foreach ($binary in $binaries) {
    Copy-Item -LiteralPath (Join-Path $sourceDir $binary) -Destination (Join-Path $StageDir $binary) -Force
}

$runtimeDlls = @()
if ($copyRuntimeDlls) {
    $runtimeDlls = @(Copy-PostinoRuntimeDlls -Binaries @($binaries | ForEach-Object { Join-Path $StageDir $_ }) -StageDir $StageDir)
}

$extraFiles = @(
    @{ Source = (Join-Path $repo 'assets\app-icon\postino.ico'); Name = 'postino.ico' },
    @{ Source = (Join-Path $repo 'LICENSE-MIT'); Name = 'LICENSE-MIT' },
    @{ Source = (Join-Path $repo 'LICENSE-APACHE'); Name = 'LICENSE-APACHE' },
    @{ Source = (Join-Path $repo 'ATTRIBUTION.toml'); Name = 'ATTRIBUTION.toml' },
    @{ Source = (Join-Path $repo 'README.md'); Name = 'README.md' },
    @{ Source = (Join-Path $repo 'SECURITY.md'); Name = 'SECURITY.md' }
)
foreach ($item in $extraFiles) {
    if (-not (Test-Path -LiteralPath $item.Source)) { throw "Missing payload file: $($item.Source)" }
    Copy-Item -LiteralPath $item.Source -Destination (Join-Path $StageDir $item.Name) -Force
}

$commit = $provenance.Commit
$dirtyText = if ($provenance.Dirty -eq $null) { 'unknown' } elseif ($provenance.Dirty) { 'yes' } else { 'no' }
$signingText = if ($Sign) { 'external Authenticode certificate, SHA-256 + RFC 3161 timestamp' } else { 'unsigned' }
$dllText = if ($runtimeDlls.Count -eq 0) { 'none beyond Windows system DLLs' } else { ($runtimeDlls -join ', ') }
$binaryHashLines = ($binaries | ForEach-Object { 'Binary ' + $_ + ': ' + (Get-PostinoSha256 (Join-Path $StageDir $_)) }) -join "`n"
$buildInfo = @"
Product: Postino
Version: $version
Commit: $commit
Commit dirty: $dirtyText
Provenance: $($provenance.Provenance)
Target: $Target
Profile: $Configuration
Built (UTC): $([DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ'))
Signing: $signingText
Runtime DLLs: $dllText
Installer: per-user, no admin, no service, no autostart, one on-demand update check
$binaryHashLines
"@
Write-Utf8NoBom -Path (Join-Path $StageDir 'BUILD-INFO.txt') -Text ($buildInfo.TrimEnd() + "`n")

$manifest = Write-PostinoManifest -Directory $StageDir
Write-Output "Staged $([IO.Path]::GetFileName($StageDir)) with $((Get-ChildItem -LiteralPath $StageDir -File | Measure-Object).Count) top-level files."
Write-Output "Manifest: $manifest"

if (-not $SkipZip) {
    if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
    Compress-Archive -Path (Join-Path $StageDir '*') -DestinationPath $zipPath -CompressionLevel Optimal
    $outer = Update-PostinoOuterManifest -DistRoot $DistRoot -Version $version
    Write-Output "ZIP: $zipPath"
    if ($outer) { Write-Output "Checksums: $outer" }
}

Write-Output 'Portable payload complete. Run installer\Test-Package.ps1 -StageDir to verify it.'
