$ErrorActionPreference = "Stop"

$TestsRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$Manifest = Join-Path $TestsRoot "debugprobe\Cargo.toml"

cargo build --manifest-path $Manifest --release

$Exe = Join-Path $TestsRoot "debugprobe\target\release\rustadmin-debugprobe.exe"
Write-Host "Built: $Exe"
