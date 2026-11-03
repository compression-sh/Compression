//! Copied files on the system clipboard, so other applications paste the files themselves and not
//! just their names. They are offered the way Nautilus does: `text/uri-list` plus
//! `x-special/gnome-copied-files` (which also says copy or cut), and as plain text for editors.
//! Wayland goes through the data-control protocol, X11 through arboard. Where neither works (a
//! compositor without data-control) the caller falls back to pasting the paths as text.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The most that is read from another program's clipboard offer; more than this is refused.
const READ_LIMIT: usize = 16 << 20;
/// How long a program may take to send its clipboard offer. The window waits meanwhile.
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// What another program sends through `reader`, a pipe from the clipboard owner. The owner decides
/// when it writes and when it closes the pipe, and may never do either, so waiting is bounded:
/// `None` when it takes longer than `timeout` or sends more than `limit` bytes.
fn read_limited<R: Read + AsRawFd>(reader: &mut R, limit: usize, timeout: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd { fd: reader.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd
        let ready = unsafe { libc::poll(&mut pfd, 1, left) };
        if ready < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if ready <= 0 {
            return None;
        }
        // Data is waiting, or the owner closed the pipe: this read does not block
        match reader.read(&mut buf) {
            Ok(0) => return Some(data),
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                if data.len() > limit {
                    return None;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// Text that this process holds on the clipboard through its own data device (`wayland_dnd`).
/// Reading the clipboard through the compositor asks its owner to write the text into a pipe;
/// when the owner is this process, the thread that would answer may be the one waiting, so the
/// text is taken from here instead.
static OWN_TEXT: Mutex<Option<String>> = Mutex::new(None);

/// Called by `wayland_dnd` when its own selection is set (`Some`) or taken by another program (`None`).
pub(crate) fn set_own_text(text: Option<String>) {
    if let Ok(mut own) = OWN_TEXT.lock() {
        *own = text;
    }
}

fn own_text() -> Option<String> {
    OWN_TEXT.lock().ok()?.clone()
}

const URI_LIST: &str = "text/uri-list";
const GNOME_FILES: &str = "x-special/gnome-copied-files";

pub(crate) fn percent_encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether the host of a `file://host/path` URI is this computer: none, or `localhost`. A path on
/// another computer is not a path here, and must not be opened as one.
pub(crate) fn is_local_host(host: &str) -> bool {
    host.is_empty() || host.eq_ignore_ascii_case("localhost")
}

pub(crate) fn percent_decode(text: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = if let Some(rest) = text.strip_prefix("file://") {
        // `file://host/path` carries a host before the first slash
        let slash = rest.find('/')?;
        if !is_local_host(&rest[..slash]) {
            return None;
        }
        &rest[slash..]
    } else {
        // `file:/path` has no host, and means the same as `file:///path`
        text.strip_prefix("file:").filter(|p| p.starts_with('/'))?
    };
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() && let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

fn on_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Puts `paths` on the system clipboard. `false` when the platform offers no way to do so.
pub fn set_files(paths: &[PathBuf], cut: bool) -> bool {
    if paths.is_empty() {
        return false;
    }
    set_own_text(None);
    if on_wayland() { set_wayland(paths, cut) } else { set_x11(paths) }
}

fn set_wayland(paths: &[PathBuf], cut: bool) -> bool {
    use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};
    let uris: Vec<String> = paths.iter().map(|p| percent_encode(p)).collect();
    let list = uris.iter().map(|u| format!("{u}\r\n")).collect::<String>();
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
    let text = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
    let source = |data: String, mime: MimeType| MimeSource { source: Source::Bytes(data.into_bytes().into_boxed_slice()), mime_type: mime };
    let sources = vec![
        source(list, MimeType::Specific(URI_LIST.to_owned())),
        source(gnome, MimeType::Specific(GNOME_FILES.to_owned())),
        source(text, MimeType::Text),
    ];
    Options::new().copy_multi(sources).is_ok()
}

fn set_x11(paths: &[PathBuf]) -> bool {
    // The selection is served by this owner, so it has to outlive the call
    static OWNER: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);
    let Ok(mut owner) = OWNER.lock() else { return false };
    if owner.is_none() {
        *owner = arboard::Clipboard::new().ok();
    }
    owner.as_mut().is_some_and(|c| c.set().file_list(paths).is_ok())
}

/// The files on the system clipboard, and whether they were cut, if any application put them there.
pub fn get_files() -> Option<(Vec<PathBuf>, bool)> {
    // What this process holds is text, never files
    if own_text().is_some() {
        return None;
    }
    if on_wayland() { get_wayland() } else { get_x11() }
}

/// Puts plain text on the system clipboard, for windows that have no data device of their own
/// (X11, or a Wayland compositor without one for this window).
pub fn set_text(text: String) -> bool {
    set_own_text(None);
    if on_wayland() {
        use wl_clipboard_rs::copy::{MimeType, Options, Source};
        return Options::new().copy(Source::Bytes(text.into_bytes().into_boxed_slice()), MimeType::Text).is_ok();
    }
    // The selection is served by this owner, so it has to outlive the call
    static OWNER: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);
    let Ok(mut owner) = OWNER.lock() else { return false };
    if owner.is_none() {
        *owner = arboard::Clipboard::new().ok();
    }
    owner.as_mut().is_some_and(|c| c.set_text(text).is_ok())
}

/// Plain text on the system clipboard, if any.
pub fn get_text() -> Option<String> {
    if let Some(text) = own_text() {
        return Some(text);
    }
    if on_wayland() {
        use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};
        let (mut pipe, _) = get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text).ok()?;
        String::from_utf8(read_limited(&mut pipe, READ_LIMIT, READ_TIMEOUT)?).ok()
    } else {
        arboard::Clipboard::new().ok()?.get_text().ok()
    }
}

