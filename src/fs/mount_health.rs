//! Whether a connected server still answers. Each mount folder is asked from time to time with a plain
//! `statvfs` call (see `answers`) on a thread of its own: that call is what blocks on a mount that has
//! stopped answering, and a blocked thread must not hold the window.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long a mount is left alone after it answered.
pub const PROBE_EVERY: Duration = Duration::from_secs(15);
/// A question unanswered for this long makes the mount "slow".
pub const SLOW_AFTER: Duration = Duration::from_secs(3);

/// How a connected server is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// It answered the last question, or there is no reason to doubt it yet.
    Connected,
    /// The question asked last has gone unanswered for `SLOW_AFTER`: a slow line, or a mount that hangs.
    Slow,
    /// The mount is there but answered with an error.
    Broken,
}

#[derive(Default)]
struct Entry {
    /// When the question in flight was asked; none while the mount is not being asked.
    asking_since: Option<Instant>,
    /// When the last answer came, and whether it was a good one.
    answered: Option<(Instant, bool)>,
}

pub type Answer = (PathBuf, bool);

pub struct Probes {
    entries: HashMap<PathBuf, Entry>,
    tx: mpsc::Sender<Answer>,
    rx: mpsc::Receiver<Answer>,
}

impl Probes {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { entries: HashMap::new(), tx, rx }
    }

    /// Take in the answers that came, forget mounts that are gone and ask the ones that are due. `start`
    /// asks one mount on a new thread and sends the answer back on the sender it is given. A mount whose
    /// question is still in flight is never asked again, so a mount that hangs holds one thread, not one
    /// per round.
    pub fn tick(&mut self, mounts: &[PathBuf], now: Instant, start: impl Fn(PathBuf, mpsc::Sender<Answer>)) {
        while let Ok((local, ok)) = self.rx.try_recv() {
            if let Some(entry) = self.entries.get_mut(&local) {
                entry.asking_since = None;
                entry.answered = Some((now, ok));
            }
        }
        self.entries.retain(|local, entry| mounts.contains(local) || entry.asking_since.is_some());
        for local in mounts {
            let entry = self.entries.entry(local.clone()).or_default();
            let due = entry.asking_since.is_none() && entry.answered.is_none_or(|(at, _)| now.saturating_duration_since(at) >= PROBE_EVERY);
            if due {
                entry.asking_since = Some(now);
                start(local.clone(), self.tx.clone());
            }
        }
    }

    /// A question is in flight, so the window should look again soon.
    pub fn asking(&self) -> bool {
        self.entries.values().any(|e| e.asking_since.is_some())
    }

    /// How the mount at `local` is doing.
    pub fn health(&self, local: &Path, now: Instant) -> Health {
        let Some(entry) = self.entries.get(local) else { return Health::Connected };
        if entry.asking_since.is_some_and(|since| now.saturating_duration_since(since) >= SLOW_AFTER) {
            Health::Slow
        } else if matches!(entry.answered, Some((_, false))) {
            Health::Broken
        } else {
            Health::Connected
        }
    }
}

/// Whether the mount folder answers. The question is `statvfs`, not `stat`: the kernel keeps what it
/// last learned about the mount's top folder for a while (about 20 seconds with sshfs) and answers from
/// that, so a server that is gone would look well until it ran out. `statvfs` is never kept: it goes to the
/// program that serves the mount each time, which asks the server (sshfs does when the server knows the
/// request, as OpenSSH's does) or fails at once when the connection is gone. A mount that cannot answer
/// `statvfs` at all is asked with `stat` as before.
pub fn answers(local: &Path) -> bool {
    answers_with(local, statvfs)
}

fn answers_with(local: &Path, statvfs: impl Fn(&Path) -> std::io::Result<()>) -> bool {
    match statvfs(local) {
        Ok(()) => true,
        Err(err) if matches!(err.raw_os_error(), Some(libc::ENOSYS | libc::EOPNOTSUPP)) => std::fs::metadata(local).is_ok(),
        Err(_) => false,
    }
}

