param(
    [string]$Corpus = "F:\GH\rustdesk\rustdesk-tests\results\process-lasso\process-lasso-display2-90f.rasc",
    [string]$OutputDir = "F:\GH\rustdesk\rustdesk-tests\results\process-lasso\nvenc-tuning-matrix",
    [string]$CodecRoot = "F:\DVS",
    [int]$Width = 2560,
    [int]$Height = 1440,
    [int]$Frames = 90,
    [int]$Fps = 30,
    [ValidateSet("core", "preset")]
    [string]$Matrix = "core"
)

$ErrorActionPreference = "Stop"
$TestsRoot = Split-Path -Parent $PSScriptRoot
$Probe = Join-Path $TestsRoot "target\release\rustadmin-av1-screen-bench.exe"
$FFmpeg = Join-Path $CodecRoot "bin\ffmpeg.exe"
$FFprobe = Join-Path $CodecRoot "bin\ffprobe.exe"
$RawInput = Join-Path $OutputDir "source.bgra"

foreach ($Path in @($Probe, $FFmpeg, $FFprobe, $Corpus)) {
    if (-not (Test-Path $Path)) {
        throw "Required file not found: $Path"
    }
}
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$env:PATH = "$(Join-Path $CodecRoot 'bin');$env:PATH"

& $Probe export-bgra --input $Corpus --output $RawInput
if ($LASTEXITCODE -ne 0) {
    throw "BGRA export failed with exit code $LASTEXITCODE"
}

function Get-Percentile([long[]]$Values, [int]$Percentile) {
    if ($Values.Count -eq 0) {
        return 0
    }
    [Array]::Sort($Values)
    $Index = [Math]::Floor((($Values.Count - 1) * $Percentile + 50) / 100)
    return $Values[$Index]
}

$Variants = @(
    [pscustomobject]@{
        Name = "current"
        VbvFrames = 0
        Args = @("-preset", "p4", "-tune", "hq", "-rc", "cbr", "-delay", "0")
    },
    [pscustomobject]@{
        Name = "hq-aq"
        VbvFrames = 0
        Args = @("-preset", "p4", "-tune", "hq", "-rc", "cbr", "-delay", "0", "-spatial-aq", "1", "-aq-strength", "8", "-rc-lookahead", "0")
    },
    [pscustomobject]@{
        Name = "ll"
        VbvFrames = 0
        Args = @("-preset", "p4", "-tune", "ll", "-rc", "cbr", "-delay", "0", "-zerolatency", "1", "-rc-lookahead", "0")
    },
    [pscustomobject]@{
        Name = "hq-vbv1"
        VbvFrames = 1
        Args = @("-preset", "p4", "-tune", "hq", "-rc", "cbr", "-delay", "0", "-strict_gop", "1", "-ldkfs", "100")
    },
    [pscustomobject]@{
        Name = "hq-vbv2"
        VbvFrames = 2
        Args = @("-preset", "p4", "-tune", "hq", "-rc", "cbr", "-delay", "0", "-strict_gop", "1", "-ldkfs", "100")
    },
    [pscustomobject]@{
        Name = "hq-vbv4"
        VbvFrames = 4
        Args = @("-preset", "p4", "-tune", "hq", "-rc", "cbr", "-delay", "0", "-strict_gop", "1", "-ldkfs", "100")
    }
)
if ($Matrix -eq "preset") {
    $Variants = @(
        [pscustomobject]@{
            Name = "p4"
            VbvFrames = 0
            Args = @("-preset", "p4", "-tune", "hq", "-rc", "cbr", "-delay", "0")
        },
        [pscustomobject]@{
            Name = "p5"
            VbvFrames = 0
            Args = @("-preset", "p5", "-tune", "hq", "-rc", "cbr", "-delay", "0")
        },
        [pscustomobject]@{
            Name = "p6"
            VbvFrames = 0
            Args = @("-preset", "p6", "-tune", "hq", "-rc", "cbr", "-delay", "0")
        },
        [pscustomobject]@{
            Name = "p5-vbv1"
            VbvFrames = 1
            Args = @("-preset", "p5", "-tune", "hq", "-rc", "cbr", "-delay", "0", "-strict_gop", "1", "-ldkfs", "100")
        }
    )
}

$Cases = @(
    [pscustomobject]@{ Encoder = "h264_nvenc"; Codec = "h264"; Quality = 0.25; Bitrate = 1500 },
    [pscustomobject]@{ Encoder = "h264_nvenc"; Codec = "h264"; Quality = 0.67; Bitrate = 4000 },
    [pscustomobject]@{ Encoder = "hevc_nvenc"; Codec = "hevc"; Quality = 0.25; Bitrate = 1125 },
    [pscustomobject]@{ Encoder = "hevc_nvenc"; Codec = "hevc"; Quality = 0.67; Bitrate = 3005 }
)

$Results = [System.Collections.Generic.List[object]]::new()
$CommonInput = @(
    "-hide_banner", "-loglevel", "error", "-y",
    "-f", "rawvideo", "-pix_fmt", "bgra",
    "-video_size", "${Width}x${Height}", "-framerate", "$Fps",
    "-i", $RawInput, "-frames:v", "$Frames", "-an", "-vf", "format=nv12"
)

