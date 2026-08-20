//! True live-heap accounting for the decode paths: unlike mem_probe (peak
//! RSS, includes the process baseline), this counts live bytes through a
//! wrapping global allocator, plus the largest single allocation request.
//! Written 2026-08-19 while chasing the img-6000.jpg device OOM (which
//! turned out to be jpeg-decoder's unbounded mpsc worker channels -- see
//! vendor/jpeg-decoder -- but this instrument is what proved the decode's
//! own demand was innocent: 17.7 MB peak for the 6000 vs 25.7 for the 4000).
//!
//!   cargo run --release --example heap_probe -- <image>

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../src/jpeg_scaled.rs"]
mod jpeg_scaled;
#[path = "../src/png_scaled.rs"]
mod png_scaled;

const IMG_WIDTH: u32 = 440;
const MAX_IMG_HEIGHT: u32 = 4096;
const MAX_DECODED_BYTES: u64 = 8 * 1024 * 1024;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static MAX_ONE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
            MAX_ONE.fetch_max(layout.size(), Ordering::SeqCst);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = System.realloc(ptr, layout, new_size);
        if !p.is_null() {
            let live = LIVE
                .fetch_add(new_size.wrapping_sub(layout.size()), Ordering::SeqCst)
                .wrapping_add(new_size.wrapping_sub(layout.size()));
            PEAK.fetch_max(live, Ordering::SeqCst);
            MAX_ONE.fetch_max(new_size, Ordering::SeqCst);
        }
        p
    }
}

#[global_allocator]
static A: Counting = Counting;

fn mb(n: usize) -> f64 {
    n as f64 / (1024.0 * 1024.0)
}

fn checkpoint(label: &str) {
    println!(
        "  [{label}] live {:.1} MB, peak {:.1} MB, biggest single alloc {:.1} MB",
        mb(LIVE.load(Ordering::SeqCst)),
        mb(PEAK.load(Ordering::SeqCst)),
        mb(MAX_ONE.load(Ordering::SeqCst)),
    );
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: heap_probe <image>");
    let bytes = std::fs::read(&path).unwrap();
    println!("{path}: {} bytes on disk", bytes.len());
    checkpoint("file read");

    let reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .unwrap();
    let fmt = reader.format().unwrap();
    let (w, h) = reader.into_dimensions().unwrap();
    let needed = (w as u64) * (h as u64) * 4;
    checkpoint("pre-flight");
    println!("  {w}x{h} {fmt:?}, full-res cost {:.1} MB", mb(needed as usize));

    let img = if needed > MAX_DECODED_BYTES && fmt == image::ImageFormat::Jpeg {
        let (tw, th) = jpeg_scaled::pick_scale(w, h, MAX_DECODED_BYTES).expect("no scale fits");
        println!("  downsampling {w}x{h} -> {tw}x{th}");
        let img = jpeg_scaled::decode_scaled(&bytes, tw, th).unwrap();
        checkpoint("scaled decode");
        img
    } else if needed > MAX_DECODED_BYTES && fmt == image::ImageFormat::Png {
        let k = png_scaled::pick_factor(w, h, MAX_DECODED_BYTES).expect("no factor fits");
        let img = png_scaled::decode_scaled(&bytes, k).unwrap();
        checkpoint("scaled decode");
        img
    } else {
        let img = image::load_from_memory(&bytes).unwrap();
        checkpoint("full decode");
        img
    };

    // to_display_buffer's shape, minus the Slint buffer (a same-size Vec copy
    // stands in for SharedPixelBuffer).
    let img = if img.width() > IMG_WIDTH || img.height() > MAX_IMG_HEIGHT {
        img.thumbnail(IMG_WIDTH, MAX_IMG_HEIGHT)
    } else {
        img
    };
    checkpoint("resize");
    let rgba = img.into_rgba8();
    let copy = rgba.as_raw().clone();
    checkpoint("display buffer");
    println!(
        "RESULT {}: peak live heap {:.1} MB, biggest single alloc {:.1} MB (display {}x{}, {} retained)",
        path,
        mb(PEAK.load(Ordering::SeqCst)),
        mb(MAX_ONE.load(Ordering::SeqCst)),
        rgba.width(),
        rgba.height(),
        copy.len(),
    );
}
