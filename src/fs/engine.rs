//! Background file operations. A job runs on its own thread and reports through shared counters
//! (for the progress popover), can be paused or cancelled, and asks the window what to do when a
//! name is taken. Finished jobs hand back the entry that undoes them.
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;

use super::history::{Edit, UndoEntry};
use super::ops::{self, unique_path};
use super::trash;
use crate::i18n::{tr, trf, trn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Copy,
    Move,
    Extract,
    Compress,
    Delete,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Copy,
    Move,
}

/// Counters a job updates while it works; the window reads them every frame.
#[derive(Default)]
pub struct Shared {
    pub bytes_done: AtomicU64,
    pub bytes_total: AtomicU64,
    pub files_done: AtomicU64,
    pub files_total: AtomicU64,
    pub cancel: AtomicBool,
    pub paused: AtomicBool,
    /// Stopped on a question the window has to answer.
    pub waiting: AtomicBool,
    pub current: Mutex<String>,
}

impl Shared {
    /// Called between chunks of work: waits while paused, and fails once cancelled.
    pub fn checkpoint(&self) -> io::Result<()> {
        loop {
            if self.cancel.load(Relaxed) {
                return Err(cancelled());
            }
            if !self.paused.load(Relaxed) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    pub fn set_current(&self, name: &str) {
        if let Ok(mut current) = self.current.lock() {
            *current = name.to_owned();
        }
    }

    pub fn current(&self) -> String {
        self.current.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// 0..1 when the size of the work is known.
    pub fn fraction(&self) -> Option<f32> {
        let (bytes_total, files_total) = (self.bytes_total.load(Relaxed), self.files_total.load(Relaxed));
        if bytes_total > 0 {
            Some((self.bytes_done.load(Relaxed) as f64 / bytes_total as f64).clamp(0.0, 1.0) as f32)
        } else if files_total > 0 {
            Some((self.files_done.load(Relaxed) as f64 / files_total as f64).clamp(0.0, 1.0) as f32)
        } else {
            None
        }
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

pub fn is_cancel(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Replace,
    Skip,
    KeepBoth,
    /// Put the contents of a folder into the folder that already exists.
    Merge,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct EntryInfo {
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

impl EntryInfo {
    fn of(path: &Path) -> Self {
        let meta = path.symlink_metadata().ok();
        Self {
            is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
            size: meta.as_ref().map_or(0, |m| m.len()),
            modified: meta.and_then(|m| m.modified().ok()),
        }
    }
}

/// A name is taken. The job waits until the window sends an answer.
pub struct ConflictRequest {
    pub job: u64,
    pub target: PathBuf,
    pub source_info: EntryInfo,
    pub target_info: EntryInfo,
    reply: mpsc::Sender<(Decision, bool)>,
}

impl ConflictRequest {
    /// `all` applies the answer to every further conflict of the same job.
    pub fn answer(self, decision: Decision, all: bool) {
        let _ = self.reply.send((decision, all));
    }

    /// Both are folders, so their contents can be merged.
    pub fn can_merge(&self) -> bool {
        self.source_info.is_dir && self.target_info.is_dir
    }
}

pub enum Effect {
    None,
    /// A new change that can be undone.
    Record(UndoEntry),
    /// An undo finished: the entry that redoes it.
    Undone(UndoEntry),
    /// A redo finished: the entry that undoes it again.
    Redone(UndoEntry),
}

pub struct Outcome {
    pub ok: bool,
    pub cancelled: bool,
    pub message: String,
    pub effect: Effect,
}

impl Outcome {
    pub fn done(message: String) -> Self {
        Self { ok: true, cancelled: false, message, effect: Effect::None }
    }

    pub fn failed(message: String) -> Self {
        Self { ok: false, cancelled: false, message, effect: Effect::None }
    }

    /// The job was cancelled and took back what it had begun.
    pub fn stopped() -> Self {
        Self { ok: false, cancelled: true, message: tr("Cancelled").to_owned(), effect: Effect::None }
    }

    pub fn with(mut self, effect: Effect) -> Self {
        self.effect = effect;
        self
    }
}

/// The target has less room than the job needs. The job waits until the window sends an answer.
pub struct SpaceRequest {
    pub job: u64,
    pub folder: PathBuf,
    pub needed: u64,
    pub free: u64,
    reply: mpsc::Sender<bool>,
}

impl SpaceRequest {
    /// `go_on` is the choice to carry on anyway; `false` cancels the job.
    pub fn answer(self, go_on: bool) {
        let _ = self.reply.send(go_on);
    }

    pub fn missing(&self) -> u64 {
        self.needed.saturating_sub(self.free)
    }
}

/// How much is missing when `needed` bytes are to go where `free` bytes are left; `None` if it fits.
pub fn shortfall(needed: u64, free: u64) -> Option<u64> {
    needed.checked_sub(free).filter(|missing| *missing > 0)
}

/// The room an ordinary user has left where `dir` is. `None` when that cannot be told reliably: the
/// folder is on a server or `statvfs` has no sizes for it, and then nothing is warned about.
pub fn free_space(dir: &Path) -> Option<u64> {
    if super::network::is_remote(dir) {
        return None;
    }
    super::drives::Space::of(dir).map(|space| space.free)
}

pub enum Event {
    Conflict(ConflictRequest),
    LowSpace(SpaceRequest),
    Done { job: u64, outcome: Outcome },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Queued behind another job that writes to the same disk.
    Waiting,
    Running,
    Done,
    Failed,
    Cancelled,
}

pub struct Job {
    pub id: u64,
    pub kind: JobKind,
    pub title: String,
    pub detail: String,
    pub shared: Arc<Shared>,
    pub state: JobState,
    /// Jobs with the same key run one at a time (the disk they write to).
    pub queue_key: Option<u64>,
    pub message: String,
    pub finished: Option<Instant>,
    /// Bytes per second, smoothed.
    pub speed: f64,
    sample: (Instant, u64),
}

impl Job {
    /// Pausing and cancelling make sense where the work moves through files one at a time.
    pub fn controllable(&self) -> bool {
        matches!(self.kind, JobKind::Copy | JobKind::Move | JobKind::Delete | JobKind::Extract | JobKind::Compress)
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Relaxed)
    }

    pub fn eta(&self) -> Option<Duration> {
        let total = self.shared.bytes_total.load(Relaxed);
        let left = total.saturating_sub(self.shared.bytes_done.load(Relaxed));
        (self.speed > 1.0 && total > 0 && !self.paused()).then(|| Duration::from_secs_f64(left as f64 / self.speed))
    }
}

/// Everything a running job needs from the outside.
pub struct Worker {
    pub shared: Arc<Shared>,
    events: mpsc::Sender<Event>,
    ctx: egui::Context,
    job: u64,
    /// The answer given with "apply to all".
    remembered: Option<Decision>,
}

impl Worker {
    fn decide(&mut self, source: &Path, target: &Path) -> io::Result<Decision> {
        let (source_info, target_info) = (EntryInfo::of(source), EntryInfo::of(target));
        let both_dirs = source_info.is_dir && target_info.is_dir;
        if let Some(decision) = self.remembered {
            return Ok(match decision {
                Decision::Merge if !both_dirs => Decision::Replace,
                other => other,
            });
        }
        let (reply, answer) = mpsc::channel();
        let request = ConflictRequest { job: self.job, target: target.to_path_buf(), source_info, target_info, reply };
        self.events.send(Event::Conflict(request)).map_err(|_| cancelled())?;
        let result = self.wait_for(answer);
        match result {
            Some((Decision::Cancel, _)) | None => Err(cancelled()),
            Some((decision, all)) => {
                if all {
                    self.remembered = Some(decision);
                }
                Ok(decision)
            }
        }
    }

    /// Waits for the window to answer a question; `None` once the job is cancelled or the window dropped it.
    fn wait_for<T>(&self, answer: mpsc::Receiver<T>) -> Option<T> {
        self.ctx.request_repaint();
        self.shared.waiting.store(true, Relaxed);
        let result = loop {
            match answer.recv_timeout(Duration::from_millis(100)) {
                Ok(reply) => break Some(reply),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.shared.cancel.load(Relaxed) {
                        break None;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            }
        };
        self.shared.waiting.store(false, Relaxed);
        result
    }

    /// Before a job writes `needed` bytes into `dest`: if `free(dest)` says they do not fit, ask the
    /// window whether to go on. Cancelled by the answer or by the job being cancelled; a job that fits, or
    /// where the room is unknown, is not asked.
    pub fn ensure_space(&self, dest: &Path, needed: u64, free: impl Fn(&Path) -> Option<u64>) -> io::Result<()> {
        let Some(free) = free(dest) else { return Ok(()) };
        if shortfall(needed, free).is_none() {
            return Ok(());
        }
        let (reply, answer) = mpsc::channel();
        let request = SpaceRequest { job: self.job, folder: dest.to_path_buf(), needed, free, reply };
        self.events.send(Event::LowSpace(request)).map_err(|_| cancelled())?;
        match self.wait_for(answer) {
            Some(true) => Ok(()),
            _ => Err(cancelled()),
        }
    }

    /// `ensure_space` for unpacking `(archive, folder)` pairs: the need of a folder is what its archives hold
    /// unpacked. A folder whose archives cannot all be listed (or are listed only in part) is not warned about.
    pub fn ensure_space_for_archives(&self, pairs: &[(PathBuf, PathBuf)], free: impl Fn(&Path) -> Option<u64> + Copy) -> io::Result<()> {
        let mut folders: Vec<(&Path, Option<u64>)> = Vec::new();
        for (archive, dest) in pairs {
            self.shared.checkpoint()?;
            let size = match super::archive_list::list(archive, &self.shared.cancel) {
                Ok(listing) if listing.cut.is_none() => Some(listing.unpacked),
                _ => None,
            };
            match folders.iter_mut().find(|(d, _)| *d == dest.as_path()) {
                Some((_, total)) => *total = total.zip(size).map(|(a, b)| a.saturating_add(b)),
                None => folders.push((dest, size)),
            }
        }
        for (dest, needed) in folders {
            if let Some(needed) = needed {
                self.ensure_space(dest, needed, free)?;
            }
        }
        Ok(())
    }

    /// Runs archive work (`ops::compress`, `ops::extract`) under this job's control: it stops when
    /// the job is cancelled or after `ops::ARCHIVE_TIME_LIMIT`, and reports into `bytes_done`.
    /// `base` is what the job's earlier archives already added; `size` is what this one adds.
    pub fn archive_control<R>(&self, base: u64, size: u64, work: impl FnOnce(&ops::Control) -> R) -> R {
        let shared = &self.shared;
        let cancelled = || shared.cancel.load(Relaxed);
        let progress = |done: u64| shared.bytes_done.store(base + done.min(size), Relaxed);
        work(&ops::Control::new(&cancelled, &progress, ops::ARCHIVE_TIME_LIMIT))
    }

    pub fn add_bytes(&self, n: u64) {
        self.shared.bytes_done.fetch_add(n, Relaxed);
    }

    pub fn file_done(&self) {
        self.shared.files_done.fetch_add(1, Relaxed);
    }
}

type Work = Box<dyn FnOnce(&mut Worker) -> Outcome + Send>;

pub struct Engine {
    ctx: egui::Context,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    next: u64,
    pub jobs: Vec<Job>,
    /// The work of the jobs in the `Waiting` state, in the order they will start.
    waiting: Vec<(u64, Work)>,
}

impl Engine {
    pub fn new(ctx: &egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        Self { ctx: ctx.clone(), tx, rx, next: 1, jobs: Vec::new(), waiting: Vec::new() }
    }

    /// Something is running, or waits for its turn.
    pub fn running(&self) -> bool {
        self.jobs.iter().any(|j| matches!(j.state, JobState::Running | JobState::Waiting))
    }

    /// Start `work` on a thread of its own. `title` names it in the popover, `detail` says where to.
    pub fn spawn(&mut self, kind: JobKind, title: String, detail: String, work: impl FnOnce(&mut Worker) -> Outcome + Send + 'static) -> u64 {
        self.spawn_queued(kind, title, detail, None, work)
    }

    /// Like `spawn`, but a job with a `queue_key` waits while another job with the same key is
    /// running: copies to one spinning disk go faster one after the other than all at once.
    pub fn spawn_queued(&mut self, kind: JobKind, title: String, detail: String, queue_key: Option<u64>, work: impl FnOnce(&mut Worker) -> Outcome + Send + 'static) -> u64 {
        let id = self.next;
        self.next += 1;
        let shared = Arc::new(Shared::default());
        let now = Instant::now();
        let must_wait = queue_key.is_some_and(|key| self.jobs.iter().any(|j| j.queue_key == Some(key) && j.state == JobState::Running));
        let state = if must_wait { JobState::Waiting } else { JobState::Running };
        self.jobs.push(Job { id, kind, title, detail, shared, state, queue_key, message: String::new(), finished: None, speed: 0.0, sample: (now, 0) });
        if must_wait {
            self.waiting.push((id, Box::new(work)));
        } else {
            self.launch(id, Box::new(work));
        }
        id
    }

    /// Runs the work of job `id` on a thread.
    fn launch(&mut self, id: u64, work: Work) {
        let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) else { return };
        job.state = JobState::Running;
        let mut worker = Worker { shared: job.shared.clone(), events: self.tx.clone(), ctx: self.ctx.clone(), job: id, remembered: None };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let mut outcome = work(&mut worker);
            if worker.shared.cancel.load(Relaxed) {
                outcome.cancelled = true;
            }
            let _ = tx.send(Event::Done { job: id, outcome });
            ctx.request_repaint();
        });
    }

    /// The job that was next in line for `key` starts, if the key is free.
    fn start_next(&mut self, key: u64) {
        if self.jobs.iter().any(|j| j.queue_key == Some(key) && j.state == JobState::Running) {
            return;
        }
        let next = self.waiting.iter().position(|(id, _)| self.jobs.iter().any(|j| j.id == *id && j.queue_key == Some(key)));
        if let Some(at) = next {
            let (id, work) = self.waiting.remove(at);
            self.launch(id, work);
        }
    }

    /// Start a waiting job now, next to the one that holds its disk.
    pub fn start_now(&mut self, id: u64) {
        if let Some(at) = self.waiting.iter().position(|(w, _)| *w == id) {
            let (id, work) = self.waiting.remove(at);
            self.launch(id, work);
        }
    }

    /// Where a waiting job stands among those waiting for the same disk: `(index, how many)`.
    pub fn queue_position(&self, id: u64) -> Option<(usize, usize)> {
        let key = self.jobs.iter().find(|j| j.id == id && j.state == JobState::Waiting)?.queue_key;
        let same: Vec<u64> = self.waiting.iter().map(|(w, _)| *w).filter(|w| self.jobs.iter().any(|j| j.id == *w && j.queue_key == key)).collect();
        same.iter().position(|w| *w == id).map(|at| (at, same.len()))
    }

    /// Moves a waiting job one place earlier (`-1`) or later (`1`) in its queue.
    pub fn move_waiting(&mut self, id: u64, delta: i32) {
        let Some((at, total)) = self.queue_position(id) else { return };
        let target = at as i32 + delta;
        if target < 0 || target >= total as i32 {
            return;
        }
        let key = self.jobs.iter().find(|j| j.id == id).and_then(|j| j.queue_key);
        let slots: Vec<usize> = self.waiting.iter().enumerate().filter(|(_, (w, _))| self.jobs.iter().any(|j| j.id == *w && j.queue_key == key)).map(|(i, _)| i).collect();
        self.waiting.swap(slots[at], slots[target as usize]);
    }

    /// Collect what the jobs reported since the last call.
    pub fn poll(&mut self) -> Vec<Event> {
        let events: Vec<Event> = self.rx.try_iter().collect();
        let mut freed = Vec::new();
        for event in &events {
            if let Event::Done { job, outcome } = event
                && let Some(job) = self.jobs.iter_mut().find(|j| j.id == *job)
            {
                job.state = if outcome.cancelled {
                    JobState::Cancelled
                } else if outcome.ok {
                    JobState::Done
                } else {
                    JobState::Failed
                };
                job.message = outcome.message.clone();
                job.finished = Some(Instant::now());
                freed.extend(job.queue_key);
            }
        }
        // The disk is free again: the next job waiting for it starts
        for key in freed {
            self.start_next(key);
        }
        events
    }

    /// Smooth the transfer speed of the running jobs; call once per frame.
    pub fn sample(&mut self) {
        let now = Instant::now();
        for job in self.jobs.iter_mut().filter(|j| j.state == JobState::Running) {
            let done = job.shared.bytes_done.load(Relaxed);
            let dt = now.duration_since(job.sample.0).as_secs_f64();
            if job.shared.paused.load(Relaxed) {
                job.sample = (now, done);
                job.speed = 0.0;
            } else if dt >= 0.3 {
                let instant = done.saturating_sub(job.sample.1) as f64 / dt;
                job.speed = if job.speed == 0.0 { instant } else { job.speed * 0.7 + instant * 0.3 };
                job.sample = (now, done);
            }
        }
    }

    /// Forget finished jobs that have been on show long enough.
    pub fn prune(&mut self, keep: Duration) {
        self.jobs.retain(|j| matches!(j.state, JobState::Running | JobState::Waiting) || j.finished.is_some_and(|f| f.elapsed() < keep));
    }

    pub fn cancel(&mut self, id: u64) {
        // A job that never started has nothing to stop: it just leaves the queue
        if let Some(at) = self.waiting.iter().position(|(w, _)| *w == id) {
            drop(self.waiting.remove(at));
            if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
                job.state = JobState::Cancelled;
                job.message = tr("Cancelled").to_owned();
                job.finished = Some(Instant::now());
            }
            return;
        }
        if let Some(job) = self.jobs.iter().find(|j| j.id == id) {
            job.shared.cancel.store(true, Relaxed);
        }
    }

    pub fn toggle_pause(&mut self, id: u64) {
        if let Some(job) = self.jobs.iter().find(|j| j.id == id) {
            let now = job.shared.paused.load(Relaxed);
            job.shared.paused.store(!now, Relaxed);
        }
    }

    /// Overall progress of everything running, if it can be told.
    pub fn overall(&self) -> Option<f32> {
        let running: Vec<&Job> = self.jobs.iter().filter(|j| j.state == JobState::Running).collect();
        let known: Vec<f32> = running.iter().filter_map(|j| j.shared.fraction()).collect();
        (!known.is_empty() && known.len() == running.len()).then(|| known.iter().sum::<f32>() / known.len() as f32)
    }
}

// ---------------------------------------------------------------------------------------------
// Copy and move

#[derive(Default)]
struct Record {
    /// New items (copies, links) an undo moves to the trash.
    created: Vec<PathBuf>,
    /// `(from, to)` of everything moved.
    moved: Vec<(PathBuf, PathBuf)>,
    /// Items replaced by newer ones, now in the trash.
    replaced: Vec<PathBuf>,
    skipped: usize,
    /// Files inside a copied folder that could not be copied, and why; the rest of the folder was copied.
    unreadable: Vec<(PathBuf, String)>,
    /// An item could only be deleted for good, so the undo cannot bring it back.
    lossy: bool,
}

fn measure(path: &Path, files: &mut u64, bytes: &mut u64) {
    let Ok(meta) = path.symlink_metadata() else { return };
    if meta.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                measure(&entry.path(), files, bytes);
            }
        }
    } else {
        *files += 1;
        *bytes += meta.len();
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (a.symlink_metadata(), b.symlink_metadata()) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// Bytes copied between two checks for pause and cancel.
const CHUNK: usize = 1 << 20;

/// Up to `CHUNK` bytes from `reader` to `writer` inside the kernel, at both files' positions. The data
/// never passes through Compression, and file systems that can (btrfs, XFS, NFS) share the blocks instead of copying them.
fn copy_range(reader: &std::fs::File, writer: &std::fs::File) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    // SAFETY: two open descriptors; null offsets make the kernel use and advance the files' own positions
    let n = unsafe { libc::copy_file_range(reader.as_raw_fd(), std::ptr::null_mut(), writer.as_raw_fd(), std::ptr::null_mut(), CHUNK, 0) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

fn copy_file(w: &Worker, src: &Path, dst: &Path) -> io::Result<()> {
    let mut reader = std::fs::File::open(src)?;
    let meta = reader.metadata()?;
    // `create_new`: a file that is already there is an error, and not ours to remove below
    let mut writer = std::fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
    let result = (|| {
        let mut in_kernel = true;
        let mut buffer = Vec::new();
        let mut copied = 0u64;
        loop {
            w.shared.checkpoint()?;
            let n = if in_kernel {
                match copy_range(&reader, &writer) {
                    // Files such as those in /proc claim to be empty to the kernel copy; read them the plain way
                    Ok(0) if copied == 0 => {
                        in_kernel = false;
                        continue;
                    }
                    Ok(n) => n,
                    // Not supported between these files: copy through a buffer instead
                    Err(e) if matches!(e.raw_os_error(), Some(libc::EXDEV | libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP | libc::EPERM | libc::EBADF)) => {
                        in_kernel = false;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            } else {
                if buffer.is_empty() {
                    buffer = vec![0u8; CHUNK];
                }
                let n = reader.read(&mut buffer)?;
                writer.write_all(&buffer[..n])?;
                n
            };
            if n == 0 {
                break;
            }
            copied += n as u64;
            w.add_bytes(n as u64);
        }
        // Tags, comments and ratings live in extended attributes; set them while the copy is still writable
        super::xattr::copy_user(src, dst);
        ops::keep_mode(writer.set_permissions(meta.permissions()))?;
        if let Ok(modified) = meta.modified() {
            let _ = writer.set_modified(modified);
        }
        Ok(())
    })();
    if result.is_err() {
        // A half-written copy is worse than none
        let _ = std::fs::remove_file(dst);
    }
    result
}

fn special_file_error(path: &Path) -> io::Error {
    io::Error::other(trf("Cannot copy {}: it is a named pipe, socket or device file", &[&name_of(path)]))
}

/// Refuse a tree with a named pipe, socket or device file in it before anything is created: opening
/// a pipe to copy it waits for a writer that may never come, and the job would hang for good.
fn refuse_special_files(w: &Worker, path: &Path, lenient: bool) -> io::Result<()> {
    w.shared.checkpoint()?;
    // A copy leaves out what it cannot read (and says so), so this look at the tree does not stop at it;
    // a move across file systems must not start with anything that would fail
    let meta = match path.symlink_metadata() {
        Ok(meta) => meta,
        Err(_) if lenient => return Ok(()),
        Err(err) => return Err(err),
    };
    if meta.is_dir() {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(_) if lenient => return Ok(()),
            Err(err) => return Err(err),
        };
        for entry in entries {
            match entry {
                Ok(entry) => refuse_special_files(w, &entry.path(), lenient)?,
                Err(_) if lenient => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    } else if meta.is_file() || meta.file_type().is_symlink() {
        Ok(())
    } else {
        Err(special_file_error(path))
    }
}

/// Whether a rename from `src` into `parent` stays on one file system, so nothing is copied.
fn same_device(src: &Path, parent: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    matches!((src.symlink_metadata(), parent.metadata()), (Ok(a), Ok(b)) if a.dev() == b.dev())
}

/// `copy_tree` for a job that is making `dst`: when the copy fails or is cancelled, everything it made
/// there is removed again, so no half-copied folder is left. A `dst` that was already there is never touched.
/// With `skipped`, a file inside a folder that cannot be copied is left out and noted there instead of
/// ending the copy; without it (a move across file systems, which removes the original afterwards) the
/// first failure ends it.
fn copy_new(w: &Worker, src: &Path, dst: &Path, skipped: Option<&mut Vec<(PathBuf, String)>>) -> io::Result<()> {
    let existed = dst.symlink_metadata().is_ok();
    let result = copy_tree(w, src, dst, skipped);
    if result.is_err() && !existed {
        let _ = ops::remove_tree(dst);
    }
    result
}

/// Whether an error would meet every file that follows as well, so skipping the file helps no one:
/// a cancel, or a target that is full or cannot be written to.
fn ends_the_copy(err: &io::Error) -> bool {
    is_cancel(err) || matches!(err.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT | libc::EROFS))
}

fn copy_tree(w: &Worker, src: &Path, dst: &Path, mut skipped: Option<&mut Vec<(PathBuf, String)>>) -> io::Result<()> {
    w.shared.checkpoint()?;
    let meta = src.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
        w.file_done();
        Ok(())
    } else if meta.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let (from, to) = (entry.path(), dst.join(entry.file_name()));
            match copy_tree(w, &from, &to, skipped.as_deref_mut()) {
                Err(err) if !ends_the_copy(&err) && skipped.is_some() => {
                    if let Some(list) = skipped.as_deref_mut() {
                        list.push((from.clone(), super::describe::io_error(&err)));
                    }
                    // A folder that could not be entered was made empty on the way: not worth keeping
                    if from.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                        let _ = std::fs::remove_dir(&to);
                    }
                }
                other => other?,
            }
        }
        super::xattr::copy_user(src, dst);
        ops::keep_mode(std::fs::set_permissions(dst, meta.permissions()))
    } else if meta.is_file() {
        w.shared.set_current(&name_of(src));
        copy_file(w, src, dst)?;
        w.file_done();
        Ok(())
    } else {
        // Made after the check above, or not seen by it
        Err(special_file_error(src))
    }
}

/// Take `target` out of the way: to the trash if possible, so the undo can bring it back.
fn clear_target(rec: &mut Record, target: &Path) -> io::Result<()> {
    match trash::trash(target) {
        Ok(trashed) => rec.replaced.push(trashed),
        Err(_) => {
            ops::remove_tree(target)?;
            rec.lossy = true;
        }
    }
    Ok(())
}

fn transfer_one(w: &mut Worker, op: Op, src: &Path, target: &Path, known: Option<(u64, u64)>, rec: &mut Record) -> io::Result<()> {
    w.shared.checkpoint()?;
    let parent = target.parent().ok_or_else(|| io::Error::other("no folder"))?;
    let name = target.file_name().ok_or_else(|| io::Error::other("no file name"))?.to_owned();
    if src.is_dir() && !src.is_symlink() && parent.canonicalize().is_ok_and(|p| src.canonicalize().is_ok_and(|s| p.starts_with(s))) {
        return Err(io::Error::other(trf("Cannot put {} inside itself", &[&name_of(src)])));
    }
    // Before a conflict is answered, so a refused item does not first send the old target to the trash
    if op == Op::Copy || !same_device(src, parent) {
        refuse_special_files(w, src, op == Op::Copy)?;
    }
    let mut target = target.to_path_buf();
    let mut merge = false;
    if target.symlink_metadata().is_ok() {
        if same_file(src, &target) {
            if op == Op::Move {
                return Ok(());
            }
            // Pasting into the folder it came from makes a copy beside it
            target = unique_path(parent, &name, "copy");
        } else {
            match w.decide(src, &target)? {
                Decision::Skip => {
                    let (files, bytes) = known.unwrap_or_else(|| {
                        let (mut f, mut b) = (0, 0);
                        measure(src, &mut f, &mut b);
                        (f, b)
                    });
                    w.shared.files_done.fetch_add(files, Relaxed);
                    w.add_bytes(bytes);
                    rec.skipped += 1;
                    return Ok(());
                }
                Decision::Cancel => return Err(cancelled()),
                Decision::KeepBoth => target = unique_path(parent, &name, "copy"),
                Decision::Replace => clear_target(rec, &target)?,
                Decision::Merge => merge = true,
            }
        }
    }
    w.shared.set_current(&name_of(src));

    if merge {
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            transfer_one(w, op, &entry.path(), &target.join(entry.file_name()), None, rec)?;
        }
        if op == Op::Move {
            let _ = std::fs::remove_dir(src);
        }
        return Ok(());
    }

