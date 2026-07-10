# RustAdmin AV1 Screen Benchmark

This tool benchmarks RustAdmin's actual software AV1 and Windows hardware
H.264/H.265 paths with deterministic screen content. Every configuration
receives the same BGRA frames. Software AV1 uses `cpu-used=10`; hardware runs
use RustAdmin's `HwRamEncoder`, NV12 input, bitrate calculation, and serialized
video protocol messages.

The benchmark compares:

- current RustAdmin baseline
- anti-aliased screen detection
- adaptive sharpness
- automatic tiling
- IntraBC
- 16x16 active maps derived from luma changes
- active maps with rotating one-second static-region refresh
- active maps with a one-second full-frame warm-up
- active maps with one-block expansion and an eight-frame activity hold
- active maps with warm-up, scene-change recovery, and periodic refresh bursts
- all experimental controls combined
- full, 4096, 256, and 16-color source modes

Reports contain encoded and serialized protocol bitrate, encode latency
percentiles, real-time deadline misses, keyframe sizes, decoded frame count,
codec luma PSNR, end-to-end luma PSNR against the original full-color source,
and active-map coverage.

## Build on Windows

From the workspace root:

```powershell
.\rustdesk-tests\scripts\build_av1_screen_bench.ps1
```

The script uses `F:\DVS` when present. Set `RUSTDESK_WINDOWS_CODEC_ROOT` and
`CMAKE_PREFIX_PATH` first when the codec prefix is elsewhere.

The hardware build uses the same static CRT mode as RustAdmin. When present,
the script also copies `iconv-2.dll` from the codec prefix beside the test
binary.

## Synthetic Benchmark

Fast baseline run:

```powershell
.\rustdesk-tests\target\release\rustadmin-av1-screen-bench.exe run `
  --output-dir .\rustdesk-tests\results\av1-screen-quick
```

The default synthetic scene is screen-only. Use `--scene mixed` to include a
high-motion full-color region alongside the UI and text patterns.

Full screen-tool and color matrix:

```powershell
.\rustdesk-tests\target\release\rustadmin-av1-screen-bench.exe run `
  --width 1920 --height 1080 --frames 180 --fps 30 --repeat 3 `
  --colors full,4096,256,16 --variants all --save-samples `
  --output-dir .\rustdesk-tests\results\av1-screen-full
```

Run I444 separately because it uses a different AV1 profile:

```powershell
.\rustdesk-tests\target\release\rustadmin-av1-screen-bench.exe run `
  --i444 --frames 180 --repeat 3 --variants all `
  --output-dir .\rustdesk-tests\results\av1-screen-i444
```

## Record and Replay Real Desktop Activity

List the capture API's display order before recording:

```powershell
.\rustdesk-tests\target\release\rustadmin-av1-screen-bench.exe list-displays
```

Display indices are one-based and can differ from Windows Settings and
RustAdmin monitor labels. Use the reported origin and primary flag to identify
the intended display. On Windows, `--capture gdi` provides a deterministic CPU
corpus when the DXGI recorder produces no changed frames.

Start recording, then scroll text, move windows, and play a short video region:

```powershell
.\rustdesk-tests\target\release\rustadmin-av1-screen-bench.exe record `
  --display 2 --capture gdi `
  --output .\rustdesk-tests\results\desktop.rasc --frames 180 --fps 30
```

Recording stops with an error after 30 seconds without a changed capture frame.
Use `--idle-timeout-ms` to change that limit.

Replay the exact captured frames through every encoder configuration:

```powershell
.\rustdesk-tests\target\release\rustadmin-av1-screen-bench.exe run `
  --input .\rustdesk-tests\results\desktop.rasc --repeat 3 `
  --colors full,4096,256,16 --variants all --save-samples `
  --output-dir .\rustdesk-tests\results\av1-screen-recorded
```

`report.json` is intended for automated comparisons. `report.md` is the compact
human-readable summary. Optional BMP files show the source and decoded middle
frame for visual inspection.

Capture and BGRA-to-YUV conversion are excluded from encoder timing. Color
quantization and active-map generation are also performed outside the timed
region so the report isolates libaom encoding cost.

## RustAdmin Hardware Baseline

Replay a recorded corpus through the production H.264/H.265 RAM encoder path.
The `default` profile preserves RustAdmin's current encoder options; `hq`
changes only H.264/H.265 NVENC from preset `p4` to `p5`:

```powershell
.\rustdesk-tests\target\release\rustadmin-av1-screen-bench.exe run-hw `
  --input .\rustdesk-tests\results\desktop.rasc `
  --qualities 0.25,0.5,0.67,1.0 `
  --encoders h264_nvenc,hevc_nvenc --profiles default,hq --save-samples `
  --output-dir .\rustdesk-tests\results\hardware-baseline
```

The same production path can use a deterministic synthetic mixed-content
corpus without recording a desktop first:

```powershell
.\rustdesk-tests\target\release\rustadmin-av1-screen-bench.exe run-hw `
  --scene mixed --width 1920 --height 1080 --frames 90 --fps 30 `
  --qualities 0.25,0.5,0.67,1.0 `
  --encoders h264_nvenc,hevc_nvenc --profiles default,hq `
  --output-dir .\rustdesk-tests\results\hardware-mixed
```

The report includes configured and actual bitrate, protocol overhead, encode
latency, decoded-frame count, luma PSNR, and packet p50/p95/p99/max sizes.
Packet percentiles are important for interactive links where a tolerable
average bitrate can still contain large bursts.

## NVENC Tuning Matrix

The companion script exports the same corpus to tight BGRA, uses the configured
FFmpeg build for controlled NVENC variants, decodes each result, and measures
quality with this tool:

```powershell
.\rustdesk-tests\scripts\run_nvenc_screen_matrix.ps1 `
  -Corpus .\rustdesk-tests\results\desktop.rasc `
  -OutputDir .\rustdesk-tests\results\nvenc-core

.\rustdesk-tests\scripts\run_nvenc_screen_matrix.ps1 `
  -Matrix preset `
  -Corpus .\rustdesk-tests\results\desktop.rasc `
  -OutputDir .\rustdesk-tests\results\nvenc-presets
```

The `core` matrix isolates low-latency tune, spatial AQ, and one-, two-, and
four-frame VBV controls. The `preset` matrix compares NVENC `p4`, `p5`, `p6`,
and `p5` with one-frame VBV. No tuning result should be applied to RustAdmin
until it improves quality without unacceptable packet bursts or encode time on
more than one representative corpus.
