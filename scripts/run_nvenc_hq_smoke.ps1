param(
    [string]$CodecRoot = "F:\DVS",
    [string]$TargetDir = ""
)

$ErrorActionPreference = "Stop"

$WorkspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$TestsRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$ScrapManifest = Join-Path $WorkspaceRoot "rustadmin\libs\scrap\Cargo.toml"
if ([string]::IsNullOrWhiteSpace($TargetDir)) {
    $TargetDir = Join-Path $TestsRoot "target-scrap"
}

$env:RUSTDESK_WINDOWS_CODEC_ROOT = $CodecRoot
$env:CMAKE_PREFIX_PATH = $CodecRoot
$env:RUSTFLAGS = "-Ctarget-feature=+crt-static"
$env:Path = (Join-Path $CodecRoot "bin") + ";" + $env:Path

cargo test `
    --manifest-path $ScrapManifest `
    --features hwcodec `
    --lib nvenc_high_quality_ `
    --target-dir $TargetDir `
    -- `
    --ignored `
    --nocapture `
    --test-threads=1
