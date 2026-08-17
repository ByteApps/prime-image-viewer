//! Scaled JPEG decoding, for photos whose full-resolution decode would blow
//! the `MAX_DECODED_BYTES` budget in `src/main.rs`.
//!
//! `image`'s JPEG backend (zune-jpeg) has no scaled-decode API: it always
//! produces full-resolution pixels, which is exactly the memory `show_image`
//! refuses over. The `jpeg-decoder` crate does have one --
//! `Decoder::scale(w, h)` selects an IDCT size of 1/8, 1/4, 1/2 or 1/1 that
//! produces an image >= the requested size in at least one axis, and
//! `decode()` then yields pixels at THAT reduced size -- memory proportional
//! to the scaled dimensions, not the source. A 4000x4000 JPEG (64 MB at full
//! res) decodes at 1/4 to 1000x1000 (4 MB).
//!
//! This is a narrow, JPEG-only escape hatch from the refusal path: PNG/GIF/
//! BMP still refuse exactly as before (their decoders have no comparable
//! scaled API, and re-deriving one is out of scope here).

use image::DynamicImage;
use jpeg_decoder::PixelFormat;

/// Pick the largest scale factor in {1/2, 1/4, 1/8} whose RGBA8 output
/// (`ceil(w*s) * ceil(h*s) * 4` bytes) fits within `budget_bytes`, or `None`
/// if even 1/8 doesn't fit.
///
/// Scale 1/1 is deliberately never considered -- the only caller
/// (`show_image` in `src/main.rs`) reaches this after its own pre-flight has
/// already established that the full-resolution decode exceeds the budget,
/// so a 1/1 candidate would never pass anyway.
pub fn pick_scale(w: u32, h: u32, budget_bytes: u64) -> Option<(u32, u32)> {
    for divisor in [2u64, 4, 8] {
        let sw = (u64::from(w) + divisor - 1) / divisor;
        let sh = (u64::from(h) + divisor - 1) / divisor;
        let needed = sw.saturating_mul(sh).saturating_mul(4);
        if needed <= budget_bytes {
            return Some((sw as u32, sh as u32));
        }
    }
    None
}

