mod theme;

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;

use slint_keyos_platform::app_ui;
use slint_keyos_platform::fs::{self, Location, OpenFlags};
use slint_keyos_platform::slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
};

app_ui!("prime-image-viewer");

/// Width images are displayed at: the window (480) minus the 20px content
/// padding on each side, so an image always fits the screen edge to edge.
const IMG_WIDTH: u32 = 440;
/// Cap on the displayed image height, to bound the allocation for absurd
/// aspect ratios (the source is downscaled to fit, never cropped).
const MAX_IMG_HEIGHT: u32 = 4096;

/// File extensions the browser lists and the viewer decodes.
const IMAGE_EXTS: [&str; 5] = [".png", ".jpg", ".jpeg", ".gif", ".bmp"];

/// Mutable app state shared across the UI callbacks.
struct State {
    location: Location,
    path: String,        // current directory, always starts with '/'
    images: Vec<String>, // image file names in `path`, in display order
    img_idx: usize,      // index into `images` of the image on screen
    viewing: bool,       // true while the viewer screen is up
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
    }));

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
            {
                let mut s = state.borrow_mut();
                s.img_idx = idx;
            }
            if show_image(&fs, &ui, &state.borrow()) {
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
        callbacks.on_close_viewer(move || {
            log::info!("cb: close-viewer");
            let mut s = state.borrow_mut();
            s.viewing = false;
            s.img_idx = 0;
            if let Some(ui) = ui_weak.upgrade() {
                let viewer = ui.global::<Viewer>();
                viewer.set_img(Image::default()); // drop the decoded pixels
                ui.global::<Ui>().set_viewing(false);
            }
        });
    }

    // Previous / next image in the folder.
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        callbacks.on_prev_image(move || {
            log::info!("cb: prev-image");
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut s = state.borrow_mut();
            if s.viewing && s.img_idx > 0 {
                s.img_idx -= 1;
                show_image(&fs, &ui, &s);
            }
        });
    }
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        callbacks.on_next_image(move || {
            log::info!("cb: next-image");
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut s = state.borrow_mut();
            if s.viewing && s.img_idx + 1 < s.images.len() {
                s.img_idx += 1;
                show_image(&fs, &ui, &s);
            }
        });
    }

    ui.run().expect("UI running");
}

/// Read + decode the current image, scale it to fit the screen width, and
/// push it into the Viewer global. On failure shows the error banner and
/// returns false (the browser stays usable, as in prime-pdf-viewer).
fn show_image(
    fs: &fs::FileSystem<fs_permissions::FileSystemPermissions>,
    ui: &AppWindow,
    st: &State,
) -> bool {
    let Some(name) = st.images.get(st.img_idx) else {
        return false;
    };
    let full = join_path(&st.path, name);

    let bytes = match read_bytes(fs, &full, st.location) {
        Ok(b) => b,
        Err(msg) => {
            show_error(ui, msg);
            return false;
        }
    };

    // Decoders are pure Rust and shouldn't panic, but a panic here would take
    // the whole app down — contain it (same policy as prime-pdf-viewer).
    let decoded = std::panic::catch_unwind(|| {
        image::load_from_memory(&bytes).map(|img| {
            let img = if img.width() > IMG_WIDTH || img.height() > MAX_IMG_HEIGHT {
                img.resize(IMG_WIDTH, MAX_IMG_HEIGHT, image::imageops::FilterType::Triangle)
            } else {
                img
            };
            img.into_rgba8()
        })
    });
    let rgba = match decoded {
        Ok(Ok(rgba)) => rgba,
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

    let (w, h) = (rgba.width(), rgba.height());
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    buf.make_mut_bytes().copy_from_slice(rgba.as_raw());

    // Displayed height at the fit-to-width scale (images narrower than the
    // screen are stretched up to it, matching the PDF viewer's behavior).
    let disp_h = (h as f32) * (IMG_WIDTH as f32) / (w as f32);

    let viewer = ui.global::<Viewer>();
    viewer.set_img(Image::from_rgba8(buf));
    viewer.set_img_h(disp_h);
    viewer.set_img_num(st.img_idx as i32 + 1);
    viewer.set_img_count(st.images.len() as i32);
    viewer.set_doc_name(name.as_str().into());
    log::info!(
        "rendered image {}/{} {}x{}",
        st.img_idx + 1,
        st.images.len(),
        w,
        h
    );
    true
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
