# Shared helpers for the Postino packaging scripts. No secrets, no network access.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-PostinoRepoRoot {
    return (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
}

function Get-PostinoVersion {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)
    $match = Select-String -LiteralPath (Join-Path $RepoRoot 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $match) { throw 'Cargo.toml has no workspace.package version.' }
    return $match.Matches[0].Groups[1].Value
}

function Write-Utf8NoBom {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Text
    )
    $encoding = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $Text, $encoding)
}

function Get-PostinoSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $stream = [System.IO.File]::OpenRead($Path)
        try {
            $bytes = $sha.ComputeHash($stream)
        } finally {
            $stream.Dispose()
        }
    } finally {
        $sha.Dispose()
    }
    return (($bytes | ForEach-Object { $_.ToString('x2') }) -join '')
}

function Write-PostinoManifest {
    param([Parameter(Mandatory = $true)][string]$Directory)
    $manifestPath = Join-Path $Directory 'SHA256SUMS.txt'
    if (Test-Path -LiteralPath $manifestPath) { Remove-Item -LiteralPath $manifestPath -Force }
    $files = Get-ChildItem -LiteralPath $Directory -File -Recurse | Sort-Object FullName
    $lines = New-Object System.Collections.Generic.List[string]
    foreach ($file in $files) {
        $relative = $file.FullName.Substring($Directory.Length).TrimStart('\')
        if ($relative -eq 'SHA256SUMS.txt') { continue }
        $lines.Add((Get-PostinoSha256 $file.FullName) + '  ' + $relative)
    }
    Write-Utf8NoBom -Path $manifestPath -Text (($lines -join "`n") + "`n")
    return $manifestPath
}

function Update-PostinoOuterManifest {
    param(
        [Parameter(Mandatory = $true)][string]$DistRoot,
        [Parameter(Mandatory = $true)][string]$Version
    )
    $names = @(
        "Postino-$Version-windows-x64.zip",
        "Postino-$Version-windows-x64.msi"
    )
    $lines = New-Object System.Collections.Generic.List[string]
    foreach ($name in $names) {
        $path = Join-Path $DistRoot $name
        if (Test-Path -LiteralPath $path) {
            $lines.Add((Get-PostinoSha256 $path) + '  ' + $name)
        }
    }
    if ($lines.Count -eq 0) { return $null }
    $manifest = Join-Path $DistRoot 'SHA256SUMS.txt'
    Write-Utf8NoBom -Path $manifest -Text (($lines -join "`n") + "`n")
    return $manifest
}

function Get-PostinoGitCommit {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)
    $commit = (& git -C $RepoRoot rev-parse HEAD 2>$null)
    if ($LASTEXITCODE -ne 0 -or -not $commit) { return 'unknown' }
    return $commit.Trim()
}

function Test-PostinoCommitExists {
    param(
        [Parameter(Mandatory = $true)][string]$RepoRoot,
        [Parameter(Mandatory = $true)][string]$Commit
    )
    if ($Commit -notmatch '^[0-9a-f]{40}$') { return $false }
    $spec = $Commit + '^{commit}'
    & git -C $RepoRoot rev-parse --verify --quiet $spec 2>$null | Out-Null
    return ($LASTEXITCODE -eq 0)
}

function Test-PostinoGitDirty {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)
    $status = (& git -C $RepoRoot status --porcelain --untracked-files=no 2>$null)
    if ($LASTEXITCODE -ne 0) { return $null }
    return [bool]$status
}

function Get-PostinoObjdump {
    param([switch]$AllowMissing)
    $command = Get-Command objdump.exe -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    $fallback = 'C:\msys64\mingw64\bin\objdump.exe'
    if (Test-Path -LiteralPath $fallback) { return $fallback }
    if ($AllowMissing) { return $null }
    throw 'objdump.exe was not found. Put C:\msys64\mingw64\bin first on PATH (MSYS2 MinGW) so the GNU runtime DLLs can be verified.'
}

