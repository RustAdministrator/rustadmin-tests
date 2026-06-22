$ErrorActionPreference = "Stop"

$Root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$Manifest = Join-Path $Root "rustdesk-tests\netprobe\Cargo.toml"

$Output = cargo build --release --manifest-path $Manifest --message-format=json-render-diagnostics @args

$Bin = $null
foreach ($Line in $Output) {
    if ([string]::IsNullOrWhiteSpace($Line)) {
        continue
    }

    try {
        $Message = $Line | ConvertFrom-Json
    } catch {
        Write-Host $Line
        continue
    }

    if ($Message.reason -eq "compiler-message" -and $Message.message.rendered) {
        Write-Host $Message.message.rendered
    }

    if ($Message.reason -eq "compiler-artifact" -and $Message.executable) {
        $Kind = @($Message.target.kind)
        if ($Kind -contains "bin" -and $Message.target.name -eq "rustadmin-netprobe") {
            $Bin = $Message.executable
        }
    }
}

if (-not $Bin -or -not (Test-Path $Bin)) {
    Write-Error "Cargo finished but did not report a rustadmin-netprobe executable. Check active target settings with: rustc -vV; cargo config get build.target; `$env:CARGO_BUILD_TARGET; `$env:CARGO_TARGET_DIR"
}

Write-Host "Built: $Bin"
