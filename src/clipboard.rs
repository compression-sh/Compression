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
    if on_wayland() { set_wayland(paths, cut) } else { set_x11(paths, cut) }
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

/// Files this process put on the X11 clipboard, and the window that held the clipboard right after. arboard offers
/// them as `text/uri-list` only, which cannot say they were cut, and gives back only the names it can read as UTF-8,
/// as their link targets. So while that window still holds the clipboard, a paste takes these, as they were given.
#[derive(Clone, Debug, PartialEq)]
struct X11Offer {
    owner: u64,
    paths: Vec<PathBuf>,
    cut: bool,
}

static X11_OFFER: Mutex<Option<X11Offer>> = Mutex::new(None);

fn set_x11_offer(offer: Option<X11Offer>) {
    if let Ok(mut held) = X11_OFFER.lock() {
        *held = offer;
    }
}

fn set_x11(paths: &[PathBuf], cut: bool) -> bool {
    set_x11_offer(None);
    // The selection is served by this owner, so it has to outlive the call
    static OWNER: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);
    let Ok(mut owner) = OWNER.lock() else { return false };
    if owner.is_none() {
        *owner = arboard::Clipboard::new().ok();
    }
    if !owner.as_mut().is_some_and(|c| c.set().file_list(paths).is_ok()) {
        return false;
    }
    set_x11_offer(x11_clipboard_owner().map(|owner| X11Offer { owner, paths: paths.to_vec(), cut }));
    true
}

/// The window that holds the X11 clipboard now; `None` when there is none, or it cannot be asked.
fn x11_clipboard_owner() -> Option<u64> {
    let xlib = x11_dl::xlib::Xlib::open().ok()?;
    // SAFETY: a connection of its own, opened, asked and closed here; the atom name is NUL-terminated
    unsafe {
        let display = (xlib.XOpenDisplay)(std::ptr::null());
        if display.is_null() {
            return None;
        }
        let selection = (xlib.XInternAtom)(display, c"CLIPBOARD".as_ptr(), 0);
        let owner = (xlib.XGetSelectionOwner)(display, selection);
        (xlib.XCloseDisplay)(display);
        (owner != 0).then_some(owner as u64)
    }
}

/// The files on the system clipboard, and whether they were cut, if any application put them there.
pub fn get_files() -> Option<(Vec<PathBuf>, bool)> {
    // What this process holds is text, never files
    if own_text().is_some() {
        return None;
    }
    if on_wayland() { get_wayland() } else { get_x11() }
}

/// What the system clipboard held when a paste asked: files, or (when nothing else was there and a picture was
/// wanted) a picture as PNG bytes.
#[derive(Debug, Default, PartialEq)]
pub struct Snapshot {
    pub files: Option<(Vec<PathBuf>, bool)>,
    pub image: Option<Vec<u8>>,
}

/// Reads what a paste needs from the system clipboard. It waits for the program that holds the clipboard, as long as
/// that takes (on Wayland each read is limited to a few seconds, on X11 it is not), so it belongs on a thread of its
/// own and its caller gives up on it by the clock (see `CompressionApp::paste`).
pub fn snapshot(want_image: bool) -> Snapshot {
    // Files of this computer that are gone are no files to paste; ones on a server are not looked at here
    let files = get_files().filter(|(paths, _)| paths.iter().all(|p| crate::fs::network::is_remote(p) || p.exists()));
    let image = if files.is_none() && want_image { get_image_png() } else { None };
    Snapshot { files, image }
}

/// What comes through `pipe` (the reading end of what a clipboard owner was asked to send), within the limits of a
/// read from the clipboard: at most 16 MiB and a few seconds. `None` when nothing whole came.
pub fn read_pipe(pipe: std::os::fd::OwnedFd) -> Option<Vec<u8>> {
    read_limited(&mut std::fs::File::from(pipe), READ_LIMIT, READ_TIMEOUT)
}

