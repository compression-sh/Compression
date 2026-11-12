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

/// What the system clipboard offers, found out without taking any of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offer {
    pub files: bool,
    pub image: bool,
}

impl Offer {
    /// Whether a paste would find something: files, or a picture to save.
    pub fn anything(self) -> bool {
        self.files || self.image
    }

    /// What a list of offered types (MIME types) stands for. A vector picture is not a picture to save.
    fn of_types<'a>(types: impl IntoIterator<Item = &'a str>) -> Self {
        let mut offer = Self::default();
        for t in types {
            offer.files |= t == URI_LIST || t == GNOME_FILES;
            offer.image |= t.starts_with("image/") && !t.contains("svg");
        }
        offer
    }
}

/// What the system clipboard offers, or `None` when that could not be found out (then nothing may be
/// said about it). It asks the compositor or the selection owner, so it belongs on a thread of its own.
pub fn offer() -> Option<Offer> {
    // What this process holds is text, never files or a picture
    if own_text().is_some() {
        return Some(Offer::default());
    }
    if on_wayland() {
        use wl_clipboard_rs::paste::{ClipboardType, Error, Seat, get_mime_types};
        match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            Ok(types) => Some(Offer::of_types(types.iter().map(String::as_str))),
            Err(Error::ClipboardEmpty | Error::NoMimeType) => Some(Offer::default()),
            Err(_) => None,
        }
    } else {
        // The X11 library has no way to ask for the types alone
        let mut board = arboard::Clipboard::new().ok()?;
        match board.get().file_list() {
            Ok(paths) if !paths.is_empty() => return Some(Offer { files: true, image: false }),
            Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
            Err(_) => return None,
        }
        match board.get_image() {
            Ok(_) => Some(Offer { files: false, image: true }),
            Err(arboard::Error::ContentNotAvailable) => Some(Offer::default()),
            Err(_) => None,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip() {
        let path = PathBuf::from("/home/me/My Photos/ünï #1 (a)%.png");
        let uri = percent_encode(&path);
        assert_eq!(uri, "file:///home/me/My%20Photos/%C3%BCn%C3%AF%20%231%20%28a%29%25.png");
        assert_eq!(percent_decode(&uri), Some(path));
    }

    /// A pipe, as a clipboard owner would hand one over: the reading end, and the writing end the
    /// test plays the owner with.
    fn pipe() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe2 fills two descriptors that the files then own
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        unsafe { (std::fs::File::from_raw_fd(fds[0]), std::fs::File::from_raw_fd(fds[1])) }
    }

    /// The text this process holds is one global, so the tests about it take turns.
    static OWN_TEXT_TESTS: Mutex<()> = Mutex::new(());

    #[test]
    fn text_held_by_this_process_is_read_back_without_asking_the_compositor() {
        let _turn = OWN_TEXT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        set_own_text(Some("/home/me/a file".to_owned()));
        // Even with no display at all there is nobody to ask, and the answer is still there
        assert_eq!(get_text().as_deref(), Some("/home/me/a file"));
        // It is text, never files or a picture
        assert_eq!(get_files(), None);
        assert_eq!(get_image_png(), None);
        // Another program took the clipboard
        set_own_text(None);
        assert_eq!(own_text(), None);
    }

    #[test]
    fn setting_files_forgets_the_text_this_process_held() {
        let _turn = OWN_TEXT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        // set_files would reach the real clipboard where there is a display
        if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some() {
            return;
        }
        set_own_text(Some("old".to_owned()));
        let _ = set_files(&[PathBuf::from("/tmp/a")], false);
        assert_eq!(own_text(), None);
        set_own_text(Some("old".to_owned()));
        let _ = set_text("new".to_owned());
        assert_eq!(own_text(), None);
    }

    #[test]
    fn a_clipboard_owner_that_never_writes_is_given_up_on() {
        use std::io::Write;
        let (mut read, mut write) = pipe();
        // Nothing is sent and the pipe stays open: the read ends at the timeout instead of waiting for good
        let started = Instant::now();
        assert_eq!(read_limited(&mut read, 1024, Duration::from_millis(150)), None);
        assert!(started.elapsed() >= Duration::from_millis(100) && started.elapsed() < Duration::from_secs(5));
        // An owner that sends some and then stalls does not get the half either
        write.write_all(b"half of it").unwrap();
        assert_eq!(read_limited(&mut read, 1024, Duration::from_millis(150)), None);
    }

    #[test]
    fn a_clipboard_offer_over_the_limit_is_refused() {
        use std::io::Write;
        let (mut read, write) = pipe();
        let owner = std::thread::spawn(move || {
            let mut write = write;
            // Keeps sending until the reader stops listening
            let chunk = [b'x'; 4096];
            while write.write_all(&chunk).is_ok() {}
        });
        assert_eq!(read_limited(&mut read, 64 * 1024, Duration::from_secs(10)), None);
        drop(read);
        owner.join().unwrap();
    }

    #[test]
    fn a_complete_clipboard_offer_is_read_whole() {
        use std::io::Write;
        let (mut read, mut write) = pipe();
        write.write_all(b"file:///tmp/a\r\nfile:///tmp/b\r\n").unwrap();
        drop(write);
        assert_eq!(read_limited(&mut read, 1024, Duration::from_secs(5)).as_deref(), Some(&b"file:///tmp/a\r\nfile:///tmp/b\r\n"[..]));
        // An exact fit is fine too, and an empty offer is an empty answer
        let (mut read, mut write) = pipe();
        write.write_all(&[1u8; 100]).unwrap();
        drop(write);
        assert_eq!(read_limited(&mut read, 100, Duration::from_secs(5)).map(|d| d.len()), Some(100));
        let (mut read, write) = pipe();
        drop(write);
        assert_eq!(read_limited(&mut read, 100, Duration::from_secs(5)), Some(Vec::new()));
    }

    #[test]
    fn decode_skips_other_schemes_and_host() {
        assert_eq!(percent_decode("https://example.org/a"), None);
        assert_eq!(percent_decode("file://localhost/tmp/a"), Some(PathBuf::from("/tmp/a")));
        // The path on another computer is not a path here
        assert_eq!(percent_decode("file://other-host/tmp/a"), None);
        assert_eq!(percent_decode("file://me@nas/tmp/a"), None);
        assert_eq!(percent_decode("file://LocalHost/tmp/a"), Some(PathBuf::from("/tmp/a")));
        assert_eq!(percent_decode("file:///tmp/a"), Some(PathBuf::from("/tmp/a")));
        // One slash is a local path too
        assert_eq!(percent_decode("file:/tmp/example"), Some(PathBuf::from("/tmp/example")));
        assert_eq!(percent_decode("file:/tmp/a%20b"), percent_decode("file:///tmp/a%20b"));
        assert_eq!(percent_decode("file:relative"), None);
        assert_eq!(percent_decode("file://other-host/tmp/example"), None);
    }

    #[test]
    fn the_types_on_offer_say_whether_files_or_a_picture_can_be_pasted() {
        let of = |types: &[&str]| Offer::of_types(types.iter().copied());
        assert_eq!(of(&["text/uri-list", "text/plain"]), Offer { files: true, image: false });
        assert_eq!(of(&["x-special/gnome-copied-files"]), Offer { files: true, image: false });
        assert_eq!(of(&["image/png"]), Offer { files: false, image: true });
        assert_eq!(of(&["image/jpeg", "text/html"]), Offer { files: false, image: true });
        assert_eq!(of(&["image/svg+xml"]), Offer::default(), "a vector picture is not saved as a picture");
        assert!(of(&["image/svg+xml", "image/png"]).image);
        assert_eq!(of(&["text/plain", "text/plain;charset=utf-8", "UTF8_STRING"]), Offer::default());
        assert_eq!(of(&[]), Offer::default());
        assert!(!Offer::default().anything());
        assert!(Offer { files: true, image: false }.anything() && Offer { files: false, image: true }.anything());
    }

    #[test]
    fn text_held_by_this_process_offers_neither_files_nor_a_picture() {
        let _turn = OWN_TEXT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        set_own_text(Some("/home/me/a file".to_owned()));
        assert_eq!(offer(), Some(Offer::default()), "answered without asking anybody");
        set_own_text(None);
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Replaces whatever is on the real system clipboard, so it never runs by itself. Run it on
    /// purpose, in a Wayland session, with `cargo test live -- --ignored`.
    #[test]
    #[ignore = "overwrites the system clipboard; run with `cargo test live -- --ignored`"]
    fn files_round_trip_through_the_system_clipboard() {
        if !on_wayland() {
            return;
        }
        let paths = vec![PathBuf::from("/tmp/compression clip test.png")];
        if !set_files(&paths, true) {
            return;
        }
        assert_eq!(get_files(), Some((paths, true)));
    }
}
