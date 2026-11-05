//! Recursive filename search running on a background thread, streaming results in batches.
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use eframe::egui;

use super::file_item::FileItem;
use super::glob;
use super::scanner::TypeFilter;
use super::recent;

pub const MAX_RESULTS: usize = 400;
const MAX_DEPTH: usize = 8;
const BATCH: usize = 24;
/// Larger files are not read when searching inside files.
const CONTENT_LIMIT: u64 = 1024 * 1024;

/// Where a search looks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SearchScope {
    /// The folder that was open when the search began.
    ThisFolder,
    /// The whole home folder.
    #[default]
    Everywhere,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub match_case: bool,
    /// Also find text files that contain the words, not only files named so.
    pub contents: bool,
}

/// Does the text file at `path` contain `needle`? Binary and large files never do.
fn file_contains(path: &std::path::Path, needle: &str, match_case: bool) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let mut bytes = Vec::new();
    if file.by_ref().take(CONTENT_LIMIT).read_to_end(&mut bytes).is_err() || bytes[..bytes.len().min(4096)].contains(&0) {
        return false;
    }
    let text = String::from_utf8_lossy(&bytes);
    if match_case { text.contains(needle) } else { text.to_lowercase().contains(&needle.to_lowercase()) }
}

/// Whether the walk may go into `path`. A folder on a server is skipped unless previews on servers
/// are switched on: reading it over the network is slow and costs data, and a mount that has
/// stopped answering would hold the search thread for good. A search that was started inside a
/// server's folder (`root_is_remote`) is the person's own choice and goes on there.
/// `allowed` says whether a path may be read without being asked to (`network::previews_allowed`).
fn may_enter(path: &std::path::Path, root_is_remote: bool, allowed: impl Fn(&std::path::Path) -> bool) -> bool {
    root_is_remote || allowed(path)
}

pub struct Search {
    generation: Arc<AtomicU64>,
    rx: Option<mpsc::Receiver<Vec<FileItem>>>,
    finished: bool,
    /// The search stopped because it had `MAX_RESULTS` results; a new one starts with a flag of its own.
    limit_hit: Arc<AtomicBool>,
}

impl Search {
    pub fn new() -> Self {
        Self { generation: Arc::new(AtomicU64::new(0)), rx: None, finished: true, limit_hit: Arc::new(AtomicBool::new(false)) }
    }

    /// Cancel any running search.
    pub fn cancel(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.rx = None;
        self.finished = true;
        self.limit_hit = Arc::new(AtomicBool::new(false));
    }

    /// The results stop at `MAX_RESULTS` because the search did, not because there were no more to find.
    pub fn limit_reached(&self) -> bool {
        self.limit_hit.load(Ordering::Relaxed)
    }

    /// A search is still walking folders.
    pub fn is_running(&self) -> bool {
        self.rx.is_some() && !self.finished
    }

