mod corpus;
#[cfg(target_os = "windows")]
mod hardware;
mod yuv;

use corpus::{
    display_inventory, record_desktop, ColorMode, ColorQuantizer, Corpus, SyntheticScene,
};
use hbb_common::{
    bytes::Bytes,
    message_proto::{EncodedVideoFrame, Message},
    protobuf::Message as ProtobufMessage,
};
#[cfg(target_os = "windows")]
use scrap::hwcodec::HwEncoderProfile;
use scrap::{
    aom::{AomDecoder, AomEncoder, AomEncoderConfig, AomEncoderTuning, AomScreenDetectionMode},
    codec::EncoderApi,
    STRIDE_ALIGN,
};
use serde::Serialize;
use std::{
    env,
    error::Error,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use yuv::{write_bgra_bmp, write_decoded_bmp, QualityAccumulator, YuvBuffer};

const HELP: &str = r#"RustAdmin deterministic AV1 screen benchmark

Usage:
  rustadmin-av1-screen-bench run [options]
  rustadmin-av1-screen-bench run-hw [options]
  rustadmin-av1-screen-bench record [options]
  rustadmin-av1-screen-bench export-bgra [options]
  rustadmin-av1-screen-bench analyze-yuv420 [options]
  rustadmin-av1-screen-bench list-displays

Run options:
  --input PATH            Replay a recorded BGRA corpus instead of synthetic input
  --output-dir PATH       Report directory (default: timestamped results directory)
  --width N               Synthetic width [default: 1280]
  --height N              Synthetic height [default: 720]
  --frames N              Synthetic frame count [default: 90]
  --fps N                 Synthetic frame rate [default: 30]
  --scene NAME            Synthetic scene: screen or mixed [default: screen]
  --repeat N              Repeat count per configuration [default: 1]
  --quality N             RustAdmin quality ratio [default: 1.0]
  --colors LIST           full,4096,256,16 [default: full]
  --variants LIST         baseline,screen-aa,adaptive-sharpness,auto-tiles,
                          intrabc,active-map,active-map-refresh,
                          active-map-warmup,active-map-hold,
                          active-map-recovery,combined
                          [default: all]
  --i444                  Encode AV1 profile 1 with I444 input
  --save-samples          Save source and decoded middle-frame BMP files

Hardware run options (Windows):
  --input PATH            Replay a recorded BGRA corpus instead of synthetic input
  --output-dir PATH       Report directory (default: timestamped results directory)
  --width N               Synthetic width [default: 1280]
  --height N              Synthetic height [default: 720]
  --frames N              Synthetic frame count [default: 90]
  --fps N                 Synthetic frame rate [default: 30]
  --scene NAME            Synthetic scene: screen or mixed [default: screen]
  --repeat N              Repeat count per configuration [default: 1]
  --qualities LIST        RustAdmin quality ratios [default: 0.25,0.5,0.67,1.0]
  --encoders LIST         FFmpeg encoder names [default: h264_nvenc,hevc_nvenc]
  --profiles LIST         Encoder profiles: default,hq [default: default,hq]
  --save-samples          Save source and decoded middle-frame BMP files

Record options:
  --output PATH           Corpus path [default: av1-screen-corpus.rasc]
  --frames N              Captured changed frames [default: 180]
  --fps N                 Nominal polling rate [default: 30]
  --idle-timeout-ms N     Stop when capture is idle [default: 30000]
  --display N             One-based display index [default: primary]
  --capture NAME          Capture backend: auto or gdi [default: auto]
  --start-delay-ms N      Wait before reading the first frame [default: 0]

BGRA export options:
  --input PATH            Recorded corpus path (required)
  --output PATH           Headerless tight BGRA output path (required)

YUV420 analysis options:
  --input PATH            Recorded BGRA corpus path (required)
  --decoded PATH          Headerless tight decoded YUV420P path (required)
  --output PATH           JSON analysis path (required)
"#;

type AnyResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    Baseline,
    ScreenAa,
    AdaptiveSharpness,
    AutoTiles,
    Intrabc,
    ActiveMap,
    ActiveMapRefresh,
    ActiveMapWarmup,
    ActiveMapHold,
    ActiveMapRecovery,
    Combined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveMapStrategy {
    Naive,
    RotatingRefresh,
    Warmup,
    HoldExpanded,
    Recovery,
}

impl Variant {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "baseline" => Some(Self::Baseline),
            "screen-aa" => Some(Self::ScreenAa),
            "adaptive-sharpness" => Some(Self::AdaptiveSharpness),
            "auto-tiles" => Some(Self::AutoTiles),
            "intrabc" => Some(Self::Intrabc),
            "active-map" => Some(Self::ActiveMap),
            "active-map-refresh" => Some(Self::ActiveMapRefresh),
            "active-map-warmup" => Some(Self::ActiveMapWarmup),
            "active-map-hold" => Some(Self::ActiveMapHold),
            "active-map-recovery" => Some(Self::ActiveMapRecovery),
            "combined" => Some(Self::Combined),
            _ => None,
        }
    }

    fn all() -> Vec<Self> {
        vec![
            Self::Baseline,
            Self::ScreenAa,
            Self::AdaptiveSharpness,
            Self::AutoTiles,
            Self::Intrabc,
            Self::ActiveMap,
            Self::ActiveMapRefresh,
            Self::ActiveMapWarmup,
            Self::ActiveMapHold,
            Self::ActiveMapRecovery,
            Self::Combined,
        ]
    }

    fn label(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::ScreenAa => "screen-aa",
            Self::AdaptiveSharpness => "adaptive-sharpness",
            Self::AutoTiles => "auto-tiles",
            Self::Intrabc => "intrabc",
            Self::ActiveMap => "active-map",
            Self::ActiveMapRefresh => "active-map-refresh",
            Self::ActiveMapWarmup => "active-map-warmup",
            Self::ActiveMapHold => "active-map-hold",
            Self::ActiveMapRecovery => "active-map-recovery",
            Self::Combined => "combined",
        }
    }

    fn tuning(self) -> AomEncoderTuning {
        let mut tuning = AomEncoderTuning {
            cpu_used: Some(10),
            ..Default::default()
        };
        match self {
            Self::Baseline => {}
            Self::ScreenAa => {
                tuning.screen_detection = Some(AomScreenDetectionMode::AntialiasingAware);
            }
            Self::AdaptiveSharpness => tuning.adaptive_sharpness = Some(4),
            Self::AutoTiles => tuning.auto_tiles = true,
            Self::Intrabc => tuning.enable_intrabc = true,
            Self::ActiveMap => {}
            Self::ActiveMapRefresh => {}
            Self::ActiveMapWarmup => {}
            Self::ActiveMapHold => {}
            Self::ActiveMapRecovery => {}
            Self::Combined => {
                tuning.screen_detection = Some(AomScreenDetectionMode::AntialiasingAware);
                tuning.adaptive_sharpness = Some(4);
                tuning.auto_tiles = true;
                tuning.enable_intrabc = true;
            }
        }
        tuning
    }

    fn active_map_strategy(self) -> Option<ActiveMapStrategy> {
        match self {
            Self::ActiveMap => Some(ActiveMapStrategy::Naive),
            Self::ActiveMapRefresh => Some(ActiveMapStrategy::RotatingRefresh),
            Self::ActiveMapWarmup => Some(ActiveMapStrategy::Warmup),
            Self::ActiveMapHold => Some(ActiveMapStrategy::HoldExpanded),
            Self::ActiveMapRecovery | Self::Combined => Some(ActiveMapStrategy::Recovery),
            _ => None,
        }
    }
}

