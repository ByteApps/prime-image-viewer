mod jpeg_scaled;
mod theme;

use std::cell::RefCell;
use std::io::{Cursor, Read};
use std::rc::Rc;
use std::time::Duration;

use image::AnimationDecoder;
use slint_keyos_platform::app_ui2;
use slint_keyos_platform::fs::{self, Location, OpenFlags};
use slint_keyos_platform::slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel,
};

app_ui2!("Image Viewer");

/// Width images are displayed at: the window (480) minus the 20px content
/// padding on each side, so an image always fits the screen edge to edge.
const IMG_WIDTH: u32 = 440;
/// Cap on the displayed image height, to bound the allocation for absurd
/// aspect ratios (the source is downscaled to fit, never cropped).
const MAX_IMG_HEIGHT: u32 = 4096;

/// File extensions the browser lists and the viewer decodes.
const IMAGE_EXTS: [&str; 5] = [".png", ".jpg", ".jpeg", ".gif", ".bmp"];

/// Cap on decoded GIF frames, to bound memory (frames are held scaled,
/// RGBA8: a full-width 440x330 frame is ~580 KB).
const MAX_GIF_FRAMES: usize = 64;

/// Budget for decoding ONE image, checked against the header's dimensions
/// BEFORE any pixels are decoded.
///
/// The file size is no guide at all here. Measured with
/// `cargo run --release --example mem_probe` (one process per file, peak RSS):
///
/// | file          | dimensions | w*h*4    | peak RSS |
/// |---------------|------------|----------|----------|
/// | 0.03 MB jpg   | 1000x1000  |   3.8 MB |  13.2 MB |
/// | 0.17 MB jpg   | 3000x3000  |  34.3 MB |  49.6 MB |
/// | 0.29 MB jpg   | 4000x4000  |  61.0 MB |  76.5 MB |
/// | 0.63 MB jpg   | 6000x6000  | 137.3 MB | 147.5 MB |
///
/// i.e. peak tracks `w*h*4` plus ~10 MB, so a 290 KB photo can want 76 MB --
/// the same shape of crash the PDF viewer hit on an 11 MB file. A decode that
/// exceeds the heap ABORTS the process (no error path, no log), so this
/// refuses first and shows the numbers.
///
/// EMPIRICAL, pending device calibration: KeyOS exposes no per-app heap
/// budget to read. 24 MB admits a 2400x2400 photo.
///
/// The 4000x4000 and 6000x6000 rows above are what a FULL-resolution decode
/// would have cost -- since `jpeg_scaled` shipped, a JPEG over this budget no
/// longer hits that fate: it's downsampled at IDCT time instead (see
/// `jpeg_scaled::pick_scale`/`decode_scaled`, wired in from `show_image`'s
/// refusal block). Re-measured with the same probe, post-downsample:
/// 4000x4000 -> 2000x2000 (1/2) peaked at 40.9 MB; 6000x6000 -> 1500x1500
/// (1/4) peaked at 27.1 MB. Non-JPEG formats (PNG/GIF/BMP have no comparable
/// scaled-decode API) still refuse exactly as this table describes.
const MAX_DECODED_BYTES: u64 = 24 * 1024 * 1024;

/// Budget for an animation's retained (already downscaled) frames.
///
/// Frames are kept for playback, so their cost is the SUM. At the display
/// width that is ~0.77 MB each, and MAX_GIF_FRAMES of them would be ~49 MB on
/// its own. Decoding stops when this is reached and the animation plays the
/// frames that fit, which beats refusing the file or dying on it.
const MAX_ANIMATION_BYTES: u64 = 12 * 1024 * 1024;

