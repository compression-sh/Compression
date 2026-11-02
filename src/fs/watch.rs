//! Watches the folders on show with inotify, so what changes on disk (by another program, a
//! download, a drive's own writes) reaches the listing without a manual refresh.
//!
//! One thread reads the kernel's events and passes them on; the app collects them for a moment
//! (see `app::watching`) because a single save or copy is a burst of them. A folder inotify cannot
//! watch (no room left for watches, an odd filesystem) is checked by its modification time instead.
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime};

use eframe::egui;

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Something in a watched folder was made, changed, moved or removed. `settled` means it is
    /// written out (closed, moved in, its attributes set), not still being written to.
    Path { path: PathBuf, settled: bool },
    /// The kernel dropped events: read the folder again.
    Overflow,
    /// A watched folder itself was removed, moved away or unmounted.
    Gone(PathBuf),
}

#[derive(Default)]
struct State {
    /// The folders each watch descriptor stands for. Several paths that lead to one folder (a link,
    /// a bind mount) share a descriptor, and each of them is told about what happens in it.
    watches: HashMap<i32, Vec<PathBuf>>,
    /// Folders watched by looking at their modification time, with the last one seen.
    polled: HashMap<PathBuf, Option<SystemTime>>,
}

pub struct Watcher {
    fd: i32,
    state: Arc<Mutex<State>>,
    rx: mpsc::Receiver<Event>,
    stop: Arc<AtomicBool>,
    dirs: Vec<PathBuf>,
}

const WATCHED: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_MODIFY
    | libc::IN_CLOSE_WRITE
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR;
const SETTLED: u32 = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_ATTRIB;
/// How often a folder that cannot be watched is looked at.
const POLL_EVERY: Duration = Duration::from_secs(2);

impl Watcher {
    pub fn new(ctx: &egui::Context) -> Option<Self> {
        // SAFETY: plain system call
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        // SAFETY: a new descriptor that nothing else owns
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Self::start(fd, ctx, |work| std::thread::Builder::new().name("compression-watch".into()).spawn(work).map(|_| ()))
    }

    /// Hands the reading of `fd` to a thread made by `spawn`. The thread owns the descriptor, so it is
    /// closed when the thread ends, and also when the thread could not be started.
    fn start(fd: OwnedFd, ctx: &egui::Context, spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>) -> Option<Self> {
        let raw = fd.as_raw_fd();
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (thread_state, thread_stop, ctx) = (state.clone(), stop.clone(), ctx.clone());
        spawn(Box::new(move || run(fd, thread_state, thread_stop, tx, ctx))).ok()?;
        Some(Self { fd: raw, state, rx, stop, dirs: Vec::new() })
    }

    /// Watch exactly these folders (nothing is done when they already are the ones watched).
    pub fn set(&mut self, dirs: &[PathBuf]) {
        if self.dirs == dirs {
            return;
        }
        self.dirs = dirs.to_vec();
        let Ok(mut state) = self.state.lock() else { return };
        for wd in state.watches.keys() {
            // SAFETY: `fd` is a live inotify descriptor; a stale `wd` only makes this fail.
            unsafe { libc::inotify_rm_watch(self.fd, *wd) };
        }
        state.watches.clear();
        state.polled.clear();
        for dir in dirs {
            let Ok(c_path) = CString::new(dir.as_os_str().as_bytes()) else { continue };
            // SAFETY: `c_path` is a NUL-terminated path that outlives the call.
            let wd = unsafe { libc::inotify_add_watch(self.fd, c_path.as_ptr(), WATCHED) };
            if wd >= 0 {
                // The same folder again, under another path, gets the same descriptor
                let paths = state.watches.entry(wd).or_default();
                if !paths.contains(dir) {
                    paths.push(dir.clone());
                }
            } else {
                state.polled.insert(dir.clone(), None);
            }
        }
    }

    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        self.rx.try_iter()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // The thread notices within a poll round and closes the descriptor
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run(owned: OwnedFd, state: Arc<Mutex<State>>, stop: Arc<AtomicBool>, tx: mpsc::Sender<Event>, ctx: egui::Context) {
    // `owned` closes the descriptor when this function ends, however it ends
    let fd = owned.as_raw_fd();
    let mut buf = vec![0u8; 64 * 1024];
    let mut polled_at = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: `pfd` is one valid pollfd.
        let ready = unsafe { libc::poll(&mut pfd, 1, 500) };
        let mut sent = false;
        if ready > 0 {
            // SAFETY: `buf` is writable for its whole length.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                sent |= decode(&buf[..n as usize], &state, &tx);
            }
        }
        if polled_at.elapsed() >= POLL_EVERY {
            polled_at = std::time::Instant::now();
            sent |= check_polled(&state, &tx);
        }
        if sent {
            ctx.request_repaint();
        }
    }
}

