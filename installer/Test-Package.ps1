# Static and staging validation for the Postino Windows package.
# It does not build an MSI, download anything or need a certificate.
[CmdletBinding()]
param(
    [string]$StageDir,
    [string]$DistRoot
)

. (Join-Path $PSScriptRoot 'lib\common.ps1')
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repo = Get-PostinoRepoRoot
$version = Get-PostinoVersion -RepoRoot $repo
if (-not $DistRoot) { $DistRoot = Join-Path $repo 'dist' }
if (-not $StageDir) { $StageDir = Join-Path $DistRoot "Postino-$version-windows-x64" }
$wxs = Join-Path $PSScriptRoot 'wix\Postino.wxs'
$script:passCount = 0
$script:failCount = 0
$script:infoCount = 0

function Add-Pass {
    param([string]$Message)
    $script:passCount++
    Write-Output "PASS $Message"
}

function Add-Fail {
    param([string]$Message)
    $script:failCount++
    Write-Output "FAIL $Message"
}

function Add-Info {
    param([string]$Message)
    $script:infoCount++
    Write-Output "INFO $Message"
}

function Get-CodeText {
    param([string]$Path)
    $text = [System.IO.File]::ReadAllText($Path)
    return [regex]::Replace($text, '(?s)<!--.*?-->', '')
}

function Test-RequiredFile {
    param([string]$Directory, [string]$Name)
    if (Test-Path -LiteralPath (Join-Path $Directory $Name)) {
        Add-Pass "payload contains $Name"
    } else {
        Add-Fail "payload is missing $Name"
    }
}

# 1. PowerShell syntax.
$scripts = @(Get-ChildItem -LiteralPath $PSScriptRoot -Filter '*.ps1' -File -Recurse | Sort-Object FullName)
foreach ($script in $scripts) {
    $tokens = $null
    $errors = $null
    [System.Management.Automation.Language.Parser]::ParseFile($script.FullName, [ref]$tokens, [ref]$errors) | Out-Null
    if ($errors.Count -eq 0) {
        Add-Pass "PowerShell syntax: $($script.Name)"
    } else {
        foreach ($parseError in $errors) { Add-Fail "PowerShell syntax: $($script.Name): $($parseError.Message)" }
    }
}