/// Mutable app state shared across the UI callbacks.
struct State {
    location: Location,
    path: String,        // current directory, always starts with '/'
    images: Vec<String>, // image file names in `path`, in display order
    img_idx: usize,      // index into `images` of the image on screen
    viewing: bool,       // true while the viewer screen is up
    // Decoded frames of the on-screen image, scaled for display. One entry
    // for static images; animated GIFs hold every frame and `anim_timer`
    // cycles `frame_idx` through them.
    frames: Vec<SharedPixelBuffer<Rgba8Pixel>>,
    frame_idx: usize,
    // True while a decode is in flight (deferred to a single_shot Timer so
    // the loading overlay paints before the blocking work runs). Guards
    // entry-activated/next/prev/go-back/close-viewer against a queued-up
    // re-tap while the UI is frozen doing the decode.
    busy: bool,
}

fn is_image_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    IMAGE_EXTS.iter().any(|ext| lower.ends_with(ext))
}

fn app_main(cx: AppContext, ui: AppWindow) {
    log_server::init_wait(env!("CARGO_CRATE_NAME")).unwrap();
    log::set_max_level(log::LevelFilter::Info);

    theme::init(&ui);

    let fs = cx.fs.clone();
    let ui_weak = ui.as_weak();
    let state = Rc::new(RefCell::new(State {
        location: Location::User,
        path: "/".to_string(),
        images: Vec::new(),
        img_idx: 0,
        viewing: false,
        frames: Vec::new(),
        frame_idx: 0,
        busy: false,
    }));
    // Drives animated GIF playback; show_image() (re)starts or stops it.
    let anim_timer = Rc::new(Timer::default());

    // Re-list the current directory (folders + images) into the Browser global.
    let refresh: Rc<dyn Fn()> = {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        Rc::new(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (loc, path) = {
                let s = state.borrow();
                (s.location, s.path.clone())
            };
            let browser = ui.global::<Browser>();

            let mut items: Vec<(bool, String, String)> = Vec::new();
            let mut status = String::new();
            match fs.open_dir(path.as_str(), loc) {
                Ok(dir) => loop {
                    match dir.next_entry() {
                        Ok(Some(entry)) => {
                            if entry.name.starts_with('.') {
                                continue; // includes "." and ".."
                            }
                            if entry.is_dir {
                                items.push((true, entry.name, "Folder".to_string()));
                            } else if is_image_name(&entry.name) {
                                items.push((false, entry.name, human_size(entry.len)));
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            status = err_msg(&e);
                            break;
                        }
                    }
                },
                Err(e) => status = err_msg(&e),
            }

            // Folders first, then alphabetical (case-insensitive).
            items.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
            });

            // Remember the images in display order, so the viewer's prev/next
            // can move between the folder's images.
            state.borrow_mut().images = items
                .iter()
                .filter(|(is_dir, ..)| !is_dir)
                .map(|(_, name, _)| name.clone())
                .collect();

            let rows: Vec<FileRow> = items
                .into_iter()
                .map(|(is_dir, name, info)| FileRow {
                    name: name.into(),
                    info: info.into(),
                    is_folder: is_dir,
                })
                .collect();

            browser.set_entries(ModelRc::new(VecModel::from(rows)));
            browser.set_path(path.clone().into());
            browser.set_at_root(path == "/");
            browser.set_status(status.into());
        })
    };

    // Populate the initial (Internal) listing.
    refresh();

    let callbacks = ui.global::<Callbacks>();

    // Switch storage tab: Internal / Airlock / USB. Resets to that root.
    {
        let state = state.clone();
        let refresh = refresh.clone();
        callbacks.on_location_changed(move |idx| {
            {
                let mut s = state.borrow_mut();
                s.location = location_for(idx);
                s.path = "/".to_string();
            }
            refresh();
        });
    }

    // Tap a row: descend into a folder, or open an image in the viewer.
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let refresh = refresh.clone();
        let anim_timer = anim_timer.clone();
        callbacks.on_entry_activated(move |name, is_folder| {
            let Some(ui) = ui_weak.upgrade() else { return };

            if is_folder {
                {
                    let mut s = state.borrow_mut();
                    s.path = join_path(&s.path, name.as_str());
                }
                refresh();
                return;
            }

            if state.borrow().busy {
                return;
            }

            log::info!("cb: open-image {name}");

            state.borrow_mut().busy = true;
            let ui_state = ui.global::<Ui>();
            ui_state.set_loading(true);
            ui_state.set_loading_text(format!("Opening {name}…").into());
            log::info!("loading: {name}");

            // Defer the actual read + decode a tick, so this frame (with the
            // overlay up) paints before the blocking work runs.
            let fs = fs.clone();
            let state = state.clone();
            let ui_weak = ui_weak.clone();
            let anim_timer = anim_timer.clone();
            let name = name.clone();
            Timer::single_shot(Duration::from_millis(0), move || {
                let Some(ui) = ui_weak.upgrade() else { return };

                let idx = state
                    .borrow()
                    .images
                    .iter()
                    .position(|n| n == name.as_str())
                    .unwrap_or(0);
                state.borrow_mut().img_idx = idx;
                if show_image(&fs, &ui, &state, &anim_timer) {
                    show_info(&ui, "");
                    state.borrow_mut().viewing = true;
                    ui.global::<Ui>().set_viewing(true);
                }

                state.borrow_mut().busy = false;
                ui.global::<Ui>().set_loading(false);
                log::info!("loading done");
            });
        });
    }

    // Back button: go up one directory.
    {
        let state = state.clone();
        let refresh = refresh.clone();
        callbacks.on_go_back(move || {
            if state.borrow().busy {
                return;
            }
            {
                let mut s = state.borrow_mut();
                s.path = parent_path(&s.path);
            }
            refresh();
        });
    }

    // Leave the viewer.
    {
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let anim_timer = anim_timer.clone();
        callbacks.on_close_viewer(move || {
            if state.borrow().busy {
                return;
            }
            log::info!("cb: close-viewer");
            anim_timer.stop();
            let mut s = state.borrow_mut();
            s.viewing = false;
            s.img_idx = 0;
            s.frames.clear(); // drop the decoded pixels
            s.frame_idx = 0;
            if let Some(ui) = ui_weak.upgrade() {
                ui.global::<Viewer>().set_img(Image::default());
                ui.global::<Ui>().set_viewing(false);
            }
        });
    }

    // Previous / next image in the folder.
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let anim_timer = anim_timer.clone();
        callbacks.on_prev_image(move || {
            if state.borrow().busy {
                return;
            }
            log::info!("cb: prev-image");
            let Some(ui) = ui_weak.upgrade() else { return };
            let (step, target) = {
                let mut s = state.borrow_mut();
                let ok = s.viewing && s.img_idx > 0;
                if ok {
                    s.img_idx -= 1;
                }
                (ok, s.images.get(s.img_idx).cloned().unwrap_or_default())
            };
            if !step {
                return;
            }

            state.borrow_mut().busy = true;
            let ui_state = ui.global::<Ui>();
            ui_state.set_loading(true);
            ui_state.set_loading_text(format!("Opening {target}…").into());
            log::info!("loading: {target}");

            let fs = fs.clone();
            let state = state.clone();
            let ui_weak = ui_weak.clone();
            let anim_timer = anim_timer.clone();
            Timer::single_shot(Duration::from_millis(0), move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                show_image(&fs, &ui, &state, &anim_timer);
                state.borrow_mut().busy = false;
                ui.global::<Ui>().set_loading(false);
                log::info!("loading done");
            });
        });
    }
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let anim_timer = anim_timer.clone();
        callbacks.on_next_image(move || {
            if state.borrow().busy {
                return;
            }
            log::info!("cb: next-image");
            let Some(ui) = ui_weak.upgrade() else { return };
            let (step, target) = {
                let mut s = state.borrow_mut();
                let ok = s.viewing && s.img_idx + 1 < s.images.len();
                if ok {
                    s.img_idx += 1;
                }
                (ok, s.images.get(s.img_idx).cloned().unwrap_or_default())
            };
            if !step {
                return;
            }

            state.borrow_mut().busy = true;
            let ui_state = ui.global::<Ui>();
            ui_state.set_loading(true);
            ui_state.set_loading_text(format!("Opening {target}…").into());
            log::info!("loading: {target}");

            let fs = fs.clone();
            let state = state.clone();
            let ui_weak = ui_weak.clone();
            let anim_timer = anim_timer.clone();
            Timer::single_shot(Duration::from_millis(0), move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                show_image(&fs, &ui, &state, &anim_timer);
                state.borrow_mut().busy = false;
                ui.global::<Ui>().set_loading(false);
                log::info!("loading done");
            });
        });
    }

    ui.run().expect("UI running");
}