foreach ($Case in $Cases) {
    foreach ($Variant in $Variants) {
        $Slug = "$($Case.Codec)-q$($Case.Quality.ToString('0.00').Replace('.', '_'))-$($Variant.Name)"
        $Bitstream = Join-Path $OutputDir "$Slug.$($Case.Codec)"
        $Decoded = Join-Path $OutputDir "$Slug.yuv"
        $Analysis = Join-Path $OutputDir "$Slug-analysis.json"
        $BufferKbits = [Math]::Max(1, [Math]::Ceiling($Case.Bitrate * $Variant.VbvFrames / $Fps))
        $RateArgs = @("-b:v", "$($Case.Bitrate)k", "-g", "2147483647", "-bf", "0")
        if ($Variant.VbvFrames -gt 0) {
            $RateArgs += @("-maxrate", "$($Case.Bitrate)k", "-bufsize", "${BufferKbits}k")
        }
        $Started = [System.Diagnostics.Stopwatch]::StartNew()
        & $FFmpeg @CommonInput -c:v $Case.Encoder @($Variant.Args) @RateArgs -f $Case.Codec $Bitstream
        if ($LASTEXITCODE -ne 0) {
            throw "Encoding failed for $Slug with exit code $LASTEXITCODE"
        }
        $Started.Stop()

        & $FFmpeg -hide_banner -loglevel error -y -f $Case.Codec -i $Bitstream -frames:v $Frames -pix_fmt yuv420p -f rawvideo $Decoded
        if ($LASTEXITCODE -ne 0) {
            throw "Decoding failed for $Slug with exit code $LASTEXITCODE"
        }
        & $Probe analyze-yuv420 --input $Corpus --decoded $Decoded --output $Analysis
        if ($LASTEXITCODE -ne 0) {
            throw "Quality analysis failed for $Slug with exit code $LASTEXITCODE"
        }

        [long[]]$PacketSizes = @(
            & $FFprobe -v error -show_entries packet=size -of csv=p=0 -f $Case.Codec $Bitstream |
                ForEach-Object { [long]($_.Trim()) }
        )
        if ($LASTEXITCODE -ne 0 -or $PacketSizes.Count -eq 0) {
            throw "Packet analysis failed for $Slug"
        }
        $Quality = Get-Content $Analysis -Raw | ConvertFrom-Json
        $Bytes = (Get-Item $Bitstream).Length
        $DurationSeconds = $Frames / [double]$Fps
        $Result = [pscustomobject]@{
            encoder = $Case.Encoder
            codec = $Case.Codec
            quality_ratio = $Case.Quality
            variant = $Variant.Name
            target_bitrate_kbps = $Case.Bitrate
            actual_bitrate_kbps = [Math]::Round($Bytes * 8.0 / $DurationSeconds / 1000.0, 2)
            encoded_bytes = $Bytes
            packet_count = $PacketSizes.Count
            packet_p50_bytes = Get-Percentile $PacketSizes 50
            packet_p95_bytes = Get-Percentile $PacketSizes 95
            packet_p99_bytes = Get-Percentile $PacketSizes 99
            largest_packet_bytes = ($PacketSizes | Measure-Object -Maximum).Maximum
            encode_wall_ms = [Math]::Round($Started.Elapsed.TotalMilliseconds, 2)
            luma_psnr_db = [Math]::Round($Quality.luma_psnr_db, 3)
        }
        $Results.Add($Result)
        Write-Host ("{0,-11} q={1,-4} {2,-11}: {3,7:N1} kbps, p99 {4,7} B, PSNR-Y {5,6:N2} dB" -f $Case.Encoder, $Case.Quality, $Variant.Name, $Result.actual_bitrate_kbps, $Result.packet_p99_bytes, $Result.luma_psnr_db)
    }
}

$ReportJson = Join-Path $OutputDir "report.json"
$Results | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 $ReportJson
$ReportMarkdown = Join-Path $OutputDir "report.md"
$Lines = [System.Collections.Generic.List[string]]::new()
$Lines.Add("# NVENC Screen Tuning Matrix")
$Lines.Add("")
$Lines.Add("- Corpus: ``$Corpus``")
$Lines.Add("- Frame: ``${Width}x${Height}``")
$Lines.Add("- Input: ``$Fps fps``, ``$Frames frames``")
$Lines.Add("")
$Lines.Add("| Encoder | Ratio | Variant | Target kbps | Actual kbps | p95 packet | p99 packet | Max packet | PSNR-Y |")
$Lines.Add("|---|---:|---|---:|---:|---:|---:|---:|---:|")
foreach ($Result in $Results) {
    $Lines.Add(("| {0} | {1:N2} | {2} | {3} | {4:N1} | {5} | {6} | {7} | {8:N2} |" -f $Result.encoder, $Result.quality_ratio, $Result.variant, $Result.target_bitrate_kbps, $Result.actual_bitrate_kbps, $Result.packet_p95_bytes, $Result.packet_p99_bytes, $Result.largest_packet_bytes, $Result.luma_psnr_db))
}
$Lines | Set-Content -Encoding utf8 $ReportMarkdown
Write-Host "JSON report: $ReportJson"
Write-Host "Markdown report: $ReportMarkdown"