function Get-PostinoImportedDlls {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Objdump
    )
    $output = & $Objdump -p $Path
    if ($LASTEXITCODE -ne 0) { throw "objdump could not read $Path." }
    $names = New-Object System.Collections.Generic.List[string]
    foreach ($line in $output) {
        if ($line -match 'DLL Name:\s*(\S+)') {
            $name = $Matches[1]
            if ($names -notcontains $name) { $names.Add($name) }
        }
    }
    return $names
}

function Test-PostinoSystemDll {
    param([Parameter(Mandatory = $true)][string]$Name)
    if ($Name -match '[\\/]') { return $false }
    if ($Name -match '^(api-ms-win-|ext-ms-)') { return $true }
    foreach ($directory in @('System32', 'SysWOW64')) {
        $full = Join-Path (Join-Path $env:SystemRoot $directory) $Name
        if (Test-Path -LiteralPath $full -PathType Leaf) { return $true }
    }
    return $false
}

function Get-PostinoPeMachine {
    param([Parameter(Mandatory = $true)][string]$Path)
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        if ($stream.Length -lt 0x40) { throw "$([IO.Path]::GetFileName($Path)) is too small to be a PE file." }
        $reader = New-Object System.IO.BinaryReader($stream)
        if ($reader.ReadUInt16() -ne 0x5A4D) { throw "$([IO.Path]::GetFileName($Path)) has no MZ header." }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
        if ($peOffset -lt 0x40 -or ($peOffset + 6) -gt $stream.Length) { throw "$([IO.Path]::GetFileName($Path)) has an invalid PE offset." }
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) { throw "$([IO.Path]::GetFileName($Path)) has no PE signature." }
        return [int]$reader.ReadUInt16()
    } finally {
        $stream.Dispose()
    }
}

function Get-PostinoMachineName {
    param([Parameter(Mandatory = $true)][int]$Machine)
    switch ($Machine) {
        0x8664 { return 'x86_64' }
        0x014c { return 'x86 (32-bit)' }
        0xAA64 { return 'ARM64' }
        0x01c4 { return 'ARM' }
        default { return ('machine 0x{0:x4}' -f $Machine) }
    }
}

function Get-PostinoToolchainAllowlist {
    $roots = New-Object System.Collections.Generic.List[string]
    foreach ($candidate in @(
        'C:\msys64\mingw64\bin',
        'C:\msys64\ucrt64\bin',
        'C:\msys64\clang64\bin',
        'C:\msys64\usr\bin',
        'C:\mingw64\bin'
    )) {
        if (Test-Path -LiteralPath $candidate -PathType Container) { $roots.Add($candidate) }
    }
    if ($env:POSTINO_MINGW_BIN) { $roots.Add($env:POSTINO_MINGW_BIN) }
    return $roots.ToArray()
}