/// Turn a buffer of `inotify_event` records into `Event`s; whether any were sent.
fn decode(buf: &[u8], state: &Mutex<State>, tx: &mpsc::Sender<Event>) -> bool {
    const HEADER: usize = 16;
    let Ok(state) = state.lock() else { return false };
    let mut sent = false;
    let mut at = 0;
    while at + HEADER <= buf.len() {
        let field = |from: usize| u32::from_ne_bytes(buf[at + from..at + from + 4].try_into().unwrap_or([0; 4]));
        let (wd, mask, len) = (field(0) as i32, field(4), field(12) as usize);
        let name = buf.get(at + HEADER..at + HEADER + len).unwrap_or_default();
        at += HEADER + len;
        let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
        let mut events = Vec::new();
        if mask & libc::IN_Q_OVERFLOW != 0 {
            events.push(Event::Overflow);
        } else {
            // One event for every path the folder was registered under
            for dir in state.watches.get(&wd).into_iter().flatten() {
                if mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_UNMOUNT) != 0 {
                    events.push(Event::Gone(dir.clone()));
                } else if !name.is_empty() {
                    let path = dir.join(std::ffi::OsString::from_vec(name.to_vec()));
                    events.push(Event::Path { path, settled: mask & SETTLED != 0 });
                }
            }
        }
        for event in events {
            sent |= tx.send(event).is_ok();
        }
    }
    sent
}

fn check_polled(state: &Mutex<State>, tx: &mpsc::Sender<Event>) -> bool {
    let dirs: Vec<(PathBuf, Option<SystemTime>)> = match state.lock() {
        Ok(state) => state.polled.iter().map(|(d, t)| (d.clone(), *t)).collect(),
        Err(_) => return false,
    };
    let mut sent = false;
    for (dir, seen) in dirs {
        let now = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
        if now == seen {
            continue;
        }
        if let Ok(mut state) = state.lock()
            && let Some(slot) = state.polled.get_mut(&dir)
        {
            *slot = now;
        }
        // The first look only records the time
        if seen.is_some() || now.is_none() {
            sent |= tx.send(if now.is_none() { Event::Gone(dir) } else { Event::Overflow }).is_ok();
        }
    }
    sent
}

