use crate::{
    corpus::{Corpus, CorpusMetadata, SyntheticScene},
    percentile_ms, unix_millis,
    yuv::{write_bgra_bmp, write_hw_decoded_bmp, QualityAccumulator, YuvBuffer},
    AnyResult,
};
use hbb_common::{
    bytes::Bytes,
    message_proto::{EncodedVideoFrame, EncodedVideoFrames, Message, VideoFrame},
    protobuf::Message as ProtobufMessage,
};
use hwcodec::{
    ffmpeg::AVHWDeviceType,
    ffmpeg_ram::decode::{DecodeContext, Decoder},
};
use scrap::{
    codec::{EncoderApi, EncoderCfg},
    hwcodec::{HwEncoderProfile, HwRamEncoder, HwRamEncoderConfig},
};
use serde::Serialize;
use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct HardwareOptions {
    pub input: Option<PathBuf>,
    pub output_dir: PathBuf,
    pub width: usize,
    pub height: usize,
    pub frames: usize,
    pub fps: u32,
    pub scene: SyntheticScene,
    pub repeat: usize,
    pub qualities: Vec<f32>,
    pub encoders: Vec<String>,
    pub profiles: Vec<HwEncoderProfile>,
    pub save_samples: bool,
}

#[derive(Serialize)]
struct HardwareReport {
    generated_unix_ms: u128,
    corpus: String,
    width: usize,
    height: usize,
    fps: u32,
    source_frames: usize,
    repeat: usize,
    results: Vec<HardwareResult>,
}

#[derive(Serialize)]
struct HardwareResult {
    encoder: String,
    codec: String,
    profile: String,
    quality_ratio: f32,
    error: Option<String>,
    configured_bitrate_kbps: u32,
    input_frames: usize,
    encoded_packets: usize,
    decoded_frames: usize,
    keyframes: usize,
    payload_bytes: u64,
    protocol_bytes: u64,
    payload_bitrate_kbps: f64,
    protocol_bitrate_kbps: f64,
    packet_p50_bytes: u64,
    packet_p95_bytes: u64,
    packet_p99_bytes: u64,
    largest_packet_bytes: u64,
    largest_keyframe_bytes: u64,
    encode_mean_ms: f64,
    encode_p50_ms: f64,
    encode_p95_ms: f64,
    encode_p99_ms: f64,
    over_16_67_ms: usize,
    over_33_33_ms: usize,
    luma_psnr_db: f64,
}

impl HardwareResult {
    fn failed(
        encoder: &str,
        profile: HwEncoderProfile,
        quality_ratio: f32,
        error: &dyn std::error::Error,
    ) -> Self {
        Self {
            encoder: encoder.to_owned(),
            codec: codec_label(encoder).to_owned(),
            profile: profile_label(profile).to_owned(),
            quality_ratio,
            error: Some(error.to_string()),
            configured_bitrate_kbps: 0,
            input_frames: 0,
            encoded_packets: 0,
            decoded_frames: 0,
            keyframes: 0,
            payload_bytes: 0,
            protocol_bytes: 0,
            payload_bitrate_kbps: 0.0,
            protocol_bitrate_kbps: 0.0,
            packet_p50_bytes: 0,
            packet_p95_bytes: 0,
            packet_p99_bytes: 0,
            largest_packet_bytes: 0,
            largest_keyframe_bytes: 0,
            encode_mean_ms: 0.0,
            encode_p50_ms: 0.0,
            encode_p95_ms: 0.0,
            encode_p99_ms: 0.0,
            over_16_67_ms: 0,
            over_33_33_ms: 0,
            luma_psnr_db: 0.0,
        }
    }
}

