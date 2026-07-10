use scrap::{Capturer, Display, Frame, Pixfmt, TraitCapturer, TraitPixelBuffer};
use serde::Serialize;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const CORPUS_MAGIC: &[u8; 8] = b"RASCBGR1";
const CORPUS_VERSION: u32 = 1;
const HEADER_SIZE: u64 = 8 + 5 * 4;
const FRAME_TIMESTAMP_SIZE: u64 = 8;
const MAX_DIMENSION: u32 = 16_384;
const MAX_FRAME_BYTES: usize = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorMode {
    Full,
    Colors4096,
    Colors256,
    Colors16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyntheticScene {
    Screen,
    Mixed,
}

impl SyntheticScene {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "screen" => Some(Self::Screen),
            "mixed" => Some(Self::Mixed),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Mixed => "mixed",
        }
    }
}

impl ColorMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Self::Full),
            "4096" => Some(Self::Colors4096),
            "256" => Some(Self::Colors256),
            "16" => Some(Self::Colors16),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Colors4096 => "4096",
            Self::Colors256 => "256",
            Self::Colors16 => "16",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CorpusMetadata {
    pub name: String,
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub frames: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
    pub index: usize,
    pub name: String,
    pub width: usize,
    pub height: usize,
    pub origin: (i32, i32),
    pub primary: bool,
}

pub enum Corpus {
    Synthetic(SyntheticCorpus),
    Recorded(RecordedCorpus),
}

impl Corpus {
    pub fn synthetic(
        width: usize,
        height: usize,
        fps: u32,
        frames: usize,
        scene: SyntheticScene,
    ) -> io::Result<Self> {
        validate_dimensions(width, height)?;
        if fps == 0 || frames == 0 {
            return Err(invalid_data("fps and frame count must be non-zero"));
        }
        Ok(Self::Synthetic(SyntheticCorpus {
            metadata: CorpusMetadata {
                name: format!("synthetic-{}-v1", scene.label()),
                width,
                height,
                fps,
                frames,
            },
            scene,
        }))
    }

    pub fn recorded(path: &Path) -> io::Result<Self> {
        Ok(Self::Recorded(RecordedCorpus::open(path)?))
    }

    pub fn metadata(&self) -> &CorpusMetadata {
        match self {
            Self::Synthetic(corpus) => &corpus.metadata,
            Self::Recorded(corpus) => &corpus.metadata,
        }
    }

    pub fn reset(&mut self) -> io::Result<()> {
        match self {
            Self::Synthetic(_) => Ok(()),
            Self::Recorded(corpus) => corpus.reset(),
        }
    }

    pub fn read_frame(&mut self, index: usize, bgra: &mut [u8]) -> io::Result<Duration> {
        match self {
            Self::Synthetic(corpus) => corpus.read_frame(index, bgra),
            Self::Recorded(corpus) => corpus.read_frame(index, bgra),
        }
    }
}

pub struct SyntheticCorpus {
    metadata: CorpusMetadata,
    scene: SyntheticScene,
}

impl SyntheticCorpus {
    fn read_frame(&self, index: usize, bgra: &mut [u8]) -> io::Result<Duration> {
        if index >= self.metadata.frames {
            return Err(invalid_data("synthetic frame index out of range"));
        }
        let expected = checked_frame_bytes(self.metadata.width, self.metadata.height)?;
        if bgra.len() != expected {
            return Err(invalid_data("synthetic BGRA buffer has the wrong length"));
        }
        render_synthetic_frame(
            bgra,
            self.metadata.width,
            self.metadata.height,
            index,
            self.metadata.frames,
            self.scene,
        );
        Ok(Duration::from_millis(
            (index as u64).saturating_mul(1_000) / self.metadata.fps as u64,
        ))
    }
}

pub struct RecordedCorpus {
    metadata: CorpusMetadata,
    file: File,
    frame_bytes: usize,
}

