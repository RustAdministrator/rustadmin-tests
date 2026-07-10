#[cfg(target_os = "windows")]
use hwcodec::{ffmpeg::AVPixelFormat, ffmpeg_ram::decode::DecodeFrame};
use scrap::{aom::Image, EncodeYuvFormat, Pixfmt};
use std::{
    fs::File,
    io::{self, Write},
    path::Path,
    slice,
};

pub struct YuvBuffer {
    pub format: EncodeYuvFormat,
    pub data: Vec<u8>,
}

impl YuvBuffer {
    pub fn new(format: EncodeYuvFormat) -> io::Result<Self> {
        let required_strides = if format.pixfmt == Pixfmt::NV12 { 2 } else { 3 };
        if format.stride.len() < required_strides || format.w == 0 || format.h == 0 {
            return Err(invalid_data("invalid encoder YUV layout"));
        }
        let chroma_height = match format.pixfmt {
            Pixfmt::I420 => format.h.div_ceil(2),
            Pixfmt::I444 => format.h,
            Pixfmt::NV12 => format.h.div_ceil(2),
            _ => return Err(invalid_data("benchmark requires I420, I444, or NV12")),
        };
        let y_end = format.stride[0]
            .checked_mul(format.h)
            .ok_or_else(|| invalid_data("Y plane size overflow"))?;
        let u_end = format
            .u
            .checked_add(format.stride[1].saturating_mul(chroma_height))
            .ok_or_else(|| invalid_data("U plane size overflow"))?;
        let len = if format.pixfmt == Pixfmt::NV12 {
            y_end.max(u_end)
        } else {
            let v_end = format
                .v
                .checked_add(format.stride[2].saturating_mul(chroma_height))
                .ok_or_else(|| invalid_data("V plane size overflow"))?;
            y_end.max(u_end).max(v_end)
        };
        Ok(Self {
            format,
            data: vec![0u8; len],
        })
    }

    pub fn convert_bgra(&mut self, bgra: &[u8]) -> io::Result<()> {
        let source_stride = self
            .format
            .w
            .checked_mul(4)
            .ok_or_else(|| invalid_data("BGRA stride overflow"))?;
        if bgra.len() != source_stride.saturating_mul(self.format.h) {
            return Err(invalid_data("BGRA source has the wrong size"));
        }
        let width =
            i32::try_from(self.format.w).map_err(|_| invalid_data("width does not fit libyuv"))?;
        let height =
            i32::try_from(self.format.h).map_err(|_| invalid_data("height does not fit libyuv"))?;
        let source_stride =
            i32::try_from(source_stride).map_err(|_| invalid_data("stride does not fit libyuv"))?;

        // Safety: source and destination slices were sized from the validated
        // dimensions and libaom-provided plane offsets. libyuv writes only the
        // visible rows using the supplied strides and does not retain pointers.
        let result = unsafe {
            match self.format.pixfmt {
                Pixfmt::I420 => scrap::convert::ARGBToI420(
                    bgra.as_ptr(),
                    source_stride,
                    self.data.as_mut_ptr(),
                    self.format.stride[0] as i32,
                    self.data.as_mut_ptr().add(self.format.u),
                    self.format.stride[1] as i32,
                    self.data.as_mut_ptr().add(self.format.v),
                    self.format.stride[2] as i32,
                    width,
                    height,
                ),
                Pixfmt::I444 => scrap::convert::ARGBToI444(
                    bgra.as_ptr(),
                    source_stride,
                    self.data.as_mut_ptr(),
                    self.format.stride[0] as i32,
                    self.data.as_mut_ptr().add(self.format.u),
                    self.format.stride[1] as i32,
                    self.data.as_mut_ptr().add(self.format.v),
                    self.format.stride[2] as i32,
                    width,
                    height,
                ),
                Pixfmt::NV12 => scrap::convert::ARGBToNV12(
                    bgra.as_ptr(),
                    source_stride,
                    self.data.as_mut_ptr(),
                    self.format.stride[0] as i32,
                    self.data.as_mut_ptr().add(self.format.u),
                    self.format.stride[1] as i32,
                    width,
                    height,
                ),
                _ => return Err(invalid_data("unsupported benchmark YUV format")),
            }
        };
        if result != 0 {
            return Err(io::Error::other(format!(
                "libyuv conversion failed with code {result}"
            )));
        }
        Ok(())
    }