pub fn run(options: HardwareOptions) -> AnyResult<()> {
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
    let mut results = Vec::with_capacity(
        options.encoders.len() * options.qualities.len() * options.profiles.len(),
    );

    println!(
        "RustAdmin hardware screen benchmark: {}x{}, {} frames, {} fps, NV12",
        metadata.width, metadata.height, metadata.frames, metadata.fps
    );
    for encoder in &options.encoders {
        validate_encoder(encoder)?;
        for profile in &options.profiles {
            for quality in &options.qualities {
                print!(
                    "  {:>12} / {:>7} / q={quality:<4}: ",
                    encoder,
                    profile_label(*profile)
                );
                io::stdout().flush()?;
                match run_configuration(
                    &mut corpus,
                    &metadata,
                    encoder,
                    *profile,
                    *quality,
                    &options,
                ) {
                    Ok(result) => {
                        println!(
                            "{:7.1} kbps, p99 packet {:7} B, p95 {:6.2} ms, PSNR-Y {:5.2} dB",
                            result.protocol_bitrate_kbps,
                            result.packet_p99_bytes,
                            result.encode_p95_ms,
                            result.luma_psnr_db
                        );
                        results.push(result);
                    }
                    Err(error) => {
                        println!("FAILED: {error}");
                        results.push(HardwareResult::failed(
                            encoder,
                            *profile,
                            *quality,
                            error.as_ref(),
                        ));
                    }
                }
            }
        }
    }

    let report = HardwareReport {
        generated_unix_ms: unix_millis(),
        corpus: metadata.name,
        width: metadata.width,
        height: metadata.height,
        fps: metadata.fps,
        source_frames: metadata.frames,
        repeat: options.repeat,
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
    metadata: &CorpusMetadata,
    encoder_name: &str,
    profile: HwEncoderProfile,
    quality_ratio: f32,
    options: &HardwareOptions,
) -> AnyResult<HardwareResult> {
    let frame_bytes = metadata
        .width
        .checked_mul(metadata.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "BGRA size overflow"))?;
    let mut bgra = vec![0u8; frame_bytes];
    let mut encode_times_ns = Vec::with_capacity(metadata.frames * options.repeat);
    let mut packet_sizes = Vec::with_capacity(metadata.frames * options.repeat);
    let mut payload_bytes = 0u64;
    let mut protocol_bytes = 0u64;
    let mut encoded_packets = 0usize;
    let mut decoded_frames = 0usize;
    let mut keyframes = 0usize;
    let mut largest_keyframe_bytes = 0u64;
    let mut total_duration_ms = 0u64;
    let mut quality = QualityAccumulator::default();
    let mut configured_bitrate_kbps = 0u32;

    for repeat in 0..options.repeat {
        corpus.reset()?;
        let config = HwRamEncoderConfig {
            name: encoder_name.to_owned(),
            mc_name: None,
            width: metadata.width,
            height: metadata.height,
            quality: quality_ratio,
            keyframe_interval: None,
            profile,
        };
        let mut encoder = HwRamEncoder::new(EncoderCfg::HWRAM(config), false)?;
        configured_bitrate_kbps = encoder.bitrate();
        let mut decoder = Decoder::new(DecodeContext {
            name: decoder_name(encoder_name).to_owned(),
            device_type: AVHWDeviceType::AV_HWDEVICE_TYPE_NONE,
            thread_count: 4,
        })
        .map_err(|()| io::Error::other("failed to create FFmpeg validation decoder"))?;
        let mut yuv = YuvBuffer::new(encoder.yuvfmt())?;
        let mut last_timestamp = Duration::ZERO;

        for frame_index in 0..metadata.frames {
            let timestamp = corpus.read_frame(frame_index, &mut bgra)?;
            last_timestamp = timestamp;
            yuv.convert_bgra(&bgra)?;

            if options.save_samples && repeat == 0 && frame_index == metadata.frames / 2 {
                write_bgra_bmp(
                    &options.output_dir.join("source-hardware.bmp"),
                    &bgra,
                    metadata.width,
                    metadata.height,
                )?;
            }

            let started = Instant::now();
            let encoded = encoder.encode(&yuv.data, timestamp.as_millis() as i64)?;
            encode_times_ns.push(started.elapsed().as_nanos() as u64);
            let mut protocol_frames = Vec::with_capacity(encoded.len());
            for packet in encoded {
                let packet_bytes = packet.data.len() as u64;
                payload_bytes = payload_bytes.saturating_add(packet_bytes);
                packet_sizes.push(packet_bytes);
                encoded_packets += 1;
                let is_key = packet.key == 1;
                if is_key {
                    keyframes += 1;
                    largest_keyframe_bytes = largest_keyframe_bytes.max(packet_bytes);
                }

                for decoded in decoder
                    .decode(&packet.data)
                    .map_err(|error| io::Error::other(format!("FFmpeg decode failed: {error}")))?
                {
                    quality.compare_hw_frame(&yuv, decoded)?;
                    decoded_frames += 1;
                    if options.save_samples && repeat == 0 && frame_index == metadata.frames / 2 {
                        write_hw_decoded_bmp(
                            &options.output_dir.join(format!(
                                "decoded-{}-{}-q{}.bmp",
                                encoder_name,
                                profile_label(profile),
                                quality_slug(quality_ratio)
                            )),
                            decoded,
                        )?;
                    }
                }

                protocol_frames.push(EncodedVideoFrame {
                    data: Bytes::from(packet.data),
                    pts: packet.pts,
                    key: is_key,
                    ..Default::default()
                });
            }
            if !protocol_frames.is_empty() {
                protocol_bytes = protocol_bytes
                    .saturating_add(serialized_video_frame_size(encoder_name, protocol_frames)?);
            }
        }
        let nominal_tail = 1_000u64 / metadata.fps.max(1) as u64;
        total_duration_ms = total_duration_ms
            .saturating_add(last_timestamp.as_millis() as u64)
            .saturating_add(nominal_tail);
    }

    if decoded_frames == 0 || encode_times_ns.is_empty() || packet_sizes.is_empty() {
        return Err(io::Error::other("encoder produced no decodable frames").into());
    }
    encode_times_ns.sort_unstable();
    packet_sizes.sort_unstable();
    let total_ns: u128 = encode_times_ns.iter().map(|value| *value as u128).sum();
    let duration_seconds = total_duration_ms.max(1) as f64 / 1_000.0;
    Ok(HardwareResult {
        encoder: encoder_name.to_owned(),
        codec: codec_label(encoder_name).to_owned(),
        profile: profile_label(profile).to_owned(),
        quality_ratio,
        error: None,
        configured_bitrate_kbps,
        input_frames: metadata.frames * options.repeat,
        encoded_packets,
        decoded_frames,
        keyframes,
        payload_bytes,
        protocol_bytes,
        payload_bitrate_kbps: payload_bytes as f64 * 8.0 / duration_seconds / 1_000.0,
        protocol_bitrate_kbps: protocol_bytes as f64 * 8.0 / duration_seconds / 1_000.0,
        packet_p50_bytes: percentile_value(&packet_sizes, 50),
        packet_p95_bytes: percentile_value(&packet_sizes, 95),
        packet_p99_bytes: percentile_value(&packet_sizes, 99),
        largest_packet_bytes: *packet_sizes.last().unwrap_or(&0),
        largest_keyframe_bytes,
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
        luma_psnr_db: quality.psnr_y(),
    })
}

