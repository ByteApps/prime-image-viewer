//! Streamed, downsampled PNG decoding, for images whose full-resolution
//! decode would blow the `MAX_DECODED_BYTES` budget in `src/main.rs`.
//!
//! `image`'s PNG backend (the `png` crate) has no scaled-decode API like
//! `jpeg-decoder`'s IDCT `scale()` -- it always produces full-resolution
//! pixels. But it DOES stream: `png::Reader::next_row()` yields one
//! (already-unfiltered) scanline at a time, instead of `image`'s
//! `load_from_memory`, which decodes the whole image into one buffer before
//! handing it back. That's enough to box-filter an oversize PNG down to a
//! target scale with peak memory tracking the file bytes, the zlib window,
//! ONE row, and the (small, downsampled) output -- never the source's full
//! `w*h*4`.
//!
//! Mirrors `jpeg_scaled.rs`'s shape: `pick_factor` decides whether (and how
//! aggressively) to downsample, `decode_scaled` does the decode. Unlike
//! JPEG's power-of-two IDCT scales, PNG has no such structural constraint,
//! so this picks an arbitrary integer factor `k` and box-filters `k x k`
//! input blocks down to one output pixel (averaging each channel).
//!
//! Interlaced (Adam7) PNGs are refused rather than handled: `next_row()`
//! streams the passes in *pass* order, not final-row order, so consuming it
//! naively would scramble the image. Adam7 PNGs are rare in the wild (most
//! encoders don't interlace by default), so this is a narrow gap, not a
//! silent correctness bug -- `decode_scaled` errors out and the caller
//! refuses the file exactly as it did before this module existed.

use image::DynamicImage;
use png::{BitDepth, ColorType, Transformations};
use std::io::Cursor;

/// Pick the smallest integer downsample factor `k >= 2` whose RGBA8 output
/// (`ceil(w/k) * ceil(h/k) * 4` bytes) fits within `budget_bytes`, or `None`
/// if even `k = 16` doesn't fit (absurd sizes stay refused rather than
/// decoding at a factor so aggressive the result is useless).
///
/// `k = 1` is deliberately never considered -- the only caller (`show_image`
/// in `src/main.rs`) reaches this after its own pre-flight has already
/// established that the full-resolution decode exceeds the budget, so a
/// `k = 1` candidate would never pass anyway.
pub fn pick_factor(w: u32, h: u32, budget_bytes: u64) -> Option<u32> {
    for k in 2..=16u32 {
        let ow = (u64::from(w) + u64::from(k) - 1) / u64::from(k);
        let oh = (u64::from(h) + u64::from(k) - 1) / u64::from(k);
        let needed = ow.saturating_mul(oh).saturating_mul(4);
        if needed <= budget_bytes {
            return Some(k);
        }
    }
    None
}