    pub fn update_active_map(
        &self,
        previous: Option<&[u8]>,
        active_map: &mut [u8],
    ) -> io::Result<usize> {
        let cols = self.format.w.div_ceil(16);
        let rows = self.format.h.div_ceil(16);
        if active_map.len() != cols.saturating_mul(rows) {
            return Err(invalid_data("active-map buffer has the wrong size"));
        }
        let Some(previous) = previous else {
            active_map.fill(1);
            return Ok(active_map.len());
        };
        if previous.len() != self.data.len() {
            return Err(invalid_data("previous YUV frame has the wrong size"));
        }

        let mut active_blocks = 0usize;
        for block_y in 0..rows {
            let top = block_y * 16;
            let bottom = (top + 16).min(self.format.h);
            for block_x in 0..cols {
                let left = block_x * 16;
                let right = (left + 16).min(self.format.w);
                let mut changed = false;
                'rows: for y in top..bottom {
                    let row = y * self.format.stride[0];
                    for x in left..right {
                        if self.data[row + x].abs_diff(previous[row + x]) > 1 {
                            changed = true;
                            break 'rows;
                        }
                    }
                }
                let index = block_y * cols + block_x;
                active_map[index] = u8::from(changed);
                active_blocks += usize::from(changed);
            }
        }
        Ok(active_blocks)
    }
}

#[derive(Default)]
pub struct QualityAccumulator {
    squared_error_y: u128,
    samples_y: u128,
}

impl QualityAccumulator {
    pub fn compare_luma(
        &mut self,
        source: &YuvBuffer,
        decoded_luma: &[u8],
        decoded_stride: usize,
    ) -> io::Result<()> {
        if decoded_stride < source.format.w
            || decoded_luma.len() < decoded_stride.saturating_mul(source.format.h)
        {
            return Err(invalid_data("decoded luma plane is too small"));
        }
        for y in 0..source.format.h {
            let source_offset = y * source.format.stride[0];
            let decoded_offset = y * decoded_stride;
            let source_row = &source.data[source_offset..source_offset + source.format.w];
            let decoded_row = &decoded_luma[decoded_offset..decoded_offset + source.format.w];
            for (expected, actual) in source_row.iter().zip(decoded_row) {
                let difference = *expected as i32 - *actual as i32;
                self.squared_error_y += (difference * difference) as u128;
            }
            self.samples_y += source.format.w as u128;
        }
        Ok(())
    }

    pub fn compare(&mut self, source: &YuvBuffer, decoded: &Image) -> io::Result<()> {
        let image = decoded.inner();
        if image.d_w as usize != source.format.w
            || image.d_h as usize != source.format.h
            || image.planes[0].is_null()
            || image.stride[0] <= 0
        {
            return Err(invalid_data("decoded frame layout does not match source"));
        }
        let decoded_stride = image.stride[0] as usize;
        if decoded_stride < source.format.w {
            return Err(invalid_data("decoded luma stride is too small"));
        }

        // Safety: libaom returned a non-null luma plane with a positive stride
        // large enough for all visible rows. The image remains alive here.
        let decoded_luma = unsafe {
            slice::from_raw_parts(
                image.planes[0],
                decoded_stride.saturating_mul(source.format.h),
            )
        };
        self.compare_luma(source, decoded_luma, decoded_stride)
    }

    pub fn psnr_y(&self) -> f64 {
        if self.samples_y == 0 {
            return 0.0;
        }
        if self.squared_error_y == 0 {
            return 100.0;
        }
        let mse = self.squared_error_y as f64 / self.samples_y as f64;
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }

    #[cfg(target_os = "windows")]
    pub fn compare_hw_frame(
        &mut self,
        source: &YuvBuffer,
        decoded: &DecodeFrame,
    ) -> io::Result<()> {
        if decoded.width != source.format.w as i32
            || decoded.height != source.format.h as i32
            || decoded.data.is_empty()
            || decoded.linesize.is_empty()
            || decoded.linesize[0] <= 0
        {
            return Err(invalid_data(
                "decoded hardware frame layout does not match source",
            ));
        }
        let decoded_stride = decoded.linesize[0] as usize;
        if decoded_stride < source.format.w
            || decoded.data[0].len() < decoded_stride.saturating_mul(source.format.h)
        {
            return Err(invalid_data("decoded hardware luma plane is too small"));
        }

        self.compare_luma(source, &decoded.data[0], decoded_stride)
    }
}

pub fn write_bgra_bmp(path: &Path, bgra: &[u8], width: usize, height: usize) -> io::Result<()> {
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| invalid_data("BMP row size overflow"))?;
    if bgra.len() != row_bytes.saturating_mul(height) {
        return Err(invalid_data("BMP source has the wrong size"));
    }
    let pixel_bytes = row_bytes
        .checked_mul(height)
        .ok_or_else(|| invalid_data("BMP pixel size overflow"))?;
    let file_size = 54usize
        .checked_add(pixel_bytes)
        .ok_or_else(|| invalid_data("BMP file size overflow"))?;
    let width_i32 = i32::try_from(width).map_err(|_| invalid_data("BMP width is too large"))?;
    let height_i32 = i32::try_from(height).map_err(|_| invalid_data("BMP height is too large"))?;
    let mut file = File::create(path)?;
    file.write_all(b"BM")?;
    file.write_all(&(file_size as u32).to_le_bytes())?;
    file.write_all(&[0u8; 4])?;
    file.write_all(&54u32.to_le_bytes())?;
    file.write_all(&40u32.to_le_bytes())?;
    file.write_all(&width_i32.to_le_bytes())?;
    file.write_all(&height_i32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&32u16.to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;
    file.write_all(&(pixel_bytes as u32).to_le_bytes())?;
    file.write_all(&[0u8; 16])?;
    for y in (0..height).rev() {
        file.write_all(&bgra[y * row_bytes..(y + 1) * row_bytes])?;
    }
    Ok(())
}

