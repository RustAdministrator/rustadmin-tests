$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$TestsRoot = Resolve-Path (Join-Path $ScriptDir "..")
$Manifest = Join-Path $TestsRoot "dxgi_probe\Cargo.toml"

cargo build --manifest-path $Manifest --release
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}

$Exe = Join-Path $TestsRoot "dxgi_probe\target\release\rustadmin-dxgi-probe.exe"
if (!(Test-Path $Exe)) {
    throw "Build succeeded but executable was not found: $Exe"
}

Write-Host "Built: $Exe"
