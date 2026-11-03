//! Whether a connected server still answers. Each mount folder is asked from time to time with a plain
//! `metadata` call on a thread of its own: that call is what blocks on a mount that has stopped
//! answering, and a blocked thread must not hold the window.
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

/// Whether the mount folder answers.
pub fn answers(local: &Path) -> bool {
    std::fs::metadata(local).is_ok()
}