    match op {
        Op::Copy => {
            copy_new(w, src, &target, Some(&mut rec.unreadable))?;
            rec.created.push(target);
        }
        Op::Move => match std::fs::rename(src, &target) {
            Ok(()) => {
                let (files, bytes) = known.unwrap_or_else(|| {
                    let (mut f, mut b) = (0, 0);
                    measure(&target, &mut f, &mut b);
                    (f, b)
                });
                w.shared.files_done.fetch_add(files, Relaxed);
                w.add_bytes(bytes);
                rec.moved.push((src.to_path_buf(), target));
            }
            // Another filesystem: copy, then remove the original once the copy is whole
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                copy_new(w, src, &target, None)?;
                ops::remove_tree(src)?;
                rec.moved.push((src.to_path_buf(), target));
            }
            Err(e) => return Err(e),
        },
    }
    Ok(())
}

/// The disk a copy or move writes to, for `Engine::spawn_queued`. A move that stays on one file
/// system is a rename and finishes at once, so it is never queued.
pub fn queue_key(op: Op, sources: &[PathBuf], dest_dir: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let dest = std::fs::metadata(dest_dir).ok()?.dev();
    match op {
        Op::Copy => Some(dest),
        Op::Move => sources.iter().any(|s| s.symlink_metadata().map_or(true, |m| m.dev() != dest)).then_some(dest),
    }
}

