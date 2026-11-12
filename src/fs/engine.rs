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
    /// New permissions for everything inside a folder.
    Permissions,
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
    /// Steps of an undo (`false`) or redo (`true`) that failed and wait for another try, see `history::apply_entry`.
    pub left: Option<(bool, UndoEntry)>,
}

impl Outcome {
    pub fn done(message: String) -> Self {
        Self { ok: true, cancelled: false, message, effect: Effect::None, left: None }
    }

    pub fn failed(message: String) -> Self {
        Self { ok: false, cancelled: false, message, effect: Effect::None, left: None }
    }

    /// The job was cancelled and took back what it had begun.
    pub fn stopped() -> Self {
        Self { ok: false, cancelled: true, message: tr("Cancelled").to_owned(), effect: Effect::None, left: None }
    }

    pub fn with(mut self, effect: Effect) -> Self {
        self.effect = effect;
        self
    }

    /// Keep `entry`, the steps that failed, for another try on the side of the history it came from.
    pub fn keeping(mut self, redo: bool, entry: Option<UndoEntry>) -> Self {
        self.left = entry.map(|entry| (redo, entry));
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
        matches!(self.kind, JobKind::Copy | JobKind::Move | JobKind::Delete | JobKind::Extract | JobKind::Compress | JobKind::Permissions)
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
    /// Files with more than one name that this job has copied: `(device, inode)` of the original, and the copy made
    /// (with its own inode), so that another name of the same file becomes another name of that copy.
    links: Mutex<std::collections::HashMap<(u64, u64), (PathBuf, u64)>>,
    /// Items copied without some of their tags and other attributes, which the target did not take.
    attrs_lost: AtomicU64,
    /// Items copied without an ACL they had (access or default), which the target did not take.
    acl_lost: AtomicU64,
    /// Of those, the ones whose access ACL did not arrive, so that their group permissions were not copied.
    group_lost: AtomicU64,
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

    /// The tags, other `user.*` attributes and ACLs of `src` onto its copy, and the permissions the copy is to get (see
    /// `ops::keep_attributes`); if the target does not take everything, that is counted so the result can say so.
    fn keep_attributes(&self, src: &Path, dst: &Path, original: &std::fs::Metadata) -> std::fs::Permissions {
        let kept = ops::keep_attributes(src, dst, original);
        if !kept.tags {
            self.attrs_lost.fetch_add(1, Relaxed);
        }
        if kept.acl.any_lost {
            self.acl_lost.fetch_add(1, Relaxed);
        }
        if kept.acl.access_lost {
            self.group_lost.fetch_add(1, Relaxed);
        }
        kept.permissions
    }

    /// Makes `dst` another name of the copy that was made of the file `meta` describes, if this job copied it before.
    /// Never replaces anything (`hard_link` fails if `dst` is there), and a copy that is not the one remembered any more
    /// is not used: then the file is copied in full.
    fn link_to_earlier_copy(&self, meta: &std::fs::Metadata, dst: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Some((first, inode)) = self.links.lock().ok().and_then(|links| links.get(&(meta.dev(), meta.ino())).cloned()) else { return false };
        if std::fs::hard_link(&first, dst).is_err() {
            return false;
        }
        if dst.symlink_metadata().is_ok_and(|now| now.ino() == inode) {
            return true;
        }
        let _ = std::fs::remove_file(dst);
        false
    }

    fn remember_copy(&self, meta: &std::fs::Metadata, dst: &Path) {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(mut links), Ok(copy)) = (self.links.lock(), dst.symlink_metadata()) {
            links.insert((meta.dev(), meta.ino()), (dst.to_path_buf(), copy.ino()));
        }
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
        let mut worker = Worker { shared: job.shared.clone(), events: self.tx.clone(), ctx: self.ctx.clone(), job: id, remembered: None, links: Mutex::default(), attrs_lost: AtomicU64::new(0), acl_lost: AtomicU64::new(0), group_lost: AtomicU64::new(0) };
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
    /// Items that could only be deleted for good (the trash could not take them), so the undo cannot bring them back.
    deleted_for_good: Vec<PathBuf>,
}

fn measure(path: &Path, files: &mut u64, bytes: &mut u64) {
    let Ok(meta) = path.symlink_metadata() else { return };
    if meta.is_dir() {
        if let Ok(entries) = ops::read_dir_keeping_atime(path) {
            for entry in entries.flatten() {
                measure(&entry, files, bytes);
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

/// Up to `most` (at most `CHUNK`) bytes from `reader` to `writer` inside the kernel, at both files' positions. The data
/// never passes through Compression, and file systems that can (btrfs, XFS, NFS) share the blocks instead of copying them.
fn copy_range(reader: &std::fs::File, writer: &std::fs::File, most: usize) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    // SAFETY: two open descriptors; null offsets make the kernel use and advance the files' own positions
    let n = unsafe { libc::copy_file_range(reader.as_raw_fd(), std::ptr::null_mut(), writer.as_raw_fd(), std::ptr::null_mut(), most.min(CHUNK), 0) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

/// How the bytes of a file get from one place to the other: inside the kernel while that works, through a buffer when it
/// does not.
struct Mover {
    in_kernel: bool,
    buffer: Vec<u8>,
}

impl Mover {
    fn new() -> Self {
        Self { in_kernel: true, buffer: Vec::new() }
    }

    /// Up to `most` bytes from `reader` to `writer`, at their positions; 0 is the end. `first`: nothing was copied yet.
    fn step(&mut self, reader: &std::fs::File, writer: &std::fs::File, most: usize, first: bool) -> io::Result<usize> {
        loop {
            if self.in_kernel {
                match copy_range(reader, writer, most) {
                    // Files such as those in /proc claim to be empty to the kernel copy; read them the plain way
                    Ok(0) if first => self.in_kernel = false,
                    Ok(n) => return Ok(n),
                    // Not supported between these files: copy through a buffer instead
                    Err(e) if matches!(e.raw_os_error(), Some(libc::EXDEV | libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP | libc::EPERM | libc::EBADF)) => self.in_kernel = false,
                    Err(e) => return Err(e),
                }
            } else {
                if self.buffer.is_empty() {
                    self.buffer = vec![0u8; CHUNK];
                }
                let n = (&*reader).read(&mut self.buffer[..most.min(CHUNK)])?;
                (&*writer).write_all(&self.buffer[..n])?;
                return Ok(n);
            }
        }
    }
}

fn copy_file(w: &Worker, src: &Path, dst: &Path) -> io::Result<()> {
    let reader = std::fs::File::open(src)?;
    let meta = reader.metadata()?;
    // `create_new`: a file that is already there is an error, and not ours to remove below
    let writer = std::fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
    let result = (|| {
        let mut mover = Mover::new();
        // A file with holes (a disk image, a database) keeps them instead of becoming as big as it says
        let holey = ops::may_have_holes(&meta)
            && ops::copy_holey(&reader, &writer, meta.len(), &mut || w.shared.checkpoint(), &mut |most, first| mover.step(&reader, &writer, most as usize, first), &mut |n| w.add_bytes(n))? == ops::Holey::Copied;
        let mut copied = 0u64;
        if !holey {
            loop {
                w.shared.checkpoint()?;
                let n = mover.step(&reader, &writer, CHUNK, copied == 0)?;
                if n == 0 {
                    break;
                }
                copied += n as u64;
                w.add_bytes(n as u64);
            }
        }
        // Tags, comments and ratings live in extended attributes; set them while the copy is still writable
        let permissions = w.keep_attributes(src, dst, &meta);
        ops::keep_times(&writer, &meta);
        ops::keep_mode(writer.set_permissions(permissions))?;
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
        let entries = match ops::read_dir_keeping_atime(path) {
            Ok(entries) => entries,
            Err(_) if lenient => return Ok(()),
            Err(err) => return Err(err),
        };
        for entry in entries {
            match entry {
                Ok(entry) => refuse_special_files(w, &entry, lenient)?,
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
        ops::keep_link_times(dst, &meta);
        w.file_done();
        Ok(())
    } else if meta.is_dir() {
        std::fs::create_dir(dst)?;
        for from in ops::read_dir_keeping_atime(src)? {
            let from = from?;
            let to = dst.join(from.file_name().unwrap_or_default());
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
        let permissions = w.keep_attributes(src, dst, &meta);
        ops::keep_folder_times(dst, &meta);
        ops::keep_mode(std::fs::set_permissions(dst, permissions))
    } else if meta.is_file() {
        use std::os::unix::fs::MetadataExt;
        w.shared.set_current(&name_of(src));
        if meta.nlink() > 1 && w.link_to_earlier_copy(&meta, dst) {
            w.add_bytes(meta.len());
        } else {
            copy_file(w, src, dst)?;
            if meta.nlink() > 1 {
                w.remember_copy(&meta, dst);
            }
        }
        w.file_done();
        Ok(())
    } else {
        // Made after the check above, or not seen by it
        Err(special_file_error(src))
    }
}

/// How the new item for `replace_target` was made.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Built {
    /// `src` itself was renamed to the staging name (a move within one file system).
    Moved,
    /// A copy of `src` is at the staging name; `src` is as it was.
    Copied,
}

/// Take away what `replace_target` made, or give a moved item back: the old target has not been touched, or is back.
fn discard_staged(src: &Path, staged: &Path, built: Built) -> io::Result<()> {
    match built {
        Built::Copied => ops::remove_tree(staged),
        Built::Moved => crate::compat::rename_noreplace(staged, src),
    }
}

/// Send the old `target` to the trash, where an undo finds it. There is no other way for it to go: it is never
/// deleted for good here.
fn trash_old(rec: &mut Record, target: &Path) -> io::Result<()> {
    match trash::trash(target) {
        Ok(trashed) => {
            rec.replaced.push(trashed);
            Ok(())
        }
        // The trash has a whole copy (and keeps its record) but what is left of the original is still here: that
        // is cleared too, and an undo brings the copy back
        Err(err) if ops::is_copied_not_removed(&err) => {
            if let Some(copied) = ops::copied_not_removed(&err) {
                rec.replaced.push(copied.copy.clone());
            }
            ops::remove_tree(target)
        }
        Err(err) => Err(err),
    }
}

/// "Replace". The new item is made complete first, next to the old one, under a name of its own; only then does the old
/// one go to the trash and the new one take its name (without replacing anything: a name taken in between is an error).
/// So a failure or a cancel before the swap leaves the old item as it was and only the new one's remains are removed. If
/// the old item cannot go to the trash it is left alone and this position is not replaced, and said not to be: it is
/// never deleted for good. Items on a server are not replaced either (no trash there, and nothing was asked about it).
fn replace_target(w: &mut Worker, op: Op, src: &Path, target: &Path, known: Option<(u64, u64)>, rec: &mut Record) -> io::Result<()> {
    w.shared.set_current(&name_of(src));
    if crate::fs::network::is_remote(target) {
        return Err(io::Error::other(tr("Not replaced: items on a server have no trash, and the old item would be lost for good")));
    }
    let parent = target.parent().ok_or_else(|| io::Error::other("no folder"))?;

    // 1. The new item, complete, under a name of its own
    let staged = (0..1000u32)
        .map(|n| parent.join(format!(".compression-new-{}-{n}", std::process::id())))
        .find(|candidate| candidate.symlink_metadata().is_err())
        .ok_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists))?;
    let built = match op {
        Op::Copy => copy_new(w, src, &staged, Some(&mut rec.unreadable)).map(|()| Built::Copied)?,
        Op::Move => match crate::compat::rename_noreplace(src, &staged) {
            Ok(()) => Built::Moved,
            // Another file system: copy, and take the original away once the copy has its place
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => copy_new(w, src, &staged, None).map(|()| Built::Copied)?,
            Err(e) => return Err(e),
        },
    };
    let give_back = |why: io::Error| -> io::Error {
        match discard_staged(src, &staged, built) {
            Ok(()) => why,
            Err(_) => io::Error::other(trf("Not replaced: the new item could not take the place of the old one ({}); it is kept as “{}”", &[&crate::fs::describe::io_error(&why), &name_of(&staged)])),
        }
    };

    // 2. A cancel before the swap costs nothing but the new item
    #[cfg(test)]
    tests::cancel_before_swap(w, target);
    if let Err(err) = w.shared.checkpoint() {
        return Err(give_back(err));
    }

    // 3. The old item to the trash; if that cannot be, it stays and this position is left out
    let kept = rec.replaced.len();
    if let Err(err) = trash_old(rec, target) {
        rec.replaced.truncate(kept);
        let why = io::Error::other(trf("Not replaced: the old item cannot go to the trash and would be lost for good ({})", &[&crate::fs::describe::io_error(&err)]));
        return Err(give_back(why));
    }

    // 4. The new item takes the name. A name taken meanwhile is not replaced
    #[cfg(test)]
    tests::race_here(target);
    if let Err(err) = crate::compat::rename_noreplace(&staged, target) {
        let old = rec.replaced.get(kept).cloned();
        let failure = give_back(err);
        // The old item back where it was, if it can be
        return Err(match old.map(|trashed| trash::restore(&trashed)) {
            Some(Ok(_)) => {
                rec.replaced.truncate(kept);
                io::Error::other(trf("Not replaced: {}", &[&crate::fs::describe::io_error(&failure)]))
            }
            _ => io::Error::other(trf("Not replaced: the old item is in the trash, but the new one could not take its place ({})", &[&crate::fs::describe::io_error(&failure)])),
        });
    }

    // 5. Done: what an undo needs, and the progress
    match (op, built) {
        (Op::Copy, _) => rec.created.push(target.to_path_buf()),
        (Op::Move, Built::Moved) => {
            let (files, bytes) = known.unwrap_or_else(|| {
                let (mut f, mut b) = (0, 0);
                measure(target, &mut f, &mut b);
                (f, b)
            });
            w.shared.files_done.fetch_add(files, Relaxed);
            w.add_bytes(bytes);
            rec.moved.push((src.to_path_buf(), target.to_path_buf()));
        }
        (Op::Move, Built::Copied) => {
            if let Err(err) = ops::remove_tree(src) {
                // The copy is whole and has its place; the original is still (at least partly) where it was. That is
                // said, and an undo takes the copy away again
                let folder = target.parent().map(name_of).unwrap_or_default();
                rec.created.push(target.to_path_buf());
                return Err(io::Error::other(trf("A complete copy was made in “{}”, but the original could not be fully removed: {}", &[&folder, &crate::fs::describe::io_error(&err)])));
            }
            rec.moved.push((src.to_path_buf(), target.to_path_buf()));
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
                Decision::Replace => return replace_target(w, op, src, &target, known, rec),
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

    #[cfg(test)]
    tests::race_here(&target);
    match op {
        Op::Copy => {
            copy_new(w, src, &target, Some(&mut rec.unreadable))?;
            rec.created.push(target);
        }
        // Never replaces: a target that appeared since the conflict was settled is not ours to overwrite
        Op::Move => match crate::compat::rename_noreplace(src, &target) {
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
                if let Err(err) = ops::remove_tree(src) {
                    // The copy is whole and stays; the original is still (at least partly) where it was. That is
                    // said, and an undo takes the copy away again
                    let folder = target.parent().map(name_of).unwrap_or_default();
                    rec.created.push(target);
                    return Err(io::Error::other(trf("A complete copy was made in “{}”, but the original could not be fully removed: {}", &[&folder, &crate::fs::describe::io_error(&err)])));
                }
                rec.moved.push((src.to_path_buf(), target));
            }
            Err(e) => return Err(e),
        },
    }
    Ok(())
}

/// The room the copy of `src` takes in `dest`, given that its size, holes included, is `size`. A file with holes
/// copied within one file system keeps them (that file system has holes, the source is the proof), so it takes what
/// the source takes; on another one the target may not have holes, so its whole size is counted. Nothing is guessed
/// about copies that share their blocks (reflinks): they are counted in full, and the question can be answered "continue".
fn room_for(src: &Path, dest: Option<&Path>, size: u64) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fn used(path: &Path, device: u64) -> u64 {
        let Ok(meta) = path.symlink_metadata() else { return 0 };
        if meta.is_dir() {
            ops::read_dir_keeping_atime(path).map_or(0, |entries| entries.flatten().fold(0u64, |sum, entry| sum.saturating_add(used(&entry, device))))
        } else if meta.dev() == device && ops::may_have_holes(&meta) {
            meta.blocks().saturating_mul(512).min(meta.len())
        } else {
            meta.len()
        }
    }
    match dest.and_then(|d| d.metadata().ok()) {
        Some(dest) => used(src, dest.dev()).min(size),
        None => size,
    }
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
            .fold(0u64, |sum, ((src, target), size)| sum.saturating_add(room_for(src, target.parent(), size.1)));
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
            // Copies whose originals could not be removed: an undo takes those copies away, it does not put the originals back
            edits.extend(rec.created.iter().cloned().map(Edit::Trash));
            if rec.created.is_empty() { trf("Move of {}", &[&label]) } else { trf("Incomplete move of {}", &[&label]) }
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
    let lost = w.attrs_lost.load(Relaxed);
    if lost > 0 {
        let note = trn(lost as usize, "{} item was copied without its tags and other attributes: the target does not keep them", "{} items were copied without their tags and other attributes: the target does not keep them");
        message.push_str(&format!(" · {note}"));
    }
    let acls = w.acl_lost.load(Relaxed);
    if acls > 0 {
        let note = trn(acls as usize, "{} item was copied without its access control list (ACL): the target does not keep ACLs", "{} items were copied without their access control lists (ACLs): the target does not keep ACLs");
        message.push_str(&format!(" · {note}"));
    }
    let groups = w.group_lost.load(Relaxed);
    if groups > 0 {
        let note = trn(groups as usize, "The group permissions of {} item were not copied, because its ACL could not be: it has none for the group", "The group permissions of {} items were not copied, because their ACLs could not be: they have none for the group");
        message.push_str(&format!(" · {note}"));
    }
    // The rest of a folder was copied; what was left out is said, and the job still counts as done
    // Said in the message of a job that goes wrong too: it is the one thing an undo cannot make good
    let gone = deleted_for_good_note(&rec.deleted_for_good);
    if let Some(note) = &gone {
        message.push_str(&format!(" · {note}"));
    }
    let left_out = (!rec.unreadable.is_empty()).then(|| unreadable_note(&rec.unreadable));
    if let Some(note) = &left_out {
        message.push_str(&format!(" · {note}"));
    }
    if !failures.is_empty() {
        let mut text = failures.join(" · ");
        for note in [&gone, &left_out].into_iter().flatten() {
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

/// What the message says about replaced items that the trash could not take and that were deleted for good.
fn deleted_for_good_note(list: &[PathBuf]) -> Option<String> {
    (!list.is_empty()).then(|| {
        trn(
            list.len(),
            "{} replaced item could not go to the trash and was deleted for good: undo cannot bring it back",
            "{} replaced items could not go to the trash and were deleted for good: undo cannot bring them back",
        )
    })
}

/// "3 items could not be copied: a/b.txt (Permission denied), …" for the files a copy left out.
fn unreadable_note(list: &[(PathBuf, String)]) -> String {
    left_out_note(list, trn(list.len(), "{} item could not be copied", "{} items could not be copied"))
}

/// `head`, then the first few of `list` with the reason each was left out, and how many more there are.
pub(crate) fn left_out_note(list: &[(PathBuf, String)], head: String) -> String {
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
    let mut text = format!("{head}: {}", named.join(", "));
    if list.len() > SHOWN {
        text.push_str(&format!(", {}", trf("and {} more", &[&(list.len() - SHOWN)])));
    }
    text
}

/// Delete for good, one item at a time.
/// What `place_extracted` did, to be undone together.
#[derive(Default)]
pub struct Placement {
    rec: Record,
}

impl Placement {
    /// The entry that takes the extracted items away again (to the trash) and brings back what they replaced. It does
    /// not move them back to where they were unpacked: that place is gone.
    fn undo(&self, label: String) -> Option<UndoEntry> {
        let mut edits: Vec<Edit> = self.rec.moved.iter().map(|(_, to)| Edit::Trash(to.clone())).collect();
        edits.extend(self.rec.created.iter().cloned().map(Edit::Trash));
        edits.extend(self.rec.replaced.iter().cloned().map(Edit::Restore));
        (!edits.is_empty()).then(|| UndoEntry::new(label, edits))
    }
}

/// Put what an archive was unpacked into (`stage`, a folder of `dest`) into `dest`, item by item, as a move does: a name
/// that is taken is a question for the window (replace, skip, keep both, merge), and what is replaced goes to the trash.
fn place_extracted(w: &mut Worker, stage: &Path, dest: &Path, placement: &mut Placement) -> io::Result<()> {
    let mut failures = Vec::new();
    for entry in std::fs::read_dir(stage)? {
        let entry = entry?;
        match transfer_one(w, Op::Move, &entry.path(), &dest.join(entry.file_name()), None, &mut placement.rec) {
            Ok(()) => {}
            Err(e) if is_cancel(&e) => return Err(e),
            Err(e) => failures.push(format!("{}: {}", name_of(&entry.path()), crate::fs::describe::io_error(&e))),
        }
    }
    if failures.is_empty() { Ok(()) } else { Err(io::Error::other(failures.join(" · "))) }
}

/// Unpack each archive of `items` into the folder `dest` ("Extract to…"). Nothing is written into `dest` that the
/// person has not been asked about: an archive is unpacked into a folder of its own inside `dest` first, and its items
/// are put into `dest` the way a move puts them, so a name that is taken is a question and what is replaced can be
/// brought back. The undo takes the extracted items away again.
pub fn extract_into(w: &mut Worker, items: Vec<PathBuf>, dest: PathBuf) -> Outcome {
    let label = if items.len() == 1 { format!("“{}”", name_of(&items[0])) } else { trn(items.len(), "{} item", "{} items") };
    let target = name_of(&dest);
    let sizes: Vec<u64> = items.iter().map(|p| p.metadata().map_or(0, |m| m.len())).collect();
    w.shared.files_total.store(items.len() as u64, Relaxed);
    w.shared.bytes_total.store(sizes.iter().sum(), Relaxed);
    let pairs: Vec<(PathBuf, PathBuf)> = items.iter().map(|item| (item.clone(), dest.clone())).collect();
    if w.ensure_space_for_archives(&pairs, free_space).is_err() {
        return Outcome::stopped();
    }
    let (mut placement, mut failures, mut done, mut stopped) = (Placement::default(), Vec::new(), 0, false);
    for (item, size) in items.iter().zip(&sizes) {
        if w.shared.checkpoint().is_err() {
            stopped = true;
            break;
        }
        let result = w.archive_control(done, *size, |control| ops::extract_staged(item, &dest, control)).and_then(|stage| {
            let placed = place_extracted(w, &stage, &dest, &mut placement);
            // What was left out (skipped, or not placed because of a failure) is not kept
            let _ = ops::remove_tree(&stage);
            placed
        });
        match result {
            Err(err) if is_cancel(&err) => {
                stopped = true;
                break;
            }
            Err(err) => failures.push(format!("{}: {}", name_of(item), crate::fs::describe::io_error(&err))),
            Ok(()) => {}
        }
        done += size;
        w.shared.bytes_done.store(done, Relaxed);
        w.shared.files_done.fetch_add(1, Relaxed);
    }
    // What was put in place stays, can be undone, and is not hidden by a later failure or a cancel
    let effect = placement.undo(trf("Extraction of {}", &[&label])).map_or(Effect::None, Effect::Record);
    let mut notes = Vec::new();
    if placement.rec.skipped > 0 {
        notes.push(trf("{} skipped", &[&placement.rec.skipped]));
    }
    notes.extend(deleted_for_good_note(&placement.rec.deleted_for_good));
    let notes: String = notes.iter().map(|n| format!(" · {n}")).collect();
    if stopped {
        return Outcome::stopped().with(effect);
    }
    if failures.is_empty() {
        Outcome::done(format!("{}{notes}", trf("Extracted {} to “{}”", &[&label, &target]))).with(effect)
    } else {
        Outcome::failed(format!("{}{notes}", failures.join(" · "))).with(effect)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Targets at which a file appears just before the move is made: the moment between settling a conflict and the rename.
    static RACES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// Targets for which the job is cancelled once the new item is complete and before the swap.
    static CANCELS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(super) fn cancel_before_swap(w: &Worker, target: &Path) {
        if CANCELS.lock().unwrap().iter().any(|p| p == target) {
            w.shared.cancel.store(true, Relaxed);
        }
    }

    pub(super) fn race_here(target: &Path) {
        if RACES.lock().unwrap().iter().any(|p| p == target) {
            let _ = std::fs::write(target, "intruder");
        }
    }

    fn engine() -> Engine {
        Engine::new(&egui::Context::default())
    }

    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("compression_engine_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Runs a job to its end; every conflict is answered with `decision`.
    fn run(kind: JobKind, op: Op, pairs: Vec<(PathBuf, PathBuf)>, decision: Decision) -> (Outcome, usize) {
        let mut e = engine();
        e.spawn(kind, String::new(), String::new(), move |w| transfer(w, op, pairs));
        let (mut done, mut asked) = (None, 0);
        for _ in 0..500 {
            for event in e.poll() {
                match event {
                    Event::Conflict(request) => {
                        asked += 1;
                        request.answer(decision, false);
                    }
                    Event::Done { outcome, .. } => done = Some(outcome),
                    _ => {}
                }
            }
            if !e.running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        (done.expect("the job never finished"), asked)
    }

    fn is_root() -> bool {
        // SAFETY: `geteuid` has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    /// `dir/src/<name>` ("new") and `dir/dst/<name>` ("old"), both files.
    fn clash(name: &str) -> PathBuf {
        let dir = sandbox(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("src/a"), "new").unwrap();
        std::fs::write(dir.join("dst/a"), "old").unwrap();
        dir
    }

    fn pair(dir: &Path) -> Vec<(PathBuf, PathBuf)> {
        vec![(dir.join("src/a"), dir.join("dst/a"))]
    }

    #[test]
    fn what_was_deleted_for_good_is_said_in_words_and_counted() {
        assert_eq!(deleted_for_good_note(&[]), None);
        let one = deleted_for_good_note(&[PathBuf::from("/x")]).unwrap();
        assert_eq!(one, "1 replaced item could not go to the trash and was deleted for good: undo cannot bring it back");
        let two = deleted_for_good_note(&[PathBuf::from("/x"), PathBuf::from("/y")]).unwrap();
        assert!(two.starts_with("2 replaced items could not go to the trash and were deleted for good"), "{two}");
    }

    #[test]
    fn replacing_a_file_puts_the_new_one_in_place_sends_the_old_one_to_the_trash_and_leaves_nothing_behind() {
        for op in [Op::Copy, Op::Move] {
            let dir = clash(&format!("replace_file_{op:?}"));
            let (outcome, asked) = run(JobKind::Copy, op, pair(&dir), Decision::Replace);
            assert_eq!(asked, 1);
            assert!(outcome.ok, "{op:?}: {}", outcome.message);
            assert!(!outcome.message.contains("deleted for good"), "{}", outcome.message);
            assert_eq!(std::fs::read_to_string(dir.join("dst/a")).unwrap(), "new");
            assert_eq!(names(&dir.join("dst")), vec!["a"], "{op:?}: no staging name is left");
            assert_eq!(dir.join("src/a").exists(), op == Op::Copy, "a move takes the original, a copy leaves it");
            let Effect::Record(entry) = outcome.effect else { panic!("no undo entry") };
            assert!(entry.edits.iter().any(|e| matches!(e, Edit::Restore(_))), "the old file can be brought back");
            let (_, error) = crate::fs::history::apply(entry);
            assert_eq!(error, None);
            assert_eq!(std::fs::read_to_string(dir.join("dst/a")).unwrap(), "old", "{op:?}: undo brings the old file back");
            assert_eq!(std::fs::read_to_string(dir.join("src/a")).unwrap(), "new", "{op:?}: and the original is where it was");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_staging_name_that_is_already_taken_is_not_used_and_what_is_there_stays() {
        let dir = clash("replace_staging_taken");
        let taken = dir.join("dst").join(format!(".compression-new-{}-0", std::process::id()));
        std::fs::write(&taken, "somebody else's").unwrap();
        let (outcome, _) = run(JobKind::Copy, Op::Copy, pair(&dir), Decision::Replace);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dir.join("dst/a")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "somebody else's", "not written over, not removed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replacing_a_folder_swaps_the_whole_folder_and_an_undo_brings_the_old_one_back() {
        let dir = sandbox("replace_folder");
        std::fs::create_dir_all(dir.join("src/tree/inner")).unwrap();
        std::fs::create_dir_all(dir.join("dst/tree")).unwrap();
        std::fs::write(dir.join("src/tree/inner/new.txt"), "new").unwrap();
        std::fs::write(dir.join("dst/tree/old.txt"), "old").unwrap();
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("src/tree"), dir.join("dst/tree"))], Decision::Replace);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(names(&dir.join("dst")), vec!["tree"]);
        assert_eq!(names(&dir.join("dst/tree")), vec!["inner"], "the folder is the new one as a whole, not a merge");
        let Effect::Record(entry) = outcome.effect else { panic!("no undo entry") };
        let (_, error) = crate::fs::history::apply(entry);
        assert_eq!(error, None);
        assert_eq!(names(&dir.join("dst/tree")), vec!["old.txt"], "undo: the old folder is back, the new one gone");
        // A file by a folder and a folder by a file
        std::fs::write(dir.join("src/mix"), "file").unwrap();
        std::fs::create_dir_all(dir.join("dst/mix")).unwrap();
        std::fs::write(dir.join("dst/mix/in.txt"), "in").unwrap();
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("src/mix"), dir.join("dst/mix"))], Decision::Replace);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dir.join("dst/mix")).unwrap(), "file");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replacing_by_a_move_across_file_systems_copies_first_and_takes_the_original_away_after_the_swap() {
        let Some((root, shm)) = crate::fs::ops::tests::two_file_systems("replace_across") else { return };
        std::fs::create_dir_all(shm.join("dst")).unwrap();
        std::fs::write(root.join("a"), "new").unwrap();
        std::fs::write(shm.join("dst/a"), "old").unwrap();
        let (outcome, _) = run(JobKind::Move, Op::Move, vec![(root.join("a"), shm.join("dst/a"))], Decision::Replace);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(shm.join("dst/a")).unwrap(), "new");
        assert!(!root.join("a").exists(), "the original is gone once the copy has its place");
        assert_eq!(names(&shm.join("dst")), vec!["a"], "no staging name is left");
        let Effect::Record(entry) = outcome.effect else { panic!("no undo entry") };
        let (_, error) = crate::fs::history::apply(entry);
        assert_eq!(error, None);
        assert_eq!(std::fs::read_to_string(shm.join("dst/a")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "new");
        crate::fs::ops::tests::unlock_and_remove(&root);
        crate::fs::ops::tests::unlock_and_remove(&shm);
    }

    #[test]
    fn replacing_on_a_drive_without_a_trash_still_puts_the_old_item_into_the_home_trash() {
        let Some((root, shm)) = crate::fs::ops::tests::two_file_systems("replace_no_drive_trash") else { return };
        crate::fs::trash::tests_refuse::no_drive_trash_for(&shm);
        std::fs::create_dir_all(shm.join("dst")).unwrap();
        std::fs::write(root.join("a"), "new").unwrap();
        std::fs::write(shm.join("dst/a"), "old").unwrap();
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(root.join("a"), shm.join("dst/a"))], Decision::Replace);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(shm.join("dst/a")).unwrap(), "new");
        let Effect::Record(entry) = outcome.effect else { panic!("no undo entry") };
        let trashed = entry.edits.iter().find_map(|e| if let crate::fs::history::Edit::Restore(t) = e { Some(t.clone()) } else { None }).expect("the old item is in a trash");
        assert!(trashed.starts_with(crate::fs::trash::files_dir().unwrap()), "{}", trashed.display());
        let (_, error) = crate::fs::history::apply(entry);
        assert_eq!(error, None);
        assert_eq!(std::fs::read_to_string(shm.join("dst/a")).unwrap(), "old");
        crate::fs::ops::tests::unlock_and_remove(&root);
        crate::fs::ops::tests::unlock_and_remove(&shm);
    }

    #[test]
    fn replacing_by_a_move_across_file_systems_whose_original_cannot_be_removed_keeps_the_copy_and_the_old_item_in_the_trash() {
        use std::os::unix::fs::PermissionsExt;
        let Some((root, shm)) = crate::fs::ops::tests::two_file_systems("replace_across_left") else { return };
        std::fs::create_dir_all(root.join("tree/box")).unwrap();
        std::fs::write(root.join("tree/box/b.txt"), "new").unwrap();
        std::fs::set_permissions(root.join("tree/box"), std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::create_dir_all(shm.join("dst/tree")).unwrap();
        std::fs::write(shm.join("dst/tree/old.txt"), "old").unwrap();
        let (outcome, _) = run(JobKind::Move, Op::Move, vec![(root.join("tree"), shm.join("dst/tree"))], Decision::Replace);
        assert!(!outcome.ok, "{}", outcome.message);
        assert!(outcome.message.contains("could not be fully removed"), "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(shm.join("dst/tree/box/b.txt")).unwrap(), "new", "the copy has its place");
        assert_eq!(names(&shm.join("dst")), vec!["tree"]);
        let Effect::Record(entry) = outcome.effect else { panic!("no undo entry") };
        let (_, error) = crate::fs::history::apply(entry);
        assert_eq!(error, None);
        assert_eq!(names(&shm.join("dst/tree")), vec!["old.txt"], "an undo takes the copy away and brings the old folder back");
        crate::fs::ops::tests::unlock_and_remove(&root);
        crate::fs::ops::tests::unlock_and_remove(&shm);
    }

    #[test]
    fn a_new_item_that_cannot_be_made_leaves_the_old_one_whole_and_no_leftover() {
        if is_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = clash("replace_unreadable");
        std::fs::set_permissions(dir.join("src/a"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let (outcome, _) = run(JobKind::Copy, Op::Copy, pair(&dir), Decision::Replace);
        assert!(!outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dir.join("dst/a")).unwrap(), "old", "the old item was not touched");
        assert_eq!(names(&dir.join("dst")), vec!["a"], "and nothing of the new one is left");
        assert!(matches!(outcome.effect, Effect::None), "nothing was done, so there is nothing to undo");
        std::fs::set_permissions(dir.join("src/a"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn when_the_trash_cannot_take_the_old_item_it_stays_and_the_position_is_said_not_to_be_replaced() {
        for op in [Op::Copy, Op::Move] {
            let dir = clash(&format!("replace_no_trash_{op:?}"));
            crate::fs::trash::tests_refuse::refuse(&dir.join("dst/a"));
            let (outcome, asked) = run(JobKind::Copy, op, pair(&dir), Decision::Replace);
            assert_eq!(asked, 1);
            assert!(!outcome.ok && !outcome.cancelled, "{op:?}: {}", outcome.message);
            assert!(outcome.message.contains("Not replaced: the old item cannot go to the trash and would be lost for good"), "{}", outcome.message);
            assert!(!outcome.message.contains("deleted for good"), "nothing was deleted: {}", outcome.message);
            assert_eq!(std::fs::read_to_string(dir.join("dst/a")).unwrap(), "old", "{op:?}: the old item was not deleted");
            assert_eq!(names(&dir.join("dst")), vec!["a"], "{op:?}: no staging name is left");
            assert_eq!(std::fs::read_to_string(dir.join("src/a")).unwrap(), "new", "{op:?}: the original is where it was");
            assert!(matches!(outcome.effect, Effect::None));
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_cancel_after_the_new_item_is_complete_and_before_the_swap_removes_only_the_new_item() {
        for op in [Op::Copy, Op::Move] {
            let dir = clash(&format!("replace_cancel_{op:?}"));
            CANCELS.lock().unwrap().push(dir.join("dst/a"));
            let (outcome, _) = run(JobKind::Copy, op, pair(&dir), Decision::Replace);
            assert!(outcome.cancelled, "{op:?}: {}", outcome.message);
            assert_eq!(std::fs::read_to_string(dir.join("dst/a")).unwrap(), "old", "{op:?}");
            assert_eq!(names(&dir.join("dst")), vec!["a"], "{op:?}: no staging name is left");
            assert_eq!(std::fs::read_to_string(dir.join("src/a")).unwrap(), "new", "{op:?}: the original is where it was");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_cancel_in_the_middle_of_making_the_new_item_leaves_the_old_one_and_no_leftover() {
        let dir = sandbox("replace_cancel_midway");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("big"), vec![1u8; 200_000_000]).unwrap();
        std::fs::write(dir.join("dst/big"), "old").unwrap();
        let mut seen_midway = false;
        for _ in 0..5 {
            let mut e = engine();
            let pairs = vec![(dir.join("big"), dir.join("dst/big"))];
            let id = e.spawn(JobKind::Copy, "Copy".into(), String::new(), move |w| transfer(w, Op::Copy, pairs));
            let mut cancelled_midway = false;
            let mut events = Vec::new();
            for _ in 0..3000 {
                for event in e.poll() {
                    match event {
                        Event::Conflict(request) => request.answer(Decision::Replace, false),
                        other => events.push(other),
                    }
                }
                // The new item is being made under its staging name: now
                if !cancelled_midway && names(&dir.join("dst")).iter().any(|n| n.starts_with(".compression-new")) {
                    e.cancel(id);
                    cancelled_midway = true;
                }
                if !e.running() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            events.extend(e.poll());
            let outcome = events.into_iter().find_map(|ev| match ev {
                Event::Done { outcome, .. } => Some(outcome),
                _ => None,
            });
            let outcome = outcome.expect("the job never finished");
            if outcome.cancelled && cancelled_midway {
                seen_midway = true;
                assert_eq!(std::fs::read_to_string(dir.join("dst/big")).unwrap(), "old", "the old item is whole");
                assert_eq!(names(&dir.join("dst")), vec!["big"], "the half-made new item is gone");
                break;
            }
            // The copy was over before the cancel took hold: put the old item back and try again
            let _ = std::fs::remove_dir_all(dir.join("dst"));
            std::fs::create_dir_all(dir.join("dst")).unwrap();
            std::fs::write(dir.join("dst/big"), "old").unwrap();
        }
        assert!(seen_midway, "a cancel in the middle of the copy was never seen");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn on_a_server_nothing_is_replaced_and_nothing_goes_to_a_trash_nobody_asked_for() {
        let dir = sandbox("replace_server");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("server")).unwrap();
        std::fs::write(dir.join("src/a"), "new").unwrap();
        std::fs::write(dir.join("server/a"), "old").unwrap();
        crate::fs::network::declare_remote_for_tests(&dir.join("server"));
        let (outcome, asked) = run(JobKind::Copy, Op::Copy, vec![(dir.join("src/a"), dir.join("server/a"))], Decision::Replace);
        assert_eq!(asked, 1, "the person is asked about the name as before");
        assert!(!outcome.ok, "{}", outcome.message);
        assert!(outcome.message.contains("Not replaced: items on a server have no trash"), "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dir.join("server/a")).unwrap(), "old");
        assert_eq!(names(&dir.join("server")), vec!["a"], "nothing was staged on the server and no trash was made there");
        assert!(matches!(outcome.effect, Effect::None));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_taken_again_between_the_trash_and_the_swap_is_not_replaced_and_the_old_item_is_not_lost() {
        let dir = clash("replace_race");
        RACES.lock().unwrap().push(dir.join("dst/a"));
        let (outcome, _) = run(JobKind::Copy, Op::Copy, pair(&dir), Decision::Replace);
        assert!(!outcome.ok, "{}", outcome.message);
        assert!(outcome.message.contains("Not replaced"), "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dir.join("dst/a")).unwrap(), "intruder", "what appeared in the meantime is not touched");
        assert!(!names(&dir.join("dst")).iter().any(|n| n.starts_with(".compression-new")), "{:?}", names(&dir.join("dst")));
        // The old item was put back (under another name, the original one being taken) or is in the trash
        let back = names(&dir.join("dst")).into_iter().any(|n| n != "a" && std::fs::read_to_string(dir.join("dst").join(&n)).is_ok_and(|c| c == "old"));
        assert!(back || outcome.message.contains("in the trash"), "{:?} {}", names(&dir.join("dst")), outcome.message);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_a_copy_over_a_folder_left_out_is_told_by_the_name_it_would_have_and_not_by_the_staging_name() {
        if is_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = sandbox("replace_note");
        std::fs::create_dir_all(dir.join("src/tree")).unwrap();
        std::fs::create_dir_all(dir.join("dst/tree")).unwrap();
        std::fs::write(dir.join("src/tree/ok.txt"), "ok").unwrap();
        std::fs::write(dir.join("src/tree/locked.txt"), "no").unwrap();
        std::fs::set_permissions(dir.join("src/tree/locked.txt"), std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::write(dir.join("dst/tree/old.txt"), "old").unwrap();
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("src/tree"), dir.join("dst/tree"))], Decision::Replace);
        assert!(outcome.ok, "{}", outcome.message);
        assert!(outcome.message.contains("locked.txt"), "{}", outcome.message);
        assert!(!outcome.message.contains(".compression-new"), "{}", outcome.message);
        assert_eq!(names(&dir.join("dst/tree")), vec!["ok.txt"]);
        std::fs::set_permissions(dir.join("src/tree/locked.txt"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_replaced_item_that_went_to_the_trash_gets_no_warning_and_can_be_brought_back() {
        let dir = sandbox("replace_with_trash");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("a"), "new").unwrap();
        std::fs::write(dir.join("dst/a"), "old").unwrap();
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("a"), dir.join("dst/a"))], Decision::Replace);
        assert!(outcome.ok, "{}", outcome.message);
        assert!(!outcome.message.contains("deleted for good"), "{}", outcome.message);
        let Effect::Record(entry) = outcome.effect else { panic!("no undo entry") };
        assert!(entry.edits.iter().any(|e| matches!(e, Edit::Restore(_))));
        let (_, error) = crate::fs::history::apply(entry);
        assert_eq!(error, None);
        assert_eq!(std::fs::read_to_string(dir.join("dst/a")).unwrap(), "old", "undo brings the old file back");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Runs any job to its end; every conflict is answered with `decision`.
    fn run_job(job: impl FnOnce(&mut Worker) -> Outcome + Send + 'static, decision: Decision) -> (Outcome, usize) {
        let mut e = engine();
        e.spawn(JobKind::Extract, String::new(), String::new(), job);
        let (mut done, mut asked) = (None, 0);
        for _ in 0..500 {
            for event in e.poll() {
                match event {
                    Event::Conflict(request) => {
                        asked += 1;
                        request.answer(decision, false);
                    }
                    Event::Done { outcome, .. } => done = Some(outcome),
                    _ => {}
                }
            }
            if !e.running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        (done.expect("the job never finished"), asked)
    }

    /// `out/pack.zip` holding `top.txt` ("from the archive"), `data/new.txt` and `data/inner/deep.txt`; `dest` is a new folder.
    fn zip_with_tree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = sandbox(name);
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("data/inner")).unwrap();
        std::fs::write(src.join("top.txt"), "from the archive").unwrap();
        std::fs::write(src.join("data/new.txt"), "new").unwrap();
        std::fs::write(src.join("data/inner/deep.txt"), "deep").unwrap();
        let pack = dir.join("pack.zip");
        ops::compress(&[src.join("top.txt"), src.join("data")], &pack, ops::ArchiveFormat::Zip, &ops::Control::unlimited()).unwrap();
        let dest = dir.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        (dir, pack, dest)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn extracting_to_a_folder_without_clashes_places_everything_leaves_no_staging_folder_and_undoes_cleanly() {
        let (dir, pack, dest) = zip_with_tree("extract_free");
        let (p, d) = (pack.clone(), dest.clone());
        let (outcome, asked) = run_job(move |w| extract_into(w, vec![p], d), Decision::Skip);
        assert!(outcome.ok && asked == 0, "{}", outcome.message);
        assert!(outcome.message.contains("Extracted"), "{}", outcome.message);
        assert_eq!(names(&dest), vec!["data", "top.txt"], "no staging folder is left");
        assert_eq!(std::fs::read_to_string(dest.join("data/inner/deep.txt")).unwrap(), "deep");
        let Effect::Record(entry) = outcome.effect else { panic!("an extraction can be undone") };
        let (_, error) = crate::fs::history::apply(entry);
        assert_eq!(error, None);
        assert_eq!(names(&dest), Vec::<String>::new(), "the undo took the extracted items away again");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extracting_over_a_name_that_is_taken_asks_and_skipping_changes_nothing() {
        let (dir, pack, dest) = zip_with_tree("extract_skip");
        std::fs::write(dest.join("top.txt"), "mine").unwrap();
        let (p, d) = (pack.clone(), dest.clone());
        let (outcome, asked) = run_job(move |w| extract_into(w, vec![p], d), Decision::Skip);
        assert_eq!(asked, 1, "the taken name is a question");
        assert!(outcome.ok, "{}", outcome.message);
        assert!(outcome.message.contains("1 skipped"), "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dest.join("top.txt")).unwrap(), "mine", "not overwritten");
        assert_eq!(names(&dest), vec!["data", "top.txt"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extracting_over_a_name_that_is_taken_and_replacing_sends_the_old_one_to_the_trash_and_undo_brings_it_back() {
        let (dir, pack, dest) = zip_with_tree("extract_replace");
        std::fs::write(dest.join("top.txt"), "mine").unwrap();
        let (p, d) = (pack.clone(), dest.clone());
        let (outcome, asked) = run_job(move |w| extract_into(w, vec![p], d), Decision::Replace);
        assert_eq!(asked, 1);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dest.join("top.txt")).unwrap(), "from the archive");
        let Effect::Record(entry) = outcome.effect else { panic!("no undo entry") };
        assert!(entry.edits.iter().any(|e| matches!(e, Edit::Restore(_))), "the replaced file can be brought back");
        let (_, error) = crate::fs::history::apply(entry);
        assert_eq!(error, None);
        assert_eq!(std::fs::read_to_string(dest.join("top.txt")).unwrap(), "mine", "undo: the old file is back");
        assert_eq!(names(&dest), vec!["top.txt"], "and the rest of the extraction is gone");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extracting_over_a_folder_that_is_taken_can_merge_and_the_undo_removes_only_what_came_from_the_archive() {
        let (dir, pack, dest) = zip_with_tree("extract_merge");
        std::fs::create_dir_all(dest.join("data")).unwrap();
        std::fs::write(dest.join("data/old.txt"), "old").unwrap();
        let (p, d) = (pack.clone(), dest.clone());
        let (outcome, _) = run_job(move |w| extract_into(w, vec![p], d), Decision::Merge);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(names(&dest.join("data")), vec!["inner", "new.txt", "old.txt"]);
        let Effect::Record(entry) = outcome.effect else { panic!("no undo entry") };
        let (_, error) = crate::fs::history::apply(entry);
        assert_eq!(error, None);
        assert_eq!(names(&dest.join("data")), vec!["old.txt"], "what was there before stays");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extracting_and_keeping_both_never_overwrites() {
        let (dir, pack, dest) = zip_with_tree("extract_both");
        std::fs::write(dest.join("top.txt"), "mine").unwrap();
        let (p, d) = (pack.clone(), dest.clone());
        let (outcome, _) = run_job(move |w| extract_into(w, vec![p], d), Decision::KeepBoth);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dest.join("top.txt")).unwrap(), "mine");
        assert_eq!(names(&dest).len(), 3, "{:?}", names(&dest));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancelling_the_question_leaves_the_target_as_it_was_and_no_staging_folder() {
        let (dir, pack, dest) = zip_with_tree("extract_cancel");
        std::fs::write(dest.join("top.txt"), "mine").unwrap();
        let (p, d) = (pack.clone(), dest.clone());
        let (outcome, asked) = run_job(move |w| extract_into(w, vec![p], d), Decision::Cancel);
        assert_eq!(asked, 1);
        assert!(outcome.cancelled, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dest.join("top.txt")).unwrap(), "mine");
        assert!(!names(&dest).iter().any(|n| n.starts_with(".compression-extract")), "{:?}", names(&dest));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_archive_that_cannot_be_read_fails_and_leaves_nothing_in_the_target() {
        let dir = sandbox("extract_bad");
        let dest = dir.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dir.join("bad.zip"), "this is not a zip file").unwrap();
        let (bad, d) = (dir.join("bad.zip"), dest.clone());
        let (outcome, _) = run_job(move |w| extract_into(w, vec![bad], d), Decision::Skip);
        assert!(!outcome.ok && !outcome.cancelled, "{}", outcome.message);
        assert!(outcome.message.contains("bad.zip"), "{}", outcome.message);
        assert_eq!(names(&dest), Vec::<String>::new());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_move_to_a_free_name_works_and_records_the_way_back() {
        let dir = sandbox("move_free");
        std::fs::write(dir.join("a"), "from").unwrap();
        let (outcome, asked) = run(JobKind::Move, Op::Move, vec![(dir.join("a"), dir.join("b"))], Decision::Skip);
        assert!(outcome.ok && asked == 0, "{}", outcome.message);
        assert!(!dir.join("a").exists());
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "from");
        let Effect::Record(entry) = outcome.effect else { panic!("no undo entry") };
        let (_, error) = crate::fs::history::apply(entry);
        assert_eq!(error, None);
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "from");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_appears_after_the_conflict_check_is_not_replaced_by_a_move() {
        let dir = sandbox("move_race");
        std::fs::write(dir.join("a"), "from").unwrap();
        // Nothing is at the target when the move starts, so no conflict is asked
        RACES.lock().unwrap().push(dir.join("b"));
        let (outcome, asked) = run(JobKind::Move, Op::Move, vec![(dir.join("a"), dir.join("b"))], Decision::Replace);
        assert_eq!(asked, 0);
        assert!(!outcome.ok, "the move must fail, not replace");
        assert!(outcome.message.to_lowercase().contains("already exists"), "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "from", "the source stays");
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "intruder", "and the new file is untouched");
        assert!(matches!(outcome.effect, Effect::None), "nothing to undo");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_that_appears_after_the_conflict_check_is_not_replaced_by_a_move() {
        let dir = sandbox("move_race_dir");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a/x"), "x").unwrap();
        // The intruder is a file; a folder move onto a taken name must fail as well
        RACES.lock().unwrap().push(dir.join("b"));
        let (outcome, _) = run(JobKind::Move, Op::Move, vec![(dir.join("a"), dir.join("b"))], Decision::Replace);
        assert!(!outcome.ok);
        assert!(dir.join("a/x").exists());
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "intruder");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_confirmed_replace_still_replaces_and_the_old_file_goes_to_the_trash() {
        let dir = sandbox("move_replace");
        std::fs::write(dir.join("a"), "new").unwrap();
        std::fs::write(dir.join("b"), "old").unwrap();
        let (outcome, asked) = run(JobKind::Move, Op::Move, vec![(dir.join("a"), dir.join("b"))], Decision::Replace);
        assert_eq!(asked, 1);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "new");
        assert!(!dir.join("a").exists());
        // The way back restores both
        let Effect::Record(entry) = outcome.effect else { panic!("no undo entry") };
        let (_, error) = crate::fs::history::apply(entry);
        assert_eq!(error, None);
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "old");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeping_both_and_skipping_still_work_for_a_move() {
        let dir = sandbox("move_keep");
        for n in ["a", "b"] {
            std::fs::write(dir.join(n), n).unwrap();
        }
        std::fs::create_dir_all(dir.join("d")).unwrap();
        std::fs::write(dir.join("d/a"), "taken").unwrap();
        let (outcome, _) = run(JobKind::Move, Op::Move, vec![(dir.join("a"), dir.join("d/a"))], Decision::KeepBoth);
        assert!(outcome.ok);
        assert_eq!(std::fs::read_to_string(dir.join("d/a (copy)")).unwrap(), "a");
        let (outcome, _) = run(JobKind::Move, Op::Move, vec![(dir.join("b"), dir.join("d/a"))], Decision::Skip);
        assert!(outcome.ok);
        assert_eq!(std::fs::read_to_string(dir.join("d/a")).unwrap(), "taken");
        assert!(dir.join("b").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_noreplace_support_a_move_is_refused_with_its_own_message_and_nothing_changes() {
        let dir = sandbox("refused_move");
        crate::compat::tests::without_support_under(&dir);
        std::fs::create_dir_all(dir.join("src/folder/inner")).unwrap();
        std::fs::write(dir.join("src/file"), "data").unwrap();
        std::os::unix::fs::symlink("file", dir.join("src/link")).unwrap();
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        for name in ["file", "link", "folder"] {
            let (outcome, asked) = run(JobKind::Move, Op::Move, vec![(dir.join("src").join(name), dir.join("dst").join(name))], Decision::Skip);
            assert_eq!(asked, 0);
            assert!(!outcome.ok, "{name}");
            assert!(outcome.message.contains("does not support moving or renaming"), "{}", outcome.message);
            assert!(!outcome.message.to_lowercase().contains("already exists"), "{}", outcome.message);
            assert!(matches!(outcome.effect, Effect::None), "nothing to undo");
        }
        assert_eq!(std::fs::read_to_string(dir.join("src/file")).unwrap(), "data");
        assert!(dir.join("src/link").symlink_metadata().is_ok() && dir.join("src/folder/inner").is_dir());
        let mut left = std::fs::read_dir(dir.join("dst")).unwrap().count();
        assert_eq!(left, 0, "nothing arrived, and no reserved or private name");
        left = std::fs::read_dir(dir.join("src")).unwrap().count();
        assert_eq!(left, 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    use std::os::unix::fs::MetadataExt;

    /// Sets the access and modification time of `path` itself (not what a link points to).
    fn age(path: &Path, atime: i64, mtime: i64) {
        let c = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).unwrap();
        let times = [libc::timespec { tv_sec: atime, tv_nsec: 111_222_333 }, libc::timespec { tv_sec: mtime, tv_nsec: 123_456_789 }];
        // SAFETY: `c` is NUL-terminated and `times` holds two timespecs.
        assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) }, 0);
    }

    fn times_of(path: &Path) -> ((i64, i64), (i64, i64)) {
        let m = path.symlink_metadata().unwrap();
        ((m.atime(), m.atime_nsec()), (m.mtime(), m.mtime_nsec()))
    }

    const ATIME: i64 = 1_000_000_000;
    const MTIME: i64 = 1_100_000_000;

    #[test]
    fn a_copied_file_keeps_name_content_times_mode_and_user_attributes() {
        let dir = sandbox("copy_meta_file");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("photo.txt"), "content").unwrap();
        std::fs::set_permissions(dir.join("photo.txt"), std::os::unix::fs::PermissionsExt::from_mode(0o640)).unwrap();
        let tagged = crate::fs::xattr::set(&dir.join("photo.txt"), "user.xdg.tags", b"holiday").is_ok();
        age(&dir.join("photo.txt"), ATIME, MTIME);
        let (outcome, asked) = run(JobKind::Copy, Op::Copy, vec![(dir.join("photo.txt"), dir.join("dst/photo.txt"))], Decision::Skip);
        assert!(outcome.ok && asked == 0, "{}", outcome.message);
        let copy = dir.join("dst/photo.txt");
        // The times first: reading the copy sets its access time on a file system mounted with relatime
        assert_eq!(times_of(&copy), ((ATIME, 111_222_333), (MTIME, 123_456_789)), "access and modification time, to the nanosecond");
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "content");
        assert_eq!(copy.metadata().unwrap().mode() & 0o7777, 0o640);
        assert_ne!(copy.metadata().unwrap().ino(), dir.join("photo.txt").metadata().unwrap().ino());
        if tagged {
            assert_eq!(crate::fs::xattr::get(&copy, "user.xdg.tags").unwrap().as_deref(), Some(&b"holiday"[..]));
        }
        assert!(!outcome.message.contains("without their tags"), "{}", outcome.message);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_copied_folder_keeps_its_own_times_mode_and_attributes_and_links_stay_links() {
        let dir = sandbox("copy_meta_tree");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::create_dir_all(dir.join("tree/inner")).unwrap();
        std::fs::write(dir.join("tree/inner/f"), "f").unwrap();
        std::os::unix::fs::symlink("inner/f", dir.join("tree/relative")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/for/compression", dir.join("tree/dangling")).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.join("tree/outside")).unwrap();
        std::fs::set_permissions(dir.join("tree/inner"), std::os::unix::fs::PermissionsExt::from_mode(0o750)).unwrap();
        let tagged = crate::fs::xattr::set(&dir.join("tree/inner"), "user.xdg.tags", b"x").is_ok();
        // Times last, since making things in a folder changes its time
        for (name, a, m) in [("tree/inner/f", ATIME + 1, MTIME + 1), ("tree/inner", ATIME + 2, MTIME + 2), ("tree/relative", ATIME + 3, MTIME + 3), ("tree/dangling", ATIME + 4, MTIME + 4), ("tree", ATIME + 5, MTIME + 5)] {
            age(&dir.join(name), a, m);
        }
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("tree"), dir.join("dst/tree"))], Decision::Skip);
        assert!(outcome.ok, "{}", outcome.message);
        let t = dir.join("dst/tree");
        // All the times before anything is read (reading sets the access time on a file system mounted with relatime)
        let seen: Vec<_> = ["inner/f", "inner", "relative", "dangling", ""].iter().map(|name| (*name, times_of(&t.join(name)))).collect();
        for ((name, ((ta, _), (tm, tn))), (a, m)) in seen.into_iter().zip([(ATIME + 1, MTIME + 1), (ATIME + 2, MTIME + 2), (ATIME + 3, MTIME + 3), (ATIME + 4, MTIME + 4), (ATIME + 5, MTIME + 5)]) {
            assert_eq!((ta, tm, tn), (a, m, 123_456_789), "{name:?}");
        }
        // The job read the original folders to size its work: that did not set their access time either
        for (name, a) in [("tree", ATIME + 5), ("tree/inner", ATIME + 2)] {
            assert_eq!(times_of(&dir.join(name)).0.0, a, "the original {name:?}");
        }
        assert_eq!(t.join("inner").metadata().unwrap().mode() & 0o7777, 0o750);
        if tagged {
            assert_eq!(crate::fs::xattr::get(&t.join("inner"), "user.xdg.tags").unwrap().as_deref(), Some(&b"x"[..]));
        }
        // The links are links, with the same text: nothing behind them was copied or followed
        for (name, to) in [("relative", "inner/f"), ("dangling", "/nonexistent/for/compression"), ("outside", "/etc/hostname")] {
            assert!(t.join(name).symlink_metadata().unwrap().file_type().is_symlink(), "{name}");
            assert_eq!(std::fs::read_link(t.join(name)).unwrap(), Path::new(to));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_of_one_file_inside_a_copy_stay_names_of_one_file() {
        let dir = sandbox("copy_hardlinks");
        std::fs::create_dir_all(dir.join("tree")).unwrap();
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("tree/a"), "shared").unwrap();
        std::fs::hard_link(dir.join("tree/a"), dir.join("tree/b")).unwrap();
        std::fs::write(dir.join("tree/c"), "shared").unwrap();
        age(dir.join("tree/a").as_path(), ATIME, MTIME);
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("tree"), dir.join("dst/tree"))], Decision::Skip);
        assert!(outcome.ok, "{}", outcome.message);
        let t = dir.join("dst/tree");
        let (a, b, c) = (t.join("a").metadata().unwrap(), t.join("b").metadata().unwrap(), t.join("c").metadata().unwrap());
        assert_eq!((a.ino(), a.nlink()), (b.ino(), 2), "a and b are two names of one file");
        assert_ne!(a.ino(), c.ino(), "a different file with the same content stays a different file");
        assert_ne!(a.ino(), dir.join("tree/a").metadata().unwrap().ino(), "and none of it is the original");
        assert_eq!(std::fs::read_to_string(t.join("b")).unwrap(), "shared");
        assert_eq!(dir.join("tree/a").metadata().unwrap().nlink(), 2, "the original is as it was");
        // Progress still adds up
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_of_one_file_in_separate_items_of_one_job_are_linked_but_a_lone_name_is_not() {
        let dir = sandbox("copy_hardlinks_items");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("a"), "x").unwrap();
        std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
        // Both names in one job
        let pairs = vec![(dir.join("a"), dir.join("dst/a")), (dir.join("b"), dir.join("dst/b"))];
        let (outcome, _) = run(JobKind::Copy, Op::Copy, pairs, Decision::Skip);
        assert!(outcome.ok);
        assert_eq!(dir.join("dst/a").metadata().unwrap().ino(), dir.join("dst/b").metadata().unwrap().ino());
        // Only one of them: an independent file
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("a"), dir.join("dst/lone"))], Decision::Skip);
        assert!(outcome.ok);
        let lone = dir.join("dst/lone").metadata().unwrap();
        assert_eq!(lone.nlink(), 1);
        assert_ne!(lone.ino(), dir.join("a").metadata().unwrap().ino());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_that_is_taken_is_never_replaced_by_a_link_or_a_copy() {
        let dir = sandbox("copy_hardlinks_taken");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("a"), "x").unwrap();
        std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
        std::fs::write(dir.join("dst/b"), "keep me").unwrap();
        let pairs = vec![(dir.join("a"), dir.join("dst/a")), (dir.join("b"), dir.join("dst/b"))];
        let (outcome, asked) = run(JobKind::Copy, Op::Copy, pairs, Decision::Skip);
        assert_eq!(asked, 1);
        assert!(outcome.ok);
        assert_eq!(std::fs::read_to_string(dir.join("dst/b")).unwrap(), "keep me");
        assert_eq!(dir.join("dst/b").metadata().unwrap().nlink(), 1, "not made a name of the copy");
        // A name that appears after the conflict check: the copy fails and leaves the new file alone
        RACES.lock().unwrap().push(dir.join("dst/late"));
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("a"), dir.join("dst/late"))], Decision::Replace);
        assert!(!outcome.ok);
        assert_eq!(std::fs::read_to_string(dir.join("dst/late")).unwrap(), "intruder");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_target_that_keeps_no_attributes_gets_the_copy_and_the_result_says_what_is_missing() {
        let dir = sandbox("copy_no_attrs");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("a"), "x").unwrap();
        std::fs::write(dir.join("b"), "y").unwrap();
        std::fs::write(dir.join("plain"), "z").unwrap();
        let tagged = crate::fs::xattr::set(&dir.join("a"), "user.xdg.tags", b"t").is_ok() && crate::fs::xattr::set(&dir.join("b"), "user.xdg.tags", b"t").is_ok();
        crate::fs::xattr::tests::refuse_attributes_under(&dir.join("dst"));
        let pairs = vec![(dir.join("a"), dir.join("dst/a")), (dir.join("b"), dir.join("dst/b")), (dir.join("plain"), dir.join("dst/plain"))];
        let (outcome, _) = run(JobKind::Copy, Op::Copy, pairs, Decision::Skip);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dir.join("dst/a")).unwrap(), "x", "the copy itself is whole");
        if tagged {
            assert!(outcome.message.contains("2 items were copied without their tags and other attributes"), "{}", outcome.message);
            assert_eq!(crate::fs::xattr::get(&dir.join("dst/a"), "user.xdg.tags").unwrap(), None);
        }
        // A file without attributes is not reported
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("plain"), dir.join("dst/plain2"))], Decision::Skip);
        assert!(!outcome.message.contains("without their tags"), "{}", outcome.message);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `setfacl` with `args` on `path`, or `false` when the tool or the file system cannot (the ACL tests then say nothing).
    fn setfacl(args: &[&str], path: &Path) -> bool {
        std::process::Command::new("setfacl").args(args).arg(path).status().is_ok_and(|s| s.success())
    }

    /// The ACL of `path` as text with numeric ids, including the default ACL of a folder.
    fn getfacl(path: &Path) -> String {
        let out = std::process::Command::new("getfacl").args(["-n", "-c", "-p"]).arg(path).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn a_copied_file_keeps_its_access_acl_with_named_entries_and_the_owning_groups_own_permission() {
        let dir = sandbox("copy_acl_file");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("f"), "x").unwrap();
        std::fs::set_permissions(dir.join("f"), std::os::unix::fs::PermissionsExt::from_mode(0o640)).unwrap();
        if !setfacl(&["-m", "u:0:rw,g:0:r,g::r,m::rw"], &dir.join("f")) {
            return;
        }
        let before = getfacl(&dir.join("f"));
        assert!(before.contains("user:0:rw-") && before.contains("group:0:r--") && before.contains("mask::rw-"), "{before}");
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("f"), dir.join("dst/f"))], Decision::Skip);
        assert!(outcome.ok && !outcome.message.contains("without") && !outcome.message.contains("not copied"), "{}", outcome.message);
        assert_eq!(getfacl(&dir.join("dst/f")).replace("dst/f", "f"), before.replace("dst/f", "f"));
        assert!(getfacl(&dir.join("dst/f")).contains("group::r--"), "the owning group did not get the mask's permission");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_copied_folder_keeps_its_access_and_its_default_acl() {
        let dir = sandbox("copy_acl_folder");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::create_dir_all(dir.join("d/inner")).unwrap();
        if !setfacl(&["-m", "u:0:rwx"], &dir.join("d")) || !setfacl(&["-d", "-m", "u:0:rx,g:0:r"], &dir.join("d")) {
            return;
        }
        let before = getfacl(&dir.join("d"));
        assert!(before.contains("default:user:0:r-x"), "{before}");
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("d"), dir.join("dst/d"))], Decision::Skip);
        assert!(outcome.ok && !outcome.message.contains("without their tags"), "{}", outcome.message);
        assert_eq!(getfacl(&dir.join("dst/d")).replace("dst/d", "d"), before.replace("dst/d", "d"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_target_that_refuses_the_acl_gets_the_whole_copy_but_no_wider_permissions_than_the_source_gave() {
        let dir = sandbox("copy_acl_refused");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("f"), "x").unwrap();
        std::fs::set_permissions(dir.join("f"), std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        // The owning group may read; a named entry may write, so the mask is rw- and the mode shows rw- for the group
        if !setfacl(&["-m", "u:0:rw,g::r,m::rw"], &dir.join("f")) {
            return;
        }
        assert_eq!(dir.join("f").metadata().unwrap().mode() & 0o7777, 0o664, "the mode shows the mask");
        crate::fs::xattr::tests::refuse_attributes_under(&dir.join("dst"));
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("f"), dir.join("dst/f"))], Decision::Skip);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dir.join("dst/f")).unwrap(), "x", "the copy is whole");
        let mode = dir.join("dst/f").metadata().unwrap().mode() & 0o7777;
        // The old defect: the copy got 664, so the owning group could write where the source let it only read
        assert_ne!(mode, 0o664);
        assert_eq!(mode & 0o070, 0, "no group permission rather than a wider one: {mode:o}");
        assert_eq!(mode & 0o707, 0o604, "owner and others as in the source");
        // Said as what it is: the ACL is missing, and with it the group permissions; the tags are not blamed
        assert!(outcome.message.contains("1 item was copied without its access control list (ACL): the target does not keep ACLs"), "{}", outcome.message);
        assert!(outcome.message.contains("The group permissions of 1 item were not copied, because its ACL could not be"), "{}", outcome.message);
        assert!(!outcome.message.contains("tags"), "{}", outcome.message);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_that_is_taken_keeps_its_own_permissions_when_the_source_has_an_acl() {
        let dir = sandbox("copy_acl_taken");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("f"), "new").unwrap();
        std::fs::write(dir.join("dst/f"), "old").unwrap();
        std::fs::set_permissions(dir.join("dst/f"), std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        if !setfacl(&["-m", "u:0:rw"], &dir.join("f")) {
            return;
        }
        let (outcome, asked) = run(JobKind::Copy, Op::Copy, vec![(dir.join("f"), dir.join("dst/f"))], Decision::Skip);
        assert!(outcome.ok && asked == 1);
        assert_eq!(std::fs::read_to_string(dir.join("dst/f")).unwrap(), "old");
        assert_eq!(dir.join("dst/f").metadata().unwrap().mode() & 0o7777, 0o600);
        assert!(!getfacl(&dir.join("dst/f")).contains("user:0:"), "no ACL was put on a file that was not replaced");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_link_to_a_file_with_an_acl_is_copied_as_a_link_without_touching_either_acl() {
        let dir = sandbox("copy_acl_link");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::create_dir_all(dir.join("t")).unwrap();
        std::fs::write(dir.join("t/f"), "x").unwrap();
        std::os::unix::fs::symlink("f", dir.join("t/link")).unwrap();
        if !setfacl(&["-m", "u:0:rw"], &dir.join("t/f")) {
            return;
        }
        let before = getfacl(&dir.join("t/f"));
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("t"), dir.join("dst/t"))], Decision::Skip);
        assert!(outcome.ok && !outcome.message.contains("without their tags"), "{}", outcome.message);
        let link = dir.join("dst/t/link");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        // A link has no ACL of its own and none is made for it; the file it points to keeps what it has
        assert_eq!(crate::fs::xattr::get(&link, "system.posix_acl_access").ok().flatten(), None);
        assert_eq!(getfacl(&dir.join("t/f")), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_job_copies_a_file_with_holes_without_filling_them() {
        let dir = sandbox("copy_holey");
        let len = 48 << 20;
        {
            use std::os::unix::fs::FileExt;
            let file = std::fs::File::create(dir.join("image")).unwrap();
            file.set_len(len).unwrap();
            file.write_all_at(b"head", 0).unwrap();
            file.write_all_at(&[9u8; 5000], 20 << 20).unwrap();
            file.write_all_at(b"tail", len - 4).unwrap();
            file.sync_all().unwrap();
        }
        let source = dir.join("image").metadata().unwrap();
        let keeps_holes = source.blocks() * 512 < source.len();
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("image"), dir.join("copy"))], Decision::Skip);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read(dir.join("image")).unwrap(), std::fs::read(dir.join("copy")).unwrap());
        if keeps_holes {
            assert!(dir.join("copy").metadata().unwrap().blocks() * 512 < 1 << 20, "the copy was filled up");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_says_more_than_it_holds_is_copied_as_it_reads() {
        // Files in /sys claim a size (a page) and give less, and use no blocks: not a file with holes
        let source = Path::new("/sys/kernel/mm/transparent_hugepage/enabled");
        let Ok(text) = std::fs::read(source) else { return };
        let dir = sandbox("copy_sysfs");
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(source.to_path_buf(), dir.join("copy"))], Decision::Skip);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read(dir.join("copy")).unwrap(), text);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `(path relative to root, content)` of every file under `root`, sorted: the real state of a folder.
    fn listing(root: &Path) -> Vec<(String, String)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    out.push((path.strip_prefix(root).unwrap().display().to_string(), std::fs::read_to_string(&path).unwrap_or_default()));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// A second file system and a user who cannot remove what is locked; `None` (with a note) where there is none.
    fn two_file_systems(name: &str) -> Option<(PathBuf, PathBuf)> {
        use std::os::unix::fs::MetadataExt;
        let (from, to) = (sandbox(name), PathBuf::from("/dev/shm").join(format!("compression_engine_{name}_{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&to);
        std::fs::create_dir_all(&to).unwrap();
        if from.metadata().unwrap().dev() == to.metadata().unwrap().dev() || unsafe { libc::geteuid() } == 0 {
            std::fs::remove_dir_all(&from).unwrap();
            std::fs::remove_dir_all(&to).unwrap();
            return None;
        }
        Some((from, to))
    }

    fn mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_move_to_another_file_system_whose_original_cannot_be_removed_says_so_and_undoes_only_the_copy() {
        let Some((from, to)) = two_file_systems("move_stays") else { return };
        std::fs::create_dir_all(from.join("box/tree/sub")).unwrap();
        std::fs::write(from.join("box/tree/a.txt"), "one").unwrap();
        std::fs::write(from.join("box/tree/sub/b.txt"), "two").unwrap();
        std::fs::write(from.join("box/file.txt"), "three").unwrap();
        // The loose file cannot be unlinked from a locked folder; in the tree one file can go and one cannot
        mode(&from.join("box/tree/sub"), 0o555);
        mode(&from.join("box"), 0o555);
        let before = listing(&from);
        let pairs = vec![(from.join("box/file.txt"), to.join("file.txt")), (from.join("box/tree"), to.join("tree"))];
        let (outcome, _) = run(JobKind::Move, Op::Move, pairs, Decision::Skip);
        // What is there once the job is over, and again a moment later: nothing tries again, nothing is put back
        let after = listing(&from);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(listing(&from), after, "nothing touches the original after the job");
        let copies = listing(&to);
        assert!(!outcome.ok, "no silent partial success: {}", outcome.message);
        assert!(outcome.message.contains("could not be fully removed") && outcome.message.contains("file.txt") && outcome.message.contains("tree"), "{}", outcome.message);
        // The copies are whole
        assert_eq!(copies, vec![("file.txt".into(), "three".into()), ("tree/a.txt".into(), "one".into()), ("tree/sub/b.txt".into(), "two".into())]);
        // The originals: the loose file is there. Of the tree, the file in the locked folder is certainly there; the other
        // may or may not be (which goes first is up to the system), so the state is read and not assumed
        assert!(after.contains(&("box/file.txt".into(), "three".into())));
        assert!(after.contains(&("box/tree/sub/b.txt".into(), "two".into())));
        assert!(after.iter().all(|entry| before.contains(entry)), "the original lost files at most, it gained none");
        // The undo is of the copies, and says so
        let Effect::Record(entry) = outcome.effect else { panic!("no undo entry") };
        assert!(entry.label.starts_with("Incomplete move of"), "{}", entry.label);
        assert!(entry.edits.iter().all(|e| matches!(e, Edit::Trash(p) if p.starts_with(&to))) && entry.edits.len() == 2, "{:?}", entry.edits);
        // Doing it takes the copies away and does not touch the originals
        // The trash of that file system is made in its top folder
        let trash = PathBuf::from("/dev/shm").join(format!(".Trash-{}", unsafe { libc::geteuid() }));
        let trash_existed = trash.exists();
        let (_, error) = crate::fs::history::apply(entry);
        assert_eq!(error, None);
        let gone = !to.join("file.txt").exists() && !to.join("tree").exists();
        let originals_unchanged = listing(&from) == after;
        mode(&from.join("box"), 0o755);
        mode(&from.join("box/tree/sub"), 0o755);
        assert!(gone, "the copies are in the trash");
        assert!(originals_unchanged, "and the originals are as they were");
        std::fs::remove_dir_all(&from).unwrap();
        if !trash_existed {
            // What was trashed has the locked folder in it; only a trash this test made is removed
            let _ = std::process::Command::new("chmod").args(["-R", "u+rwx"]).arg(&trash).status();
            let _ = std::fs::remove_dir_all(&trash);
        }
        let _ = std::fs::remove_dir_all(&to);
    }

    #[test]
    fn the_room_a_copy_needs_counts_holes_only_where_they_stay() {
        use std::os::unix::fs::{FileExt, MetadataExt};
        let dir = sandbox("room");
        let len = 64u64 << 20;
        std::fs::create_dir_all(dir.join("tree")).unwrap();
        {
            let file = std::fs::File::create(dir.join("tree/image")).unwrap();
            file.set_len(len).unwrap();
            file.write_all_at(&[1u8; 8192], 1 << 20).unwrap();
            file.sync_all().unwrap();
        }
        std::fs::write(dir.join("tree/plain"), vec![2u8; 1 << 20]).unwrap();
        let image = dir.join("tree/image").metadata().unwrap();
        if image.blocks() * 512 >= image.len() {
            return;
        }
        let all = len + (1 << 20);
        // Within one file system the holes stay holes: what the data uses, and a plain file in full
        let same = room_for(&dir.join("tree"), Some(&dir), all);
        assert!(((1 << 20)..(2 << 20)).contains(&same), "{same}");
        assert_eq!(room_for(&dir.join("tree/plain"), Some(&dir), 1 << 20), 1 << 20);
        assert_eq!(room_for(&dir.join("tree/image"), Some(&dir), len), image.blocks() * 512);
        // Nowhere to look, or another file system: the whole size, as before
        assert_eq!(room_for(&dir.join("tree"), None, all), all);
        let other = Path::new("/dev/shm");
        if other.metadata().is_ok_and(|m| m.dev() != dir.metadata().unwrap().dev()) {
            assert_eq!(room_for(&dir.join("tree"), Some(other), all), all);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Runs a copy where `free` bytes are left; returns the outcome and the `needed` of every question about room.
    fn run_with_room(pairs: Vec<(PathBuf, PathBuf)>, free: u64) -> (Outcome, Vec<u64>) {
        let mut e = engine();
        e.spawn(JobKind::Copy, String::new(), String::new(), move |w| transfer_with(w, Op::Copy, pairs, move |_| Some(free)));
        let (mut done, mut asked) = (None, Vec::new());
        for _ in 0..500 {
            for event in e.poll() {
                match event {
                    Event::LowSpace(request) => {
                        asked.push(request.needed);
                        request.answer(false);
                    }
                    Event::Done { outcome, .. } => done = Some(outcome),
                    _ => {}
                }
            }
            if !e.running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        (done.expect("the job never finished"), asked)
    }

    #[test]
    fn a_copy_of_a_file_with_holes_is_not_warned_about_when_what_it_uses_fits() {
        use std::os::unix::fs::FileExt;
        let dir = sandbox("room_job");
        let len = 64u64 << 20;
        let file = std::fs::File::create(dir.join("image")).unwrap();
        file.set_len(len).unwrap();
        file.write_all_at(&[1u8; 8192], 1 << 20).unwrap();
        file.sync_all().unwrap();
        std::fs::write(dir.join("plain"), vec![2u8; 4 << 20]).unwrap();
        if std::os::unix::fs::MetadataExt::blocks(&dir.join("image").metadata().unwrap()) * 512 >= len {
            return;
        }
        // 10 MiB are left: the image uses next to nothing of that, the same size in plain data would not fit
        let (outcome, asked) = run_with_room(vec![(dir.join("image"), dir.join("copy"))], 10 << 20);
        assert!(outcome.ok && asked.is_empty(), "{} {asked:?}", outcome.message);
        let (outcome, asked) = run_with_room(vec![(dir.join("plain"), dir.join("copy2")), (dir.join("image"), dir.join("copy3"))], 4 << 20);
        assert!(!outcome.ok && asked.len() == 1 && asked[0] >= 4 << 20 && asked[0] < len, "the plain file is counted in full, the image by its data: {asked:?}");
        assert!(!dir.join("copy2").exists(), "and nothing was copied after the answer");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_job_copies_a_file_the_plain_way_when_the_file_system_cannot_tell_holes() {
        use std::os::unix::fs::FileExt;
        let dir = sandbox("copy_no_holes_info");
        let len = 12u64 << 20;
        {
            let file = std::fs::File::create(dir.join("image")).unwrap();
            file.set_len(len).unwrap();
            file.write_all_at(b"head", 0).unwrap();
            file.write_all_at(&[5u8; 4096], 6 << 20).unwrap();
            file.write_all_at(b"tail", len - 4).unwrap();
            file.sync_all().unwrap();
        }
        let seek_fails = ops::tests::make_seek_fail(&dir.join("image"), libc::EOPNOTSUPP);
        let (outcome, _) = run(JobKind::Copy, Op::Copy, vec![(dir.join("image"), dir.join("copy"))], Decision::Skip);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(dir.join("copy").metadata().unwrap().len(), len);
        assert_eq!(std::fs::read(dir.join("image")).unwrap(), std::fs::read(dir.join("copy")).unwrap());
        drop(seek_fails);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_move_tried_again_after_an_incomplete_one_asks_about_the_copy_that_is_there_and_nothing_goes_unasked() {
        let Some((from, to)) = two_file_systems("move_again") else { return };
        let trash = PathBuf::from("/dev/shm").join(format!(".Trash-{}", unsafe { libc::geteuid() }));
        let trash_existed = trash.exists();
        std::fs::create_dir_all(from.join("box/tree/sub")).unwrap();
        std::fs::write(from.join("box/tree/a.txt"), "one").unwrap();
        std::fs::write(from.join("box/tree/sub/b.txt"), "two").unwrap();
        mode(&from.join("box/tree/sub"), 0o555);
        mode(&from.join("box"), 0o555);
        let pair = || vec![(from.join("box/tree"), to.join("tree"))];
        let (first, _) = run(JobKind::Move, Op::Move, pair(), Decision::Skip);
        assert!(!first.ok && first.message.contains("could not be fully removed"), "{}", first.message);
        let copy_after_first = listing(&to);
        // The person unlocks what was in the way and tries again
        mode(&from.join("box"), 0o755);
        mode(&from.join("box/tree/sub"), 0o755);
        let unlocked = listing(&from);

        // Saying "skip" changes nothing at all
        let (second, asked) = run(JobKind::Move, Op::Move, pair(), Decision::Skip);
        assert_eq!(asked, 1, "the copy that is there is a name that is taken, and that is asked");
        assert!(second.ok, "{}", second.message);
        assert_eq!((listing(&from), listing(&to)), (unlocked.clone(), copy_after_first.clone()), "nothing was touched");

        // Saying "replace" moves what is left of the original over the copy and the copy goes to the trash
        let (third, asked) = run(JobKind::Move, Op::Move, pair(), Decision::Replace);
        assert_eq!(asked, 1);
        assert!(third.ok, "{}", third.message);
        assert!(!from.join("box/tree").exists(), "the original is gone now");
        assert_eq!(listing(&to), unlocked.iter().map(|(p, c)| (p.strip_prefix("box/").unwrap().to_owned(), c.clone())).collect::<Vec<_>>(), "and it is all there");
        let Effect::Record(entry) = third.effect else { panic!("no undo entry") };
        assert!(entry.edits.iter().any(|e| matches!(e, Edit::Restore(_))), "the replaced copy can be brought back");

        std::fs::remove_dir_all(&from).unwrap();
        if !trash_existed {
            let _ = std::process::Command::new("chmod").args(["-R", "u+rwx"]).arg(&trash).status();
            let _ = std::fs::remove_dir_all(&trash);
        }
        let _ = std::fs::remove_dir_all(&to);
    }

    fn wait(engine: &mut Engine) -> Vec<Event> {
        let mut all = Vec::new();
        for _ in 0..500 {
            all.extend(engine.poll());
            if !engine.running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        all
    }

    /// Runs `e` until its jobs end, handing every low-space question to `reply` (which may answer it or
    /// keep it); gives back what was asked as (needed, free).
    fn run_asking(e: &mut Engine, mut reply: impl FnMut(&mut Engine, SpaceRequest)) -> (Vec<(u64, u64)>, Vec<Event>) {
        let (mut asked, mut done) = (Vec::new(), Vec::new());
        for _ in 0..600 {
            for event in e.poll() {
                match event {
                    Event::LowSpace(request) => {
                        asked.push((request.needed, request.free));
                        reply(e, request);
                    }
                    other => done.push(other),
                }
            }
            if !e.running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        (asked, done)
    }

    /// Whether the two folders are on different file systems.
    fn same_device_not(a: &Path, b: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        matches!((a.metadata(), b.metadata()), (Ok(x), Ok(y)) if x.dev() != y.dev())
    }

    fn outcome_of(events: &[Event]) -> &Outcome {
        events.iter().find_map(|ev| if let Event::Done { outcome, .. } = ev { Some(outcome) } else { None }).expect("the job ended")
    }

    /// A folder with a 100-byte file and an empty target, and the pair a copy or move would work on.
    fn space_case(name: &str) -> (PathBuf, Vec<(PathBuf, PathBuf)>) {
        let dir = sandbox(name);
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("a"), vec![1u8; 100]).unwrap();
        let pairs = vec![(dir.join("a"), dir.join("dst/a"))];
        (dir, pairs)
    }

    fn mkfifo(path: &Path) {
        let c = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).unwrap();
        // SAFETY: `c` is a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// A tree whose second file cannot be read, so a copy fails after it has made the folder and the first file.
    /// `None` as root, who can read everything.
    fn tree_that_fails_midway(root: &Path) -> Option<()> {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return None;
        }
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), "first").unwrap();
        std::fs::write(root.join("sub/locked.txt"), "second").unwrap();
        std::fs::set_permissions(root.join("sub/locked.txt"), std::fs::Permissions::from_mode(0o000)).unwrap();
        Some(())
    }





    /// A job that runs until `release` is set, and records that it started.
    fn blocker(started: Arc<AtomicBool>, release: Arc<AtomicBool>) -> impl FnOnce(&mut Worker) -> Outcome + Send + 'static {
        move |_| {
            started.store(true, Relaxed);
            while !release.load(Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Outcome::done("done".into())
        }
    }

    fn pause_a_little(engine: &mut Engine) {
        for _ in 0..10 {
            engine.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn copy_reports_progress_and_an_undo() {
        let dir = sandbox("copy");
        std::fs::create_dir_all(dir.join("src/d")).unwrap();
        std::fs::write(dir.join("src/a.bin"), vec![7u8; 3_000_000]).unwrap();
        std::fs::write(dir.join("src/d/b.txt"), "b").unwrap();
        std::fs::create_dir_all(dir.join("dst")).unwrap();

        let mut e = engine();
        let pairs = vec![(dir.join("src/a.bin"), dir.join("dst/a.bin")), (dir.join("src/d"), dir.join("dst/d"))];
        let id = e.spawn(JobKind::Copy, "Copy".into(), String::new(), move |w| transfer(w, Op::Copy, pairs));
        let events = wait(&mut e);
        let Some(Event::Done { outcome, .. }) = events.into_iter().find(|ev| matches!(ev, Event::Done { .. })) else { panic!("no result") };
        assert!(outcome.ok, "{}", outcome.message);
        let job = e.jobs.iter().find(|j| j.id == id).unwrap();
        assert_eq!(job.shared.bytes_done.load(Relaxed), 3_000_001);
        assert_eq!(job.shared.files_done.load(Relaxed), 2);
        assert_eq!(std::fs::read_to_string(dir.join("dst/d/b.txt")).unwrap(), "b");
        let Effect::Record(entry) = outcome.effect else { panic!("no undo") };
        assert_eq!(entry.edits.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copies_keep_tags_and_other_user_attributes() {
        let dir = sandbox("xattr");
        std::fs::create_dir_all(dir.join("src/d")).unwrap();
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("src/a.txt"), "a").unwrap();
        if crate::fs::xattr::set(&dir.join("src/a.txt"), "user.xdg.tags", b"work").is_err() {
            // This file system keeps no attributes
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }
        crate::fs::xattr::set(&dir.join("src/d"), "user.xdg.tags", b"folder").unwrap();
        let mut e = engine();
        let pairs = vec![(dir.join("src/a.txt"), dir.join("dst/a.txt")), (dir.join("src/d"), dir.join("dst/d"))];
        e.spawn(JobKind::Copy, "Copy".into(), String::new(), move |w| transfer(w, Op::Copy, pairs));
        let events = wait(&mut e);
        assert!(events.iter().any(|ev| matches!(ev, Event::Done { outcome, .. } if outcome.ok)));
        assert_eq!(crate::fs::xattr::get(&dir.join("dst/a.txt"), "user.xdg.tags").unwrap().as_deref(), Some(&b"work"[..]));
        assert_eq!(crate::fs::xattr::get(&dir.join("dst/d"), "user.xdg.tags").unwrap().as_deref(), Some(&b"folder"[..]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn conflicts_wait_for_an_answer_and_remember_it() {
        let dir = sandbox("conflict");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        for n in ["a", "b"] {
            std::fs::write(dir.join(n), format!("new {n}")).unwrap();
            std::fs::write(dir.join("dst").join(n), format!("old {n}")).unwrap();
        }
        let mut e = engine();
        let pairs = vec![(dir.join("a"), dir.join("dst/a")), (dir.join("b"), dir.join("dst/b"))];
        e.spawn(JobKind::Copy, "Copy".into(), String::new(), move |w| transfer(w, Op::Copy, pairs));

        // The first name asks; "apply to all" answers the second without asking
        let mut asked = 0;
        for _ in 0..500 {
            for event in e.poll() {
                if let Event::Conflict(request) = event {
                    asked += 1;
                    assert!(!request.can_merge());
                    request.answer(Decision::KeepBoth, true);
                }
            }
            if !e.running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(asked, 1);
        assert_eq!(std::fs::read_to_string(dir.join("dst/a")).unwrap(), "old a");
        assert_eq!(std::fs::read_to_string(dir.join("dst/a (copy)")).unwrap(), "new a");
        assert_eq!(std::fs::read_to_string(dir.join("dst/b (copy)")).unwrap(), "new b");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_shortfall_is_what_is_missing_and_only_when_something_is() {
        assert_eq!(shortfall(100, 10), Some(90));
        assert_eq!(shortfall(11, 10), Some(1));
        assert_eq!(shortfall(10, 10), None);
        assert_eq!(shortfall(0, 0), None);
        assert_eq!(shortfall(0, 10), None);
        assert_eq!(shortfall(u64::MAX, 0), Some(u64::MAX));
        assert_eq!(shortfall(5, u64::MAX), None);
    }

    #[test]
    fn a_copy_that_does_not_fit_asks_and_cancel_writes_nothing() {
        let (dir, pairs) = space_case("space_cancel");
        let mut e = engine();
        e.spawn(JobKind::Copy, "Copy".into(), String::new(), move |w| transfer_with(w, Op::Copy, pairs, |_| Some(10)));
        let (asked, events) = run_asking(&mut e, |_, request| request.answer(false));
        assert_eq!(asked, vec![(100, 10)]);
        let outcome = outcome_of(&events);
        assert!(outcome.cancelled && !outcome.ok);
        assert!(dir.join("dst/a").symlink_metadata().is_err(), "nothing was written");
        assert!(matches!(outcome.effect, Effect::None));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn going_on_anyway_copies() {
        let (dir, pairs) = space_case("space_go");
        let mut e = engine();
        e.spawn(JobKind::Copy, "Copy".into(), String::new(), move |w| transfer_with(w, Op::Copy, pairs, |_| Some(10)));
        let (asked, events) = run_asking(&mut e, |_, request| {
            assert_eq!(request.missing(), 90);
            request.answer(true);
        });
        assert_eq!(asked.len(), 1);
        assert!(outcome_of(&events).ok);
        assert_eq!(std::fs::read(dir.join("dst/a")).unwrap().len(), 100);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn enough_room_or_unknown_room_asks_nothing() {
        for (name, free) in [("space_fits", Some(100)), ("space_unknown", None)] {
            let (dir, pairs) = space_case(name);
            let mut e = engine();
            e.spawn(JobKind::Copy, "Copy".into(), String::new(), move |w| transfer_with(w, Op::Copy, pairs, move |_| free));
            let (asked, events) = run_asking(&mut e, |_, request| request.answer(false));
            assert!(asked.is_empty(), "{name}");
            assert!(outcome_of(&events).ok, "{name}");
            assert!(dir.join("dst/a").exists(), "{name}");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_move_within_one_file_system_never_asks_but_one_across_does() {
        let (dir, pairs) = space_case("space_move");
        let mut e = engine();
        e.spawn(JobKind::Move, "Move".into(), String::new(), move |w| transfer_with(w, Op::Move, pairs, |_| Some(0)));
        let (asked, events) = run_asking(&mut e, |_, request| request.answer(false));
        assert!(asked.is_empty());
        assert!(outcome_of(&events).ok);
        assert!(dir.join("dst/a").exists() && !dir.join("a").exists());
        std::fs::remove_dir_all(&dir).unwrap();

        // Across file systems the bytes are written again (/dev/shm is the second file system here)
        let shm = PathBuf::from("/dev/shm").join(format!("compression_space_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&shm);
        let (dir, _) = space_case("space_across");
        if std::fs::create_dir_all(&shm).is_err() || !same_device_not(&dir, &shm) {
            let _ = std::fs::remove_dir_all(&shm);
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }
        let pairs = vec![(dir.join("a"), shm.join("a"))];
        let mut e = engine();
        e.spawn(JobKind::Move, "Move".into(), String::new(), move |w| transfer_with(w, Op::Move, pairs, |_| Some(10)));
        let (asked, events) = run_asking(&mut e, |_, request| request.answer(false));
        assert_eq!(asked, vec![(100, 10)]);
        assert!(outcome_of(&events).cancelled);
        assert!(dir.join("a").exists(), "the source is untouched");
        let _ = std::fs::remove_dir_all(&shm);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancelling_the_job_while_the_question_is_open_ends_it() {
        let (dir, pairs) = space_case("space_midway");
        let mut e = engine();
        let id = e.spawn(JobKind::Copy, "Copy".into(), String::new(), move |w| transfer_with(w, Op::Copy, pairs, |_| Some(10)));
        let mut held = Vec::new();
        let (asked, events) = run_asking(&mut e, |e, request| {
            // The question stays unanswered (and alive) while the job is cancelled
            e.cancel(id);
            held.push(request);
        });
        assert_eq!(asked.len(), 1);
        assert!(outcome_of(&events).cancelled);
        assert!(dir.join("dst/a").symlink_metadata().is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unpacking_needs_what_the_archives_hold_unpacked_per_folder() {
        use std::io::Write as _;
        let dir = sandbox("space_unpack");
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let mut archives = Vec::new();
        for (name, sizes) in [("one.zip", vec![300usize, 200]), ("two.zip", vec![50])] {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(dir.join(name)).unwrap());
            for (i, size) in sizes.iter().enumerate() {
                zip.start_file(format!("f{i}"), zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
                zip.write_all(&vec![0u8; *size]).unwrap();
            }
            zip.finish().unwrap();
            archives.push(dir.join(name));
        }
        let pairs: Vec<(PathBuf, PathBuf)> = archives.iter().map(|a| (a.clone(), dir.join("out"))).collect();
        let mut e = engine();
        e.spawn(JobKind::Extract, "Extract".into(), String::new(), move |w| match w.ensure_space_for_archives(&pairs, |_| Some(10)) {
            Ok(()) => Outcome::done(String::new()),
            Err(_) => Outcome::stopped(),
        });
        let (asked, events) = run_asking(&mut e, |_, request| request.answer(false));
        // One question for the folder, for both archives together: 300 + 200 + 50
        assert_eq!(asked, vec![(550, 10)]);
        assert!(outcome_of(&events).cancelled);

        // An archive that cannot be listed leaves its folder unwarned
        std::fs::write(dir.join("bad.zip"), b"not a zip").unwrap();
        let pairs = vec![(dir.join("bad.zip"), dir.join("out")), (dir.join("one.zip"), dir.join("out"))];
        let mut e = engine();
        e.spawn(JobKind::Extract, "Extract".into(), String::new(), move |w| match w.ensure_space_for_archives(&pairs, |_| Some(10)) {
            Ok(()) => Outcome::done(String::new()),
            Err(_) => Outcome::stopped(),
        });
        let (asked, events) = run_asking(&mut e, |_, request| request.answer(false));
        assert!(asked.is_empty());
        assert!(outcome_of(&events).ok);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancel_stops_a_job_and_pasting_beside_the_original_makes_a_copy() {
        let dir = sandbox("cancel");
        std::fs::write(dir.join("big"), vec![1u8; 50_000_000]).unwrap();
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        let mut e = engine();
        let pairs = vec![(dir.join("big"), dir.join("dst/big"))];
        let id = e.spawn(JobKind::Copy, "Copy".into(), String::new(), move |w| transfer(w, Op::Copy, pairs));
        e.toggle_pause(id);
        std::thread::sleep(Duration::from_millis(30));
        e.cancel(id);
        let events = wait(&mut e);
        assert!(events.iter().any(|ev| matches!(ev, Event::Done { outcome, .. } if outcome.cancelled)));
        assert!(!dir.join("dst/big").exists(), "a cancelled copy leaves nothing half-written");

        let pairs = vec![(dir.join("big"), dir.join("big"))];
        e.spawn(JobKind::Copy, "Copy".into(), String::new(), move |w| transfer(w, Op::Copy, pairs));
        wait(&mut e);
        assert!(dir.join("big (copy)").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }



    #[test]
    fn a_pipe_made_after_the_check_is_still_refused_by_the_copy_itself() {
        let dir = sandbox("fifo_late");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        mkfifo(&dir.join("src/pipe"));
        let mut e = engine();
        e.spawn(JobKind::Copy, String::new(), String::new(), {
            let (src, dst) = (dir.join("src/pipe"), dir.join("pipe"));
            move |w| match copy_tree(w, &src, &dst, None) {
                Ok(()) => Outcome::done(String::new()),
                Err(err) => Outcome::failed(err.to_string()),
            }
        });
        let Some(Event::Done { outcome, .. }) = wait(&mut e).into_iter().find(|ev| matches!(ev, Event::Done { .. })) else { panic!("the copy hung") };
        assert!(!outcome.ok && outcome.message.contains("named pipe"), "{}", outcome.message);
        assert!(!dir.join("pipe").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }





    #[test]
    fn a_strict_copy_still_leaves_no_half_copied_folder() {
        // What a move across file systems uses: the first failure ends it and the copy is taken back
        let dir = sandbox("strict_copy");
        if tree_that_fails_midway(&dir.join("src/tree")).is_none() {
            return;
        }
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        let mut e = engine();
        let (src, dst) = (dir.join("src/tree"), dir.join("dst/tree"));
        e.spawn(JobKind::Copy, String::new(), String::new(), move |w| match copy_new(w, &src, &dst, None) {
            Ok(()) => Outcome::done(String::new()),
            Err(err) => Outcome::failed(err.to_string()),
        });
        let Some(Event::Done { outcome, .. }) = wait(&mut e).into_iter().find(|ev| matches!(ev, Event::Done { .. })) else { panic!("no result") };
        assert!(!outcome.ok);
        assert!(!dir.join("dst/tree").exists(), "the half-copied folder was left behind");
        let _ = std::fs::set_permissions(dir.join("src/tree/sub/locked.txt"), std::os::unix::fs::PermissionsExt::from_mode(0o600));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_full_or_unwritable_target_or_a_cancel_ends_a_copy_for_good() {
        for errno in [libc::ENOSPC, libc::EDQUOT, libc::EROFS] {
            assert!(ends_the_copy(&io::Error::from_raw_os_error(errno)), "{errno}");
        }
        assert!(ends_the_copy(&cancelled()));
        for errno in [libc::EACCES, libc::EPERM, libc::ENOENT, libc::EIO, libc::EISDIR] {
            assert!(!ends_the_copy(&io::Error::from_raw_os_error(errno)), "{errno}");
        }
        assert!(!ends_the_copy(&io::Error::other("a named pipe")));
    }

    #[test]
    fn the_note_names_the_first_three_and_counts_the_rest() {
        let item = |path: &str| (PathBuf::from(path), "Permission denied".to_owned());
        assert_eq!(unreadable_note(&[item("/x/a/one.txt")]), "1 item could not be copied: a/one.txt (Permission denied)");
        let many: Vec<_> = ["/x/a/1", "/x/a/2", "/x/b/3", "/x/b/4", "/x/c/5"].into_iter().map(item).collect();
        assert_eq!(
            unreadable_note(&many),
            "5 items could not be copied: a/1 (Permission denied), a/2 (Permission denied), b/3 (Permission denied), and 2 more"
        );
    }





    #[test]
    fn cleaning_up_after_a_failed_copy_never_touches_a_target_that_was_already_there() {
        let dir = sandbox("existing_target");
        std::fs::create_dir_all(dir.join("src/folder")).unwrap();
        std::fs::write(dir.join("src/folder/a.txt"), "new").unwrap();
        std::fs::write(dir.join("src/file.txt"), "new").unwrap();
        std::fs::create_dir_all(dir.join("dst/folder")).unwrap();
        std::fs::write(dir.join("dst/folder/mine.txt"), "mine").unwrap();
        std::fs::write(dir.join("dst/file.txt"), "mine").unwrap();
        let mut e = engine();
        let (src, dst) = (dir.join("src"), dir.join("dst"));
        e.spawn(JobKind::Copy, String::new(), String::new(), move |w| {
            let errors = [copy_new(w, &src.join("folder"), &dst.join("folder"), None), copy_new(w, &src.join("file.txt"), &dst.join("file.txt"), None)];
            if errors.iter().all(|r| r.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::AlreadyExists)) { Outcome::done(String::new()) } else { Outcome::failed(format!("{errors:?}")) }
        });
        let Some(Event::Done { outcome, .. }) = wait(&mut e).into_iter().find(|ev| matches!(ev, Event::Done { .. })) else { panic!("no result") };
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(std::fs::read_to_string(dir.join("dst/folder/mine.txt")).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(dir.join("dst/file.txt")).unwrap(), "mine");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn move_merges_folders_and_records_the_way_back() {
        let dir = sandbox("merge");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("x/a")).unwrap();
        std::fs::write(dir.join("a/new.txt"), "n").unwrap();
        std::fs::write(dir.join("x/a/old.txt"), "o").unwrap();
        let mut e = engine();
        let pairs = vec![(dir.join("a"), dir.join("x/a"))];
        e.spawn(JobKind::Move, "Move".into(), String::new(), move |w| transfer(w, Op::Move, pairs));
        for _ in 0..500 {
            for event in e.poll() {
                if let Event::Conflict(request) = event {
                    assert!(request.can_merge());
                    request.answer(Decision::Merge, false);
                }
            }
            if !e.running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(dir.join("x/a/new.txt").exists() && dir.join("x/a/old.txt").exists());
        assert!(!dir.join("a").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jobs_for_one_disk_take_turns_and_other_disks_overlap() {
        let mut e = engine();
        let flags = || (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let ((s1, r1), (s2, r2), (s3, r3)) = (flags(), flags(), flags());
        let first = e.spawn_queued(JobKind::Copy, "one".into(), String::new(), Some(7), blocker(s1.clone(), r1.clone()));
        let second = e.spawn_queued(JobKind::Copy, "two".into(), String::new(), Some(7), blocker(s2.clone(), r2.clone()));
        let other_disk = e.spawn_queued(JobKind::Copy, "three".into(), String::new(), Some(8), blocker(s3.clone(), r3.clone()));
        pause_a_little(&mut e);
        let state = |e: &Engine, id| e.jobs.iter().find(|j| j.id == id).unwrap().state;
        assert_eq!(state(&e, first), JobState::Running);
        assert_eq!(state(&e, second), JobState::Waiting);
        assert!(!s2.load(Relaxed), "a queued job does not start");
        assert_eq!(state(&e, other_disk), JobState::Running, "another disk is not held up");
        assert!(s3.load(Relaxed));
        assert!(e.running());

        r1.store(true, Relaxed);
        for _ in 0..300 {
            e.poll();
            if s2.load(Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(s2.load(Relaxed), "the next job starts when the disk is free");
        assert_eq!(state(&e, second), JobState::Running);
        r2.store(true, Relaxed);
        r3.store(true, Relaxed);
        wait(&mut e);
        assert!(e.jobs.iter().all(|j| j.state == JobState::Done));
    }

    #[test]
    fn a_cancelled_queued_job_never_starts_and_start_now_jumps_the_queue() {
        let mut e = engine();
        let flags = || (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let ((s1, r1), (s2, r2), (s3, r3), (s4, r4)) = (flags(), flags(), flags(), flags());
        e.spawn_queued(JobKind::Copy, "holds the disk".into(), String::new(), Some(1), blocker(s1.clone(), r1.clone()));
        let doomed = e.spawn_queued(JobKind::Copy, "cancelled".into(), String::new(), Some(1), blocker(s2.clone(), r2.clone()));
        let eager = e.spawn_queued(JobKind::Copy, "start now".into(), String::new(), Some(1), blocker(s3.clone(), r3.clone()));
        let last = e.spawn_queued(JobKind::Copy, "last".into(), String::new(), Some(1), blocker(s4.clone(), r4.clone()));
        pause_a_little(&mut e);
        assert_eq!(e.queue_position(doomed), Some((0, 3)));
        assert_eq!(e.queue_position(last), Some((2, 3)));

        e.cancel(doomed);
        assert_eq!(e.jobs.iter().find(|j| j.id == doomed).unwrap().state, JobState::Cancelled);
        assert_eq!(e.queue_position(last), Some((1, 2)));
        // Order can be changed while waiting
        e.move_waiting(last, -1);
        assert_eq!(e.queue_position(last), Some((0, 2)));
        e.move_waiting(last, -1);
        assert_eq!(e.queue_position(last), Some((0, 2)), "already first");
        e.move_waiting(last, 1);

        e.start_now(eager);
        pause_a_little(&mut e);
        assert!(s3.load(Relaxed), "start now does not wait for the disk");
        assert!(!s2.load(Relaxed) && !s4.load(Relaxed));

        r1.store(true, Relaxed);
        r2.store(true, Relaxed);
        r3.store(true, Relaxed);
        r4.store(true, Relaxed);
        // The disk is still held by the job that jumped the queue, so "last" waits for it
        wait(&mut e);
        assert!(s4.load(Relaxed), "the rest of the queue still runs");
        assert!(!s2.load(Relaxed), "the cancelled job never ran");
    }

    #[test]
    fn only_copies_across_disks_and_writes_are_queued() {
        use std::os::unix::fs::MetadataExt;
        let dir = sandbox("queue_key");
        std::fs::write(dir.join("f"), "x").unwrap();
        let dev = std::fs::metadata(&dir).unwrap().dev();
        assert_eq!(queue_key(Op::Copy, &[dir.join("f")], &dir), Some(dev));
        // A move inside one file system is a rename
        assert_eq!(queue_key(Op::Move, &[dir.join("f")], &dir), None);
        assert_eq!(queue_key(Op::Copy, &[], &dir.join("missing")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn archive_work_reports_into_the_job_and_is_cut_short_by_a_cancel() {
        let mut engine = engine();
        let id = engine.spawn(JobKind::Extract, "unpacking".into(), String::new(), |w| {
            // The second archive of a job: the first one added 100 bytes, this one is 50
            let reported = w.archive_control(100, 50, |control| {
                control.report(30);
                let first = w.shared.bytes_done.load(Relaxed);
                control.report(80);
                // Never more than the archive's own size
                (first, w.shared.bytes_done.load(Relaxed))
            });
            assert_eq!(reported, (130, 150));
            // Then the long part: only a cancel ends it
            let ended = w.archive_control(0, 1, |control| loop {
                if let Err(err) = control.check() {
                    break err;
                }
                std::thread::sleep(Duration::from_millis(10));
            });
            assert!(is_cancel(&ended));
            Outcome::stopped()
        });
        assert!(engine.jobs[0].controllable(), "an archive job can be cancelled from the window");
        std::thread::sleep(Duration::from_millis(150));
        assert!(engine.running(), "it is still working");
        engine.cancel(id);
        let events = wait(&mut engine);
        assert!(!engine.running(), "the cancel ended the job");
        let Some(Event::Done { outcome, .. }) = events.into_iter().find(|e| matches!(e, Event::Done { .. })) else { panic!("no result") };
        assert!(outcome.cancelled && !outcome.ok);
        assert_eq!(outcome.message, tr("Cancelled"));
        assert_eq!(engine.jobs[0].state, JobState::Cancelled);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_named_with_the_reason_in_plain_words() {
        use std::os::unix::fs::PermissionsExt;
        let dir = sandbox("plain_words");
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("secret.txt"), "x").unwrap();
        std::fs::set_permissions(dir.join("secret.txt"), std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads anything, so there is nothing to see
        if std::fs::File::open(dir.join("secret.txt")).is_ok() {
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }
        let mut e = engine();
        let pairs = vec![(dir.join("secret.txt"), dir.join("dst/secret.txt"))];
        e.spawn(JobKind::Copy, "Copy".into(), String::new(), move |w| transfer(w, Op::Copy, pairs));
        let events = wait(&mut e);
        let outcome = outcome_of(&events);
        assert!(!outcome.ok);
        assert_eq!(outcome.message, "secret.txt: Permission denied");
        std::fs::set_permissions(dir.join("secret.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
