# Fixture tests for the packaging resolvers: build-stamp provenance, PE architecture and the
# toolchain-only DLL allowlist. Synthetic PEs only; nothing is built and no real DLL is copied.
# Writes only inside one unique temp folder, which it removes itself.
[CmdletBinding()]
param()

. (Join-Path $PSScriptRoot '..\lib\common.ps1')
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:failCount = 0
$binaries = @('postino-app.exe', 'postino.exe', 'postino-daemon.exe')

function Test-Fixture {
    param([bool]$Condition, [string]$Message)
    if ($Condition) { Write-Output "PASS $Message" } else { $script:failCount++; Write-Output "FAIL $Message" }
}

function New-FakePe {
    param([string]$Path, [int]$Machine, [string]$Marker)
    $parent = Split-Path -Parent $Path
    if (-not (Test-Path -LiteralPath $parent -PathType Container)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    $markerBytes = [System.Text.Encoding]::UTF8.GetBytes($Marker)
    $peOffset = 0x80
    $bytes = New-Object byte[] (512 + $markerBytes.Length)
    $bytes[0] = 0x4D
    $bytes[1] = 0x5A
    [System.Array]::Copy([BitConverter]::GetBytes([uint32]$peOffset), 0, $bytes, 0x3C, 4)
    $bytes[$peOffset] = 0x50
    $bytes[$peOffset + 1] = 0x45
    [System.Array]::Copy([BitConverter]::GetBytes([uint16]$Machine), 0, $bytes, $peOffset + 4, 2)
    [System.Array]::Copy($markerBytes, 0, $bytes, 512, $markerBytes.Length)
    [System.IO.File]::WriteAllBytes($Path, $bytes)
}

function New-FakeStage {
    param([string]$Dir, [string]$Provenance, [string]$Dirty, [string]$Commit)
    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add('Product: Postino')
    $lines.Add("Commit: $Commit")
    $lines.Add("Commit dirty: $Dirty")
    $lines.Add("Provenance: $Provenance")
    foreach ($name in $binaries) {
        New-FakePe -Path (Join-Path $Dir $name) -Machine 0x8664 -Marker $name
        $lines.Add('Binary ' + $name + ': ' + (Get-PostinoSha256 (Join-Path $Dir $name)))
    }
    [System.IO.File]::WriteAllText((Join-Path $Dir 'BUILD-INFO.txt'), (($lines -join "`n") + "`n"))
}

function Invoke-Catching {
    param([scriptblock]$Action)
    try { & $Action | Out-Null; return $false } catch { return $true }
}

$temp = Join-Path ([System.IO.Path]::GetTempPath()) ('postino-p12e-fixtures-' + [Guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Path $temp -Force | Out-Null

# A throwaway repository makes the clean/dirty provenance cases deterministic.
$fixtureRepo = Join-Path $temp 'repo'
New-Item -ItemType Directory -Path $fixtureRepo -Force | Out-Null
& git -C $fixtureRepo init -q 2>&1 | Out-Null
Set-Content -LiteralPath (Join-Path $fixtureRepo 'seed.txt') -Value 'seed'
& git -C $fixtureRepo add 'seed.txt' 2>&1 | Out-Null
& git -C $fixtureRepo -c user.name=fixture -c user.email=fixture@example.com -c commit.gpgsign=false commit -q -m 'fixture seed' 2>&1 | Out-Null
$head = Get-PostinoGitCommit -RepoRoot $fixtureRepo

try {
    # 1. Build-stamp provenance.
    $release = Join-Path $temp 'release'
    foreach ($name in $binaries) { New-FakePe -Path (Join-Path $release $name) -Machine 0x8664 -Marker $name }
    $stamp = Join-Path $temp 'stamp.json'

    $result = Test-PostinoProvenance -RepoRoot $fixtureRepo -ReleaseDir $release -Binaries $binaries -StampPath $stamp
    Test-Fixture (-not $result.Ok) 'a missing build stamp is refused'
    Test-Fixture ($result.Provenance -eq 'unknown') 'a missing stamp reports provenance unknown'

    if ($head -match '^[0-9a-f]{40}$') {
        Write-PostinoBuildStamp -StampPath $stamp -Commit $head -Target 'x86_64-pc-windows-gnu' -Version '0.0.0' `
            -ReleaseDir $release -Binaries $binaries -Dirty $false | Out-Null
        $result = Test-PostinoProvenance -RepoRoot $fixtureRepo -ReleaseDir $release -Binaries $binaries -StampPath $stamp
        Test-Fixture ($result.Ok -and $result.Provenance -eq 'verified') 'a matching stamp verifies the release binaries'

        New-FakePe -Path (Join-Path $release 'postino.exe') -Machine 0x8664 -Marker 'rebuilt'
        $result = Test-PostinoProvenance -RepoRoot $fixtureRepo -ReleaseDir $release -Binaries $binaries -StampPath $stamp
        Test-Fixture (-not $result.Ok) 'a rebuilt binary with a different hash is refused'

        New-FakePe -Path (Join-Path $release 'postino.exe') -Machine 0x8664 -Marker 'postino.exe'
        Write-PostinoBuildStamp -StampPath $stamp -Commit $head -Target 'x86_64-pc-windows-gnu' -Version '0.0.0' `
            -ReleaseDir $release -Binaries $binaries -Dirty $true | Out-Null
        $result = Test-PostinoProvenance -RepoRoot $fixtureRepo -ReleaseDir $release -Binaries $binaries -StampPath $stamp
        Test-Fixture ((-not $result.Ok) -and $result.Provenance -eq 'dirty') 'a stamp from a dirty tree is refused'

        Add-Content -LiteralPath (Join-Path $fixtureRepo 'seed.txt') -Value 'dirty-now'
        Write-PostinoBuildStamp -StampPath $stamp -Commit $head -Target 'x86_64-pc-windows-gnu' -Version '0.0.0' `
            -ReleaseDir $release -Binaries $binaries -Dirty $false | Out-Null
        $result = Test-PostinoProvenance -RepoRoot $fixtureRepo -ReleaseDir $release -Binaries $binaries -StampPath $stamp
        Test-Fixture ((-not $result.Ok) -and $result.Dirty -eq $true) 'a clean stamp over a now-dirty tree is refused'

        Write-PostinoBuildStamp -StampPath $stamp -Commit ('0' * 40) -Target 'x86_64-pc-windows-gnu' -Version '0.0.0' `
            -ReleaseDir $release -Binaries $binaries -Dirty $false | Out-Null
        $result = Test-PostinoProvenance -RepoRoot $fixtureRepo -ReleaseDir $release -Binaries $binaries -StampPath $stamp
        Test-Fixture (-not $result.Ok) 'a stamp naming a commit outside this repository is refused'

        $verifiedStage = Join-Path $temp 'stage-verified'
        New-FakeStage -Dir $verifiedStage -Provenance 'verified' -Dirty 'no' -Commit $head
        $problems = @(Test-PostinoReleaseProvenance -RepoRoot $fixtureRepo -StageDir $verifiedStage)
        Test-Fixture ($problems.Count -eq 0) 'BUILD-INFO with a real clean commit and matching hashes passes'
    } else {
        Write-Output 'INFO git is unavailable; the commit-bound provenance cases were skipped'
    }

    # 2. BUILD-INFO release gate refusals.
    $unknownStage = Join-Path $temp 'stage-unknown'
    New-FakeStage -Dir $unknownStage -Provenance 'unknown' -Dirty 'no' -Commit 'unknown'
    Test-Fixture (@(Test-PostinoReleaseProvenance -RepoRoot $fixtureRepo -StageDir $unknownStage).Count -gt 0) 'BUILD-INFO with unknown provenance fails the release check'

    $dirtyStage = Join-Path $temp 'stage-dirty'
    New-FakeStage -Dir $dirtyStage -Provenance 'verified' -Dirty 'yes' -Commit $head
    Test-Fixture (@(Test-PostinoReleaseProvenance -RepoRoot $fixtureRepo -StageDir $dirtyStage).Count -gt 0) 'BUILD-INFO from a dirty tree fails the release check'

    $hashStage = Join-Path $temp 'stage-hash'
    New-FakeStage -Dir $hashStage -Provenance 'verified' -Dirty 'no' -Commit $head
    New-FakePe -Path (Join-Path $hashStage 'postino-app.exe') -Machine 0x8664 -Marker 'tampered-after-staging'
    Test-Fixture (@(Test-PostinoReleaseProvenance -RepoRoot $fixtureRepo -StageDir $hashStage).Count -gt 0) 'BUILD-INFO whose binary hash no longer matches fails the release check'

    # 3. Toolchain-only DLL resolution and PE architecture.
    $toolchain = Join-Path $temp 'mingw64\bin'
    New-FakePe -Path (Join-Path $toolchain 'libgnufixture-1.dll') -Machine 0x8664 -Marker 'gnu-bundle'
    $resolved = Resolve-PostinoRuntimeDll -Name 'libgnufixture-1.dll' -AllowedRoots @($toolchain)
    Test-Fixture ($resolved -and $resolved.StartsWith($toolchain, [System.StringComparison]::OrdinalIgnoreCase)) 'an x86_64 GNU bundle DLL resolves to its full toolchain path'

    New-FakePe -Path (Join-Path $toolchain 'libforeign-1.dll') -Machine 0x014c -Marker 'i386'
    Test-Fixture (Invoke-Catching { Resolve-PostinoRuntimeDll -Name 'libforeign-1.dll' -AllowedRoots @($toolchain) }) 'a 32-bit DLL in the toolchain is refused'

    New-FakePe -Path (Join-Path $toolchain 'libarm64-1.dll') -Machine 0xAA64 -Marker 'arm64'
    Test-Fixture (Invoke-Catching { Resolve-PostinoRuntimeDll -Name 'libarm64-1.dll' -AllowedRoots @($toolchain) }) 'a foreign-architecture DLL in the toolchain is refused'

    $decoy = Join-Path $temp 'path-decoy'
    New-FakePe -Path (Join-Path $decoy 'libdecoy-1.dll') -Machine 0x8664 -Marker 'decoy'
    Test-Fixture ($null -eq (Resolve-PostinoRuntimeDll -Name 'libdecoy-1.dll' -AllowedRoots @($toolchain))) 'a same-named DLL outside the allowlist is never resolved'

    Test-Fixture (Invoke-Catching { Resolve-PostinoRuntimeDll -Name '..\libgnufixture-1.dll' -AllowedRoots @($toolchain) }) 'an import name containing a path separator is refused'
    Test-Fixture (Invoke-Catching { Resolve-PostinoRuntimeDll -Name 'kernel32.dll' -AllowedRoots @((Join-Path $env:SystemRoot 'System32')) }) 'a Windows system directory is refused as a DLL source'

    Test-Fixture (Test-PostinoSystemDll -Name 'kernel32.dll') 'a system DLL is still recognised by full System32 path'
    Test-Fixture (-not (Test-PostinoSystemDll -Name 'libgnufixture-1.dll')) 'a GNU runtime DLL is not mistaken for a system DLL'
} finally {
    if (Test-Path -LiteralPath $temp) { Remove-Item -LiteralPath $temp -Recurse -Force }
}

if ($script:failCount -gt 0) { exit 1 }
exit 0