# 2. WiX source is well formed and has the expected per-user contract.
if (-not (Test-Path -LiteralPath $wxs)) {
    Add-Fail 'installer/wix/Postino.wxs is missing'
} else {
    $wxsText = Get-CodeText -Path $wxs
    try {
        $xml = New-Object System.Xml.XmlDocument
        $xml.PreserveWhitespace = $true
        $xml.Load($wxs)
        Add-Pass 'WiX source is well-formed XML'
    } catch {
        Add-Fail "WiX source is not well-formed XML: $($_.Exception.Message)"
        $xml = $null
    }

    if ($xml) {
        $ns = New-Object System.Xml.XmlNamespaceManager($xml.NameTable)
        $ns.AddNamespace('w', 'http://wixtoolset.org/schemas/v4/wxs')
        $ns.AddNamespace('ui', 'http://wixtoolset.org/schemas/v4/wxs/ui')
        $package = $xml.SelectSingleNode('/w:Wix/w:Package', $ns)
        if (-not $package) {
            Add-Fail 'WiX Package element is missing'
        } else {
            if ($package.GetAttribute('Scope') -eq 'perUser') { Add-Pass 'MSI scope is perUser' } else { Add-Fail 'MSI scope is not perUser' }
            if ($package.GetAttribute('Manufacturer') -eq 'Postino') { Add-Pass 'MSI manufacturer is Postino' } else { Add-Fail 'MSI manufacturer is not Postino' }
        }

        if ($xml.SelectSingleNode('//w:StandardDirectory[@Id="LocalAppDataFolder"]//w:Directory[@Id="INSTALLFOLDER"]', $ns)) {
            Add-Pass 'install folder is under LocalAppDataFolder'
        } else {
            Add-Fail 'install folder is not under LocalAppDataFolder'
        }
        if ($xml.SelectSingleNode('//w:StandardDirectory[@Id="ProgramMenuFolder"]/w:Directory[@Id="PostinoMenuFolder"]', $ns)) {
            Add-Pass 'Start Menu folder is ProgramMenuFolder\\Postino'
        } else {
            Add-Fail 'Start Menu folder is not ProgramMenuFolder\\Postino'
        }

        $shortcut = $xml.SelectSingleNode('//w:Shortcut[@Id="PostinoStartMenuShortcut"]', $ns)
        if ($shortcut) {
            if ($shortcut.GetAttribute('Target') -eq '[INSTALLFOLDER]postino-app.exe') {
                Add-Pass 'Start Menu shortcut targets postino-app.exe'
            } else {
                Add-Fail 'Start Menu shortcut has an unexpected target'
            }
            if ($shortcut.GetAttribute('WorkingDirectory') -eq 'INSTALLFOLDER') {
                Add-Pass 'shortcut working directory is INSTALLFOLDER'
            } else {
                Add-Fail 'shortcut working directory is not INSTALLFOLDER'
            }
        } else {
            Add-Fail 'Start Menu shortcut component is missing'
        }

        $feature = $xml.SelectSingleNode('//w:Feature[@Id="Main"]', $ns)
        $groupRef = $null
        $componentRef = $null
        if ($feature) {
            $groupRef = $feature.SelectSingleNode('w:ComponentGroupRef[@Id="ProductFiles"]', $ns)
            $componentRef = $feature.SelectSingleNode('w:ComponentRef[@Id="StartMenuShortcut"]', $ns)
        }
        if ($groupRef -and $componentRef) {
            Add-Pass 'Main feature includes product files and the Start Menu shortcut'
        } else {
            Add-Fail 'Main feature does not include the product files and shortcut'
        }

        $removeProperty = $xml.SelectSingleNode('//w:Property[@Id="REMOVEDATA"]', $ns)
        if ($removeProperty -and $removeProperty.GetAttribute('Value') -eq '0') {
            Add-Pass 'data removal defaults to off'
        } else {
            Add-Fail 'REMOVEDATA does not default to 0'
        }
        $checkbox = $xml.SelectSingleNode('//w:Control[@Id="RemoveDataCheckBox"]', $ns)
        if ($checkbox -and $checkbox.GetAttribute('Property') -eq 'REMOVEDATA' -and $checkbox.GetAttribute('CheckBoxValue') -eq '1') {
            Add-Pass 'uninstall dialog has an explicit data-removal checkbox'
        } else {
            Add-Fail 'uninstall data-removal checkbox is missing or misconfigured'
        }
        if ($xml.SelectSingleNode('//w:Dialog[@Id="PostinoRemoveDataDlg"]', $ns)) {
            Add-Pass 'uninstall data-choice dialog exists'
        } else {
            Add-Fail 'uninstall data-choice dialog is missing'
        }

        $action = $xml.SelectSingleNode('//w:Package/w:CustomAction[@Id="RemovePostinoUserData"]', $ns)
        if ($action) {
            if ($action.GetAttribute('Execute') -eq 'immediate' -and $action.GetAttribute('Impersonate') -eq 'yes' -and $action.GetAttribute('Return') -eq 'ignore') {
                Add-Pass 'data removal custom action is immediate, per-user and non-fatal'
            } else {
                Add-Fail 'data removal custom action is not immediate/per-user/non-fatal'
            }
            $command = $action.GetAttribute('ExeCommand')
            $directories = [regex]::Matches($command, '\[([^\]]+)\]') | ForEach-Object { $_.Groups[1].Value }
            $unexpected = @($directories | Where-Object { $_ -notin @('SystemFolder', 'LocalAppDataFolder', 'AppDataFolder') })
            if ($unexpected.Count -eq 0 -and $command -match 'rmdir' -and $command -match '\[LocalAppDataFolder\]Postino' -and $command -match '\[AppDataFolder\]Postino') {
                Add-Pass 'data removal targets only the two Postino app-data folders'
            } else {
                Add-Fail 'data removal command targets an unexpected path'
            }
        } else {
            Add-Fail 'data removal custom action is missing'
        }

        $sequence = $xml.SelectSingleNode('//w:Package/w:InstallExecuteSequence/w:Custom[@Action="RemovePostinoUserData"]', $ns)
        if ($sequence) {
            $condition = $sequence.GetAttribute('Condition')
            if ($condition -match 'REMOVEDATA=&quot;1&quot;' -or $condition -match 'REMOVEDATA="1"') {
                if ($condition -match 'REMOVE=&quot;ALL&quot;' -or $condition -match 'REMOVE="ALL"') {
                    if ($sequence.GetAttribute('After') -eq 'RemoveFiles') {
                        Add-Pass 'data removal runs only on full uninstall with the checkbox selected'
                    } else {
                        Add-Fail 'data removal is not sequenced after RemoveFiles'
                    }
                } else {
                    Add-Fail 'data removal is not limited to a full uninstall'
                }
            } else {
                Add-Fail 'data removal is not guarded by REMOVEDATA=1'
            }
        } else {
            Add-Fail 'data removal sequence entry is missing'
        }

        if ($xml.SelectSingleNode('//w:Package/ui:WixUI[@Id="WixUI_Minimal"]', $ns)) {
            Add-Pass 'WixUI_Minimal is referenced'
        } else {
            Add-Fail 'WixUI_Minimal is not referenced'
        }

        $forbidden = [regex]::Matches($wxsText, '(?i)\b(ALLUSERS|RequireAdministrator|ServiceInstall|ServiceControl|ServiceConfig|PerMachine)\b')
        if ($forbidden.Count -eq 0) { Add-Pass 'no admin/service/per-machine constructs' } else { Add-Fail "forbidden installer constructs: $((($forbidden | ForEach-Object { $_.Value }) | Sort-Object -Unique) -join ', ')" }
        if ($wxsText -match '(?i)CurrentVersion\s*\\+\s*Run') { Add-Fail 'installer writes an autostart Run value' } else { Add-Pass 'installer does not write an autostart Run value' }
        if ($wxsText -match '(?i)\b(squirrel|winsparkle|autoupdater|checkforupdates|telemetry)\b') { Add-Fail 'installer mentions an updater or telemetry component' } else { Add-Pass 'no updater or telemetry component in the MSI' }
    }
}