/// Read + decode the current image (every frame, for animated GIFs), scale
/// to fit the screen width, push the first frame into the Viewer global, and
/// start the animation timer when there is more than one frame. On failure
/// shows the error banner and returns false (the browser stays usable, as in
/// prime-pdf-viewer).
fn show_image(
    fs: &fs::FileSystem<fs_permissions::FileSystemPermissions>,
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    anim_timer: &Rc<Timer>,
) -> bool {
    anim_timer.stop();

    let (name, full, loc, pos, count) = {
        let s = state.borrow();
        let Some(name) = s.images.get(s.img_idx) else {
            return false;
        };
        (
            name.clone(),
            join_path(&s.path, name),
            s.location,
            s.img_idx + 1,
            s.images.len(),
        )
    };

    let bytes = match read_bytes(fs, &full, loc) {
        Ok(b) => b,
        Err(msg) => {
            show_error(ui, msg);
            return false;
        }
    };

    // Pre-flight: the header gives us the dimensions for a few hundred bytes,
    // and w*h*4 is what the decode will cost. Refuse here, because the decode
    // itself cannot fail gracefully -- it aborts the process.
    //
    // JPEGs get one more chance before refusal: `jpeg_scaled::pick_scale`
    // looks for an IDCT scale (1/2, 1/4, 1/8) whose output fits the budget,
    // and if it finds one, `downsample_target` routes the decode below
    // through `jpeg_scaled::decode_scaled` instead of the full-resolution
    // `image::load_from_memory` path. Every other format (and any JPEG
    // where even 1/8 doesn't fit) refuses exactly as before.
    let mut downsample_target: Option<(u32, u32)> = None;
    match image_dimensions_and_format(&bytes) {
        Some((iw, ih, fmt)) => {
            let needed = (iw as u64).saturating_mul(ih as u64).saturating_mul(4);
            if needed > MAX_DECODED_BYTES {
                if fmt == image::ImageFormat::Jpeg {
                    downsample_target = jpeg_scaled::pick_scale(iw, ih, MAX_DECODED_BYTES);
                }
                match downsample_target {
                    Some((tw, th)) => {
                        log::info!("downsampled {iw}x{ih} -> {tw}x{th}");
                    }
                    None => {
                        log::warn!(
                            "{name} is {iw}x{ih}, needs ~{} to decode (limit {})",
                            human_size(needed),
                            human_size(MAX_DECODED_BYTES)
                        );
                        let msg = format!(
                            "This image is {iw}x{ih} — too large to display (needs ~{}).",
                            human_size(needed)
                        );
                        show_error(ui, msg.clone());
                        // Keep the viewer on THIS image rather than silently
                        // leaving the previous one on screen: the index
                        // already advanced, so without this the app looks
                        // like it ignored the tap.
                        let viewer = ui.global::<Viewer>();
                        viewer.set_img(Image::default());
                        viewer.set_img_h(0.0);
                        viewer.set_message(msg.into());
                        viewer.set_doc_name(name.as_str().into());
                        viewer.set_img_num(pos as i32);
                        viewer.set_img_count(count as i32);
                        return false;
                    }
                }
            }
        }
        // Unknown format/dimensions: let the decoder produce the real error
        // rather than guessing. It will fail on the format, not on memory.
        None => {}
    }

    // Decoders are pure Rust and shouldn't panic, but a panic here would take
    // the whole app down — contain it (same policy as prime-pdf-viewer).
    let decoded: std::thread::Result<Result<(Vec<SharedPixelBuffer<Rgba8Pixel>>, Duration), String>> =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some((tw, th)) = downsample_target {
                let img = jpeg_scaled::decode_scaled(&bytes, tw, th)?;
                Ok((vec![to_display_buffer(img)], Duration::ZERO))
            } else {
                decode_frames(&name, &bytes).map_err(|e| e.to_string())
            }
        }));
    let (frames, delay) = match decoded {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            log::warn!("open-image failed: {e}");
            show_error(
                ui,
                "Couldn't open this file. It may not be an image, or the format is unsupported."
                    .to_string(),
            );
            return false;
        }
        Err(_) => {
            log::warn!("decode panicked on {name}");
            show_error(ui, "Couldn't decode this image".to_string());
            return false;
        }
    };

    let (w, h) = (frames[0].width(), frames[0].height());
    // Displayed height at the fit-to-width scale (images narrower than the
    // screen are stretched up to it, matching the PDF viewer's behavior).
    let disp_h = (h as f32) * (IMG_WIDTH as f32) / (w as f32);
    let frame_count = frames.len();

    {
        let mut s = state.borrow_mut();
        s.frames = frames;
        s.frame_idx = 0;
    }

    let viewer = ui.global::<Viewer>();
    viewer.set_message("".into());
    viewer.set_img(Image::from_rgba8(state.borrow().frames[0].clone()));
    viewer.set_img_h(disp_h);
    viewer.set_img_num(pos as i32);
    viewer.set_img_count(count as i32);
    viewer.set_doc_name(name.as_str().into());
    log::info!("rendered image {pos}/{count} {w}x{h}");

    if frame_count > 1 {
        log::info!(
            "gif: {name} playing {frame_count} frames at {}ms",
            delay.as_millis()
        );
        let ui_weak = ui.as_weak();
        let st = state.clone();
        anim_timer.start(TimerMode::Repeated, delay, move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut s = st.borrow_mut();
            if s.frames.len() < 2 {
                return;
            }
            s.frame_idx = (s.frame_idx + 1) % s.frames.len();
            ui.global::<Viewer>()
                .set_img(Image::from_rgba8(s.frames[s.frame_idx].clone()));
        });
    }
    true
}