struct ActiveMapController {
    strategy: ActiveMapStrategy,
    cols: usize,
    rows: usize,
    fps: usize,
    hold: Vec<u8>,
    scratch: Vec<u8>,
    full_active_remaining: usize,
}

impl ActiveMapController {
    const HOLD_FRAMES: u8 = 8;
    const PERIODIC_RECOVERY_SECONDS: usize = 5;
    const PERIODIC_RECOVERY_FRAMES: usize = 5;
    const SCENE_CHANGE_PERCENT: usize = 60;

    fn new(strategy: ActiveMapStrategy, cols: usize, rows: usize, fps: usize) -> Self {
        let map_len = cols.saturating_mul(rows);
        Self {
            strategy,
            cols,
            rows,
            fps: fps.max(1),
            hold: vec![0; map_len],
            scratch: vec![0; map_len],
            full_active_remaining: 0,
        }
    }

    fn apply(
        &mut self,
        active_map: &mut [u8],
        frame_index: usize,
        detected_active: usize,
    ) -> usize {
        debug_assert_eq!(active_map.len(), self.cols.saturating_mul(self.rows));
        match self.strategy {
            ActiveMapStrategy::Naive => {}
            ActiveMapStrategy::RotatingRefresh => {
                apply_rotating_active_refresh(active_map, frame_index, self.fps);
            }
            ActiveMapStrategy::Warmup => {
                if frame_index < self.fps {
                    active_map.fill(1);
                }
            }
            ActiveMapStrategy::HoldExpanded => self.expand_and_hold(active_map),
            ActiveMapStrategy::Recovery => {
                self.expand_and_hold(active_map);

                if frame_index >= self.fps
                    && detected_active.saturating_mul(100)
                        >= active_map.len().saturating_mul(Self::SCENE_CHANGE_PERCENT)
                {
                    self.full_active_remaining = self.full_active_remaining.max(self.fps / 2);
                }

                let recovery_period = self.fps.saturating_mul(Self::PERIODIC_RECOVERY_SECONDS);
                if frame_index > 0 && frame_index.is_multiple_of(recovery_period) {
                    self.full_active_remaining = self
                        .full_active_remaining
                        .max(Self::PERIODIC_RECOVERY_FRAMES);
                }

                if frame_index < self.fps || self.full_active_remaining > 0 {
                    active_map.fill(1);
                    self.full_active_remaining = self.full_active_remaining.saturating_sub(1);
                }
            }
        }
        active_map.iter().filter(|value| **value != 0).count()
    }

    fn expand_and_hold(&mut self, active_map: &mut [u8]) {
        self.scratch.fill(0);
        for row in 0..self.rows {
            for col in 0..self.cols {
                let index = row * self.cols + col;
                if active_map[index] == 0 {
                    continue;
                }
                let top = row.saturating_sub(1);
                let bottom = (row + 1).min(self.rows.saturating_sub(1));
                let left = col.saturating_sub(1);
                let right = (col + 1).min(self.cols.saturating_sub(1));
                for neighbor_row in top..=bottom {
                    let start = neighbor_row * self.cols + left;
                    let end = neighbor_row * self.cols + right;
                    self.scratch[start..=end].fill(1);
                }
            }
        }

        for ((active, hold), expanded) in active_map
            .iter_mut()
            .zip(self.hold.iter_mut())
            .zip(self.scratch.iter())
        {
            if *expanded != 0 {
                *hold = Self::HOLD_FRAMES;
            } else {
                *hold = hold.saturating_sub(1);
            }
            *active = u8::from(*hold != 0);
        }
    }
}

#[derive(Debug)]
struct RunOptions {
    input: Option<PathBuf>,
    output_dir: PathBuf,
    width: usize,
    height: usize,
    frames: usize,
    fps: u32,
    scene: SyntheticScene,
    repeat: usize,
    quality: f32,
    colors: Vec<ColorMode>,
    variants: Vec<Variant>,
    i444: bool,
    save_samples: bool,
}

#[derive(Debug)]
struct RecordOptions {
    output: PathBuf,
    frames: usize,
    fps: u32,
    idle_timeout_ms: u64,
    display: Option<usize>,
    force_gdi: bool,
    start_delay_ms: u64,
}

#[derive(Debug)]
struct ExportOptions {
    input: PathBuf,
    output: PathBuf,
}

#[derive(Debug)]
struct AnalyzeOptions {
    input: PathBuf,
    decoded: PathBuf,
    output: PathBuf,
}