impl RecordedCorpus {
    fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut magic = [0u8; 8];
        file.read_exact(&mut magic)?;
        if &magic != CORPUS_MAGIC {
            return Err(invalid_data("not a RustAdmin AV1 screen corpus"));
        }
        let version = read_u32(&mut file)?;
        if version != CORPUS_VERSION {
            return Err(invalid_data("unsupported corpus version"));
        }
        let width = read_u32(&mut file)? as usize;
        let height = read_u32(&mut file)? as usize;
        let fps = read_u32(&mut file)?;
        let frames = read_u32(&mut file)? as usize;
        validate_dimensions(width, height)?;
        if fps == 0 || frames == 0 {
            return Err(invalid_data("invalid corpus timing or frame count"));
        }
        let frame_bytes = checked_frame_bytes(width, height)?;
        let record_bytes = FRAME_TIMESTAMP_SIZE
            .checked_add(frame_bytes as u64)
            .ok_or_else(|| invalid_data("corpus record size overflow"))?;
        let expected_len = HEADER_SIZE
            .checked_add(record_bytes.saturating_mul(frames as u64))
            .ok_or_else(|| invalid_data("corpus size overflow"))?;
        if file.metadata()?.len() != expected_len {
            return Err(invalid_data("truncated or oversized corpus"));
        }
        Ok(Self {
            metadata: CorpusMetadata {
                name: path.display().to_string(),
                width,
                height,
                fps,
                frames,
            },
            file,
            frame_bytes,
        })
    }

    fn reset(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(HEADER_SIZE))?;
        Ok(())
    }

    fn read_frame(&mut self, index: usize, bgra: &mut [u8]) -> io::Result<Duration> {
        if index >= self.metadata.frames || bgra.len() != self.frame_bytes {
            return Err(invalid_data("recorded frame request is out of range"));
        }
        let timestamp_ms = read_u64(&mut self.file)?;
        self.file.read_exact(bgra)?;
        Ok(Duration::from_millis(timestamp_ms))
    }
}