    pub fn start(&mut self, ctx: &egui::Context, root: PathBuf, query: String, show_hidden: bool, filter: TypeFilter, options: SearchOptions) {
        let my_gen = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let generation = self.generation.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.finished = false;
        self.limit_hit = Arc::new(AtomicBool::new(false));
        let limit_hit = self.limit_hit.clone();
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            let cancelled = || generation.load(Ordering::Relaxed) != my_gen;
            let matches = |name: &str| glob::matches(&query, name, options.match_case);
            let mut seen: HashSet<PathBuf> = HashSet::new();
            let mut found = 0usize;
            let root_is_remote = super::network::is_remote(&root);
            let enter = |path: &std::path::Path| may_enter(path, root_is_remote, super::network::previews_allowed);

            // Stage 1: files the user opened lately answer instantly, before any directory walk
            let recents: Vec<FileItem> = recent::load()
                .into_iter()
                .filter(|p| p.starts_with(&root))
                .filter(|p| enter(p))
                .filter(|p| p.file_name().is_some_and(|n| matches(&n.to_string_lossy())))
                .filter(|p| show_hidden || !p.strip_prefix(&root).is_ok_and(|r| r.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'))))
                .filter_map(|p| FileItem::from_path(&p).filter(|_| p.exists()))
                .filter(|i| filter.accepts(i))
                .take(MAX_RESULTS)
                .collect();
            if !recents.is_empty() {
                seen.extend(recents.iter().map(|i| i.path.clone()));
                found += recents.len();
                let _ = tx.send(recents);
                ctx.request_repaint();
            }

            // Stage 2: breadth-first walk, most recently modified folders first
            let mut queue = VecDeque::from([(root, 0usize)]);
            let mut batch = Vec::new();

            while let Some((dir, depth)) = queue.pop_front() {
                let Ok(entries) = std::fs::read_dir(&dir) else { continue };
                let mut subdirs: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
                for entry in entries.flatten() {
                    if cancelled() {
                        return;
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !show_hidden && name.starts_with('.') {
                        continue;
                    }
                    let path = entry.path();
                    let is_real_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    // Not even looked at: asking a mount that does not answer would hold this thread
                    if is_real_dir && !enter(&path) {
                        continue;
                    }
                    if is_real_dir && depth < MAX_DEPTH {
                        let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                        subdirs.push((mtime, path.clone()));
                    }
                    let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
                    let hit = matches(&name)
                        || options.contents && is_file && !seen.contains(&path)
                            && entry.metadata().is_ok_and(|m| m.len() <= CONTENT_LIMIT)
                            && file_contains(&path, &query, options.match_case);
                    if hit && !seen.contains(&path) {
                        if let Some(item) = FileItem::from_path(&path).filter(|i| filter.accepts(i)) {
                            batch.push(item);
                            found += 1;
                        }
                        if batch.len() >= BATCH {
                            let _ = tx.send(std::mem::take(&mut batch));
                            ctx.request_repaint();
                        }
                        if found >= MAX_RESULTS {
                            limit_hit.store(true, Ordering::Relaxed);
                            let _ = tx.send(batch);
                            ctx.request_repaint();
                            return;
                        }
                    }
                }
                subdirs.sort_by_key(|a| std::cmp::Reverse(a.0));
                queue.extend(subdirs.into_iter().map(|(_, p)| (p, depth + 1)));
            }
            let _ = tx.send(batch);
            ctx.request_repaint();
        });
    }

    pub fn poll(&mut self) -> Vec<FileItem> {
        let mut out = Vec::new();
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(mut batch) => out.append(&mut batch),
                    Err(mpsc::TryRecvError::Empty) => break,
                    // The worker is done and dropped its sender
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.finished = true;
                        break;
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_on_servers_are_entered_only_when_allowed_or_searched_from_inside() {
        let remote = |p: &std::path::Path| p.starts_with("/srv/nas");
        let allowed = |p: &std::path::Path| !remote(p);
        assert!(may_enter(std::path::Path::new("/home/me/docs"), false, allowed));
        assert!(!may_enter(std::path::Path::new("/srv/nas/share"), false, allowed));
        // Previews on servers switched on: everything may be entered
        assert!(may_enter(std::path::Path::new("/srv/nas/share"), false, |_| true));
        // A search that begins in a server's folder goes on there
        assert!(may_enter(std::path::Path::new("/srv/nas/share/sub"), true, allowed));
    }

    /// The paths a search from `root` finds for "needle" (waits for it to finish).
    fn found(root: &std::path::Path) -> Vec<PathBuf> {
        let mut search = Search::new();
        search.start(&egui::Context::default(), root.to_path_buf(), "needle".into(), true, TypeFilter::All, SearchOptions::default());
        let mut paths = Vec::new();
        let started = std::time::Instant::now();
        loop {
            paths.extend(search.poll().into_iter().map(|i| i.path));
            if !search.is_running() || started.elapsed() > std::time::Duration::from_secs(10) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        paths.sort();
        paths
    }

    #[test]
    fn a_search_skips_folders_on_servers_but_finds_local_files() {
        let dir = std::env::temp_dir().join(format!("compression_search_remote_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["local", "nas", "nas/deep"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        for file in ["needle-top.txt", "local/needle-a.txt", "nas/needle-b.txt", "nas/deep/needle-c.txt"] {
            std::fs::write(dir.join(file), "x").unwrap();
        }
        super::super::network::declare_remote_for_tests(&dir.join("nas"));
        // Previews on servers are off by default, and nothing in the tests turns them on
        let names = |paths: Vec<PathBuf>| paths.iter().map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(names(found(&dir)), ["local/needle-a.txt", "needle-top.txt"]);
        // Started inside the server's folder, the search is the person's own choice
        let inside: Vec<_> = found(&dir.join("nas")).into_iter().map(|p| p.strip_prefix(dir.join("nas")).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(inside, ["deep/needle-c.txt", "needle-b.txt"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn contents_match_text_files_only() {
        let dir = std::env::temp_dir().join(format!("compression_search_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "Remember the Milk").unwrap();
        std::fs::write(dir.join("blob.bin"), b"milk\0\0\0").unwrap();
        assert!(file_contains(&dir.join("note.txt"), "milk", false));
        assert!(!file_contains(&dir.join("note.txt"), "milk", true));
        assert!(file_contains(&dir.join("note.txt"), "Milk", true));
        assert!(!file_contains(&dir.join("blob.bin"), "milk", false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Runs a search to the end; gives back how many results it sent and whether it hit its limit.
    fn run_to_the_end(search: &mut Search, root: &std::path::Path) -> (usize, bool) {
        search.start(&egui::Context::default(), root.to_path_buf(), "needle".into(), true, TypeFilter::All, SearchOptions::default());
        let mut count = 0;
        let started = std::time::Instant::now();
        loop {
            count += search.poll().len();
            if !search.is_running() || started.elapsed() > std::time::Duration::from_secs(10) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        (count, search.limit_reached())
    }

    #[test]
    fn a_search_says_when_it_stopped_at_its_limit() {
        let many = std::env::temp_dir().join(format!("compression_search_many_{}", std::process::id()));
        let few = std::env::temp_dir().join(format!("compression_search_few_{}", std::process::id()));
        for dir in [&many, &few] {
            let _ = std::fs::remove_dir_all(dir);
            std::fs::create_dir_all(dir).unwrap();
        }
        for n in 0..450 {
            std::fs::write(many.join(format!("needle-{n:03}.txt")), "x").unwrap();
        }
        for n in 0..10 {
            std::fs::write(few.join(format!("needle-{n}.txt")), "x").unwrap();
        }
        let mut search = Search::new();
        assert!(!search.limit_reached(), "nothing was searched yet");
        assert_eq!(run_to_the_end(&mut search, &many), (MAX_RESULTS, true));
        // The next search starts without the flag, and a search with few results never sets it
        assert_eq!(run_to_the_end(&mut search, &few), (10, false));
        // Cancelling forgets it too
        assert_eq!(run_to_the_end(&mut search, &many), (MAX_RESULTS, true));
        search.cancel();
        assert!(!search.limit_reached());
        std::fs::remove_dir_all(&many).unwrap();
        std::fs::remove_dir_all(&few).unwrap();
    }
}