#[derive(Serialize)]
struct Yuv420Analysis {
    corpus: String,
    decoded: String,
    width: usize,
    height: usize,
    source_frames: usize,
    decoded_frames: usize,
    luma_psnr_db: f64,
}

enum Command {
    Run(RunOptions),
    #[cfg(target_os = "windows")]
    RunHardware(hardware::HardwareOptions),
    Record(RecordOptions),
    ExportBgra(ExportOptions),
    AnalyzeYuv420(AnalyzeOptions),
    ListDisplays,
    Help,
}

#[derive(Serialize)]
struct Report {
    generated_unix_ms: u128,
    corpus: String,
    width: usize,
    height: usize,
    fps: u32,
    source_frames: usize,
    repeat: usize,
    quality: f32,
    chroma: String,
    cpu_used: u32,
    results: Vec<BenchmarkResult>,
}

#[derive(Serialize)]
struct BenchmarkResult {
    variant: String,
    color_mode: String,
    error: Option<String>,
    input_frames: usize,
    encoded_packets: usize,
    decoded_frames: usize,
    keyframes: usize,
    payload_bytes: u64,
    protocol_bytes: u64,
    keyframe_bytes: u64,
    bitrate_kbps: f64,
    protocol_bitrate_kbps: f64,
    encode_mean_ms: f64,
    encode_p50_ms: f64,
    encode_p95_ms: f64,
    encode_p99_ms: f64,
    over_16_67_ms: usize,
    over_33_33_ms: usize,
    codec_luma_psnr_db: f64,
    end_to_end_luma_psnr_db: f64,
    mean_active_blocks_pct: Option<f64>,
}

impl BenchmarkResult {
    fn failed(variant: Variant, color: ColorMode, error: &dyn Error) -> Self {
        Self {
            variant: variant.label().to_owned(),
            color_mode: color.label().to_owned(),
            error: Some(error.to_string()),
            input_frames: 0,
            encoded_packets: 0,
            decoded_frames: 0,
            keyframes: 0,
            payload_bytes: 0,
            protocol_bytes: 0,
            keyframe_bytes: 0,
            bitrate_kbps: 0.0,
            protocol_bitrate_kbps: 0.0,
            encode_mean_ms: 0.0,
            encode_p50_ms: 0.0,
            encode_p95_ms: 0.0,
            encode_p99_ms: 0.0,
            over_16_67_ms: 0,
            over_33_33_ms: 0,
            codec_luma_psnr_db: 0.0,
            end_to_end_luma_psnr_db: 0.0,
            mean_active_blocks_pct: None,
        }
    }
}

fn main() {
    let result = match parse_command(env::args().skip(1).collect()) {
        Ok(Command::Help) => {
            print!("{HELP}");
            Ok(())
        }
        Ok(Command::Record(options)) => run_record(options),
        Ok(Command::Run(options)) => run_benchmark(options),
        #[cfg(target_os = "windows")]
        Ok(Command::RunHardware(options)) => hardware::run(options),
        Ok(Command::ExportBgra(options)) => run_export_bgra(options),
        Ok(Command::AnalyzeYuv420(options)) => run_analyze_yuv420(options),
        Ok(Command::ListDisplays) => run_list_displays(),
        Err(error) => {
            eprintln!("{error}\n\n{HELP}");
            Err(io::Error::new(io::ErrorKind::InvalidInput, error).into())
        }
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run_analyze_yuv420(options: AnalyzeOptions) -> AnyResult<()> {
    if let Some(parent) = options
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut corpus = Corpus::recorded(&options.input)?;
    let metadata = corpus.metadata().clone();
    let luma_bytes = metadata
        .width
        .checked_mul(metadata.height)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "luma size overflow"))?;
    let chroma_width = metadata.width.div_ceil(2);
    let chroma_height = metadata.height.div_ceil(2);
    let chroma_bytes = chroma_width
        .checked_mul(chroma_height)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "chroma size overflow"))?;
    let decoded_frame_bytes = luma_bytes
        .checked_add(chroma_bytes.saturating_mul(2))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "YUV frame size overflow"))?;
    let decoded_len = File::open(&options.decoded)?.metadata()?.len() as usize;
    if decoded_len == 0 || !decoded_len.is_multiple_of(decoded_frame_bytes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "decoded YUV420P file has a partial frame",
        )
        .into());
    }
    let decoded_frames = decoded_len / decoded_frame_bytes;
    if decoded_frames != metadata.frames {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "decoded frame count mismatch: expected {}, found {decoded_frames}",
                metadata.frames
            ),
        )
        .into());
    }

    let bgra_bytes = luma_bytes
        .checked_mul(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "BGRA size overflow"))?;
    let format = scrap::EncodeYuvFormat {
        pixfmt: scrap::Pixfmt::I420,
        w: metadata.width,
        h: metadata.height,
        stride: vec![metadata.width, chroma_width, chroma_width],
        u: luma_bytes,
        v: luma_bytes + chroma_bytes,
    };
    let mut source_yuv = YuvBuffer::new(format)?;
    let mut bgra = vec![0u8; bgra_bytes];
    let mut decoded_yuv = vec![0u8; decoded_frame_bytes];
    let mut decoded = File::open(&options.decoded)?;
    let mut quality = QualityAccumulator::default();
    corpus.reset()?;
    for frame_index in 0..metadata.frames {
        corpus.read_frame(frame_index, &mut bgra)?;
        source_yuv.convert_bgra(&bgra)?;
        decoded.read_exact(&mut decoded_yuv)?;
        quality.compare_luma(&source_yuv, &decoded_yuv[..luma_bytes], metadata.width)?;
    }
    let analysis = Yuv420Analysis {
        corpus: metadata.name,
        decoded: options.decoded.display().to_string(),
        width: metadata.width,
        height: metadata.height,
        source_frames: metadata.frames,
        decoded_frames,
        luma_psnr_db: quality.psnr_y(),
    };
    serde_json::to_writer_pretty(File::create(&options.output)?, &analysis)?;
    println!(
        "Analyzed {} decoded YUV420P frames: PSNR-Y {:.2} dB; JSON: {}",
        decoded_frames,
        analysis.luma_psnr_db,
        options.output.display()
    );
    Ok(())
}