/// Decode `bytes` into display-scaled RGBA frame buffers. Static formats
/// produce one frame; animated GIFs produce up to MAX_GIF_FRAMES plus the
/// inter-frame delay (frame 0's delay is used for the whole loop — GIFs with
/// per-frame delays play at a uniform rate).
fn decode_frames(
    name: &str,
    bytes: &[u8],
) -> image::ImageResult<(Vec<SharedPixelBuffer<Rgba8Pixel>>, Duration)> {
    if name.to_lowercase().ends_with(".gif") {
        let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))?;
        // Scale each frame as it arrives and drop the full-resolution buffer
        // immediately. Collecting the frames first (which this used to do)
        // holds EVERY frame at full size at once: measured at 179 MB peak for
        // a 13 MB, 24-frame 1200x1200 GIF, against ~13 MB for the streamed
        // version. Only one full-res frame is alive at a time now, and the
        // retained scaled frames are capped by MAX_ANIMATION_BYTES.
        let mut delay = Duration::ZERO;
        let mut bufs: Vec<SharedPixelBuffer<Rgba8Pixel>> = Vec::new();
        let mut retained: u64 = 0;
        for (i, frame) in decoder.into_frames().take(MAX_GIF_FRAMES).enumerate() {
            let frame = frame?;
            if i == 0 {
                delay = Duration::from(frame.delay());
            }
            let buf = to_display_buffer(image::DynamicImage::ImageRgba8(frame.into_buffer()));
            retained = retained
                .saturating_add(buf.width() as u64 * buf.height() as u64 * 4);
            bufs.push(buf);
            if retained >= MAX_ANIMATION_BYTES {
                log::warn!("animation truncated at {} frames ({})", bufs.len(), human_size(retained));
                break;
            }
        }
        if bufs.len() > 1 {
            let delay = if delay.is_zero() {
                Duration::from_millis(100) // browsers' fallback for 0-delay GIFs
            } else {
                delay
            };
            return Ok((bufs, delay));
        }
        // 0- or 1-frame GIF: decode as a static image below.
    }
    let img = image::load_from_memory(bytes)?;
    Ok((vec![to_display_buffer(img)], Duration::ZERO))
}