fn serialized_video_frame_size(
    encoder_name: &str,
    frames: Vec<EncodedVideoFrame>,
) -> AnyResult<u64> {
    let frames = EncodedVideoFrames {
        frames,
        ..Default::default()
    };
    let mut video_frame = VideoFrame::new();
    if encoder_name.contains("h264") {
        video_frame.set_h264s(frames);
    } else if encoder_name.contains("hevc") || encoder_name.contains("h265") {
        video_frame.set_h265s(frames);
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hardware benchmark supports H.264 and H.265 encoders",
        )
        .into());
    }
    let mut message = Message::new();
    message.set_video_frame(video_frame);
    Ok(message.compute_size())
}

fn validate_encoder(name: &str) -> AnyResult<()> {
    if name.contains("h264") || name.contains("hevc") || name.contains("h265") {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported hardware benchmark encoder: {name}"),
        )
        .into())
    }
}

fn decoder_name(encoder_name: &str) -> &'static str {
    if encoder_name.contains("h264") {
        "h264"
    } else {
        "hevc"
    }
}

fn codec_label(encoder_name: &str) -> &'static str {
    if encoder_name.contains("h264") {
        "H.264"
    } else {
        "H.265"
    }
}

fn profile_label(profile: HwEncoderProfile) -> &'static str {
    match profile {
        HwEncoderProfile::Default => "default",
        HwEncoderProfile::HighQuality => "hq",
    }
}

fn quality_slug(quality: f32) -> String {
    format!("{quality:.2}").replace('.', "_")
}

fn percentile_value(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) * percentile + 50) / 100;
    sorted[index]
}

fn write_markdown(path: &Path, report: &HardwareReport) -> io::Result<()> {
    let mut file = File::create(path)?;
    writeln!(file, "# RustAdmin Hardware Screen Benchmark")?;
    writeln!(file)?;
    writeln!(file, "- Corpus: `{}`", report.corpus)?;
    writeln!(file, "- Frame: `{}x{}`", report.width, report.height)?;
    writeln!(
        file,
        "- Input: `{} fps`, `{} frames`",
        report.fps, report.source_frames
    )?;
    writeln!(file, "- Encoder input: `NV12`")?;
    writeln!(file)?;
    writeln!(
        file,
        "| Encoder | Profile | Ratio | Target kbps | Actual kbps | p95 packet | p99 packet | Max packet | Max key | p50 ms | p95 ms | p99 ms | PSNR-Y | Error |"
    )?;
    writeln!(
        file,
        "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|"
    )?;
    for result in &report.results {
        writeln!(
            file,
            "| {} | {} | {:.2} | {} | {:.1} | {} | {} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {} |",
            result.encoder,
            result.profile,
            result.quality_ratio,
            result.configured_bitrate_kbps,
            result.protocol_bitrate_kbps,
            result.packet_p95_bytes,
            result.packet_p99_bytes,
            result.largest_packet_bytes,
            result.largest_keyframe_bytes,
            result.encode_p50_ms,
            result.encode_p95_ms,
            result.encode_p99_ms,
            result.luma_psnr_db,
            result.error.as_deref().unwrap_or("-")
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_percentile_uses_sorted_samples() {
        assert_eq!(percentile_value(&[100, 200, 900], 50), 200);
        assert_eq!(percentile_value(&[100, 200, 900], 99), 900);
    }

    #[test]
    fn recognizes_nvenc_codec_names() {
        assert_eq!(decoder_name("h264_nvenc"), "h264");
        assert_eq!(decoder_name("hevc_nvenc"), "hevc");
    }
}
