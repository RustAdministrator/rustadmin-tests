param(
    [switch]$Clean
)

$ErrorActionPreference = "Stop"
$TestsRoot = Split-Path -Parent $PSScriptRoot
$Manifest = Join-Path $TestsRoot "av1_screen_bench\Cargo.toml"
$TargetDir = Join-Path $TestsRoot "target"

if (-not $env:RUSTDESK_WINDOWS_CODEC_ROOT -and (Test-Path "F:\DVS")) {
    $env:RUSTDESK_WINDOWS_CODEC_ROOT = "F:\DVS"
}
if (-not $env:CMAKE_PREFIX_PATH -and $env:RUSTDESK_WINDOWS_CODEC_ROOT) {
    $env:CMAKE_PREFIX_PATH = $env:RUSTDESK_WINDOWS_CODEC_ROOT
}
if (-not $env:RUSTFLAGS) {
    $env:RUSTFLAGS = "-Ctarget-feature=+crt-static"
} elseif ($env:RUSTFLAGS -notmatch "crt-static") {
    $env:RUSTFLAGS = "$env:RUSTFLAGS -Ctarget-feature=+crt-static"
}

if ($Clean) {
    cargo clean --manifest-path $Manifest --target-dir $TargetDir
}

cargo build --manifest-path $Manifest --target-dir $TargetDir --release
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$Binary = Join-Path $TargetDir "release\rustadmin-av1-screen-bench.exe"
$Iconv = if ($env:RUSTDESK_WINDOWS_CODEC_ROOT) {
    Join-Path $env:RUSTDESK_WINDOWS_CODEC_ROOT "bin\iconv-2.dll"
}
if ($Iconv -and (Test-Path $Iconv)) {
    Copy-Item -Force $Iconv (Split-Path -Parent $Binary)
}
Write-Host "Built: $Binary"