/// Whether `path` is one of the watched folders' own entries: its parent is `dir`.
pub fn is_inside(path: &Path, dirs: &[PathBuf]) -> bool {
    path.parent().is_some_and(|p| dirs.iter().any(|d| d == p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(watcher: &Watcher, want: impl Fn(&Event) -> bool) -> bool {
        for _ in 0..100 {
            if watcher.events().any(|e| want(&e)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn changes_in_a_watched_folder_arrive_and_a_removed_folder_is_reported() {
        let dir = std::env::temp_dir().join(format!("compression-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut watcher = Watcher::new(&egui::Context::default()).expect("inotify");
        watcher.set(std::slice::from_ref(&dir));

        let file = dir.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(wait_for(&watcher, |e| matches!(e, Event::Path { path, settled: true } if *path == file)), "a new file is reported once written");
        std::fs::remove_file(&file).unwrap();
        assert!(wait_for(&watcher, |e| matches!(e, Event::Path { path, .. } if *path == file)), "and again when it goes");

        std::fs::remove_dir_all(&dir).unwrap();
        assert!(wait_for(&watcher, |e| *e == Event::Gone(dir.clone())));
    }

    /// Every event that arrives within a while.
    fn collect(watcher: &Watcher, until: impl Fn(&[Event]) -> bool) -> Vec<Event> {
        let mut all = Vec::new();
        for _ in 0..100 {
            all.extend(watcher.events());
            if until(&all) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        all
    }

    #[test]
    fn a_folder_reached_by_two_paths_reports_to_both() {
        let dir = std::env::temp_dir().join(format!("compression-watch-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        let (real, alias) = (dir.join("real"), dir.join("alias"));
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let mut watcher = Watcher::new(&egui::Context::default()).expect("inotify");
        watcher.set(&[real.clone(), alias.clone()]);
        // One descriptor, two paths
        assert_eq!(watcher.state.lock().unwrap().watches.len(), 1);

        std::fs::write(real.join("a.txt"), "x").unwrap();
        let both = |events: &[Event]| [&real, &alias].iter().all(|d| events.iter().any(|e| matches!(e, Event::Path { path, settled: true } if *path == d.join("a.txt"))));
        let events = collect(&watcher, both);
        assert!(both(&events), "each path hears of the new file: {events:?}");

        // A folder that is gone is gone under both names
        std::fs::remove_dir_all(&real).unwrap();
        let gone = |events: &[Event]| events.contains(&Event::Gone(real.clone())) && events.contains(&Event::Gone(alias.clone()));
        let events = collect(&watcher, gone);
        assert!(gone(&events), "{events:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn registering_the_same_path_twice_does_not_report_twice() {
        let dir = std::env::temp_dir().join(format!("compression-watch-twice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut watcher = Watcher::new(&egui::Context::default()).expect("inotify");
        watcher.set(&[dir.clone(), dir.clone()]);
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let events = collect(&watcher, |e| e.iter().any(|e| matches!(e, Event::Path { settled: true, .. })));
        std::thread::sleep(Duration::from_millis(100));
        let more: Vec<_> = watcher.events().collect();
        let settled = events.iter().chain(&more).filter(|e| matches!(e, Event::Path { settled: true, .. })).count();
        assert_eq!(settled, 1, "{events:?} {more:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_descriptor_is_closed_when_the_thread_cannot_be_started() {
        // A pipe stands in for the inotify descriptor: its read end sees the end of the data
        // once the write end, which the watcher owns, is closed
        let mut fds = [0 as std::os::fd::RawFd; 2];
        // SAFETY: pipe2 fills two descriptors that are owned from here on
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) }, 0);
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let started = Watcher::start(write, &egui::Context::default(), |_work| Err(std::io::Error::other("no thread")));
        assert!(started.is_none());
        let mut byte = 0u8;
        // SAFETY: reads one byte into a variable of ours
        let n = unsafe { libc::read(read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
        assert_eq!(n, 0, "the write end was still open (a read would give EAGAIN)");
    }

    #[test]
    fn the_descriptor_is_closed_when_the_watcher_stops() {
        let mut fds = [0 as std::os::fd::RawFd; 2];
        // SAFETY: as above
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) }, 0);
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let watcher = Watcher::start(write, &egui::Context::default(), |work| std::thread::Builder::new().spawn(work).map(|_| ())).expect("started");
        drop(watcher);
        // The thread notices the stop within a poll round (half a second)
        let started = std::time::Instant::now();
        let mut byte = 0u8;
        loop {
            let n = unsafe { libc::read(read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
            if n == 0 {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "the descriptor stayed open");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_folder_that_cannot_be_watched_is_polled() {
        let mut watcher = Watcher::new(&egui::Context::default()).expect("inotify");
        let missing = std::env::temp_dir().join(format!("compression-watch-missing-{}", std::process::id()));
        watcher.set(std::slice::from_ref(&missing));
        assert!(watcher.state.lock().unwrap().polled.contains_key(&missing));
    }
}