fn run_export_bgra(options: ExportOptions) -> AnyResult<()> {
    if let Some(parent) = options
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut corpus = Corpus::recorded(&options.input)?;
    let metadata = corpus.metadata().clone();
    let frame_bytes = metadata
        .width
        .checked_mul(metadata.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "BGRA size overflow"))?;
    let mut bgra = vec![0u8; frame_bytes];
    let mut output = File::create(&options.output)?;
    corpus.reset()?;
    for frame_index in 0..metadata.frames {
        corpus.read_frame(frame_index, &mut bgra)?;
        output.write_all(&bgra)?;
    }
    output.flush()?;
    println!(
        "Exported {} tight BGRA frames: {}x{} at {} fps to {}",
        metadata.frames,
        metadata.width,
        metadata.height,
        metadata.fps,
        options.output.display()
    );
    Ok(())
}

fn run_record(options: RecordOptions) -> AnyResult<()> {
    let displays = display_inventory()?;
    let selected = options
        .display
        .and_then(|index| displays.iter().find(|display| display.index == index))
        .or_else(|| displays.iter().find(|display| display.primary))
        .or_else(|| displays.first())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no displays found"))?;
    println!(
        "Recording {} changed frames from display {}: {} ({}x{} at {},{}, primary={}) to {}",
        options.frames,
        selected.index,
        selected.name,
        selected.width,
        selected.height,
        selected.origin.0,
        selected.origin.1,
        selected.primary,
        options.output.display()
    );
    record_desktop(
        &options.output,
        options.frames,
        options.fps,
        Duration::from_millis(options.idle_timeout_ms),
        options.display,
        options.force_gdi,
        Duration::from_millis(options.start_delay_ms),
    )?;
    println!("Corpus written: {}", options.output.display());
    Ok(())
}

fn run_list_displays() -> AnyResult<()> {
    let displays = display_inventory()?;
    if displays.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no displays found").into());
    }
    for display in displays {
        println!(
            "{}: {} | {}x{} | origin {},{} | primary={}",
            display.index,
            display.name,
            display.width,
            display.height,
            display.origin.0,
            display.origin.1,
            display.primary
        );
    }
    Ok(())
}

fn run_benchmark(options: RunOptions) -> AnyResult<()> {
    fs::create_dir_all(&options.output_dir)?;
    let mut corpus = if let Some(path) = &options.input {
        Corpus::recorded(path)?
    } else {
        Corpus::synthetic(
            options.width,
            options.height,
            options.fps,
            options.frames,
            options.scene,
        )?
    };
    let metadata = corpus.metadata().clone();
    let quantizer = ColorQuantizer::new();
    let mut results = Vec::with_capacity(options.colors.len() * options.variants.len());

    println!(
        "AV1 screen benchmark: {}x{}, {} frames, {} fps, cpu-used=10, {}",
        metadata.width,
        metadata.height,
        metadata.frames,
        metadata.fps,
        if options.i444 { "I444" } else { "I420" }
    );
    for color in &options.colors {
        for variant in &options.variants {
            print!("  {:>20} / {:>4}: ", variant.label(), color.label());
            io::stdout().flush()?;
            match run_configuration(
                &mut corpus,
                &metadata,
                *variant,
                *color,
                &quantizer,
                &options,
            ) {
                Ok(result) => {
                    println!(
                        "{:8.1} kbps, p95 {:6.2} ms, PSNR-Y {:5.2} dB",
                        result.protocol_bitrate_kbps,
                        result.encode_p95_ms,
                        result.end_to_end_luma_psnr_db
                    );
                    results.push(result);
                }
                Err(error) => {
                    println!("FAILED: {error}");
                    results.push(BenchmarkResult::failed(*variant, *color, error.as_ref()));
                }
            }
        }
    }

    let report = Report {
        generated_unix_ms: unix_millis(),
        corpus: metadata.name,
        width: metadata.width,
        height: metadata.height,
        fps: metadata.fps,
        source_frames: metadata.frames,
        repeat: options.repeat,
        quality: options.quality,
        chroma: if options.i444 { "I444" } else { "I420" }.to_owned(),
        cpu_used: 10,
        results,
    };
    let json_path = options.output_dir.join("report.json");
    let markdown_path = options.output_dir.join("report.md");
    serde_json::to_writer_pretty(File::create(&json_path)?, &report)?;
    write_markdown(&markdown_path, &report)?;
    println!("JSON report: {}", json_path.display());
    println!("Markdown report: {}", markdown_path.display());
    Ok(())
}