# 3. No network, certificate or secret in the packaging sources.
$secretTargets = @($scripts) + @(Get-ChildItem -LiteralPath $PSScriptRoot -File -Recurse |
    Where-Object { $_.Extension -in @('.ps1', '.wxs', '.md', '.rtf', '.txt') })
$secretTargets = @($secretTargets | Where-Object { $_.FullName -ne $PSCommandPath } | Sort-Object FullName -Unique)
$secretPattern = '(?i)(-----BEGIN[^-]*PRIVATE KEY-----|\.pfx\b|\.p12\b|\.pem\b|POSTINO_SIGN_CERT_PASSWORD\s*=)'
$thumbprintPattern = '(?i)(?<![0-9a-f])[0-9a-f]{40}(?![0-9a-f])'
$secretHits = New-Object System.Collections.Generic.List[string]
foreach ($file in $secretTargets) {
    $text = [System.IO.File]::ReadAllText($file.FullName)
    if ($text -match $secretPattern) { $secretHits.Add("$($file.Name): certificate/private-key material") }
    if ($text -match $thumbprintPattern) { $secretHits.Add("$($file.Name): hard-coded 40-hex thumbprint") }
}
$certFiles = @(Get-ChildItem -LiteralPath $PSScriptRoot -File -Recurse | Where-Object { $_.Extension -in @('.pfx', '.p12', '.pem', '.key', '.cer') })
if ($certFiles.Count -gt 0) { $secretHits.Add("certificate files: $($certFiles.Name -join ', ')") }
if ($secretHits.Count -eq 0) { Add-Pass 'no certificate, private key or hard-coded thumbprint in installer/' } else { foreach ($hit in $secretHits) { Add-Fail "secret scan: $hit" } }

