//! Recently used paths, so searches can look at files the user touched lately before walking
//! the whole disk. Combines Compression's own history with the desktop-wide `recently-used.xbel`.
use std::path::PathBuf;

const HISTORY_LIMIT: usize = 200;

fn history_file() -> Option<PathBuf> {
    // A test never reads or writes the history of whoever runs it: it names a file of its own, or has none
    #[cfg(test)]
    return TEST_FILE.with(|file| file.borrow().clone());
    #[cfg(not(test))]
    dirs::data_dir().map(|d| d.join("compression").join("recent.json"))
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_FILE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn shared_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("recently-used.xbel"))
}

/// The files the Recent view is made from: Compression's own history and the desktop-wide list.
pub fn sources() -> Vec<PathBuf> {
    history_file().into_iter().chain(shared_file()).collect()
}

static LAST_OWN_WRITE: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Whether Compression itself wrote the history a moment ago, as opposed to another window or app: the Recent
/// view stays put when its own click is what changed it.
pub fn written_here_lately() -> bool {
    LAST_OWN_WRITE.lock().ok().and_then(|t| *t).is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    path: PathBuf,
    /// Seconds since the epoch.
    at: u64,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Compression's own history, newest first.
fn load_history() -> Vec<Entry> {
    history_file().map(|file| load_history_from(&file)).unwrap_or_default()
}

/// The history in `file`. Files written before entries carried times get the file's modification time, one second
/// apart, so they still interleave sensibly with other apps.
fn load_history_from(file: &std::path::Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(file) else { return Vec::new() };
    if let Ok(entries) = serde_json::from_str::<Vec<Entry>>(&text) {
        return entries;
    }
    let base = std::fs::metadata(file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    serde_json::from_str::<Vec<PathBuf>>(&text)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, path)| Entry { path, at: base.saturating_sub(i as u64) })
        .collect()
}

/// What was used since the history was last written, and when the last of it was: a burst of navigation is
/// written once, after a pause (`flush_if_idle`), not once per step.
struct Waiting {
    entries: Vec<Entry>,
    last: Option<std::time::Instant>,
}

impl Waiting {
    fn push(&mut self, path: &std::path::Path, now: std::time::Instant) {
        self.entries.push(Entry { path: path.to_path_buf(), at: self::now() });
        self.last = Some(now);
    }

    /// Everything waiting, if the last of it is at least `delay` old at `now`.
    fn take_if_idle(&mut self, now: std::time::Instant, delay: std::time::Duration) -> Vec<Entry> {
        if self.last.is_some_and(|last| now.saturating_duration_since(last) >= delay) { self.take() } else { Vec::new() }
    }

    fn take(&mut self) -> Vec<Entry> {
        self.last = None;
        std::mem::take(&mut self.entries)
    }
}

#[cfg(not(test))]
static WAITING: std::sync::Mutex<Waiting> = std::sync::Mutex::new(Waiting { entries: Vec::new(), last: None });

// A test has its own queue (and its own file, `TEST_FILE`), so tests that run side by side cannot take each
// other's entries
#[cfg(test)]
thread_local! {
    static WAITING: std::cell::RefCell<Waiting> = const { std::cell::RefCell::new(Waiting { entries: Vec::new(), last: None }) };
}

fn with_waiting<R>(f: impl FnOnce(&mut Waiting) -> R) -> R {
    #[cfg(not(test))]
    return f(&mut WAITING.lock().unwrap_or_else(|e| e.into_inner()));
    #[cfg(test)]
    WAITING.with(|waiting| f(&mut waiting.borrow_mut()))
}

/// Remember a file that was opened or a folder that was entered (most recent first). Nothing is written now:
/// `flush_if_idle` writes after a pause, `flush` at once, and `load` sees what waits.
pub fn record(path: &std::path::Path) {
    with_waiting(|waiting| waiting.push(path, std::time::Instant::now()));
}

/// Whether something waits to be written.
pub fn has_waiting() -> bool {
    with_waiting(|waiting| !waiting.entries.is_empty())
}