/// Copy or move every `(source, target)` pair. A taken target name is a question for the window.
pub fn transfer(w: &mut Worker, op: Op, pairs: Vec<(PathBuf, PathBuf)>) -> Outcome {
    transfer_with(w, op, pairs, free_space)
}

/// `transfer` with the source of the free room given, so a test can say how much there is.
fn transfer_with(w: &mut Worker, op: Op, pairs: Vec<(PathBuf, PathBuf)>, free: impl Fn(&Path) -> Option<u64>) -> Outcome {
    let sizes: Vec<(u64, u64)> = pairs
        .iter()
        .map(|(src, _)| {
            let (mut files, mut bytes) = (0, 0);
            measure(src, &mut files, &mut bytes);
            (files, bytes)
        })
        .collect();
    w.shared.files_total.store(sizes.iter().map(|s| s.0).sum(), Relaxed);
    w.shared.bytes_total.store(sizes.iter().map(|s| s.1).sum(), Relaxed);

    // Replaced files are not counted back, so a replace may warn a little early; a move inside one file
    // system is a rename and needs no room
    if let Some(dest) = pairs.first().and_then(|(_, target)| target.parent()) {
        let needed = pairs
            .iter()
            .zip(&sizes)
            .filter(|((src, target), _)| op == Op::Copy || target.parent().is_none_or(|parent| !same_device(src, parent)))
            .fold(0u64, |sum, (_, size)| sum.saturating_add(size.1));
        if let Err(err) = w.ensure_space(dest, needed, free) {
            return if is_cancel(&err) { Outcome::stopped() } else { Outcome::failed(err.to_string()) };
        }
    }

    let mut rec = Record::default();
    let mut failures = Vec::new();
    let mut was_cancelled = false;
    for ((src, target), known) in pairs.iter().zip(&sizes) {
        match transfer_one(w, op, src, target, Some(*known), &mut rec) {
            Ok(()) => {}
            Err(e) if is_cancel(&e) => {
                was_cancelled = true;
                break;
            }
            Err(e) => failures.push(format!("{}: {}", name_of(src), crate::fs::describe::io_error(&e))),
        }
    }

    let count = pairs.len();
    let label = if count == 1 { format!("“{}”", name_of(&pairs[0].0)) } else { trn(count, "{} item", "{} items") };
    let dest = pairs.first().and_then(|(_, t)| t.parent()).map(name_of).unwrap_or_default();

    let mut edits: Vec<Edit> = Vec::new();
    let undo_label = match op {
        Op::Copy => {
            edits.extend(rec.created.iter().cloned().map(Edit::Trash));
            trf("Copy of {}", &[&label])
        }
        Op::Move => {
            edits.extend(rec.moved.iter().rev().map(|(from, to)| Edit::Move { from: to.clone(), to: from.clone() }));
            trf("Move of {}", &[&label])
        }
    };
    edits.extend(rec.replaced.iter().cloned().map(Edit::Restore));
    let effect = if edits.is_empty() { Effect::None } else { Effect::Record(UndoEntry::new(undo_label, edits)) };

    let mut message = if was_cancelled {
        tr("Cancelled").to_owned()
    } else {
        match op {
            Op::Copy => trf("Copied {} to “{}”", &[&label, &dest]),
            Op::Move => trf("Moved {} to “{}”", &[&label, &dest]),
        }
    };
    if rec.skipped > 0 {
        message.push_str(&format!(" · {}", trf("{} skipped", &[&rec.skipped])));
    }
    // The rest of a folder was copied; what was left out is said, and the job still counts as done
    let left_out = (!rec.unreadable.is_empty()).then(|| unreadable_note(&rec.unreadable));
    if let Some(note) = &left_out {
        message.push_str(&format!(" · {note}"));
    }
    if !failures.is_empty() {
        let mut text = failures.join(" · ");
        if let Some(note) = &left_out {
            text.push_str(&format!(" · {note}"));
        }
        let mut outcome = Outcome::failed(text).with(effect);
        outcome.cancelled = was_cancelled;
        return outcome;
    }
    let mut outcome = Outcome::done(message).with(effect);
    outcome.cancelled = was_cancelled;
    outcome
}

