# Build and run the libFuzzer targets with MSVC AddressSanitizer on Windows.
#
# Prerequisites:
#   Visual Studio 2022 Build Tools with:
#     Microsoft.VisualStudio.Workload.VCTools
#     Microsoft.VisualStudio.Component.VC.Tools.x86.x64
#     Microsoft.VisualStudio.Component.VC.ASAN
#     Microsoft.VisualStudio.Component.Windows11SDK.26100
#   rustup toolchain install nightly-x86_64-pc-windows-msvc --profile minimal
#   rustup component add rust-src --toolchain nightly-x86_64-pc-windows-msvc
#   cargo install cargo-fuzz --locked
#
# Example:
#   pwsh -File fuzz/run-windows.ps1 -Runs 2000
#   pwsh -File fuzz/run-windows.ps1 -Target mime_parse -MaxTotalTime 60

[CmdletBinding()]
param(
    [ValidateSet('mime_parse', 'html_sanitize', 'pgp_mime_detect', 'calendar_parse')]
    [string]$Target,
    [ValidateRange(0, [int]::MaxValue)]
    [int]$Runs = 2000,
    [ValidateRange(0, [int]::MaxValue)]
    [int]$MaxTotalTime = 0
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$vcvars = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat'
if (-not (Test-Path $vcvars)) {
    throw "vcvars64.bat not found. Install the VS 2022 C++ workload and Windows SDK first: $vcvars"
}
if (-not (Get-Command cargo-fuzz -ErrorAction SilentlyContinue)) {
    throw 'cargo-fuzz is not installed. Run: cargo install cargo-fuzz --locked'
}

$env:RUSTUP_TOOLCHAIN = 'nightly-x86_64-pc-windows-msvc'
$env:CARGO_TARGET_DIR = Join-Path $root 'target\fuzz-msvc'
$targets = if ($Target) {
    @($Target)
} else {
    @('mime_parse', 'html_sanitize', 'pgp_mime_detect', 'calendar_parse')
}

$fixtures = Get-ChildItem (Join-Path $root 'crates\mime\tests\fixtures') -Filter *.eml |
    Select-Object -ExpandProperty FullName
$seeds = @{
    mime_parse      = $fixtures
    pgp_mime_detect = $fixtures
    html_sanitize   = @((Join-Path $root 'fuzz\seeds\html.html'))
    calendar_parse  = @((Join-Path $root 'fuzz\seeds\calendar.ics'))
}

foreach ($name in $targets) {
    $corpus = Join-Path $root "fuzz\corpus\$name"
    New-Item -ItemType Directory -Force -Path $corpus | Out-Null
    foreach ($seed in $seeds[$name]) {
        if (Test-Path $seed) {
            Copy-Item -Force $seed (Join-Path $corpus (Split-Path -Leaf $seed))
        }
    }

    Write-Host "== build $name"
    $build = 'call "' + $vcvars + '" >nul && cargo fuzz build --target x86_64-pc-windows-msvc ' + $name
    cmd.exe /d /c $build
    if ($LASTEXITCODE -ne 0) {
        throw "cargo fuzz build failed for $name (exit $LASTEXITCODE)"
    }

    if ($Runs -gt 0 -or $MaxTotalTime -gt 0) {
        $flags = @()
        if ($Runs -gt 0) { $flags += "-runs=$Runs" }
        if ($MaxTotalTime -gt 0) { $flags += "-max_total_time=$MaxTotalTime" }
        Write-Host "== run $name ($($flags -join ' '))"
        $run = 'call "' + $vcvars + '" >nul && cargo fuzz run --target x86_64-pc-windows-msvc ' +
            $name + ' -- ' + ($flags -join ' ')
        cmd.exe /d /c $run
        if ($LASTEXITCODE -ne 0) {
            throw "cargo fuzz run failed for $name (exit $LASTEXITCODE); see fuzz/artifacts/$name"
        }
    }
}

Write-Host "Windows fuzz targets passed: $($targets -join ', ')"