pub fn write_decoded_bmp(path: &Path, decoded: &Image) -> io::Result<()> {
    let image = decoded.inner();
    let width = image.d_w as usize;
    let height = image.d_h as usize;
    if width == 0
        || height == 0
        || image.planes[0].is_null()
        || image.planes[1].is_null()
        || image.planes[2].is_null()
        || image.stride.iter().take(3).any(|stride| *stride <= 0)
    {
        return Err(invalid_data("decoded image cannot be written"));
    }
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| invalid_data("decoded BMP size overflow"))?;
    let mut bgra = vec![0u8; row_bytes.saturating_mul(height)];
    let i444 = image.fmt == scrap::aom::aom_img_fmt::AOM_IMG_FMT_I444;
    for y in 0..height {
        let chroma_y = if i444 { y } else { y / 2 };
        for x in 0..width {
            let chroma_x = if i444 { x } else { x / 2 };
            // Safety: plane pointers, dimensions, and positive strides were
            // validated above. AV1 I420/I444 chroma coordinates stay in range.
            let (luma, cb, cr) = unsafe {
                (
                    *image.planes[0].add(y * image.stride[0] as usize + x),
                    *image.planes[1].add(chroma_y * image.stride[1] as usize + chroma_x),
                    *image.planes[2].add(chroma_y * image.stride[2] as usize + chroma_x),
                )
            };
            let c = (luma as i32 - 16).max(0);
            let d = cb as i32 - 128;
            let e = cr as i32 - 128;
            let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
            let offset = (y * width + x) * 4;
            bgra[offset..offset + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    write_bgra_bmp(path, &bgra, width, height)
}

#[cfg(target_os = "windows")]
pub fn write_hw_decoded_bmp(path: &Path, decoded: &DecodeFrame) -> io::Result<()> {
    let width = usize::try_from(decoded.width).map_err(|_| invalid_data("invalid frame width"))?;
    let height =
        usize::try_from(decoded.height).map_err(|_| invalid_data("invalid frame height"))?;
    if width == 0
        || height == 0
        || decoded.data.len() < 2
        || decoded.linesize.len() < 2
        || decoded.linesize[0] <= 0
        || decoded.linesize[1] <= 0
    {
        return Err(invalid_data("decoded hardware image cannot be written"));
    }
    let planar = decoded.pixfmt == AVPixelFormat::AV_PIX_FMT_YUV420P;
    let semiplanar = decoded.pixfmt == AVPixelFormat::AV_PIX_FMT_NV12;
    if (!planar && !semiplanar) || (planar && decoded.data.len() < 3) {
        return Err(invalid_data("unsupported decoded hardware pixel format"));
    }
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| invalid_data("decoded BMP size overflow"))?;
    let mut bgra = vec![0u8; row_bytes.saturating_mul(height)];
    let y_stride = decoded.linesize[0] as usize;
    let uv_stride = decoded.linesize[1] as usize;
    let v_stride = decoded.linesize.get(2).copied().unwrap_or_default().max(0) as usize;
    for y in 0..height {
        for x in 0..width {
            let luma = decoded.data[0][y * y_stride + x];
            let (cb, cr) = if planar {
                (
                    decoded.data[1][(y / 2) * uv_stride + x / 2],
                    decoded.data[2][(y / 2) * v_stride + x / 2],
                )
            } else {
                let offset = (y / 2) * uv_stride + (x / 2) * 2;
                (decoded.data[1][offset], decoded.data[1][offset + 1])
            };
            let c = (luma as i32 - 16).max(0);
            let d = cb as i32 - 128;
            let e = cr as i32 - 128;
            let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
            let offset = (y * width + x) * 4;
            bgra[offset..offset + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    write_bgra_bmp(path, &bgra, width, height)
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_active_map_is_fully_active() {
        let format = EncodeYuvFormat {
            pixfmt: Pixfmt::I420,
            w: 32,
            h: 17,
            stride: vec![64, 32, 32],
            u: 64 * 17,
            v: 64 * 17 + 32 * 9,
        };
        let buffer = YuvBuffer::new(format).expect("YUV buffer");
        let mut map = vec![0u8; 4];
        let active = buffer
            .update_active_map(None, &mut map)
            .expect("active map");
        assert_eq!(active, 4);
        assert_eq!(map, vec![1, 1, 1, 1]);
    }

    #[test]
    fn allocates_nv12_layout() {
        let format = EncodeYuvFormat {
            pixfmt: Pixfmt::NV12,
            w: 32,
            h: 17,
            stride: vec![64, 64],
            u: 64 * 17,
            v: 0,
        };
        let buffer = YuvBuffer::new(format).expect("NV12 buffer");
        assert_eq!(buffer.data.len(), 64 * 17 + 64 * 9);
    }
}