/// Decode a PNG at roughly `1/k` scale, box-filtering `k x k` input blocks
/// (fewer at the right/bottom edges, when `w`/`h` aren't multiples of `k`)
/// into one averaged output pixel per channel.
///
/// Uses `png::Transformations::EXPAND | STRIP_16` so the decoder always
/// hands back 8-bit-per-channel Grayscale, GrayscaleAlpha, Rgb or Rgba rows
/// -- paletted images arrive pre-expanded to Rgb/Rgba, and 16-bit images are
/// pre-truncated to 8. Returns `Err` for interlaced PNGs (see the module
/// doc) and for anything the decoder itself rejects.
pub fn decode_scaled(bytes: &[u8], k: u32) -> Result<DynamicImage, String> {
    let k = k.max(2) as u64;

    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(Transformations::EXPAND | Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| format!("png header: {e}"))?;

    if reader.info().interlaced {
        return Err("interlaced (Adam7) PNG: scaled decode not supported".to_string());
    }

    let (w, h) = (reader.info().width, reader.info().height);
    let (color_type, bit_depth) = reader.output_color_type();
    if bit_depth != BitDepth::Eight {
        return Err(format!(
            "png: unexpected output bit depth {bit_depth:?} after STRIP_16"
        ));
    }
    let channels: usize = match color_type {
        ColorType::Grayscale => 1,
        ColorType::GrayscaleAlpha => 2,
        ColorType::Rgb => 3,
        ColorType::Rgba => 4,
        ColorType::Indexed => {
            return Err("png: indexed color type persisted through EXPAND transform".to_string())
        }
    };

    let out_w = ((u64::from(w) + k - 1) / k) as u32;
    let out_h = ((u64::from(h) + k - 1) / k) as u32;
    if out_w == 0 || out_h == 0 {
        return Err(format!("png: degenerate output dimensions {out_w}x{out_h}"));
    }

    // How many input columns feed output column `ox` -- always `k`, except
    // possibly the last column band when `w` isn't a multiple of `k`.
    let col_count: Vec<u32> = (0..out_w)
        .map(|ox| {
            let start = u64::from(ox) * k;
            let end = (start + k).min(u64::from(w));
            (end - start) as u32
        })
        .collect();

    let mut out = vec![0u8; out_w as usize * out_h as usize * channels];
    // Running per-output-column, per-channel sums for the row band currently
    // being accumulated (reset after each band is flushed).
    let mut acc: Vec<u32> = vec![0u32; out_w as usize * channels];
    let mut rows_in_band: u32 = 0;
    let mut out_row: u32 = 0;

    while let Some(row) = reader.next_row().map_err(|e| format!("png row: {e}"))? {
        let data = row.data();
        for (ox, &ccount) in col_count.iter().enumerate() {
            let cstart = ox * k as usize;
            let base = (ox) * channels;
            for c in 0..ccount as usize {
                let px = (cstart + c) * channels;
                for ch in 0..channels {
                    acc[base + ch] += u32::from(data[px + ch]);
                }
            }
        }
        rows_in_band += 1;

        let band_start = u64::from(out_row) * k;
        let band_end = (band_start + k).min(u64::from(h));
        let band_size = (band_end - band_start) as u32;

        if rows_in_band >= band_size {
            for (ox, &ccount) in col_count.iter().enumerate() {
                let denom = (rows_in_band * ccount).max(1);
                let base = ox * channels;
                let out_base = (out_row as usize * out_w as usize + ox) * channels;
                for ch in 0..channels {
                    out[out_base + ch] = (acc[base + ch] / denom) as u8;
                    acc[base + ch] = 0;
                }
            }
            rows_in_band = 0;
            out_row += 1;
        }
    }

    if out_row != out_h {
        return Err(format!(
            "png: expected {out_h} output rows, only produced {out_row} (truncated source?)"
        ));
    }

    match color_type {
        ColorType::Grayscale => image::GrayImage::from_raw(out_w, out_h, out)
            .map(DynamicImage::ImageLuma8)
            .ok_or_else(|| "png: luma buffer size mismatch".to_string()),
        ColorType::GrayscaleAlpha => image::GrayAlphaImage::from_raw(out_w, out_h, out)
            .map(DynamicImage::ImageLumaA8)
            .ok_or_else(|| "png: luma-alpha buffer size mismatch".to_string()),
        ColorType::Rgb => image::RgbImage::from_raw(out_w, out_h, out)
            .map(DynamicImage::ImageRgb8)
            .ok_or_else(|| "png: rgb buffer size mismatch".to_string()),
        ColorType::Rgba => image::RgbaImage::from_raw(out_w, out_h, out)
            .map(DynamicImage::ImageRgba8)
            .ok_or_else(|| "png: rgba buffer size mismatch".to_string()),
        ColorType::Indexed => unreachable!("filtered above -- EXPAND always removes Indexed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (3000,3000) at a 24 MiB budget: full RGBA8 decode would be
    /// 3000*3000*4 = 36,000,000 bytes, over the 25,165,824-byte budget --
    /// but k=2 (1500x1500) is 1500*1500*4 = 9,000,000 bytes, which fits, so
    /// the smallest fitting k (2) is the answer.
    #[test]
    fn pick_factor_3000_picks_two() {
        assert_eq!(pick_factor(3000, 3000, 24 * 1024 * 1024), Some(2));
    }

    /// A decimal-24-MB budget (24,000,000 bytes, not the binary 24 MiB used
    /// elsewhere) makes k=4 (2500x2500 -> 25,000,000 bytes) NOT fit, so the
    /// answer is the next factor, k=5 (2000x2000 -> 16,000,000 bytes).
    /// Verified by hand: k=4 needs 2500*2500*4 = 25,000,000 > 24,000,000;
    /// k=5 needs 2000*2000*4 = 16,000,000 <= 24,000,000.
    #[test]
    fn pick_factor_10000_picks_five_under_a_decimal_budget() {
        assert_eq!(pick_factor(10000, 10000, 24_000_000), Some(5));
    }

    /// Even k=16 doesn't bring a 100000x100000 image under budget --
    /// refuses rather than decoding at a factor so aggressive the image
    /// would be useless.
    #[test]
    fn pick_factor_huge_none() {
        assert_eq!(pick_factor(100_000, 100_000, 24 * 1024 * 1024), None);
    }

    /// Encode an uncompressed (RGB8) PNG via `image`'s encoder -- fine for
    /// the non-interlaced fixtures below, which only need `decode_scaled` to
    /// exercise its normal streaming path.
    fn encode_png(img: &image::DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encode fixture png");
        out
    }

    #[test]
    fn decode_scaled_gradient_matches_dims_and_corners() {
        let (w, h) = (3000u32, 3000u32);
        let mut img = image::RgbImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let r = (x * 255 / (w - 1)) as u8;
                let g = (y * 255 / (h - 1)) as u8;
                img.put_pixel(x, y, image::Rgb([r, g, 128]));
            }
        }
        let bytes = encode_png(&image::DynamicImage::ImageRgb8(img));

        let k = pick_factor(w, h, 24 * 1024 * 1024).expect("should fit at some k");
        assert_eq!(k, 2);

        let decoded = decode_scaled(&bytes, k).expect("decode_scaled");
        assert_eq!((decoded.width(), decoded.height()), (1500, 1500));

        let rgb = decoded.to_rgb8();
        let top_left = rgb.get_pixel(0, 0);
        let bottom_right = rgb.get_pixel(1499, 1499);

        // Top-left corner averages the source's (0,0)-ish block: r,g near 0.
        assert!(top_left[0] <= 8, "top-left r too high: {top_left:?}");
        assert!(top_left[1] <= 8, "top-left g too high: {top_left:?}");
        assert!(
            (top_left[2] as i16 - 128).abs() <= 8,
            "top-left b off: {top_left:?}"
        );

        // Bottom-right corner averages the source's (w-1,h-1)-ish block:
        // r,g near 255.
        assert!(
            bottom_right[0] >= 247,
            "bottom-right r too low: {bottom_right:?}"
        );
        assert!(
            bottom_right[1] >= 247,
            "bottom-right g too low: {bottom_right:?}"
        );
        assert!(
            (bottom_right[2] as i16 - 128).abs() <= 8,
            "bottom-right b off: {bottom_right:?}"
        );
    }

    #[test]
    fn decode_scaled_rgba_alpha_survives_averaging() {
        let (w, h) = (3200u32, 3200u32);
        let mut img = image::RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                // Left half opaque, right half half-transparent -- so a
                // column straddling the midpoint (once downsampled) still
                // shows a clearly non-255, non-0 averaged alpha somewhere
                // near the boundary, and the far right is uniformly ~128.
                let a = if x < w / 2 { 255u8 } else { 128u8 };
                img.put_pixel(x, y, image::Rgba([200, 50, 50, a]));
            }
        }
        let bytes = encode_png(&image::DynamicImage::ImageRgba8(img));

        let k = pick_factor(w, h, 24 * 1024 * 1024).expect("should fit at some k");
        let decoded = decode_scaled(&bytes, k).expect("decode_scaled");
        assert!(matches!(decoded, DynamicImage::ImageRgba8(_)));

        let rgba = decoded.to_rgba8();
        let far_right = rgba.get_pixel(rgba.width() - 1, rgba.height() / 2);
        assert!(
            (far_right[3] as i16 - 128).abs() <= 4,
            "far-right alpha should be ~128, got {far_right:?}"
        );
        let far_left = rgba.get_pixel(0, rgba.height() / 2);
        assert_eq!(far_left[3], 255, "far-left alpha should stay opaque");
    }

    #[test]
    fn decode_scaled_grayscale_reports_luma_dims() {
        let (w, h) = (3000u32, 3000u32);
        let img = image::GrayImage::from_pixel(w, h, image::Luma([77]));
        let bytes = encode_png(&image::DynamicImage::ImageLuma8(img));

        let k = pick_factor(w, h, 24 * 1024 * 1024).expect("should fit at some k");
        let decoded = decode_scaled(&bytes, k).expect("decode_scaled");
        assert!(matches!(decoded, DynamicImage::ImageLuma8(_)));
        assert_eq!(
            (decoded.width(), decoded.height()),
            (
                ((w as u64 + k as u64 - 1) / k as u64) as u32,
                ((h as u64 + k as u64 - 1) / k as u64) as u32
            )
        );
        let luma = decoded.to_luma8();
        assert_eq!(luma.get_pixel(0, 0)[0], 77);
    }

    /// Standard Adam7 pass geometry (x_offset, x_step, y_offset, y_step),
    /// the same public constants the PNG spec (and libpng) define -- `png`
    /// 0.18's own copies are crate-private, so this fixture reimplements
    /// them rather than reaching into the crate's internals.
    const ADAM7_PASSES: [(u32, u32, u32, u32); 7] = [
        (0, 8, 0, 8),
        (4, 8, 0, 8),
        (0, 4, 4, 8),
        (2, 4, 0, 4),
        (0, 2, 2, 4),
        (1, 2, 0, 2),
        (0, 1, 1, 2),
    ];

    /// A minimal zlib stream wrapping `data` in a single uncompressed
    /// ("stored") DEFLATE block -- enough for the tiny fixture below,
    /// without pulling in a compression crate.
    fn stored_zlib(data: &[u8]) -> Vec<u8> {
        assert!(data.len() <= 0xFFFF, "fixture too large for one stored block");
        let mut out = Vec::with_capacity(data.len() + 11);
        out.push(0x78); // CMF: deflate, 32K window
        out.push(0x01); // FLG: fastest/no compression, valid checksum bits
        out.push(0x01); // BFINAL=1, BTYPE=00 (stored), byte-aligned
        let len = data.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(data);
        let mut a: u32 = 1;
        let mut b: u32 = 0;
        for &byte in data {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    /// Hand-builds a genuinely Adam7-interlaced 8-bit grayscale PNG (the
    /// `image` crate's encoder can't produce interlaced output at all, so
    /// this goes through `png`'s `Encoder`/`Writer` directly, writing the
    /// IDAT payload ourselves in real Adam7 pass order).
    fn encode_interlaced_gray_png(w: u32, h: u32) -> Vec<u8> {
        let mut raw = Vec::new();
        for &(xoff, xstep, yoff, ystep) in &ADAM7_PASSES {
            if w <= xoff || h <= yoff {
                continue; // this pass contributes zero samples or zero lines
            }
            let mut y = yoff;
            while y < h {
                raw.push(0u8); // filter type: None
                let mut x = xoff;
                while x < w {
                    raw.push(((x + y) % 256) as u8);
                    x += xstep;
                }
                y += ystep;
            }
        }
        let zlib_data = stored_zlib(&raw);

        let mut info = png::Info::with_size(w, h);
        info.bit_depth = BitDepth::Eight;
        info.color_type = ColorType::Grayscale;
        info.interlaced = true;

        let mut buf = Vec::new();
        {
            let encoder = png::Encoder::with_info(&mut buf, info).expect("with_info");
            let mut writer = encoder.write_header().expect("write_header");
            writer
                .write_chunk(png::chunk::IDAT, &zlib_data)
                .expect("write IDAT chunk");
            writer.finish().expect("finish");
        }
        buf
    }

    #[test]
    fn decode_scaled_rejects_interlaced_png() {
        let bytes = encode_interlaced_gray_png(64, 64);
        // Sanity check the fixture is genuinely interlaced before trusting
        // the rejection below.
        let decoder = png::Decoder::new(Cursor::new(&bytes));
        let reader = decoder.read_info().expect("fixture must parse");
        assert!(reader.info().interlaced, "fixture PNG must be interlaced");

        let err = decode_scaled(&bytes, 2).expect_err("interlaced PNG must be refused");
        assert!(
            err.contains("interlaced"),
            "error should mention interlacing, got: {err}"
        );
    }
}
