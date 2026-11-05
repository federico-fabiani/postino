# Builds the per-user MSI from a staged payload with WiX Toolset v4.
# This script never downloads WiX, a certificate or a runtime.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$StageDir,
    [string]$Version,
    [string]$OutputPath,
    [switch]$Sign,
    [string]$TimestampUrl = 'http://timestamp.digicert.com',
    [switch]$DryRun
)

. (Join-Path $PSScriptRoot 'lib\common.ps1')

$repo = Get-PostinoRepoRoot
if (-not $Version) { $Version = Get-PostinoVersion -RepoRoot $repo }
$StageDir = [System.IO.Path]::GetFullPath($StageDir).TrimEnd('\')
if (-not (Test-Path -LiteralPath $StageDir)) { throw "Staged payload not found: $StageDir. Run Build-Package.ps1 first." }
$dist = Join-Path $repo 'dist'
$obj = Join-Path $dist 'obj'
$fragmentPath = Join-Path $obj 'StageFiles.wxs'
if (-not $OutputPath) { $OutputPath = Join-Path $dist "Postino-$Version-windows-x64.msi" }
$OutputPath = [System.IO.Path]::GetFullPath($OutputPath)

$wix = $null
foreach ($name in @('wix.exe', 'wix')) {
    $command = Get-Command $name -ErrorAction SilentlyContinue
    if ($command) { $wix = $command.Source; break }
}
if (-not $wix) {
    foreach ($candidate in @(
        (Join-Path $env:USERPROFILE '.dotnet\tools\wix.exe'),
        'C:\Program Files\WiX Toolset v4\bin\wix.exe',
        'C:\Program Files (x86)\WiX Toolset v4\bin\wix.exe'
    )) {
        if (Test-Path -LiteralPath $candidate) { $wix = $candidate; break }
    }
}

$uiWxs = Join-Path $PSScriptRoot 'wix\installer-ui.wxs'
$wxs = Join-Path $PSScriptRoot 'wix\Postino.wxs'
$arguments = @(
    'build', '-arch', 'x64',
    '-d', "StageDir=$StageDir",
    '-d', "ProductVersion=$Version",
    $wxs, $uiWxs, $fragmentPath,
    '-o', $OutputPath
)

if ($DryRun) {
    $payloadCount = @(Get-ChildItem -LiteralPath $StageDir -File -Recurse | Measure-Object).Count
    $wixText = if ($wix) { $wix } else { 'not installed (MSI cannot be built on this host)' }
    Write-Output "WiX: $wixText"
    Write-Output "Would generate the file list at $fragmentPath ($payloadCount payload files)"
    Write-Output 'Dry run: no MSI was built and dist\obj was not written.'
    Write-Output ("  wix " + ($arguments -join ' '))
    return
}

New-Item -ItemType Directory -Path $obj -Force | Out-Null
New-PostinoStageFragment -StageDir $StageDir -OutputPath $fragmentPath | Out-Null
$payloadCount = (Get-ChildItem -LiteralPath $StageDir -File -Recurse | Measure-Object).Count

if (-not $wix) {
    Write-Output "WiX Toolset v4 (wix.exe) was not found; no MSI was built."
    Write-Output "Source: $wxs"
    Write-Output "Generated file list: $fragmentPath ($payloadCount payload files)"
    Write-Output 'Install WiX v4, then run:'
    Write-Output '  dotnet tool install --global wix'
    Write-Output ("  wix " + ($arguments -join ' '))
    exit 3
}

Write-Output "Building per-user MSI with $wix ..."
& $wix @arguments
if ($LASTEXITCODE -ne 0) {
    throw 'wix build failed. The MSI was not produced.'
}
if (-not (Test-Path -LiteralPath $OutputPath)) { throw "wix build returned success but $OutputPath is missing." }

if ($Sign) {
    try {
        Invoke-PostinoSign -Files @($OutputPath) -TimestampUrl $TimestampUrl
    } catch {
        $reason = $_.Exception.Message
        if (Test-Path -LiteralPath $OutputPath) { Remove-Item -LiteralPath $OutputPath -Force }
        throw "Signing failed, so the unsigned MSI was removed and nothing in dist is signed: $reason"
    }
}
$outer = Update-PostinoOuterManifest -DistRoot $dist -Version $Version
Write-Output "MSI: $OutputPath"
if ($outer) { Write-Output "Checksums: $outer" }