$networkHits = @()
foreach ($script in $scripts) {
    if ($script.FullName -eq $PSCommandPath) { continue }
    $text = Get-CodeText -Path $script.FullName
    if ($text -match '(?i)(Invoke-WebRequest|Invoke-RestMethod|Start-BitsTransfer|DownloadFile|curl\.exe|wget\.exe)') { $networkHits += $script.Name }
}
if ($networkHits.Count -eq 0) { Add-Pass 'packaging scripts download nothing' } else { Add-Fail "packaging scripts contain a download call: $($networkHits -join ', ')" }

# 4. Staged payload and SHA-256 manifest.
if (-not (Test-Path -LiteralPath $StageDir)) {
    Add-Info "staged payload not present at $StageDir (static validation only)"
} else {
    $stage = [System.IO.Path]::GetFullPath($StageDir).TrimEnd('\')
    foreach ($name in @('postino-app.exe', 'postino.exe', 'postino-daemon.exe', 'postino.ico', 'LICENSE-MIT', 'LICENSE-APACHE', 'ATTRIBUTION.toml', 'BUILD-INFO.txt', 'SHA256SUMS.txt')) {
        Test-RequiredFile -Directory $stage -Name $name
    }
    $unwanted = @(Get-ChildItem -LiteralPath $stage -File -Recurse | Where-Object { $_.Extension -in @('.pdb', '.zip', '.msi', '.pfx', '.p12', '.pem', '.key', '.cer') })
    if ($unwanted.Count -eq 0) { Add-Pass 'payload has no debug, archive, installer or certificate files' } else { Add-Fail "payload has unexpected files: $($unwanted.Name -join ', ')" }

    $buildInfoPath = Join-Path $stage 'BUILD-INFO.txt'
    if ((Test-Path -LiteralPath $buildInfoPath) -and ([System.IO.File]::ReadAllText($buildInfoPath) -match '[A-Za-z]:\\')) {
        Add-Fail 'BUILD-INFO.txt contains an absolute local path'
    } else {
        Add-Pass 'BUILD-INFO.txt contains no absolute local path'
    }

    # Provenance: a real commit, a clean tree and the exact shipped binary hashes.
    $provenanceProblems = @(Test-PostinoReleaseProvenance -RepoRoot $repo -StageDir $stage)
    if ($provenanceProblems.Count -eq 0) {
        Add-Pass 'BUILD-INFO provenance names a real clean commit and the shipped binary hashes'
    } else {
        foreach ($problem in $provenanceProblems) { Add-Fail "provenance: $problem" }
    }

    # Every staged portable executable must be x86_64; a 32-bit or foreign DLL would not load.
    $peFiles = @(Get-ChildItem -LiteralPath $stage -File -Recurse | Where-Object { $_.Extension -in @('.exe', '.dll') })
    $wrongArch = New-Object System.Collections.Generic.List[string]
    foreach ($pe in $peFiles) {
        try {
            $machine = Get-PostinoPeMachine -Path $pe.FullName
            if ($machine -ne 0x8664) { $wrongArch.Add("$($pe.Name) is $(Get-PostinoMachineName -Machine $machine)") }
        } catch {
            $wrongArch.Add("$($pe.Name) is not a readable PE file")
        }
    }
    if ($wrongArch.Count -eq 0) { Add-Pass "every staged executable and DLL is x86_64 ($($peFiles.Count) files)" } else { Add-Fail "payload architecture: $($wrongArch -join ', ')" }

    $manifestPath = Join-Path $stage 'SHA256SUMS.txt'
    if (Test-Path -LiteralPath $manifestPath) {
        $seen = @{}
        $bad = 0
        foreach ($line in [System.IO.File]::ReadAllLines($manifestPath)) {
            if (-not $line) { continue }
            if ($line -notmatch '^([0-9a-f]{64})  (.+)$') { Add-Fail "malformed manifest line: $line"; $bad++; continue }
            $hash = $Matches[1]
            $relative = $Matches[2]
            if ($relative -match '(^[\\/]|(^|[\\/])\.\.([\\/]|$))') { Add-Fail "manifest path escapes the payload: $relative"; $bad++; continue }
            $path = Join-Path $stage $relative
            if (-not (Test-Path -LiteralPath $path)) { Add-Fail "manifest names a missing file: $relative"; $bad++; continue }
            if ((Get-PostinoSha256 $path) -ne $hash) { Add-Fail "manifest hash mismatch: $relative"; $bad++; continue }
            $seen[$relative.ToLowerInvariant()] = $true
        }
        $actual = @(Get-ChildItem -LiteralPath $stage -File -Recurse | ForEach-Object { $_.FullName.Substring($stage.Length).TrimStart('\') } | Where-Object { $_ -ne 'SHA256SUMS.txt' })
        foreach ($relative in $actual) {
            if (-not $seen.ContainsKey($relative.ToLowerInvariant())) { Add-Fail "payload file is not in the manifest: $relative"; $bad++ }
        }
        if ($bad -eq 0) { Add-Pass "SHA-256 manifest covers every payload file ($($actual.Count) files)" }
    }

    # Every non-system DLL imported by a staged PE must be in the payload.
    $objdump = Get-PostinoObjdump -AllowMissing
    if (-not $objdump) {
        Add-Info 'objdump.exe is not installed: the non-system DLL import check was skipped (put C:\msys64\mingw64\bin first on PATH to enable it)'
    } else {
        try {
            $missing = New-Object System.Collections.Generic.List[string]
            $stageNames = @($peFiles | ForEach-Object { $_.Name.ToLowerInvariant() })
            foreach ($pe in $peFiles) {
                foreach ($name in (Get-PostinoImportedDlls -Path $pe.FullName -Objdump $objdump)) {
                    if (Test-PostinoSystemDll -Name $name) { continue }
                    if ($stageNames -notcontains $name.ToLowerInvariant()) { $missing.Add("$($pe.Name) -> $name") }
                }
            }
            if ($missing.Count -eq 0) { Add-Pass 'every non-system DLL import resolves inside the payload' } else { Add-Fail "missing runtime DLLs: $($missing -join ', ')" }
        } catch {
            Add-Fail "runtime DLL check failed: $($_.Exception.Message)"
        }
    }

    # Generate the WiX file list twice: it must parse and stay deterministic.
    $fragmentScratch = Join-Path ([System.IO.Path]::GetTempPath()) ('postino-p12e-validate-' + [Guid]::NewGuid().ToString('n'))
    try {
        New-Item -ItemType Directory -Path $fragmentScratch -Force | Out-Null
        $first = Join-Path $fragmentScratch 'StageFiles.wxs'
        $second = Join-Path $fragmentScratch 'StageFiles.check.wxs'
        New-PostinoStageFragment -StageDir $stage -OutputPath $first | Out-Null
        New-PostinoStageFragment -StageDir $stage -OutputPath $second | Out-Null
        $fragmentXml = New-Object System.Xml.XmlDocument
        $fragmentXml.Load($first)
        $componentCount = @($fragmentXml.SelectNodes('//*[local-name()="Component"]')).Count
        $payloadCount = @(Get-ChildItem -LiteralPath $stage -File -Recurse | Where-Object { $_.Extension -notin @('.pdb', '.zip', '.msi') }).Count
        if ($componentCount -eq $payloadCount) {
            Add-Pass "generated WiX file list parses and covers $componentCount payload files"
        } else {
            Add-Fail "generated WiX file list has $componentCount components for $payloadCount files"
        }
        if ((Get-PostinoSha256 $first) -eq (Get-PostinoSha256 $second)) {
            Add-Pass 'generated component GUIDs and ids are deterministic'
        } else {
            Add-Fail 'generated WiX file list is not deterministic'
        }
    } catch {
        Add-Fail "WiX file-list generation failed: $($_.Exception.Message)"
    } finally {
        if (Test-Path -LiteralPath $fragmentScratch) { Remove-Item -LiteralPath $fragmentScratch -Recurse -Force }
    }
}

# 5. Resolver fixtures: provenance refusals, toolchain-only DLL resolution, PE architecture.
$fixtureScript = Join-Path $PSScriptRoot 'tests\Test-Resolvers.ps1'
if (-not (Test-Path -LiteralPath $fixtureScript)) {
    Add-Fail 'installer/tests/Test-Resolvers.ps1 is missing'
} else {
    $fixtureOutput = @(& powershell -NoProfile -ExecutionPolicy Bypass -File $fixtureScript)
    $fixtureExit = $LASTEXITCODE
    $fixtureFailed = $false
    foreach ($line in $fixtureOutput) {
        if ($line -like 'PASS *') {
            Add-Pass ($line.Substring(5) + ' (fixture)')
        } elseif ($line -like 'FAIL *') {
            $fixtureFailed = $true
            Add-Fail ($line.Substring(5) + ' (fixture)')
        } elseif ($line -like 'INFO *') {
            Add-Info ($line.Substring(5))
        }
    }
    if ($fixtureExit -ne 0 -and -not $fixtureFailed) { Add-Fail 'resolver fixtures exited with a failure but reported no FAIL line' }
}

# 6. ZIP checksum manifest, if a portable ZIP exists.
$zip = Join-Path $DistRoot "Postino-$version-windows-x64.zip"
$outerManifest = Join-Path $DistRoot 'SHA256SUMS.txt'
if (Test-Path -LiteralPath $zip) {
    if (Test-Path -LiteralPath $outerManifest) {
        $expected = (Get-PostinoSha256 $zip)
        $line = [System.IO.File]::ReadAllLines($outerManifest) | Where-Object { $_ -match [regex]::Escape((Split-Path -Leaf $zip)) } | Select-Object -First 1
        if ($line -and $line.StartsWith($expected)) { Add-Pass 'portable ZIP hash matches dist/SHA256SUMS.txt' } else { Add-Fail 'portable ZIP is not correctly listed in dist/SHA256SUMS.txt' }
    } else {
        Add-Fail 'portable ZIP exists without dist/SHA256SUMS.txt'
    }
} else {
    Add-Info 'portable ZIP not present (skip ZIP hash check)'
}

# 7. MSI checksum, if the MSI was built.
$msi = Join-Path $DistRoot "Postino-$version-windows-x64.msi"
if (Test-Path -LiteralPath $msi) {
    if (Test-Path -LiteralPath $outerManifest) {
        $expected = (Get-PostinoSha256 $msi)
        $line = [System.IO.File]::ReadAllLines($outerManifest) | Where-Object { $_ -match [regex]::Escape((Split-Path -Leaf $msi)) } | Select-Object -First 1
        if ($line -and $line.StartsWith($expected)) { Add-Pass 'MSI hash matches dist/SHA256SUMS.txt' } else { Add-Fail 'MSI is not correctly listed in dist/SHA256SUMS.txt' }
    } else {
        Add-Fail 'MSI exists without dist/SHA256SUMS.txt'
    }
} else {
    Add-Info 'MSI not present (skip MSI hash check)'
}

# 8. Report the optional MSI tool. Never claim an MSI that was not built.
$wixCommand = Get-Command wix.exe -ErrorAction SilentlyContinue
if (-not $wixCommand) { $wixCommand = Get-Command wix -ErrorAction SilentlyContinue }
if ($wixCommand) {
    Add-Info "WiX found: $($wixCommand.Source) (this validator does not build the MSI)"
} else {
    Add-Info 'WiX Toolset v4 (wix.exe) is not installed: the MSI source is validated, no MSI was built'
}

Write-Output ''
Write-Output "Result: $passCount passed, $failCount failed, $infoCount informational."
if ($failCount -gt 0) { exit 1 }
exit 0
