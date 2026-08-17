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

use std::io::Cursor;
use std::time::Duration;

use image::AnimationDecoder;

const IMG_WIDTH: u32 = 440; // must match src/main.rs
const MAX_IMG_HEIGHT: u32 = 4096;
const MAX_GIF_FRAMES: usize = 64;

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
            // Mirrors src/main.rs: collect frames, THEN scale each one.
            let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(&bytes)).unwrap();
            let frames: Vec<_> = decoder.into_frames().take(MAX_GIF_FRAMES).collect::<Result<_, _>>().unwrap();
            let n = frames.len();
            let scaled: Vec<_> = frames
                .into_iter()
                .map(|f| {
                    let img = image::DynamicImage::ImageRgba8(f.into_buffer());
                    img.resize(IMG_WIDTH, MAX_IMG_HEIGHT, image::imageops::FilterType::Triangle)
                })
                .collect();
            let peak = peak_rss();
            println!(
                "{:<24} {:>8.1} {:>12} {:>10.1} {:>11.1} {:>7.1}  ({} frames)",
                name, file_mb, format!("{w}x{h}"), decoded_mb, mb(peak),
                mb(peak.saturating_sub(base).max(1)) / decoded_mb.max(0.1), n
            );
            drop(scaled);
            let _ = Duration::ZERO;
        } else {
            let img = image::load_from_memory(&bytes).unwrap();
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