# The MinGW prefix is explicit: POSTINO_MINGW_BIN, the directory that owns objdump.exe, or a known
# MSYS2 prefix. PATH is never searched for a DLL, so a same-named file elsewhere cannot be staged.
function Get-PostinoToolchainDir {
    param([string[]]$AllowedRoots)
    if (-not $AllowedRoots) { $AllowedRoots = Get-PostinoToolchainAllowlist }
    $candidates = New-Object System.Collections.Generic.List[string]
    if ($env:POSTINO_MINGW_BIN) { $candidates.Add($env:POSTINO_MINGW_BIN) }
    $objdump = Get-PostinoObjdump -AllowMissing
    if ($objdump) { $candidates.Add((Split-Path -Parent $objdump)) }
    foreach ($root in $AllowedRoots) { $candidates.Add($root) }
    $systemRoot = ([System.IO.Path]::GetFullPath($env:SystemRoot)).TrimEnd('\')
    foreach ($candidate in $candidates) {
        if (-not $candidate) { continue }
        $full = ([System.IO.Path]::GetFullPath($candidate)).TrimEnd('\')
        if (-not (Test-Path -LiteralPath $full -PathType Container)) { continue }
        if ($full.StartsWith($systemRoot + '\', [System.StringComparison]::OrdinalIgnoreCase)) { continue }
        if (-not (Test-Path -LiteralPath (Join-Path $full 'objdump.exe') -PathType Leaf)) { continue }
        return $full
    }
    throw 'No MinGW toolchain directory was found. Set POSTINO_MINGW_BIN or put C:\msys64\mingw64\bin first on PATH (MSYS2 MinGW).'
}

# Resolves one imported DLL name inside an explicit allowlist, by full path, and refuses a
# non-x86_64 image or any candidate inside a Windows system directory.
function Resolve-PostinoRuntimeDll {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string[]]$AllowedRoots
    )
    if ($Name -match '[\\/]') { throw "Imported DLL name '$Name' contains a path separator; refusing to resolve it." }
    if ($Name -notmatch '^[A-Za-z0-9._+-]+\.dll$') { throw "Imported DLL name '$Name' is not a plain DLL file name." }
    $systemRoot = ([System.IO.Path]::GetFullPath($env:SystemRoot)).TrimEnd('\')
    foreach ($root in $AllowedRoots) {
        if (-not $root) { continue }
        $rootFull = ([System.IO.Path]::GetFullPath($root)).TrimEnd('\')
        if ($rootFull.StartsWith($systemRoot + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to stage '$Name' from $rootFull`: Windows system directories are matched by full path only."
        }
        $candidate = [System.IO.Path]::GetFullPath((Join-Path $rootFull $Name))
        if (-not $candidate.StartsWith($rootFull + '\', [System.StringComparison]::OrdinalIgnoreCase)) { continue }
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) { continue }
        $machine = Get-PostinoPeMachine -Path $candidate
        if ($machine -ne 0x8664) {
            throw "Refusing '$Name' from $rootFull`: it is $(Get-PostinoMachineName -Machine $machine), not x86_64."
        }
        return $candidate
    }
    return $null
}

function Copy-PostinoRuntimeDlls {
    param(
        [Parameter(Mandatory = $true)][string[]]$Binaries,
        [Parameter(Mandatory = $true)][string]$StageDir,
        [string]$ToolchainDir
    )
    $objdump = Get-PostinoObjdump
    if (-not $ToolchainDir) { $ToolchainDir = Get-PostinoToolchainDir }
    $toolchain = ([System.IO.Path]::GetFullPath($ToolchainDir)).TrimEnd('\')
    if (-not (Test-Path -LiteralPath $toolchain -PathType Container)) {
        throw "The MinGW toolchain directory does not exist: $toolchain"
    }
    $queue = New-Object System.Collections.Generic.Queue[string]
    foreach ($binary in $Binaries) { $queue.Enqueue($binary) }
    $seen = @{}
    $copied = New-Object System.Collections.Generic.List[string]

    while ($queue.Count -gt 0) {
        $current = $queue.Dequeue()
        if ($seen.ContainsKey($current)) { continue }
        $seen[$current] = $true
        foreach ($name in (Get-PostinoImportedDlls -Path $current -Objdump $objdump)) {
            if (Test-PostinoSystemDll -Name $name) { continue }
            if ($copied -contains $name) { continue }
            $source = Resolve-PostinoRuntimeDll -Name $name -AllowedRoots @($toolchain)
            if (-not $source) {
                throw "Missing GNU runtime DLL '$name' imported by $([IO.Path]::GetFileName($current)). Expected it in the MinGW toolchain directory $toolchain; PATH is not searched."
            }
            $destination = Join-Path $StageDir $name
            Copy-Item -LiteralPath $source -Destination $destination -Force
            $copied.Add($name)
            $queue.Enqueue($destination)
        }
    }
    return $copied
}

function Get-PostinoSigntool {
    $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    foreach ($kit in @('C:\Program Files (x86)\Windows Kits\10\bin', 'C:\Program Files\Windows Kits\10\bin')) {
        if (Test-Path -LiteralPath $kit) {
            $found = Get-ChildItem -LiteralPath $kit -Filter signtool.exe -Recurse -ErrorAction SilentlyContinue |
                Where-Object { $_.FullName -match '\\x64\\signtool\.exe$' } |
                Sort-Object FullName -Descending | Select-Object -First 1
            if ($found) { return $found.FullName }
        }
    }
    throw 'signtool.exe was not found. Install the Windows SDK or put signtool.exe on PATH.'
}

function Invoke-PostinoSign {
    param(
        [Parameter(Mandatory = $true)][string[]]$Files,
        [Parameter(Mandatory = $true)][string]$TimestampUrl
    )
    $thumbprint = $env:POSTINO_SIGN_CERT_SHA1
    if (-not $thumbprint -or $thumbprint -notmatch '^[0-9A-Fa-f]{40}$') {
        throw 'Signing requested but POSTINO_SIGN_CERT_SHA1 is not a 40-hex certificate thumbprint. No certificate is stored in this repository.'
    }
    $signtool = Get-PostinoSigntool
    foreach ($file in $Files) {
        if (-not (Test-Path -LiteralPath $file)) { throw "Cannot sign missing file: $file" }
        & $signtool sign /sha1 $thumbprint /fd sha256 /tr $TimestampUrl /td sha256 /v $file
        if ($LASTEXITCODE -ne 0) { throw "signtool failed for $([IO.Path]::GetFileName($file))." }
    }
}

function Assert-PostinoStagePath {
    param(
        [Parameter(Mandatory = $true)][string]$StageDir,
        [Parameter(Mandatory = $true)][string]$DistRoot
    )
    $stage = [System.IO.Path]::GetFullPath($StageDir).TrimEnd('\')
    $dist = [System.IO.Path]::GetFullPath($DistRoot).TrimEnd('\')
    if (-not $stage.StartsWith($dist + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "The staging directory must be a child of $dist. Refusing to clean '$stage'."
    }
    if ($stage -notmatch '\\Postino-[^\\]+-windows-x64$') {
        throw "The staging directory name must match Postino-<version>-windows-x64. Refusing to clean '$stage'."
    }
    return $stage
}

function Get-PostinoStableHex {
    param([Parameter(Mandatory = $true)][string]$Text)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = $sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($Text))
        return (($bytes[0..15] | ForEach-Object { $_.ToString('x2') }) -join '')
    } finally {
        $sha.Dispose()
    }
}

function Get-PostinoStableGuid {
    param([Parameter(Mandatory = $true)][string]$Text)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = $sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($Text))
    } finally {
        $sha.Dispose()
    }
    $guid = New-Object byte[] 16
    [Array]::Copy($bytes, $guid, 16)
    $guid[6] = [byte](($guid[6] -band 0x0f) -bor 0x50)
    $guid[8] = [byte](($guid[8] -band 0x3f) -bor 0x80)
    $hex = ($guid | ForEach-Object { $_.ToString('x2') }) -join ''
    return '{' + $hex.Substring(0, 8) + '-' + $hex.Substring(8, 4) + '-' + $hex.Substring(12, 4) + '-' + $hex.Substring(16, 4) + '-' + $hex.Substring(20, 12) + '}'
}

function New-PostinoStageFragment {
    param(
        [Parameter(Mandatory = $true)][string]$StageDir,
        [Parameter(Mandatory = $true)][string]$OutputPath
    )
    $files = Get-ChildItem -LiteralPath $StageDir -File -Recurse | Sort-Object FullName
    if ($files.Count -eq 0) { throw "The staged payload is empty: $StageDir" }
    $builder = New-Object System.Text.StringBuilder
    [void]$builder.AppendLine('<?xml version="1.0" encoding="utf-8"?>')
    [void]$builder.AppendLine('<!-- Generated by installer/Build-Msi.ps1 from the staged payload. Do not edit. -->')
    [void]$builder.AppendLine('<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">')
    [void]$builder.AppendLine('  <Fragment>')
    [void]$builder.AppendLine('    <ComponentGroup Id="ProductFiles" Directory="INSTALLFOLDER">')
    foreach ($file in $files) {
        $relative = $file.FullName.Substring($StageDir.Length).TrimStart('\')
        if ($relative -match '\.(pdb|zip|msi)$') { continue }
        $key = $relative.ToLowerInvariant()
        $id = Get-PostinoStableHex -Text $key
        $guid = Get-PostinoStableGuid -Text $key
        $source = '$(var.StageDir)\' + $relative.Replace('/', '\')
        $escaped = [System.Security.SecurityElement]::Escape($source)
        [void]$builder.AppendLine("      <Component Id=`"cmp_$id`" Guid=`"$guid`">")
        [void]$builder.AppendLine("        <File Id=`"fil_$id`" Source=`"$escaped`" KeyPath=`"yes`" />")
        [void]$builder.AppendLine('      </Component>')
    }
    [void]$builder.AppendLine('    </ComponentGroup>')
    [void]$builder.AppendLine('  </Fragment>')
    [void]$builder.AppendLine('</Wix>')
    Write-Utf8NoBom -Path $OutputPath -Text $builder.ToString()
    return $OutputPath
}

function Write-PostinoBuildStamp {
    param(
        [Parameter(Mandatory = $true)][string]$StampPath,
        [Parameter(Mandatory = $true)][string]$Commit,
        [Parameter(Mandatory = $true)][string]$Target,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$ReleaseDir,
        [Parameter(Mandatory = $true)][string[]]$Binaries,
        [Parameter(Mandatory = $true)][bool]$Dirty
    )
    $hashes = [ordered]@{}
    foreach ($name in $Binaries) {
        $path = Join-Path $ReleaseDir $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Cannot stamp a missing binary: $name" }
        $hashes[$name] = Get-PostinoSha256 $path
    }
    $stamp = [ordered]@{
        product     = 'Postino'
        version     = $Version
        commit      = $Commit
        dirty       = $Dirty
        target      = $Target
        written_utc = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ')
        binaries    = $hashes
    }
    Write-Utf8NoBom -Path $StampPath -Text (($stamp | ConvertTo-Json -Depth 4) + "`n")
    return $StampPath
}

function Get-PostinoJsonField {
    param(
        [Parameter(Mandatory = $true)]$Object,
        [Parameter(Mandatory = $true)][string]$Name
    )
    $property = $Object.PSObject.Properties[$Name]
    if (-not $property) { return $null }
    return $property.Value
}

# A pre-existing release build is only accepted when a stamp ties it to an explicit source commit and
# to the exact binaries. Anything else is refused, never silently labelled with HEAD.
function Test-PostinoProvenance {
    param(
        [Parameter(Mandatory = $true)][string]$RepoRoot,
        [Parameter(Mandatory = $true)][string]$ReleaseDir,
        [Parameter(Mandatory = $true)][string[]]$Binaries,
        [Parameter(Mandatory = $true)][string]$StampPath
    )
    $result = [pscustomobject]@{ Ok = $false; Commit = ''; Dirty = $null; Provenance = 'unknown'; Reason = '' }
    if (-not (Test-Path -LiteralPath $StampPath -PathType Leaf)) {
        $result.Reason = "no build stamp at $StampPath; build without -SkipBuild or pass a stamped release directory"
        return $result
    }
    try {
        $stamp = [System.IO.File]::ReadAllText($StampPath) | ConvertFrom-Json
    } catch {
        $result.Reason = 'the build stamp is not valid JSON'
        return $result
    }
    $commit = [string](Get-PostinoJsonField -Object $stamp -Name 'commit')
    $result.Commit = $commit
    if ($commit -notmatch '^[0-9a-f]{40}$') {
        $result.Reason = 'the build stamp does not record a full 40-hex source commit'
        return $result
    }
    if (-not (Test-PostinoCommitExists -RepoRoot $RepoRoot -Commit $commit)) {
        $result.Reason = "the stamped commit $($commit.Substring(0, 8)) does not exist in this repository"
        return $result
    }
    $stampDirty = Get-PostinoJsonField -Object $stamp -Name 'dirty'
    $hashes = Get-PostinoJsonField -Object $stamp -Name 'binaries'
    if ($null -eq $hashes) {
        $result.Reason = 'the build stamp records no binary hashes'
        return $result
    }
    foreach ($name in $Binaries) {
        $path = Join-Path $ReleaseDir $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            $result.Reason = "the stamped binary is missing: $name"
            return $result
        }
        $recorded = [string](Get-PostinoJsonField -Object $hashes -Name $name)
        if (-not $recorded) {
            $result.Reason = "the build stamp records no hash for $name"
            return $result
        }
        if ((Get-PostinoSha256 $path) -ne $recorded) {
            $result.Reason = "$name does not match the stamped hash (rebuilt or replaced after the stamp)"
            return $result
        }
    }
    $dirtyNow = Test-PostinoGitDirty -RepoRoot $RepoRoot
    if ($stampDirty -eq $true -or $dirtyNow -eq $true) {
        $result.Dirty = $true
        $result.Provenance = 'dirty'
        $result.Reason = 'the stamped build came from a modified working tree'
        return $result
    }
    if ($dirtyNow -eq $null) {
        $result.Reason = 'git could not report whether the working tree is clean'
        return $result
    }
    $result.Ok = $true
    $result.Dirty = $false
    $result.Provenance = 'verified'
    return $result
}

# Release check over a staged payload: BUILD-INFO must name a real commit, a clean tree and the exact
# hashes of the three shipped executables.
function Test-PostinoReleaseProvenance {
    param(
        [Parameter(Mandatory = $true)][string]$RepoRoot,
        [Parameter(Mandatory = $true)][string]$StageDir
    )
    $problems = New-Object System.Collections.Generic.List[string]
    $binaries = @('postino-app.exe', 'postino.exe', 'postino-daemon.exe')
    $path = Join-Path $StageDir 'BUILD-INFO.txt'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        $problems.Add('BUILD-INFO.txt is missing, so provenance is unknown')
        return $problems
    }
    $fields = @{}
    $hashes = @{}
    foreach ($line in ([System.IO.File]::ReadAllText($path) -split "`r?`n")) {
        if ($line -match '^Binary\s+(\S+):\s*([0-9a-fA-F]{64})$') {
            $hashes[$Matches[1]] = $Matches[2].ToLowerInvariant()
        } elseif ($line -match '^([A-Za-z][A-Za-z0-9 ()#-]*):\s*(.*)$') {
            $fields[$Matches[1]] = $Matches[2].Trim()
        }
    }
    $provenance = [string]$fields['Provenance']
    if (-not $provenance -or ($provenance -split '\s+')[0] -ne 'verified') {
        $problems.Add("BUILD-INFO.txt provenance is '$provenance', not verified")
    }
    if ([string]$fields['Commit dirty'] -ne 'no') {
        $problems.Add("BUILD-INFO.txt reports 'Commit dirty: $($fields['Commit dirty'])', so the package is not release-clean")
    }
    $commit = [string]$fields['Commit']
    if ($commit -notmatch '^[0-9a-f]{40}$') {
        $problems.Add('BUILD-INFO.txt does not record a full 40-hex source commit')
    } elseif (-not (Test-PostinoCommitExists -RepoRoot $RepoRoot -Commit $commit)) {
        $problems.Add("BUILD-INFO.txt names commit $($commit.Substring(0, 8)), which does not exist in this repository")
    }
    foreach ($name in $binaries) {
        $binaryPath = Join-Path $StageDir $name
        if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) { continue }
        if (-not $hashes.ContainsKey($name)) {
            $problems.Add("BUILD-INFO.txt records no binary hash for $name")
            continue
        }
        if ((Get-PostinoSha256 $binaryPath) -ne $hashes[$name]) {
            $problems.Add("$name does not match the hash recorded in BUILD-INFO.txt")
        }
    }
    return $problems
}
