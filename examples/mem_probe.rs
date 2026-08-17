//! Measure what opening an image actually costs in memory.
//!
//! Same question the PDF viewer had to answer, and the same trap: the FILE
//! size says almost nothing. A 200 KB JPEG of a 4000x4000 photo decodes to
//! 48 MB (RGB8) and then 64 MB (RGBA8) before it is scaled down to something
//! the 480x800 screen can show. KeyOS publishes no per-app heap budget, so
//! the limit the app enforces has to come from measurements like these.
//!
//!   cargo run --release --example mem_probe -- <image> [more...]
//!
//! Run ONE file per process: peak RSS is a high-water mark, so a second file
//! inherits the first one's and every ratio after it is meaningless.
//!
//! This probe mirrors `src/main.rs`'s actual decode paths -- both branches
//! below must be kept in step with it, or the numbers this tool reports stop
//! meaning anything:
//!   - the static-image branch now takes the same scaled-JPEG detour
//!     `show_image` does (via `jpeg_scaled`, `#[path]`-included below) once
//!     `w*h*4` exceeds `MAX_DECODED_BYTES`;
//!   - the GIF branch streams frame-by-frame (scale-then-drop, capped by
//!     `MAX_ANIMATION_BYTES`) instead of collecting every full-res frame
//!     first -- `decode_frames` switched to this shape to fix a 179 MB peak
//!     on a 13 MB GIF, and a probe still doing the old collect-then-scale
//!     thing would silently misreport that.

use std::io::Cursor;
use std::time::Duration;

use image::AnimationDecoder;

// Kept in step with src/jpeg_scaled.rs by literal inclusion (not a copy) --
// see the module doc comment there for what it does and why.
#[path = "../src/jpeg_scaled.rs"]
mod jpeg_scaled;

const IMG_WIDTH: u32 = 440; // must match src/main.rs
const MAX_IMG_HEIGHT: u32 = 4096; // must match src/main.rs
const MAX_GIF_FRAMES: usize = 64; // must match src/main.rs
const MAX_DECODED_BYTES: u64 = 24 * 1024 * 1024; // must match src/main.rs
const MAX_ANIMATION_BYTES: u64 = 12 * 1024 * 1024; // must match src/main.rs

fn peak_rss() -> u64 {
    #[repr(C)]
    #[derive(Default)]
    struct RUsage {
        ru_utime: [i64; 2],
        ru_stime: [i64; 2],
        ru_maxrss: i64,
        rest: [i64; 14],
    }
    extern "C" {
        fn getrusage(who: i32, usage: *mut RUsage) -> i32;
    }
    let mut u = RUsage::default();
    unsafe { getrusage(0, &mut u) };
    if cfg!(target_os = "macos") { u.ru_maxrss as u64 } else { u.ru_maxrss as u64 * 1024 }
}

fn mb(bytes: u64) -> f64 { bytes as f64 / (1024.0 * 1024.0) }

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: mem_probe <image> [more...]");
        std::process::exit(2);
    }

    println!(
        "{:<24} {:>8} {:>12} {:>10} {:>11} {:>7}",
        "file", "size MB", "dimensions", "w*h*4 MB", "peak RSS MB", "ratio"
    );
    for path in &args {
        let Ok(bytes) = std::fs::read(path) else {
            println!("{path}: read failed");
            continue;
        };
        let file_mb = mb(bytes.len() as u64);
        let base = peak_rss();

        // Header-only read: this is what the app's pre-flight can afford to do
        // before committing to a decode.
        let dims = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .ok()
            .and_then(|r| r.into_dimensions().ok());
        let (w, h) = dims.unwrap_or((0, 0));
        let decoded_mb = mb(w as u64 * h as u64 * 4);

        let name = path.rsplit('/').next().unwrap_or(path);
        let is_gif = name.to_lowercase().ends_with(".gif");
        if is_gif {
            // Mirrors src/main.rs's decode_frames: scale each frame as it
            // arrives and drop the full-resolution buffer immediately,
            // capped by MAX_ANIMATION_BYTES -- NOT collect-then-scale (that
            // was the 179 MB / 13 MB GIF bug decode_frames was fixed for).
            let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(&bytes)).unwrap();
            let mut n = 0usize;
            let mut retained: u64 = 0;
            let mut bufs: Vec<image::RgbaImage> = Vec::new();
            for frame in decoder.into_frames().take(MAX_GIF_FRAMES) {
                let frame = frame.unwrap();
                let img = image::DynamicImage::ImageRgba8(frame.into_buffer());
                let scaled = img
                    .resize(IMG_WIDTH, MAX_IMG_HEIGHT, image::imageops::FilterType::Triangle)
                    .into_rgba8();
                retained = retained
                    .saturating_add(scaled.width() as u64 * scaled.height() as u64 * 4);
                n += 1;
                bufs.push(scaled);
                if retained >= MAX_ANIMATION_BYTES {
                    break;
                }
            }
            let peak = peak_rss();
            println!(
                "{:<24} {:>8.1} {:>12} {:>10.1} {:>11.1} {:>7.1}  ({} frames, {:.1} MB retained)",
                name, file_mb, format!("{w}x{h}"), decoded_mb, mb(peak),
                mb(peak.saturating_sub(base).max(1)) / decoded_mb.max(0.1), n, mb(retained)
            );
            drop(bufs);
            let _ = Duration::ZERO;
        } else {
            // Mirrors src/main.rs's show_image: an oversize JPEG (w*h*4 over
            // MAX_DECODED_BYTES) takes the scaled-decode detour instead of
            // image::load_from_memory's always-full-resolution path.
            let is_jpeg = name.to_lowercase().ends_with(".jpg") || name.to_lowercase().ends_with(".jpeg");
            let needed = w as u64 * h as u64 * 4;
            let img = if is_jpeg && needed > MAX_DECODED_BYTES {
                match jpeg_scaled::pick_scale(w, h, MAX_DECODED_BYTES) {
                    Some((tw, th)) => match jpeg_scaled::decode_scaled(&bytes, tw, th) {
                        Ok(img) => img,
                        Err(e) => {
                            println!("{name}: decode_scaled failed: {e}");
                            continue;
                        }
                    },
                    None => {
                        println!("{name}: refused, too large even at 1/8 scale");
                        continue;
                    }
                }
            } else {
                image::load_from_memory(&bytes).unwrap()
            };
            let scaled = img.resize(IMG_WIDTH, MAX_IMG_HEIGHT, image::imageops::FilterType::Triangle);
            let rgba = scaled.into_rgba8();
            let peak = peak_rss();
            println!(
                "{:<24} {:>8.1} {:>12} {:>10.1} {:>11.1} {:>7.1}",
                name, file_mb, format!("{w}x{h}"), decoded_mb, mb(peak),
                mb(peak.saturating_sub(base).max(1)) / decoded_mb.max(0.1)
            );
            drop(rgba);
        }
    }
}