/// Read just the header to get an image's dimensions and detected format,
/// without decoding pixels. Returns None when the format is unknown or the
/// header is unreadable -- the decoder then produces the real error. The
/// format is what lets `show_image` offer JPEGs (and only JPEGs) the scaled
/// decode path instead of a flat refusal.
fn image_dimensions_and_format(bytes: &[u8]) -> Option<(u32, u32, image::ImageFormat)> {
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let fmt = reader.format()?;
    let (w, h) = reader.into_dimensions().ok()?;
    Some((w, h, fmt))
}

/// Downscale to the display width/height caps (never upscale — small images
/// are stretched at display time) and convert to a Slint pixel buffer.
fn to_display_buffer(img: image::DynamicImage) -> SharedPixelBuffer<Rgba8Pixel> {
    let img = if img.width() > IMG_WIDTH || img.height() > MAX_IMG_HEIGHT {
        img.resize(IMG_WIDTH, MAX_IMG_HEIGHT, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.into_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    buf.make_mut_bytes().copy_from_slice(rgba.as_raw());
    buf
}

/// Read a whole file; returns a user-facing message on failure.
fn read_bytes(
    fs: &fs::FileSystem<fs_permissions::FileSystemPermissions>,
    path: &str,
    loc: Location,
) -> Result<Vec<u8>, String> {
    let mut file = fs
        .open_file(path, loc, OpenFlags::READ_ONLY)
        .map_err(|e| err_msg(&e))?;
    let len = file.metadata().map(|m| m.size).unwrap_or(0);
    let mut buf = Vec::new();
    // `read_to_end` grows by doubling and ABORTS if an allocation fails, which
    // is a crash with no error path. Reserve the exact size fallibly instead.
    buf.try_reserve_exact(len as usize)
        .map_err(|_| "Not enough memory to open this image".to_string())?;
    file.read_to_end(&mut buf)
        .map_err(|_| "Read failed".to_string())?;
    Ok(buf)
}

fn show_info(ui: &AppWindow, msg: &str) {
    let u = ui.global::<Ui>();
    u.set_message(msg.into());
    u.set_message_error(false);
}

fn show_error(ui: &AppWindow, msg: String) {
    let u = ui.global::<Ui>();
    u.set_message(msg.into());
    u.set_message_error(true);
}

fn location_for(index: i32) -> Location {
    match index {
        1 => Location::Airlock,
        2 => Location::Usb,
        _ => Location::User,
    }
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn parent_path(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn err_msg(e: &fs::Error) -> String {
    use slint_keyos_platform::fs::Error::*;
    match e {
        NoMedia => "Not connected".to_string(),
        AccessDenied => "Access denied".to_string(),
        FileNotFound => "Not found".to_string(),
        FileAlreadyExists => "Already exists".to_string(),
        FileInUse => "File is in use".to_string(),
        InvalidPath => "Invalid name".to_string(),
        other => format!("Error: {other:?}"),
    }
}