fn run_configuration(
    corpus: &mut Corpus,
    metadata: &corpus::CorpusMetadata,
    variant: Variant,
    color: ColorMode,
    quantizer: &ColorQuantizer,
    options: &RunOptions,
) -> AnyResult<BenchmarkResult> {
    let config = AomEncoderConfig {
        width: metadata.width as u32,
        height: metadata.height as u32,
        quality: options.quality,
        keyframe_interval: None,
    };
    let frame_bytes = metadata
        .width
        .checked_mul(metadata.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "BGRA size overflow"))?;
    let mut bgra = vec![0u8; frame_bytes];
    let mut encode_times_ns = Vec::with_capacity(metadata.frames * options.repeat);
    let mut payload_bytes = 0u64;
    let mut protocol_bytes = 0u64;
    let mut keyframe_bytes = 0u64;
    let mut encoded_packets = 0usize;
    let mut decoded_frames = 0usize;
    let mut keyframes = 0usize;
    let mut total_duration_ms = 0u64;
    let mut codec_quality = QualityAccumulator::default();
    let mut end_to_end_quality = QualityAccumulator::default();
    let mut total_active_blocks = 0u64;
    let mut total_map_blocks = 0u64;

    for repeat in 0..options.repeat {
        corpus.reset()?;
        let mut encoder = AomEncoder::new_with_tuning(config, options.i444, variant.tuning())?;
        let mut decoder = AomDecoder::new()?;
        let mut yuv = YuvBuffer::new(encoder.yuvfmt())?;
        let mut original_yuv = YuvBuffer::new(encoder.yuvfmt())?;
        let mut previous_yuv = vec![0u8; yuv.data.len()];
        let (map_cols, map_rows) = encoder.active_map_dimensions();
        let mut active_map = vec![1u8; map_cols.saturating_mul(map_rows)];
        let mut active_map_controller = variant.active_map_strategy().map(|strategy| {
            ActiveMapController::new(strategy, map_cols, map_rows, metadata.fps as usize)
        });
        let mut last_timestamp = Duration::ZERO;

        for frame_index in 0..metadata.frames {
            let timestamp = corpus.read_frame(frame_index, &mut bgra)?;
            last_timestamp = timestamp;
            original_yuv.convert_bgra(&bgra)?;
            quantizer.apply(&mut bgra, color);
            yuv.convert_bgra(&bgra)?;

            if let Some(controller) = active_map_controller.as_mut() {
                let detected_active = yuv.update_active_map(
                    if frame_index == 0 {
                        None
                    } else {
                        Some(&previous_yuv)
                    },
                    &mut active_map,
                )?;
                let active = controller.apply(&mut active_map, frame_index, detected_active);
                encoder.set_active_map(&active_map)?;
                total_active_blocks += active as u64;
                total_map_blocks += active_map.len() as u64;
                previous_yuv.copy_from_slice(&yuv.data);
            }

            if options.save_samples
                && repeat == 0
                && frame_index == metadata.frames / 2
                && variant == Variant::Baseline
            {
                write_bgra_bmp(
                    &options
                        .output_dir
                        .join(format!("source-{}.bmp", color.label())),
                    &bgra,
                    metadata.width,
                    metadata.height,
                )?;
            }

            let started = Instant::now();
            let encoded = encoder.encode(timestamp.as_millis() as i64, &yuv.data, STRIDE_ALIGN)?;
            encode_times_ns.push(started.elapsed().as_nanos() as u64);

            let mut protocol_frames = Vec::new();
            for packet in encoded {
                payload_bytes += packet.data.len() as u64;
                encoded_packets += 1;
                if packet.key {
                    keyframes += 1;
                    keyframe_bytes += packet.data.len() as u64;
                }
                protocol_frames.push(EncodedVideoFrame {
                    data: Bytes::copy_from_slice(packet.data),
                    key: packet.key,
                    pts: packet.pts,
                    ..Default::default()
                });

                for decoded in decoder.decode(packet.data)? {
                    codec_quality.compare(&yuv, &decoded)?;
                    end_to_end_quality.compare(&original_yuv, &decoded)?;
                    decoded_frames += 1;
                    if options.save_samples && repeat == 0 && frame_index == metadata.frames / 2 {
                        write_decoded_bmp(
                            &options.output_dir.join(format!(
                                "decoded-{}-{}.bmp",
                                variant.label(),
                                color.label()
                            )),
                            &decoded,
                        )?;
                    }
                }
            }
            if !protocol_frames.is_empty() {
                let video_frame = AomEncoder::create_video_frame(protocol_frames);
                let mut message = Message::new();
                message.set_video_frame(video_frame);
                protocol_bytes += message.compute_size();
            }
        }
        let nominal_tail = 1_000u64 / metadata.fps.max(1) as u64;
        total_duration_ms = total_duration_ms
            .saturating_add(last_timestamp.as_millis() as u64)
            .saturating_add(nominal_tail);
    }

    if decoded_frames == 0 || encode_times_ns.is_empty() {
        return Err(io::Error::other("encoder produced no decodable frames").into());
    }
    encode_times_ns.sort_unstable();
    let total_ns: u128 = encode_times_ns.iter().map(|value| *value as u128).sum();
    let duration_seconds = total_duration_ms.max(1) as f64 / 1_000.0;
    Ok(BenchmarkResult {
        variant: variant.label().to_owned(),
        color_mode: color.label().to_owned(),
        error: None,
        input_frames: metadata.frames * options.repeat,
        encoded_packets,
        decoded_frames,
        keyframes,
        payload_bytes,
        protocol_bytes,
        keyframe_bytes,
        bitrate_kbps: payload_bytes as f64 * 8.0 / duration_seconds / 1_000.0,
        protocol_bitrate_kbps: protocol_bytes as f64 * 8.0 / duration_seconds / 1_000.0,
        encode_mean_ms: total_ns as f64 / encode_times_ns.len() as f64 / 1_000_000.0,
        encode_p50_ms: percentile_ms(&encode_times_ns, 50),
        encode_p95_ms: percentile_ms(&encode_times_ns, 95),
        encode_p99_ms: percentile_ms(&encode_times_ns, 99),
        over_16_67_ms: encode_times_ns
            .iter()
            .filter(|value| **value > 16_670_000)
            .count(),
        over_33_33_ms: encode_times_ns
            .iter()
            .filter(|value| **value > 33_330_000)
            .count(),
        codec_luma_psnr_db: codec_quality.psnr_y(),
        end_to_end_luma_psnr_db: end_to_end_quality.psnr_y(),
        mean_active_blocks_pct: if total_map_blocks == 0 {
            None
        } else {
            Some(total_active_blocks as f64 * 100.0 / total_map_blocks as f64)
        },
    })
}

