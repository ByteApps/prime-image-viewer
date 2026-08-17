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

            log::info!("cb: open-image {name}");
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
        });
    }

    // Back button: go up one directory.
    {
        let state = state.clone();
        let refresh = refresh.clone();
        callbacks.on_go_back(move || {
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
            log::info!("cb: prev-image");
            let Some(ui) = ui_weak.upgrade() else { return };
            let step = {
                let mut s = state.borrow_mut();
                let ok = s.viewing && s.img_idx > 0;
                if ok {
                    s.img_idx -= 1;
                }
                ok
            };
            if step {
                show_image(&fs, &ui, &state, &anim_timer);
            }
        });
    }
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let anim_timer = anim_timer.clone();
        callbacks.on_next_image(move || {
            log::info!("cb: next-image");
            let Some(ui) = ui_weak.upgrade() else { return };
            let step = {
                let mut s = state.borrow_mut();
                let ok = s.viewing && s.img_idx + 1 < s.images.len();
                if ok {
                    s.img_idx += 1;
                }
                ok
            };
            if step {
                show_image(&fs, &ui, &state, &anim_timer);
            }
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

    // Decoders are pure Rust and shouldn't panic, but a panic here would take
    // the whole app down — contain it (same policy as prime-pdf-viewer).
    let decoded =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_frames(&name, &bytes)));
    let (frames, delay) = match decoded {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            log::warn!("open-image failed: {e:?}");
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
        let frames = decoder
            .into_frames()
            .take(MAX_GIF_FRAMES)
            .collect::<image::ImageResult<Vec<_>>>()?;
        if frames.len() > 1 {
            let delay = Duration::from(frames[0].delay());
            let delay = if delay.is_zero() {
                Duration::from_millis(100) // browsers' fallback for 0-delay GIFs
            } else {
                delay
            };
            let bufs = frames
                .into_iter()
                .map(|f| to_display_buffer(image::DynamicImage::ImageRgba8(f.into_buffer())))
                .collect();
            return Ok((bufs, delay));
        }
        // 0- or 1-frame GIF: decode as a static image below.
    }
    let img = image::load_from_memory(bytes)?;
    Ok((vec![to_display_buffer(img)], Duration::ZERO))
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
    let mut buf = Vec::new();
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