/// Write what waits, once, when nothing was recorded for `delay`.
pub fn flush_if_idle(delay: std::time::Duration) {
    let due = with_waiting(|waiting| waiting.take_if_idle(std::time::Instant::now(), delay));
    write_waiting(due);
}

/// Write what waits, now: before the history is read, and when the window closes.
pub fn flush() {
    let all = with_waiting(Waiting::take);
    write_waiting(all);
}

fn write_waiting(entries: Vec<Entry>) {
    if entries.is_empty() {
        return;
    }
    if let Some(file) = history_file() {
        write_entries(&file, entries);
    }
}

/// Put `entries` (oldest first) in front of the history in `file`: one read, one write.
fn write_entries(file: &std::path::Path, entries: Vec<Entry>) {
    if let Ok(mut last) = LAST_OWN_WRITE.lock() {
        *last = Some(std::time::Instant::now());
    }
    let mut history = load_history_from(file);
    for entry in entries {
        history.retain(|e| e.path != entry.path);
        history.insert(0, entry);
    }
    history.truncate(HISTORY_LIMIT);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(&history) {
        let _ = crate::fs::atomic::write(file, json);
    }
}

/// Most recently used first, by when each path was last used in Compression or any other app.
pub fn load() -> Vec<PathBuf> {
    flush();
    let history = load_history().into_iter().map(|e| (e.at, e.path)).collect();
    let shared = dirs::data_dir()
        .and_then(|_| shared_file())
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|xml| parse_xbel(&xml))
        .unwrap_or_default();
    merge(history, shared)
}

/// Both sources newest first (Compression's wins a tie), each path once.
fn merge(mut all: Vec<(u64, PathBuf)>, shared: Vec<(u64, PathBuf)>) -> Vec<PathBuf> {
    all.extend(shared);
    all.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut seen = std::collections::HashSet::new();
    all.into_iter().map(|(_, p)| p).filter(|p| seen.insert(p.clone())).collect()
}

fn parse_xbel(xml: &str) -> Vec<(u64, PathBuf)> {
    xml.lines()
        .filter_map(|line| {
            let href = attr(line, "href")?;
            let path = percent_decode(href.strip_prefix("file://")?);
            let stamp = attr(line, "visited").or_else(|| attr(line, "modified")).map_or(0, iso_seconds);
            Some((stamp, PathBuf::from(path)))
        })
        .collect()
}