fn write_markdown(path: &Path, report: &Report) -> io::Result<()> {
    let mut file = File::create(path)?;
    writeln!(file, "# RustAdmin AV1 Screen Benchmark")?;
    writeln!(file)?;
    writeln!(file, "- Corpus: `{}`", report.corpus)?;
    writeln!(file, "- Frame: `{}x{}`", report.width, report.height)?;
    writeln!(
        file,
        "- Input: `{} fps`, `{} frames`",
        report.fps, report.source_frames
    )?;
    writeln!(file, "- Chroma: `{}`", report.chroma)?;
    writeln!(file, "- Quality ratio: `{}`", report.quality)?;
    writeln!(file, "- CPU-used: `{}`", report.cpu_used)?;
    writeln!(file)?;
    writeln!(
        file,
        "| Variant | Colors | Protocol kbps | p50 ms | p95 ms | p99 ms | >16.67 | >33.33 | Codec PSNR-Y | E2E PSNR-Y | Active | Error |"
    )?;
    writeln!(
        file,
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|"
    )?;
    for result in &report.results {
        writeln!(
            file,
            "| {} | {} | {:.1} | {:.2} | {:.2} | {:.2} | {} | {} | {:.2} | {:.2} | {} | {} |",
            result.variant,
            result.color_mode,
            result.protocol_bitrate_kbps,
            result.encode_p50_ms,
            result.encode_p95_ms,
            result.encode_p99_ms,
            result.over_16_67_ms,
            result.over_33_33_ms,
            result.codec_luma_psnr_db,
            result.end_to_end_luma_psnr_db,
            result
                .mean_active_blocks_pct
                .map(|value| format!("{value:.1}%"))
                .unwrap_or_else(|| "-".to_owned()),
            result.error.as_deref().unwrap_or("-")
        )?;
    }
    Ok(())
}

fn parse_command(arguments: Vec<String>) -> Result<Command, String> {
    if arguments.is_empty() {
        return Ok(Command::Run(default_run_options()));
    }
    if matches!(arguments[0].as_str(), "-h" | "--help" | "help") {
        return Ok(Command::Help);
    }
    match arguments[0].as_str() {
        "run" => parse_run_options(&arguments[1..]).map(Command::Run),
        #[cfg(target_os = "windows")]
        "run-hw" => parse_hardware_options(&arguments[1..]).map(Command::RunHardware),
        #[cfg(not(target_os = "windows"))]
        "run-hw" => Err("run-hw is available only on Windows".to_owned()),
        "record" => parse_record_options(&arguments[1..]).map(Command::Record),
        "export-bgra" => parse_export_options(&arguments[1..]).map(Command::ExportBgra),
        "analyze-yuv420" => parse_analyze_options(&arguments[1..]).map(Command::AnalyzeYuv420),
        "list-displays" if arguments.len() == 1 => Ok(Command::ListDisplays),
        unknown => Err(format!("unknown command: {unknown}")),
    }
}

#[cfg(target_os = "windows")]
fn parse_hardware_options(arguments: &[String]) -> Result<hardware::HardwareOptions, String> {
    let mut input = None;
    let mut output_dir =
        PathBuf::from("results").join(format!("hardware-screen-{}", unix_millis()));
    let mut width = 1280usize;
    let mut height = 720usize;
    let mut frames = 90usize;
    let mut fps = 30u32;
    let mut scene = SyntheticScene::Screen;
    let mut repeat = 1usize;
    let mut qualities = vec![0.25, 0.5, 0.67, 1.0];
    let mut encoders = vec!["h264_nvenc".to_owned(), "hevc_nvenc".to_owned()];
    let mut profiles = vec![HwEncoderProfile::Default, HwEncoderProfile::HighQuality];
    let mut save_samples = false;
    let mut index = 0usize;
    while index < arguments.len() {
        if arguments[index] == "--save-samples" {
            save_samples = true;
            index += 1;
            continue;
        }
        let (key, value, consumed_next) = option_value(arguments, index)?;
        match key {
            "--input" => input = Some(PathBuf::from(value)),
            "--output-dir" => output_dir = PathBuf::from(value),
            "--width" => width = parse_number(key, value)?,
            "--height" => height = parse_number(key, value)?,
            "--frames" => frames = parse_number(key, value)?,
            "--fps" => fps = parse_number(key, value)?,
            "--scene" => {
                scene = SyntheticScene::parse(value)
                    .ok_or_else(|| format!("unknown synthetic scene: {value}"))?
            }
            "--repeat" => repeat = parse_number(key, value)?,
            "--qualities" => qualities = parse_float_list(key, value)?,
            "--encoders" => encoders = parse_string_list(key, value)?,
            "--profiles" => profiles = parse_hardware_profiles(value)?,
            _ => return Err(format!("unknown hardware run option: {key}")),
        }
        index += 1 + usize::from(consumed_next);
    }
    if width == 0
        || height == 0
        || frames == 0
        || fps == 0
        || repeat == 0
        || qualities.is_empty()
        || qualities
            .iter()
            .any(|quality| !quality.is_finite() || *quality <= 0.0)
        || encoders.is_empty()
        || profiles.is_empty()
    {
        return Err(
            "hardware dimensions, timing, repeat, qualities, encoders, and profiles must be positive"
                .to_owned(),
        );
    }
    Ok(hardware::HardwareOptions {
        input,
        output_dir,
        width,
        height,
        frames,
        fps,
        scene,
        repeat,
        qualities,
        encoders,
        profiles,
        save_samples,
    })
}

#[cfg(target_os = "windows")]
fn parse_hardware_profiles(value: &str) -> Result<Vec<HwEncoderProfile>, String> {
    let mut profiles = Vec::new();
    for item in value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let profile = match item {
            "default" => HwEncoderProfile::Default,
            "hq" => HwEncoderProfile::HighQuality,
            _ => return Err(format!("unknown hardware encoder profile: {item}")),
        };
        if !profiles.contains(&profile) {
            profiles.push(profile);
        }
    }
    if profiles.is_empty() {
        return Err("hardware encoder profile list must not be empty".to_owned());
    }
    Ok(profiles)
}

fn parse_export_options(arguments: &[String]) -> Result<ExportOptions, String> {
    let mut input = None;
    let mut output = None;
    let mut index = 0usize;
    while index < arguments.len() {
        let (key, value, consumed_next) = option_value(arguments, index)?;
        match key {
            "--input" => input = Some(PathBuf::from(value)),
            "--output" => output = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown BGRA export option: {key}")),
        }
        index += 1 + usize::from(consumed_next);
    }
    Ok(ExportOptions {
        input: input.ok_or_else(|| "export-bgra requires --input".to_owned())?,
        output: output.ok_or_else(|| "export-bgra requires --output".to_owned())?,
    })
}