/// Puts plain text on the system clipboard, for windows that have no data device of their own
/// (X11, or a Wayland compositor without one for this window).
pub fn set_text(text: String) -> bool {
    set_own_text(None);
    if on_wayland() {
        use wl_clipboard_rs::copy::{MimeType, Options, Source};
        return Options::new().copy(Source::Bytes(text.into_bytes().into_boxed_slice()), MimeType::Text).is_ok();
    }
    // Text takes the clipboard from the files this process put there, through the same window
    set_x11_offer(None);
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
    let ours = X11_OFFER.lock().ok().and_then(|held| held.clone());
    x11_files(ours, x11_clipboard_owner(), || arboard::Clipboard::new().ok()?.get().file_list().ok())
}

/// The files on the X11 clipboard: this process's own offer as it was given while the window that took the clipboard
/// for it (`ours.owner`) still holds it, else what `read` gets. X11 does not say whether another program's files
/// were cut, so they are copied.
fn x11_files(ours: Option<X11Offer>, owner: Option<u64>, read: impl FnOnce() -> Option<Vec<PathBuf>>) -> Option<(Vec<PathBuf>, bool)> {
    if let Some(ours) = ours
        && owner == Some(ours.owner)
    {
        return Some((ours.paths, ours.cut));
    }
    let paths = read()?;
    (!paths.is_empty()).then_some((paths, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_this_process_put_on_the_x11_clipboard_come_back_as_given_and_cut_while_it_holds_it() {
        use std::os::unix::ffi::OsStringExt;
        let odd = PathBuf::from(std::ffi::OsString::from_vec(b"/home/me/caf\xe9.txt".to_vec()));
        let link = PathBuf::from("/home/me/link to photos");
        let ours = X11Offer { owner: 0x40_0001, paths: vec![odd.clone(), link.clone()], cut: true };
        // What arboard reads back: names it can read as UTF-8 only, links as their targets
        let read = || Some(vec![PathBuf::from("/home/me/Photos")]);
        // Still held by the window that took it for this process: as given, and cut
        assert_eq!(x11_files(Some(ours.clone()), Some(0x40_0001), || panic!("asked the clipboard")), Some((vec![odd, link], true)));
        // Another program took it: what it offers, copied, since X11 does not say whether it was cut
        assert_eq!(x11_files(Some(ours.clone()), Some(0x60_0003), read), Some((vec![PathBuf::from("/home/me/Photos")], false)));
        // Nobody holds it, or it cannot be asked: not taken for this process's own
        assert_eq!(x11_files(Some(ours.clone()), None, read), Some((vec![PathBuf::from("/home/me/Photos")], false)));
        assert_eq!(x11_files(None, Some(0x40_0001), read), Some((vec![PathBuf::from("/home/me/Photos")], false)));
        assert_eq!(x11_files(Some(ours), Some(0x60_0003), || Some(Vec::new())), None);
    }

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
    fn the_text_a_holder_writes_to_the_pipe_is_read_on_a_thread_and_a_holder_that_does_not_is_given_up_on() {
        use std::io::Write;
        use std::os::fd::OwnedFd;
        // A holder that writes its text and closes the pipe
        let (read, mut write) = pipe();
        let reader = std::thread::spawn(move || read_pipe(OwnedFd::from(read)));
        write.write_all("Grüße\r\n".as_bytes()).unwrap();
        drop(write);
        assert_eq!(reader.join().unwrap().as_deref(), Some("Grüße\r\n".as_bytes()));
        // One that keeps the pipe and never writes: the reader ends by the clock (a few seconds), and the caller is
        // free all along, since it is a thread that waits
        let (read, write) = pipe();
        let started = std::time::Instant::now();
        let reader = std::thread::spawn(move || read_pipe(OwnedFd::from(read)));
        assert!(!reader.is_finished(), "the reader waits for the holder");
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(reader.join().unwrap(), None);
        assert!(started.elapsed() >= READ_TIMEOUT - Duration::from_millis(200) && started.elapsed() < READ_TIMEOUT + Duration::from_secs(2), "{:?}", started.elapsed());
        drop(write);
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