fn statvfs(local: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(local.as_os_str().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: a zeroed statvfs is a valid out-parameter, and the path is a terminated string that lives through the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// Whether the folder `dir` answers a plain look (`lstat`) within `wait`, asked on a thread of its own: on a mount that has
/// stopped answering that look blocks, and so would every change made there from the window. Any answer counts, an error
/// too: then the change itself says what is wrong. A thread left blocked ends when the mount answers or goes.
pub fn answers_within(dir: &Path, wait: Duration) -> bool {
    let (tx, rx) = mpsc::channel();
    let dir = dir.to_path_buf();
    let started = std::thread::Builder::new().name("compression-look".into()).spawn(move || {
        #[cfg(test)]
        tests::hang_if_asked_to(&dir);
        let _ = dir.symlink_metadata();
        let _ = tx.send(());
    });
    started.is_ok() && rx.recv_timeout(wait).is_ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Folders whose look hangs, in the tests that say so: a mount that has stopped answering.
    static HANGING: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

    pub(crate) fn hang_under(dir: &Path) {
        HANGING.lock().unwrap().push(dir.to_path_buf());
    }

    pub(crate) fn hang_if_asked_to(dir: &Path) {
        while HANGING.lock().unwrap().iter().any(|h| dir.starts_with(h)) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub(crate) fn answer_again(dir: &Path) {
        HANGING.lock().unwrap().retain(|h| h != dir);
    }

    #[test]
    fn a_folder_that_answers_does_so_in_time_and_one_that_hangs_does_not_hold_the_caller() {
        let dir = std::env::temp_dir().join(format!("compression_answers_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(answers_within(&dir, Duration::from_secs(2)));
        assert!(answers_within(&dir.join("not-there"), Duration::from_secs(2)), "an error is an answer");
        hang_under(&dir);
        let start = Instant::now();
        assert!(!answers_within(&dir, Duration::from_millis(200)));
        assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
        answer_again(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    use std::cell::RefCell;

    fn mount(name: &str) -> PathBuf {
        PathBuf::from(format!("/run/user/1/gvfs/{name}"))
    }

    /// A probe that records who was asked; the answer is sent by the test.
    struct Asked(RefCell<Vec<(PathBuf, mpsc::Sender<Answer>)>>);
    impl Asked {
        fn new() -> Self {
            Asked(RefCell::new(Vec::new()))
        }
        fn start(&self) -> impl Fn(PathBuf, mpsc::Sender<Answer>) + '_ {
            |local, tx| self.0.borrow_mut().push((local, tx))
        }
        fn count(&self) -> usize {
            self.0.borrow().len()
        }
        fn answer(&self, which: usize, ok: bool) {
            let asked = self.0.borrow();
            asked[which].1.send((asked[which].0.clone(), ok)).unwrap();
        }
    }

    #[test]
    fn a_good_answer_keeps_a_mount_connected_and_is_asked_again_after_fifteen_seconds() {
        let (mut probes, asked, a) = (Probes::new(), Asked::new(), mount("a"));
        let t0 = Instant::now();
        probes.tick(std::slice::from_ref(&a), t0, asked.start());
        assert_eq!(asked.count(), 1);
        assert_eq!(probes.health(&a, t0), Health::Connected);
        asked.answer(0, true);
        let t1 = t0 + Duration::from_secs(1);
        probes.tick(std::slice::from_ref(&a), t1, asked.start());
        assert!(!probes.asking());
        assert_eq!(probes.health(&a, t1), Health::Connected);
        // Not yet due, then due
        probes.tick(std::slice::from_ref(&a), t1 + PROBE_EVERY - Duration::from_secs(1), asked.start());
        assert_eq!(asked.count(), 1);
        probes.tick(std::slice::from_ref(&a), t1 + PROBE_EVERY, asked.start());
        assert_eq!(asked.count(), 2);
    }

    #[test]
    fn a_question_unanswered_for_three_seconds_is_slow_and_a_late_answer_makes_it_connected_again() {
        let (mut probes, asked, a) = (Probes::new(), Asked::new(), mount("a"));
        let t0 = Instant::now();
        probes.tick(std::slice::from_ref(&a), t0, asked.start());
        assert_eq!(probes.health(&a, t0 + SLOW_AFTER - Duration::from_millis(1)), Health::Connected);
        assert_eq!(probes.health(&a, t0 + SLOW_AFTER), Health::Slow);
        assert!(probes.asking());
        asked.answer(0, true);
        let t1 = t0 + Duration::from_secs(10);
        probes.tick(std::slice::from_ref(&a), t1, asked.start());
        assert_eq!(probes.health(&a, t1), Health::Connected);
    }

    #[test]
    fn a_mount_that_never_answers_is_asked_once_not_every_round() {
        let (mut probes, asked, a) = (Probes::new(), Asked::new(), mount("a"));
        let t0 = Instant::now();
        for second in 0..120 {
            probes.tick(std::slice::from_ref(&a), t0 + Duration::from_secs(second), asked.start());
        }
        assert_eq!(asked.count(), 1);
        assert_eq!(probes.health(&a, t0 + Duration::from_secs(120)), Health::Slow);
    }

    #[test]
    fn an_error_makes_a_mount_broken_until_it_answers_well() {
        let (mut probes, asked, a) = (Probes::new(), Asked::new(), mount("a"));
        let t0 = Instant::now();
        probes.tick(std::slice::from_ref(&a), t0, asked.start());
        asked.answer(0, false);
        let t1 = t0 + Duration::from_secs(1);
        probes.tick(std::slice::from_ref(&a), t1, asked.start());
        assert_eq!(probes.health(&a, t1), Health::Broken);
        // Asked again later; while that question is young the mount stays broken, then slow
        let t2 = t1 + PROBE_EVERY;
        probes.tick(std::slice::from_ref(&a), t2, asked.start());
        assert_eq!(asked.count(), 2);
        assert_eq!(probes.health(&a, t2 + Duration::from_secs(1)), Health::Broken);
        assert_eq!(probes.health(&a, t2 + SLOW_AFTER), Health::Slow);
        asked.answer(1, true);
        let t3 = t2 + Duration::from_secs(4);
        probes.tick(std::slice::from_ref(&a), t3, asked.start());
        assert_eq!(probes.health(&a, t3), Health::Connected);
    }

    #[test]
    fn each_mount_has_its_own_state_and_a_mount_that_went_away_is_forgotten() {
        let (mut probes, asked, a, b) = (Probes::new(), Asked::new(), mount("a"), mount("b"));
        let t0 = Instant::now();
        probes.tick(&[a.clone(), b.clone()], t0, asked.start());
        assert_eq!(asked.count(), 2);
        asked.answer(0, false);
        asked.answer(1, true);
        let t1 = t0 + Duration::from_secs(1);
        probes.tick(&[a.clone(), b.clone()], t1, asked.start());
        assert_eq!((probes.health(&a, t1), probes.health(&b, t1)), (Health::Broken, Health::Connected));
        // `a` is gone: forgotten, and a mount that comes back starts afresh
        probes.tick(std::slice::from_ref(&b), t1, asked.start());
        assert_eq!(probes.health(&a, t1), Health::Connected);
        probes.tick(&[a.clone(), b], t1, asked.start());
        assert_eq!(asked.count(), 3);
    }

    #[test]
    fn an_answer_from_a_mount_that_is_not_listed_any_more_is_dropped() {
        let (mut probes, asked, a) = (Probes::new(), Asked::new(), mount("a"));
        let t0 = Instant::now();
        probes.tick(std::slice::from_ref(&a), t0, asked.start());
        probes.tick(&[], t0 + Duration::from_secs(1), asked.start());
        asked.answer(0, true);
        probes.tick(&[], t0 + Duration::from_secs(2), asked.start());
        assert!(probes.entries.is_empty());
    }

    #[test]
    fn a_folder_answers_and_a_missing_one_does_not() {
        let dir = std::env::temp_dir().join(format!("compression_health_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(answers(&dir));
        assert!(!answers(&dir.join("gone")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_mount_is_asked_with_statvfs_which_the_kernel_does_not_answer_from_its_cache() {
        let dir = std::env::temp_dir();
        let enotconn = |_: &Path| Err(std::io::Error::from_raw_os_error(libc::ENOTCONN));
        // The folder itself still answers stat (the cache of a dead sshfs does), but statvfs finds the server gone
        assert!(std::fs::metadata(&dir).is_ok());
        assert!(!answers_with(&dir, enotconn));
        assert!(!answers_with(&dir, |_| Err(std::io::Error::from_raw_os_error(libc::EIO))));
        // A server that answers is well
        assert!(answers_with(&dir, |_| Ok(())));
    }

    #[test]
    fn a_mount_that_cannot_answer_statvfs_is_asked_with_stat_instead() {
        let dir = std::env::temp_dir();
        let missing = dir.join("compression_health_missing_folder");
        for unsupported in [libc::ENOSYS, libc::EOPNOTSUPP] {
            let answer = |_: &Path| Err(std::io::Error::from_raw_os_error(unsupported));
            assert!(answers_with(&dir, answer), "{unsupported}");
            assert!(!answers_with(&missing, answer), "{unsupported}");
        }
    }

    #[test]
    fn the_real_question_is_yes_for_a_folder_and_no_for_a_missing_one_or_a_name_with_a_nul() {
        let dir = std::env::temp_dir();
        assert!(answers(&dir));
        assert!(!answers(&dir.join("compression_health_missing_folder")));
        assert!(!answers(Path::new("/tmp/a\0b")));
    }
}