fn parse_analyze_options(arguments: &[String]) -> Result<AnalyzeOptions, String> {
    let mut input = None;
    let mut decoded = None;
    let mut output = None;
    let mut index = 0usize;
    while index < arguments.len() {
        let (key, value, consumed_next) = option_value(arguments, index)?;
        match key {
            "--input" => input = Some(PathBuf::from(value)),
            "--decoded" => decoded = Some(PathBuf::from(value)),
            "--output" => output = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown YUV420 analysis option: {key}")),
        }
        index += 1 + usize::from(consumed_next);
    }
    Ok(AnalyzeOptions {
        input: input.ok_or_else(|| "analyze-yuv420 requires --input".to_owned())?,
        decoded: decoded.ok_or_else(|| "analyze-yuv420 requires --decoded".to_owned())?,
        output: output.ok_or_else(|| "analyze-yuv420 requires --output".to_owned())?,
    })
}

fn default_run_options() -> RunOptions {
    RunOptions {
        input: None,
        output_dir: default_output_dir(),
        width: 1280,
        height: 720,
        frames: 90,
        fps: 30,
        scene: SyntheticScene::Screen,
        repeat: 1,
        quality: 1.0,
        colors: vec![ColorMode::Full],
        variants: Variant::all(),
        i444: false,
        save_samples: false,
    }
}

fn parse_run_options(arguments: &[String]) -> Result<RunOptions, String> {
    let mut options = default_run_options();
    let mut index = 0usize;
    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--i444" => options.i444 = true,
            "--save-samples" => options.save_samples = true,
            "-h" | "--help" => return Err(HELP.to_owned()),
            _ => {
                let (key, value, consumed_next) = option_value(arguments, index)?;
                match key {
                    "--input" => options.input = Some(PathBuf::from(value)),
                    "--output-dir" => options.output_dir = PathBuf::from(value),
                    "--width" => options.width = parse_number(key, value)?,
                    "--height" => options.height = parse_number(key, value)?,
                    "--frames" => options.frames = parse_number(key, value)?,
                    "--fps" => options.fps = parse_number(key, value)?,
                    "--scene" => {
                        options.scene = SyntheticScene::parse(value)
                            .ok_or_else(|| format!("unknown synthetic scene: {value}"))?
                    }
                    "--repeat" => options.repeat = parse_number(key, value)?,
                    "--quality" => options.quality = parse_number(key, value)?,
                    "--colors" => options.colors = parse_colors(value)?,
                    "--variants" => options.variants = parse_variants(value)?,
                    _ => return Err(format!("unknown run option: {key}")),
                }
                index += usize::from(consumed_next);
            }
        }
        index += 1;
    }
    if options.width == 0
        || options.height == 0
        || options.frames == 0
        || options.fps == 0
        || options.repeat == 0
        || !options.quality.is_finite()
        || options.quality <= 0.0
    {
        return Err("run dimensions, timing, repeat, and quality must be positive".to_owned());
    }
    Ok(options)
}

fn parse_record_options(arguments: &[String]) -> Result<RecordOptions, String> {
    let mut options = RecordOptions {
        output: PathBuf::from("av1-screen-corpus.rasc"),
        frames: 180,
        fps: 30,
        idle_timeout_ms: 30_000,
        display: None,
        force_gdi: false,
        start_delay_ms: 0,
    };
    let mut index = 0usize;
    while index < arguments.len() {
        let (key, value, consumed_next) = option_value(arguments, index)?;
        match key {
            "--output" => options.output = PathBuf::from(value),
            "--frames" => options.frames = parse_number(key, value)?,
            "--fps" => options.fps = parse_number(key, value)?,
            "--idle-timeout-ms" => options.idle_timeout_ms = parse_number(key, value)?,
            "--display" => options.display = Some(parse_number(key, value)?),
            "--capture" => match value {
                "auto" => options.force_gdi = false,
                "gdi" => options.force_gdi = true,
                _ => return Err(format!("unknown capture backend: {value}")),
            },
            "--start-delay-ms" => options.start_delay_ms = parse_number(key, value)?,
            _ => return Err(format!("unknown record option: {key}")),
        }
        index += 1 + usize::from(consumed_next);
    }
    if options.frames == 0
        || options.fps == 0
        || options.idle_timeout_ms == 0
        || options.display == Some(0)
    {
        return Err("record frame count, fps, and idle timeout must be positive".to_owned());
    }
    Ok(options)
}

fn option_value(arguments: &[String], index: usize) -> Result<(&str, &str, bool), String> {
    let argument = arguments
        .get(index)
        .ok_or_else(|| "missing option".to_owned())?;
    if let Some((key, value)) = argument.split_once('=') {
        if value.is_empty() {
            return Err(format!("missing value for {key}"));
        }
        return Ok((key, value, false));
    }
    let value = arguments
        .get(index + 1)
        .ok_or_else(|| format!("missing value for {argument}"))?;
    Ok((argument, value, true))
}

fn parse_number<T>(key: &str, value: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .parse()
        .map_err(|_| format!("invalid value for {key}: {value}"))
}