/// Seconds since the epoch of a UTC `2024-05-17T09:30:00.123Z` stamp; 0 if it can't be read.
fn iso_seconds(stamp: &str) -> u64 {
    let num = |range: std::ops::Range<usize>| stamp.get(range)?.parse::<i64>().ok();
    let parse = || {
        let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
        let (h, min, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
        // Days since 1970-01-01 from the civil date (Howard Hinnant's algorithm)
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
        u64::try_from(days * 86400 + h * 3600 + min * 60 + s).ok()
    };
    parse().unwrap_or(0)
}

fn attr<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("{name}=\"");
    let start = line.find(&marker)? + marker.len();
    let len = line[start..].find('"')?;
    Some(&line[start..start + len])
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{Duration, Instant};

    fn paths(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.path.to_str().unwrap()).collect()
    }

    #[test]
    fn what_waits_is_taken_once_and_only_after_a_pause_that_starts_over_with_every_step() {
        let (delay, start) = (Duration::from_millis(1500), Instant::now());
        let mut waiting = Waiting { entries: Vec::new(), last: None };
        assert!(waiting.take_if_idle(start + Duration::from_secs(60), delay).is_empty(), "nothing waits");
        waiting.push(std::path::Path::new("/a"), start);
        waiting.push(std::path::Path::new("/b"), start + Duration::from_millis(1000));
        // The pause is counted from the last step, not the first
        assert!(waiting.take_if_idle(start + Duration::from_millis(2000), delay).is_empty());
        waiting.push(std::path::Path::new("/c"), start + Duration::from_millis(2000));
        assert!(waiting.take_if_idle(start + Duration::from_millis(3000), delay).is_empty());
        let taken = waiting.take_if_idle(start + Duration::from_millis(3500), delay);
        assert_eq!(paths(&taken), ["/a", "/b", "/c"], "all of it, oldest first");
        assert!(waiting.take_if_idle(start + Duration::from_secs(60), delay).is_empty(), "taken once");
        waiting.push(std::path::Path::new("/d"), start + Duration::from_secs(61));
        assert_eq!(paths(&waiting.take()), ["/d"]);
        assert!(waiting.take().is_empty());
    }

    #[test]
    fn a_burst_is_written_in_one_go_in_front_of_the_history_newest_first_and_each_path_once() {
        let dir = std::env::temp_dir().join(format!("compression_recent_burst_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("recent.json");
        write_entries(&file, vec![Entry { path: "/old1".into(), at: 1 }, Entry { path: "/old2".into(), at: 2 }]);
        assert_eq!(paths(&load_history_from(&file)), ["/old2", "/old1"]);
        // "x" was used before and again in the burst: it comes once, in front
        let burst = ["/a", "/b", "/old1", "/c"].map(|p| Entry { path: p.into(), at: 10 }).into_iter().collect();
        write_entries(&file, burst);
        assert_eq!(paths(&load_history_from(&file)), ["/c", "/old1", "/b", "/a", "/old2"]);
        // The list stays within its limit
        write_entries(&file, (0..HISTORY_LIMIT + 5).map(|i| Entry { path: format!("/many{i}").into(), at: 20 }).collect());
        let history = load_history_from(&file);
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history[0].path, PathBuf::from(format!("/many{}", HISTORY_LIMIT + 4)));
        assert!(written_here_lately());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_writes_nothing_until_flushed_and_loading_flushes_first() {
        let dir = std::env::temp_dir().join(format!("compression_recent_flush_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("recent.json");
        TEST_FILE.with(|f| *f.borrow_mut() = Some(file.clone()));
        let paths_of = |h: Vec<Entry>| h.into_iter().map(|e| e.path.to_string_lossy().into_owned()).collect::<Vec<_>>();
        for p in ["/one", "/two", "/three"] {
            record(std::path::Path::new(p));
        }
        assert!(has_waiting());
        assert!(!file.exists(), "nothing is written by recording");
        // Not yet idle: still nothing
        flush_if_idle(Duration::from_secs(3600));
        assert!(!file.exists() && has_waiting());
        // Once idle, everything goes in one write
        flush_if_idle(Duration::ZERO);
        assert!(!has_waiting());
        assert_eq!(paths_of(load_history_from(&file)), ["/three", "/two", "/one"]);
        // Loading writes what waits first, so the Recent view has it
        record(std::path::Path::new("/four"));
        assert!(load().contains(&PathBuf::from("/four")));
        assert_eq!(paths_of(load_history_from(&file)), ["/four", "/three", "/two", "/one"]);
        // flush() at once
        record(std::path::Path::new("/five"));
        flush();
        assert_eq!(paths_of(load_history_from(&file)).first().map(String::as_str), Some("/five"));
        TEST_FILE.with(|f| *f.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamps_become_epoch_seconds() {
        assert_eq!(iso_seconds("1970-01-02T00:00:01.5Z"), 86401);
        assert_eq!(iso_seconds("2024-03-01T00:00:00Z"), 1709251200);
        assert_eq!(iso_seconds("junk"), 0);
    }

    #[test]
    fn a_write_by_compression_counts_as_ours_only_for_a_moment() {
        *LAST_OWN_WRITE.lock().unwrap() = None;
        assert!(!written_here_lately());
        *LAST_OWN_WRITE.lock().unwrap() = Some(std::time::Instant::now());
        assert!(written_here_lately());
        *LAST_OWN_WRITE.lock().unwrap() = Some(std::time::Instant::now() - std::time::Duration::from_secs(10));
        assert!(!written_here_lately());
        assert!(sources().iter().all(|s| s.is_absolute()));
    }

    #[test]
    fn newest_use_wins_across_sources() {
        let p = |s: &str| PathBuf::from(s);
        let merged = merge(vec![(100, p("/old-in-compression")), (500, p("/both"))], vec![(900, p("/in-gimp")), (400, p("/both"))]);
        assert_eq!(merged, [p("/in-gimp"), p("/both"), p("/old-in-compression")]);
    }
}