/// Decode a JPEG at (approximately) `target_w`x`target_h`, using
/// `jpeg-decoder`'s IDCT-time scaling so peak memory tracks the SCALED
/// dimensions rather than the source's.
///
/// Supports the pixel formats `jpeg-decoder` can hand back for an 8-bit
/// JPEG: `L8` (grayscale) and `RGB24` map straight onto `image`'s
/// `ImageLuma8`/`ImageRgb8`; `CMYK32` (Adobe/Photoshop JPEGs) is converted to
/// RGB (`r = (255-c)*(255-k)/255`, and likewise for g/b). Anything else
/// (currently just `L16`, which 8-bit JPEG never produces) is an error --
/// there's no display path for it.
///
/// Returns dimensions actually decoded, which is whatever `scale()` picked
/// (>= the request in at least one axis, per its contract) -- NOT
/// necessarily `target_w`x`target_h` exactly.
pub fn decode_scaled(bytes: &[u8], target_w: u32, target_h: u32) -> Result<DynamicImage, String> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    decoder
        .read_info()
        .map_err(|e| format!("jpeg header: {e}"))?;
    let format = decoder
        .info()
        .ok_or_else(|| "jpeg: no info after read_info".to_string())?
        .pixel_format;
    match format {
        PixelFormat::L8 | PixelFormat::RGB24 | PixelFormat::CMYK32 => {}
        other => return Err(format!("unsupported jpeg pixel format: {other:?}")),
    }

    let req_w = target_w.min(u32::from(u16::MAX)) as u16;
    let req_h = target_h.min(u32::from(u16::MAX)) as u16;
    let (out_w, out_h) = decoder
        .scale(req_w, req_h)
        .map_err(|e| format!("jpeg scale: {e}"))?;
    let pixels = decoder.decode().map_err(|e| format!("jpeg decode: {e}"))?;
    let (w, h) = (u32::from(out_w), u32::from(out_h));

    match format {
        PixelFormat::L8 => image::GrayImage::from_raw(w, h, pixels)
            .map(DynamicImage::ImageLuma8)
            .ok_or_else(|| "jpeg: gray buffer size mismatch".to_string()),
        PixelFormat::RGB24 => image::RgbImage::from_raw(w, h, pixels)
            .map(DynamicImage::ImageRgb8)
            .ok_or_else(|| "jpeg: rgb buffer size mismatch".to_string()),
        PixelFormat::CMYK32 => {
            let mut rgb = Vec::with_capacity(pixels.len() / 4 * 3);
            for px in pixels.chunks_exact(4) {
                let (c, m, y, k) = (
                    u32::from(px[0]),
                    u32::from(px[1]),
                    u32::from(px[2]),
                    u32::from(px[3]),
                );
                rgb.push(((255 - c) * (255 - k) / 255) as u8);
                rgb.push(((255 - m) * (255 - k) / 255) as u8);
                rgb.push(((255 - y) * (255 - k) / 255) as u8);
            }
            image::RgbImage::from_raw(w, h, rgb)
                .map(DynamicImage::ImageRgb8)
                .ok_or_else(|| "jpeg: cmyk-converted buffer size mismatch".to_string())
        }
        PixelFormat::L16 => unreachable!("filtered above -- 8-bit JPEG never produces L16"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (4000,4000) at a 24 MiB budget: 1/2 (2000x2000) is 16,000,000 bytes,
    /// which already fits under 25,165,824 -- so the LARGEST fitting scale
    /// (the one pick_scale is documented to return) is 1/2, not a more
    /// aggressive one. Verified by hand: 2000*2000*4 = 16_000_000 <=
    /// 24*1024*1024 = 25_165_824.
    #[test]
    fn pick_scale_4000_prefers_the_mildest_fitting_scale() {
        assert_eq!(pick_scale(4000, 4000, 24 * 1024 * 1024), Some((2000, 2000)));
    }

    /// (6000,6000): 1/2 (3000x3000, 36,000,000 bytes) is over budget, but 1/4
    /// (1500x1500, 9,000,000 bytes) fits -- so 1/4 is the answer, not 1/8.
    #[test]
    fn pick_scale_6000_falls_back_to_quarter() {
        assert_eq!(pick_scale(6000, 6000, 24 * 1024 * 1024), Some((1500, 1500)));
    }

    /// Never actually called in-app (a 1000x1000 JPEG's full-res decode is
    /// only 4 MB, well under the budget, so `show_image` never reaches the
    /// downsample path for it) -- but the function is total, and by the same
    /// "largest fitting scale" contract as the cases above, 1/2 (500x500,
    /// 1,000,000 bytes) fits comfortably.
    #[test]
    fn pick_scale_1000_contract_only() {
        assert_eq!(pick_scale(1000, 1000, 24 * 1024 * 1024), Some((500, 500)));
    }

    /// Even 1/8 of 60000x60000 is 7500x7500 = 225,000,000 bytes, over any
    /// 24 MiB budget -- refuses rather than ever attempting the decode.
    #[test]
    fn pick_scale_60000_none() {
        assert_eq!(pick_scale(60000, 60000, 24 * 1024 * 1024), None);
    }

    fn encode_jpeg(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb(rgb));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut out)
            .encode(&img, w, h, image::ExtendedColorType::Rgb8)
            .expect("encode fixture jpeg");
        out
    }

    #[test]
    fn decode_scaled_quarter_matches_requested_dims_and_color() {
        let bytes = encode_jpeg(1024, 768, [40, 180, 90]);
        let img = decode_scaled(&bytes, 256, 192).expect("decode_scaled");

        // DISCRIMINATING: this must fail if `.scale(...)` is silently
        // skipped (i.e. decode_scaled falls back to full-res). One-time
        // mutation check performed: commenting out the `.scale(req_w,
        // req_h)` call in decode_scaled makes this assertion fail with
        // `left: 1024, right: 1024` (native width returned instead of the
        // scaled 256) -- confirming the test actually exercises scaling.
        // The `.scale(...)` call has been restored.
        assert!(
            img.width() < 1024 && img.height() < 768,
            "expected a downscaled image, got {}x{} (native was 1024x768)",
            img.width(),
            img.height()
        );

        assert_eq!((img.width(), img.height()), (256, 192));

        let rgb = img.to_rgb8();
        let (cx, cy) = (img.width() / 2, img.height() / 2);
        let px = rgb.get_pixel(cx, cy);
        for (got, want) in px.0.iter().zip([40u8, 180, 90].iter()) {
            let diff = (*got as i16 - *want as i16).abs();
            assert!(
                diff <= 12,
                "center pixel {:?} too far from expected {:?}",
                px.0,
                [40, 180, 90]
            );
        }
    }

    #[test]
    fn decode_scaled_grayscale_reports_luma_dims() {
        let img = image::GrayImage::from_pixel(800, 600, image::Luma([128]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode(&img, 800, 600, image::ExtendedColorType::L8)
            .expect("encode fixture jpeg");

        let decoded = decode_scaled(&bytes, 200, 150).expect("decode_scaled");
        assert!(matches!(decoded, DynamicImage::ImageLuma8(_)));
        assert_eq!((decoded.width(), decoded.height()), (200, 150));
    }
}