fn parse_colors(value: &str) -> Result<Vec<ColorMode>, String> {
    let mut modes = Vec::new();
    for item in value.split(',') {
        let mode = ColorMode::parse(item).ok_or_else(|| format!("unknown color mode: {item}"))?;
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    if modes.is_empty() {
        return Err("at least one color mode is required".to_owned());
    }
    Ok(modes)
}

fn parse_variants(value: &str) -> Result<Vec<Variant>, String> {
    if value == "all" {
        return Ok(Variant::all());
    }
    let mut variants = Vec::new();
    for item in value.split(',') {
        let variant = Variant::parse(item).ok_or_else(|| format!("unknown variant: {item}"))?;
        if !variants.contains(&variant) {
            variants.push(variant);
        }
    }
    if variants.is_empty() {
        return Err("at least one variant is required".to_owned());
    }
    Ok(variants)
}

#[cfg(target_os = "windows")]
fn parse_float_list(key: &str, value: &str) -> Result<Vec<f32>, String> {
    value
        .split(',')
        .map(|item| parse_number(key, item))
        .collect()
}

#[cfg(target_os = "windows")]
fn parse_string_list(key: &str, value: &str) -> Result<Vec<String>, String> {
    let values: Vec<_> = value
        .split(',')
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect();
    if values.is_empty() {
        Err(format!("{key} requires at least one value"))
    } else {
        Ok(values)
    }
}

fn percentile_ms(sorted_ns: &[u64], percentile: usize) -> f64 {
    if sorted_ns.is_empty() {
        return 0.0;
    }
    let index = ((sorted_ns.len() - 1) * percentile + 50) / 100;
    sorted_ns[index] as f64 / 1_000_000.0
}

fn apply_rotating_active_refresh(active_map: &mut [u8], frame_index: usize, period: usize) {
    let period = period.max(1);
    for (block_index, active) in active_map.iter_mut().enumerate() {
        if (block_index + frame_index).is_multiple_of(period) {
            *active = 1;
        }
    }
}

fn default_output_dir() -> PathBuf {
    PathBuf::from("results").join(format!("av1-screen-{}", unix_millis()))
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requested_matrix() {
        let options = parse_run_options(&[
            "--colors=full,256".to_owned(),
            "--variants".to_owned(),
            "baseline,active-map".to_owned(),
            "--i444".to_owned(),
        ])
        .expect("run options");
        assert_eq!(options.colors, vec![ColorMode::Full, ColorMode::Colors256]);
        assert_eq!(
            options.variants,
            vec![Variant::Baseline, Variant::ActiveMap]
        );
        assert!(options.i444);
    }

    #[test]
    fn parses_one_based_record_display() {
        let options = parse_record_options(&[
            "--display=2".to_owned(),
            "--frames".to_owned(),
            "90".to_owned(),
        ])
        .expect("record options");
        assert_eq!(options.display, Some(2));
        assert_eq!(options.frames, 90);
        assert!(parse_record_options(&["--display=0".to_owned()]).is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parses_hardware_matrix() {
        let options = parse_hardware_options(&[
            "--input=screen.rasc".to_owned(),
            "--qualities=0.25,0.67".to_owned(),
            "--encoders".to_owned(),
            "h264_nvenc,hevc_nvenc".to_owned(),
            "--profiles=hq,default".to_owned(),
        ])
        .expect("hardware options");
        assert_eq!(options.input, Some(PathBuf::from("screen.rasc")));
        assert_eq!(options.qualities, vec![0.25, 0.67]);
        assert_eq!(options.encoders, vec!["h264_nvenc", "hevc_nvenc"]);
        assert_eq!(
            options.profiles,
            vec![HwEncoderProfile::HighQuality, HwEncoderProfile::Default]
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parses_synthetic_hardware_corpus() {
        let options = parse_hardware_options(&[
            "--scene=mixed".to_owned(),
            "--width=1920".to_owned(),
            "--height=1080".to_owned(),
            "--frames=60".to_owned(),
            "--fps=20".to_owned(),
        ])
        .expect("synthetic hardware options");
        assert!(options.input.is_none());
        assert_eq!(options.width, 1920);
        assert_eq!(options.height, 1080);
        assert_eq!(options.frames, 60);
        assert_eq!(options.fps, 20);
        assert_eq!(options.scene, SyntheticScene::Mixed);
    }

    #[test]
    fn percentile_uses_sorted_samples() {
        assert_eq!(percentile_ms(&[1_000_000, 2_000_000, 9_000_000], 50), 2.0);
        assert_eq!(percentile_ms(&[1_000_000, 2_000_000, 9_000_000], 99), 9.0);
    }

    #[test]
    fn rotating_refresh_eventually_activates_every_block() {
        let mut seen = [false; 8];
        for frame in 0..4 {
            let mut map = [0u8; 8];
            apply_rotating_active_refresh(&mut map, frame, 4);
            for (index, active) in map.iter().enumerate() {
                seen[index] |= *active != 0;
            }
        }
        assert!(seen.into_iter().all(|value| value));
    }

    #[test]
    fn warmup_uses_full_map_for_one_second() {
        let mut controller = ActiveMapController::new(ActiveMapStrategy::Warmup, 4, 1, 2);
        let mut map = [0u8; 4];

        assert_eq!(controller.apply(&mut map, 0, 0), 4);
        map.fill(0);
        assert_eq!(controller.apply(&mut map, 1, 0), 4);
        map.fill(0);
        assert_eq!(controller.apply(&mut map, 2, 0), 0);
    }

    #[test]
    fn expanded_map_holds_neighboring_blocks() {
        let mut controller = ActiveMapController::new(ActiveMapStrategy::HoldExpanded, 5, 5, 30);
        let mut map = [0u8; 25];
        map[12] = 1;

        assert_eq!(controller.apply(&mut map, 0, 1), 9);
        for _ in 0..7 {
            map.fill(0);
            assert_eq!(controller.apply(&mut map, 1, 0), 9);
        }
        map.fill(0);
        assert_eq!(controller.apply(&mut map, 8, 0), 0);
    }

    #[test]
    fn recovery_uses_bounded_full_frame_bursts() {
        let mut controller = ActiveMapController::new(ActiveMapStrategy::Recovery, 5, 2, 10);
        let mut map = [0u8; 10];

        assert_eq!(controller.apply(&mut map, 0, 0), 10);

        map.fill(0);
        map[..6].fill(1);
        assert_eq!(controller.apply(&mut map, 10, 6), 10);

        map.fill(0);
        assert_eq!(controller.apply(&mut map, 11, 0), 10);

        controller.full_active_remaining = 0;
        map.fill(0);
        assert_eq!(controller.apply(&mut map, 50, 0), 10);
    }
}