pub fn record_desktop(
    path: &Path,
    frame_count: usize,
    fps: u32,
    idle_timeout: Duration,
    display_index: Option<usize>,
    force_gdi: bool,
    start_delay: Duration,
) -> io::Result<()> {
    if frame_count == 0 || frame_count > u32::MAX as usize || fps == 0 {
        return Err(invalid_data("invalid record frame count or fps"));
    }
    let mut displays = Display::all().map_err(scrap_error)?;
    if displays.is_empty() {
        return Err(invalid_data("no displays found"));
    }
    let primary = displays
        .iter()
        .position(Display::is_primary)
        .unwrap_or_default();
    let selected = match display_index {
        Some(index) => index
            .checked_sub(1)
            .filter(|index| *index < displays.len())
            .ok_or_else(|| invalid_data("display index is out of range"))?,
        None => primary,
    };
    let display = displays.remove(selected);
    let mut capturer = Capturer::new(display).map_err(scrap_error)?;
    if force_gdi && !capturer.set_gdi() {
        return Err(invalid_data("failed to switch the recorder to Windows GDI"));
    }
    let width = capturer.width();
    let height = capturer.height();
    validate_dimensions(width, height)?;
    let frame_bytes = checked_frame_bytes(width, height)?;

    let mut file = File::create(path)?;
    write_header(&mut file, width, height, fps, frame_count)?;
    let mut tight_bgra = vec![0u8; frame_bytes];
    if !start_delay.is_zero() {
        std::thread::sleep(start_delay);
    }
    let start = Instant::now();
    let mut last_frame = Instant::now();
    let poll = Duration::from_millis((1_000 / fps.max(1)) as u64);
    let mut written = 0usize;
    while written < frame_count {
        match capturer.frame(poll) {
            Ok(Frame::PixelBuffer(pixel_buffer)) => {
                copy_pixel_buffer_to_bgra(&pixel_buffer, &mut tight_bgra)?;
                file.write_all(&(start.elapsed().as_millis() as u64).to_le_bytes())?;
                file.write_all(&tight_bgra)?;
                written += 1;
                last_frame = Instant::now();
                eprint!("\rRecorded {written}/{frame_count}");
            }
            Ok(Frame::Texture(_)) => {
                return Err(invalid_data(
                    "capture returned a GPU texture; record without the VRAM feature",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if last_frame.elapsed() >= idle_timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "desktop capture produced no changed frame before the idle timeout",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    eprintln!();
    file.flush()?;
    Ok(())
}

pub fn display_inventory() -> io::Result<Vec<DisplayInfo>> {
    let displays = Display::all().map_err(scrap_error)?;
    Ok(displays
        .iter()
        .enumerate()
        .map(|(index, display)| DisplayInfo {
            index: index + 1,
            name: display.name(),
            width: display.width(),
            height: display.height(),
            origin: display.origin(),
            primary: display.is_primary(),
        })
        .collect())
}

pub struct ColorQuantizer {
    palette16: Vec<[u8; 3]>,
}

impl ColorQuantizer {
    pub fn new() -> Self {
        Self {
            palette16: build_palette16_lut(),
        }
    }

    pub fn apply(&self, bgra: &mut [u8], mode: ColorMode) {
        if mode == ColorMode::Full {
            return;
        }
        for pixel in bgra.chunks_exact_mut(4) {
            match mode {
                ColorMode::Full => {}
                ColorMode::Colors4096 => {
                    pixel[0] = quantize_channel(pixel[0], 15);
                    pixel[1] = quantize_channel(pixel[1], 15);
                    pixel[2] = quantize_channel(pixel[2], 15);
                }
                ColorMode::Colors256 => {
                    pixel[0] = quantize_channel(pixel[0], 3);
                    pixel[1] = quantize_channel(pixel[1], 7);
                    pixel[2] = quantize_channel(pixel[2], 7);
                }
                ColorMode::Colors16 => {
                    let index = ((pixel[2] as usize >> 3) << 10)
                        | ((pixel[1] as usize >> 3) << 5)
                        | (pixel[0] as usize >> 3);
                    let color = self.palette16[index];
                    pixel[0] = color[0];
                    pixel[1] = color[1];
                    pixel[2] = color[2];
                }
            }
        }
    }
}

fn build_palette16_lut() -> Vec<[u8; 3]> {
    const PALETTE: [[u8; 3]; 16] = [
        [0, 0, 0],
        [128, 0, 0],
        [0, 128, 0],
        [128, 128, 0],
        [0, 0, 128],
        [128, 0, 128],
        [0, 128, 128],
        [192, 192, 192],
        [128, 128, 128],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [0, 0, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    let mut lut = vec![[0u8; 3]; 32 * 32 * 32];
    for r5 in 0..32usize {
        for g5 in 0..32usize {
            for b5 in 0..32usize {
                let r = (r5 * 255 / 31) as i32;
                let g = (g5 * 255 / 31) as i32;
                let b = (b5 * 255 / 31) as i32;
                let mut best = 0usize;
                let mut best_distance = i32::MAX;
                for (index, color) in PALETTE.iter().enumerate() {
                    let dr = r - color[2] as i32;
                    let dg = g - color[1] as i32;
                    let db = b - color[0] as i32;
                    let distance = 2 * dr * dr + 4 * dg * dg + db * db;
                    if distance < best_distance {
                        best = index;
                        best_distance = distance;
                    }
                }
                lut[(r5 << 10) | (g5 << 5) | b5] = PALETTE[best];
            }
        }
    }
    lut
}

fn quantize_channel(value: u8, levels: u32) -> u8 {
    let bucket = (value as u32 * levels + 127) / 255;
    ((bucket * 255 + levels / 2) / levels) as u8
}

fn copy_pixel_buffer_to_bgra<T: TraitPixelBuffer>(
    pixel_buffer: &T,
    output: &mut [u8],
) -> io::Result<()> {
    let width = pixel_buffer.width();
    let height = pixel_buffer.height();
    let source = pixel_buffer.data();
    let strides = pixel_buffer.stride();
    let stride = *strides
        .first()
        .ok_or_else(|| invalid_data("capture has no stride"))?;
    let source_bpp = pixel_buffer.pixfmt().bytes_per_pixel();
    if source_bpp == 0
        || stride < width.saturating_mul(source_bpp)
        || source.len() < stride.saturating_mul(height)
        || output.len() != checked_frame_bytes(width, height)?
    {
        return Err(invalid_data("invalid captured pixel buffer"));
    }

    for y in 0..height {
        let source_row = &source[y * stride..y * stride + width * source_bpp];
        let output_row = &mut output[y * width * 4..(y + 1) * width * 4];
        match pixel_buffer.pixfmt() {
            Pixfmt::BGRA => output_row.copy_from_slice(source_row),
            Pixfmt::RGBA => {
                for (input, output) in source_row
                    .chunks_exact(4)
                    .zip(output_row.chunks_exact_mut(4))
                {
                    output.copy_from_slice(&[input[2], input[1], input[0], input[3]]);
                }
            }
            Pixfmt::RGB565LE => {
                for (input, output) in source_row
                    .chunks_exact(2)
                    .zip(output_row.chunks_exact_mut(4))
                {
                    let value = u16::from_le_bytes([input[0], input[1]]);
                    let r = ((value >> 11) & 0x1f) as u8;
                    let g = ((value >> 5) & 0x3f) as u8;
                    let b = (value & 0x1f) as u8;
                    output.copy_from_slice(&[
                        (b << 3) | (b >> 2),
                        (g << 2) | (g >> 4),
                        (r << 3) | (r >> 2),
                        255,
                    ]);
                }
            }
            format => {
                return Err(invalid_data(&format!(
                    "unsupported capture pixel format: {format:?}"
                )));
            }
        }
    }
    Ok(())
}

fn render_synthetic_frame(
    bgra: &mut [u8],
    width: usize,
    height: usize,
    frame: usize,
    frame_count: usize,
    scene: SyntheticScene,
) {
    for y in 0..height {
        for x in 0..width {
            let offset = (y * width + x) * 4;
            let panel = if x < width * 2 / 3 { 28 } else { 44 };
            bgra[offset] = (panel + y * 18 / height.max(1)) as u8;
            bgra[offset + 1] = (panel + x * 22 / width.max(1)) as u8;
            bgra[offset + 2] = (panel + 12) as u8;
            bgra[offset + 3] = 255;
        }
    }

    fill_rect(bgra, width, height, 0, 0, width, 42, [36, 38, 42, 255]);
    fill_rect(bgra, width, height, 12, 9, width / 4, 25, [82, 88, 96, 255]);
    fill_rect(
        bgra,
        width,
        height,
        0,
        42,
        width / 6,
        height.saturating_sub(42),
        [42, 48, 54, 255],
    );

    let scroll = if frame < frame_count / 3 {
        0
    } else {
        (frame * 3) % 18
    };
    let content_x = width / 6 + 18;
    let content_w = width * 47 / 100;
    let line_count = height.saturating_sub(70) / 18;
    for line in 0..line_count {
        let y = 58 + line * 18;
        let shifted = y.saturating_sub(scroll);
        let tone = 116 + (line * 17 % 90) as u8;
        draw_pseudo_text_line(
            bgra,
            width,
            height,
            content_x,
            shifted,
            content_w,
            line as u32,
            [tone, tone.saturating_add(8), tone.saturating_add(18), 255],
        );
    }

    let video_x = width * 69 / 100;
    let video_y = 70;
    let video_w = width.saturating_sub(video_x + 18);
    let video_h = height.saturating_sub(video_y + 24);
    for y in 0..video_h {
        for x in 0..video_w {
            let absolute_x = video_x + x;
            let absolute_y = video_y + y;
            let offset = (absolute_y * width + absolute_x) * 4;
            let scene_frame = if scene == SyntheticScene::Mixed {
                frame
            } else {
                0
            };
            let motion = ((x + scene_frame * 5) ^ (y + scene_frame * 3)) as u8;
            bgra[offset] = motion.wrapping_add((x * 255 / video_w.max(1)) as u8);
            bgra[offset + 1] = ((y + scene_frame * 2) * 255 / video_h.max(1)) as u8;
            bgra[offset + 2] = motion.rotate_left(2).wrapping_add(48);
            bgra[offset + 3] = 255;
        }
    }

    let box_width = (width / 8).max(24);
    let travel = content_w.saturating_sub(box_width).max(1);
    let moving_x = content_x + (frame * 7) % travel;
    let moving_y = height * 3 / 5;
    fill_rect(
        bgra,
        width,
        height,
        moving_x,
        moving_y,
        box_width,
        34,
        [42, 128, 228, 255],
    );
    if frame % 30 < 15 {
        fill_rect(
            bgra,
            width,
            height,
            content_x + 8,
            height.saturating_sub(42),
            3,
            18,
            [230, 230, 230, 255],
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_pseudo_text_line(
    bgra: &mut [u8],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    max_width: usize,
    seed: u32,
    color: [u8; 4],
) {
    if y + 9 >= height {
        return;
    }
    let glyphs = max_width / 7;
    for glyph in 0..glyphs {
        let bits = seed
            .wrapping_mul(0x9e37_79b9)
            .wrapping_add((glyph as u32).wrapping_mul(0x85eb_ca6b));
        for row in 0..7usize {
            let row_bits = bits.rotate_left((row * 5) as u32) ^ (0x15u32 << (row % 3));
            for col in 0..5usize {
                if (row_bits >> col) & 1 != 0 {
                    put_pixel(bgra, width, height, x + glyph * 7 + col, y + row, color);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_rect(
    bgra: &mut [u8],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    rect_width: usize,
    rect_height: usize,
    color: [u8; 4],
) {
    let right = x.saturating_add(rect_width).min(width);
    let bottom = y.saturating_add(rect_height).min(height);
    for row in y.min(height)..bottom {
        for column in x.min(width)..right {
            let offset = (row * width + column) * 4;
            bgra[offset..offset + 4].copy_from_slice(&color);
        }
    }
}

fn put_pixel(bgra: &mut [u8], width: usize, height: usize, x: usize, y: usize, color: [u8; 4]) {
    if x < width && y < height {
        let offset = (y * width + x) * 4;
        bgra[offset..offset + 4].copy_from_slice(&color);
    }
}

fn write_header(
    file: &mut File,
    width: usize,
    height: usize,
    fps: u32,
    frames: usize,
) -> io::Result<()> {
    file.write_all(CORPUS_MAGIC)?;
    file.write_all(&CORPUS_VERSION.to_le_bytes())?;
    file.write_all(&(width as u32).to_le_bytes())?;
    file.write_all(&(height as u32).to_le_bytes())?;
    file.write_all(&fps.to_le_bytes())?;
    file.write_all(&(frames as u32).to_le_bytes())?;
    Ok(())
}

fn read_u32(file: &mut File) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(file: &mut File) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn checked_frame_bytes(width: usize, height: usize) -> io::Result<usize> {
    let bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| invalid_data("frame size overflow"))?;
    if bytes > MAX_FRAME_BYTES {
        return Err(invalid_data("frame exceeds benchmark safety limit"));
    }
    Ok(bytes)
}

fn validate_dimensions(width: usize, height: usize) -> io::Result<()> {
    if width == 0
        || height == 0
        || width > MAX_DIMENSION as usize
        || height > MAX_DIMENSION as usize
    {
        return Err(invalid_data("invalid corpus dimensions"));
    }
    Ok(())
}

fn scrap_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[allow(dead_code)]
pub fn default_record_path() -> PathBuf {
    PathBuf::from("av1-screen-corpus.rasc")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantized_modes_have_expected_channel_levels() {
        let mut pixel = [123, 177, 231, 255];
        let quantizer = ColorQuantizer::new();
        quantizer.apply(&mut pixel, ColorMode::Colors4096);
        assert!(pixel[..3].iter().all(|value| value % 17 == 0));

        let mut pixel = [123, 177, 231, 255];
        quantizer.apply(&mut pixel, ColorMode::Colors256);
        assert_eq!(pixel[0] % 85, 0);
    }

    #[test]
    fn synthetic_frames_are_deterministic() {
        let mut corpus =
            Corpus::synthetic(64, 48, 30, 3, SyntheticScene::Screen).expect("synthetic corpus");
        let mut first = vec![0u8; 64 * 48 * 4];
        let mut second = vec![0u8; first.len()];
        corpus.read_frame(1, &mut first).expect("first render");
        corpus.read_frame(1, &mut second).expect("second render");
        assert_eq!(first, second);
    }
}