/// An image on the system clipboard (a screenshot, "copy image" in a browser), as PNG bytes.
pub fn get_image_png() -> Option<Vec<u8>> {
    if own_text().is_some() {
        return None;
    }
    if on_wayland() {
        use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents, get_mime_types};
        let offered = get_mime_types(ClipboardType::Regular, Seat::Unspecified).ok()?;
        let mime = if offered.contains("image/png") {
            "image/png"
        } else {
            offered.iter().find(|m| m.starts_with("image/") && !m.contains("svg")).map(String::as_str)?
        };
        let (mut pipe, _) = get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Specific(mime)).ok()?;
        let bytes = read_limited(&mut pipe, READ_LIMIT, READ_TIMEOUT)?;
        if mime == "image/png" {
            return (!bytes.is_empty()).then_some(bytes);
        }
        let decoded = image::load_from_memory(&bytes).ok()?;
        let mut png = std::io::Cursor::new(Vec::new());
        decoded.write_to(&mut png, image::ImageFormat::Png).ok()?;
        Some(png.into_inner())
    } else {
        let img = arboard::Clipboard::new().ok()?.get_image().ok()?;
        let buf = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())?;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(buf).write_to(&mut png, image::ImageFormat::Png).ok()?;
        Some(png.into_inner())
    }
}

fn get_wayland() -> Option<(Vec<PathBuf>, bool)> {
    use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};
    let read = |mime: &str| -> Option<String> {
        let (mut pipe, _) = get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Specific(mime)).ok()?;
        String::from_utf8(read_limited(&mut pipe, READ_LIMIT, READ_TIMEOUT)?).ok()
    };
    if let Some(text) = read(GNOME_FILES) {
        let mut lines = text.lines();
        let cut = lines.next().is_some_and(|l| l.trim() == "cut");
        let paths: Vec<PathBuf> = lines.filter_map(|l| percent_decode(l.trim())).collect();
        if !paths.is_empty() {
            return Some((paths, cut));
        }
    }
    let paths: Vec<PathBuf> = read(URI_LIST)?.lines().filter_map(|l| percent_decode(l.trim())).collect();
    (!paths.is_empty()).then_some((paths, false))
}

fn get_x11() -> Option<(Vec<PathBuf>, bool)> {
    let paths = arboard::Clipboard::new().ok()?.get().file_list().ok()?;
    (!paths.is_empty()).then_some((paths, false))
}

