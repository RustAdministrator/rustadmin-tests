$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$Manifest = Join-Path $RepoRoot "rustdesk-tests\debugprobe\Cargo.toml"

cargo build --manifest-path $Manifest --release

$Exe = Join-Path $RepoRoot "rustdesk-tests\debugprobe\target\release\rustadmin-debugprobe.exe"
Write-Host "Built: $Exe"