/// "3 items could not be copied: a/b.txt (Permission denied), …" for the files a copy left out.
fn unreadable_note(list: &[(PathBuf, String)]) -> String {
    const SHOWN: usize = 3;
    let named: Vec<String> = list
        .iter()
        .take(SHOWN)
        .map(|(path, why)| {
            let folder = path.parent().map(name_of).filter(|f| !f.is_empty());
            let name = match folder {
                Some(folder) => format!("{folder}/{}", name_of(path)),
                None => name_of(path),
            };
            format!("{name} ({why})")
        })
        .collect();
    let mut text = format!("{}: {}", trn(list.len(), "{} item could not be copied", "{} items could not be copied"), named.join(", "));
    if list.len() > SHOWN {
        text.push_str(&format!(", {}", trf("and {} more", &[&(list.len() - SHOWN)])));
    }
    text
}

/// Delete for good, one item at a time.
pub fn delete_forever(w: &mut Worker, paths: Vec<PathBuf>) -> Outcome {
    w.shared.files_total.store(paths.len() as u64, Relaxed);
    let mut failures = Vec::new();
    let mut was_cancelled = false;
    for path in &paths {
        if w.shared.checkpoint().is_err() {
            was_cancelled = true;
            break;
        }
        w.shared.set_current(&name_of(path));
        if let Err(err) = trash::delete_forever(path) {
            failures.push(format!("{}: {}", name_of(path), crate::fs::describe::io_error(&err)));
        }
        w.file_done();
    }
    let label = if paths.len() == 1 { format!("“{}”", name_of(&paths[0])) } else { trn(paths.len(), "{} item", "{} items") };
    let mut outcome = if !failures.is_empty() {
        Outcome::failed(failures.join(" · "))
    } else if was_cancelled {
        Outcome::done(tr("Cancelled").to_owned())
    } else {
        Outcome::done(trf("Deleted {}", &[&label]))
    };
    outcome.cancelled = was_cancelled;
    outcome
}

