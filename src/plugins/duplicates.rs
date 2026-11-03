//! Duplicate finder: groups files with identical contents so the extra copies can go to the trash.
//!
//! The scan runs as a file operation (progress, pause and cancel in the operations list) and sends
//! its groups to the plugin's window. To keep the reading low it goes in steps: files of a size seen
//! once are dropped, then the first and last 4 KiB are compared with a quick hash, and only what is
//! still alike is read whole and hashed with SHA-256. Files with the same inode (hard links) are one
//! file, links are not followed and empty files are ignored.
//!
//! Nothing is selected for removal until you choose. One copy of every group always stays, and
//! before a file is trashed it must still have the size and modification time it had in the scan.
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::SystemTime;

use eframe::egui;

use super::checksum::Sha256;
use super::progress::{OnWorker, Progress};
use super::{Job as AppJob, MenuItem, Plugin, Reaction};
use crate::fs::engine::{Effect, Outcome};
use crate::fs::file_item::{FileItem, format_bytes};
use crate::fs::history::{Edit, UndoEntry};
use crate::fs::trash;
use crate::i18n::{tr, trf, trn};
use crate::ui::icons;
use crate::ui::material;
use crate::ui::theme::SerpantinumTheme;

/// How much of the start and the end of a file the quick comparison reads.
const SAMPLE: usize = 4096;
/// Files are hashed on this many threads.
const THREADS: usize = 2;
const ROW_HEIGHT: f32 = 30.0;

/// One of the equal files, as the scan saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Names the file has; trashing one of several frees nothing.
    pub links: u64,
}

impl Member {
    /// Whether the file on disk is still the one the scan saw.
    fn unchanged(&self) -> bool {
        std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.is_file() && m.len() == self.size && m.modified().ok() == self.modified)
    }
}

/// Files with identical contents.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub size: u64,
    pub files: Vec<Member>,
}

impl Group {
    /// What removing all but one copy gives back.
    pub fn reclaimable(&self) -> u64 {
        let freeing = self.files.iter().filter(|m| m.links <= 1).count() as u64;
        self.size * (self.files.len() as u64 - 1).min(freeing)
    }
}

/// Which copy of each group stays when a rule picks the ones to remove.
#[derive(Debug, Clone, PartialEq)]
pub enum Keep {
    Newest,
    Oldest,
    ShortestPath,
    /// The copy inside this folder; groups without one keep their shortest path.
    InFolder(PathBuf),
}

fn shortest(group: &Group) -> usize {
    (0..group.files.len()).min_by_key(|&i| (group.files[i].path.as_os_str().len(), group.files[i].path.clone())).unwrap_or(0)
}

/// The index of the copy `rule` keeps.
pub fn keeper(group: &Group, rule: &Keep) -> usize {
    let time = |i: usize| group.files[i].modified.unwrap_or(SystemTime::UNIX_EPOCH);
    // Ties go to the shorter path, then to the lower one
    let tie = |i: usize| (group.files[i].path.as_os_str().len(), group.files[i].path.clone());
    let by_time = |newest: bool| {
        (0..group.files.len())
            .min_by(|&a, &b| {
                let order = if newest { time(b).cmp(&time(a)) } else { time(a).cmp(&time(b)) };
                order.then_with(|| tie(a).cmp(&tie(b)))
            })
            .unwrap_or(0)
    };
    match rule {
        Keep::Newest => by_time(true),
        Keep::Oldest => by_time(false),
        Keep::ShortestPath => shortest(group),
        Keep::InFolder(dir) => group.files.iter().position(|m| m.path.starts_with(dir)).unwrap_or_else(|| shortest(group)),
    }
}

struct Candidate {
    path: PathBuf,
    dev: u64,
    ino: u64,
    size: u64,
    modified: Option<SystemTime>,
    links: u64,
}

/// Every regular file below `roots`, grouped by size. Links are not followed, empty files are
/// skipped, and a file reached twice (hard links, overlapping roots) counts once.
fn walk(roots: &[PathBuf], progress: &dyn Progress) -> io::Result<HashMap<u64, Vec<Candidate>>> {
    let mut by_size: HashMap<u64, Vec<Candidate>> = HashMap::new();
    // Each folder with the device of the root it was reached from: mounts inside a tree are left out
    let mut stack: Vec<(PathBuf, Option<u64>)> = roots.iter().map(|r| (r.clone(), std::fs::metadata(r).ok().map(|m| m.dev()))).collect();
    while let Some((dir, root_dev)) = stack.pop() {
        progress.checkpoint()?;
        progress.current(&dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dir.display().to_string()));
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if root_dev.is_none() || entry.metadata().ok().map(|m| m.dev()) == root_dev {
                    stack.push((entry.path(), root_dev));
                }
            } else if kind.is_file()
                && let Ok(meta) = entry.metadata()
                && meta.len() > 0
            {
                by_size.entry(meta.len()).or_default().push(Candidate { path: entry.path(), dev: meta.dev(), ino: meta.ino(), size: meta.len(), modified: meta.modified().ok(), links: meta.nlink() });
            }
        }
    }
    // A file reached twice (hard links, overlapping roots) stays under its lowest path
    for files in by_size.values_mut() {
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut seen: HashSet<(u64, u64)> = HashSet::new();
        files.retain(|c| seen.insert((c.dev, c.ino)));
    }
    by_size.retain(|_, files| files.len() > 1);
    Ok(by_size)
}

/// FNV-1a, enough to tell files apart cheaply.
fn quick_hash(bytes: &[u8], mut hash: u64) -> u64 {
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The first and last `SAMPLE` bytes of the file, hashed.
fn sample_hash(path: &Path, size: u64) -> io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; SAMPLE];
    let mut hash = 0xcbf29ce484222325;
    let n = read_full(&mut file, &mut buffer)?;
    hash = quick_hash(&buffer[..n], hash);
    if size > 2 * SAMPLE as u64 {
        file.seek(SeekFrom::Start(size - SAMPLE as u64))?;
        let n = read_full(&mut file, &mut buffer)?;
        hash = quick_hash(&buffer[..n], hash);
    }
    Ok(hash)
}

fn read_full(file: &mut std::fs::File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

fn full_hash(path: &Path, progress: &dyn Progress) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        progress.checkpoint()?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            return Ok(hasher.finish());
        }
        hasher.update(&buffer[..n]);
        progress.bytes(n as u64);
    }
}

/// Runs `f` on every item with `THREADS` threads, in order of the list. A file that can't be read
/// gives `None`; cancelling gives an error.
fn par_map<T: Sync, R: Send>(items: &[T], progress: &dyn Progress, f: impl Fn(&T) -> io::Result<R> + Sync) -> io::Result<Vec<Option<R>>> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Option<R>)>> = Mutex::new(Vec::with_capacity(items.len()));
    let stopped: Mutex<Option<io::Error>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..THREADS.min(items.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= items.len() || stopped.lock().is_ok_and(|s| s.is_some()) {
                        return;
                    }
                    if let Err(e) = progress.checkpoint() {
                        *stopped.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                        return;
                    }
                    let value = match f(&items[i]) {
                        Ok(v) => Some(v),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                            *stopped.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                            return;
                        }
                        Err(_) => None,
                    };
                    progress.file_done();
                    results.lock().unwrap_or_else(|e| e.into_inner()).push((i, value));
                }
            });
        }
    });
    if let Some(e) = stopped.into_inner().unwrap_or_else(|e| e.into_inner()) {
        return Err(e);
    }
    let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    results.sort_by_key(|(i, _)| *i);
    Ok(results.into_iter().map(|(_, v)| v).collect())
}

/// Finds the files below `roots` that have identical contents.
pub fn find(roots: &[PathBuf], progress: &dyn Progress) -> io::Result<Vec<Group>> {
    let by_size = walk(roots, progress)?;
    // Reading in inode order keeps a spinning disk from jumping around
    let mut candidates: Vec<Candidate> = by_size.into_values().flatten().collect();
    candidates.sort_by_key(|c| (c.dev, c.ino));

    progress.plan(candidates.len() as u64, 0);
    progress.current(tr("Comparing the start and end of files"));
    let samples = par_map(&candidates, progress, |c| sample_hash(&c.path, c.size))?;
    let mut by_sample: HashMap<(u64, u64), Vec<Candidate>> = HashMap::new();
    for (candidate, sample) in candidates.into_iter().zip(samples) {
        if let Some(sample) = sample {
            by_sample.entry((candidate.size, sample)).or_default().push(candidate);
        }
    }
    let mut candidates: Vec<Candidate> = by_sample.into_values().filter(|files| files.len() > 1).flatten().collect();
    candidates.sort_by_key(|c| (c.dev, c.ino));

    progress.plan(candidates.len() as u64, candidates.iter().map(|c| c.size).sum());
    progress.current(tr("Reading the files that still look alike"));
    let hashes = par_map(&candidates, progress, |c| full_hash(&c.path, progress))?;
    let mut by_hash: HashMap<(u64, String), Vec<Member>> = HashMap::new();
    for (candidate, hash) in candidates.into_iter().zip(hashes) {
        if let Some(hash) = hash {
            by_hash.entry((candidate.size, hash)).or_default().push(Member { path: candidate.path, size: candidate.size, modified: candidate.modified, links: candidate.links });
        }
    }
    let mut groups: Vec<Group> = by_hash
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|((size, _), mut files)| {
            files.sort_by(|a, b| a.path.cmp(&b.path));
            Group { size, files }
        })
        .collect();
    groups.sort_by(|a, b| b.reclaimable().cmp(&a.reclaimable()).then_with(|| a.files[0].path.cmp(&b.files[0].path)));
    Ok(groups)
}

/// Copies to remove from one group, with the ones that stay.
#[derive(Debug, Clone, PartialEq)]
struct Planned {
    keepers: Vec<Member>,
    doomed: Vec<Member>,
}

/// Trashes what `plans` lists. A group is skipped when none of its keepers is still the file the
/// scan saw, and a copy that changed since is left alone. Returns `(original, trashed)` pairs and
/// how many were skipped, and whether it was stopped early. `remove` does the trashing.
fn trash_planned(plans: &[Planned], progress: &dyn Progress, remove: impl Fn(&Path) -> io::Result<PathBuf>) -> (Vec<(PathBuf, PathBuf)>, usize, bool) {
    let (mut trashed, mut skipped) = (Vec::new(), 0);
    for plan in plans {
        if !plan.keepers.iter().any(Member::unchanged) {
            skipped += plan.doomed.len();
            continue;
        }
        for member in &plan.doomed {
            if progress.checkpoint().is_err() {
                return (trashed, skipped, true);
            }
            progress.current(&member.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            if !member.unchanged() {
                skipped += 1;
                continue;
            }
            match remove(&member.path) {
                Ok(to) => trashed.push((member.path.clone(), to)),
                Err(_) => skipped += 1,
            }
            progress.file_done();
        }
    }
    (trashed, skipped, false)
}

enum Msg {
    Found { generation: u64, result: Result<Vec<Group>, String>, cancelled: bool },
    Trashed { generation: u64, paths: Vec<PathBuf> },
}

#[derive(PartialEq)]
enum Phase {
    Scanning,
    Done,
}

enum Row {
    Group(usize),
    File(usize, usize),
}

struct View {
    generation: u64,
    roots: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
    phase: Phase,
    groups: Vec<Group>,
    selected: HashSet<PathBuf>,
    rows: Vec<Row>,
    error: Option<String>,
    cancelled: bool,
    /// A trash job for this scan is running; its result has not come back yet.
    removing: bool,
}

impl View {
    fn rebuild_rows(&mut self) {
        self.rows = self.groups.iter().enumerate().flat_map(|(g, group)| std::iter::once(Row::Group(g)).chain((0..group.files.len()).map(move |f| Row::File(g, f)))).collect();
    }

    /// Selects every copy but the one `rule` keeps, in each group.
    fn select_all_but(&mut self, rule: &Keep) {
        self.selected.clear();
        for group in &self.groups {
            let stays = keeper(group, rule);
            self.selected.extend(group.files.iter().enumerate().filter(|(i, _)| *i != stays).map(|(_, m)| m.path.clone()));
        }
    }

    /// Whether `member` of group `g` may be ticked: one copy must stay unticked.
    fn can_select(&self, g: usize, member: &Member) -> bool {
        self.selected.contains(&member.path) || self.groups[g].files.iter().filter(|m| m.path != member.path && !self.selected.contains(&m.path)).count() > 0
    }

    /// The removal the ticks stand for. Even if every copy of a group were ticked, one stays.
    fn plan(&self) -> Vec<Planned> {
        self.groups
            .iter()
            .filter_map(|group| {
                let mut doomed: Vec<Member> = group.files.iter().filter(|m| self.selected.contains(&m.path)).cloned().collect();
                if doomed.is_empty() {
                    return None;
                }
                if doomed.len() == group.files.len() {
                    doomed.remove(0);
                }
                let keepers = group.files.iter().filter(|m| !doomed.contains(m)).cloned().collect();
                Some(Planned { keepers, doomed })
            })
            .collect()
    }

    fn selected_size(&self) -> (usize, u64) {
        self.plan().iter().flat_map(|p| &p.doomed).fold((0, 0), |(n, bytes), m| (n + 1, bytes + m.size))
    }

    fn forget(&mut self, gone: &[PathBuf]) {
        let gone: HashSet<&PathBuf> = gone.iter().collect();
        for group in &mut self.groups {
            group.files.retain(|m| !gone.contains(&m.path));
        }
        self.groups.retain(|g| g.files.len() > 1);
        self.selected.retain(|p| !gone.contains(p));
        self.rebuild_rows();
    }
}

pub struct Duplicates {
    view: Option<View>,
    generation: u64,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    job: Option<AppJob>,
    messages: Vec<String>,
}

impl Default for Duplicates {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { view: None, generation: 0, tx, rx, job: None, messages: Vec::new() }
    }
}

impl Duplicates {
    fn start(&mut self, roots: Vec<PathBuf>) -> Reaction {
        if roots.is_empty() {
            return Reaction::None;
        }
        self.generation += 1;
        let (generation, tx, cancel) = (self.generation, self.tx.clone(), Arc::new(AtomicBool::new(false)));
        self.view = Some(View {
            generation,
            roots: roots.clone(),
            cancel: Arc::clone(&cancel),
            phase: Phase::Scanning,
            groups: Vec::new(),
            selected: HashSet::new(),
            rows: Vec::new(),
            error: None,
            cancelled: false,
            removing: false,
        });
        let title = tr("Looking for duplicate files").to_owned();
        Reaction::Job(AppJob::new(title, move |w| {
            let progress = OnWorker { worker: w, cancel: &cancel };
            let result = find(&roots, &progress);
            let (cancelled, outcome) = match &result {
                Ok(groups) => (false, Outcome::done(trn(groups.len(), "Found {} group of duplicates", "Found {} groups of duplicates"))),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (true, Outcome::done(tr("Cancelled").to_owned())),
                Err(e) => (false, Outcome::failed(e.to_string())),
            };
            let _ = tx.send(Msg::Found { generation, result: result.map_err(|e| e.to_string()), cancelled });
            outcome
        }))
    }

    fn start_removal(&mut self) {
        let Some(view) = &self.view else { return };
        let plans = view.plan();
        if plans.is_empty() {
            return;
        }
        let (generation, tx) = (view.generation, self.tx.clone());
        if let Some(view) = self.view.as_mut() {
            view.removing = true;
        }
        let doomed: Vec<PathBuf> = plans.iter().flat_map(|p| p.doomed.iter().map(|m| m.path.clone())).collect();
        let count = doomed.len();
        let title = trn(count, "Moving {} duplicate to the trash", "Moving {} duplicates to the trash");
        self.job = Some(
            AppJob::new(title, move |w| {
                let never = AtomicBool::new(false);
                let progress = OnWorker { worker: w, cancel: &never };
                progress.plan(count as u64, 0);
                let (done, skipped, cancelled) = trash_planned(&plans, &progress, trash::trash);
                let _ = tx.send(Msg::Trashed { generation, paths: done.iter().map(|(from, _)| from.clone()).collect() });
                let mut message = trn(done.len(), "Moved {} duplicate to the trash", "Moved {} duplicates to the trash");
                if skipped > 0 {
                    message.push_str(&format!(" · {}", trf("{} skipped", &[&skipped])));
                }
                let label = trn(done.len(), "{} duplicate", "{} duplicates");
                let effect = if done.is_empty() { Effect::None } else { Effect::Record(UndoEntry::new(trf("Removal of {}", &[&label]), done.into_iter().map(|(_, to)| Edit::Restore(to)).collect())) };
                let mut outcome = Outcome::done(message).with(effect);
                outcome.cancelled = cancelled;
                outcome
            })
            .removing(doomed),
        );
    }
}

fn short(path: &Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

impl Plugin for Duplicates {
    fn id(&self) -> &'static str {
        "duplicates"
    }

    fn name(&self) -> &'static str {
        tr("Duplicate finder")
    }

    fn description(&self) -> &'static str {
        tr("Finds files with identical contents in folders and moves the extra copies to the trash, always keeping one")
    }

    fn icon(&self) -> &'static str {
        icons::DUPLICATE
    }

    fn busy(&self) -> bool {
        self.view.as_ref().is_some_and(|v| v.phase == Phase::Scanning)
    }

    fn poll(&mut self) -> bool {
        let mut changed = false;
        for msg in self.rx.try_iter().collect::<Vec<_>>() {
            match msg {
                Msg::Found { generation, result, cancelled } => {
                    let Some(view) = self.view.as_mut().filter(|v| v.generation == generation) else { continue };
                    view.phase = Phase::Done;
                    view.cancelled = cancelled;
                    match result {
                        Ok(groups) => view.groups = groups,
                        Err(e) => view.error = Some(e),
                    }
                    view.rebuild_rows();
                    changed = true;
                }
                Msg::Trashed { generation, paths } => {
                    if let Some(view) = self.view.as_mut().filter(|v| v.generation == generation) {
                        view.forget(&paths);
                        view.removing = false;
                    }
                    changed = true;
                }
            }
        }
        changed
    }

    fn menu_items(&self, selection: &[FileItem]) -> Vec<MenuItem> {
        if !selection.is_empty() && selection.iter().all(|i| i.is_dir && !i.is_symlink) {
            vec![MenuItem::new("scan", tr("Find duplicate files…"), icons::DUPLICATE)]
        } else {
            Vec::new()
        }
    }

    fn folder_menu_items(&self, _here: &Path, _others: &[PathBuf]) -> Vec<MenuItem> {
        vec![MenuItem::new("scan_here", tr("Find duplicate files…"), icons::DUPLICATE)]
    }

    fn run(&mut self, id: &str, paths: &[PathBuf], _dir: &Path) -> Reaction {
        match id {
            "scan" => self.start(paths.iter().filter(|p| p.is_dir()).cloned().collect()),
            "scan_here" => self.start(paths.iter().take(1).filter(|p| p.is_dir()).cloned().collect()),
            _ => Reaction::None,
        }
    }

    fn reaction(&mut self) -> Reaction {
        self.job.take().map_or(Reaction::None, Reaction::Job)
    }

    fn window_title(&self) -> Option<&'static str> {
        self.view.as_ref().map(|_| tr("Duplicate files"))
    }

    fn close_window(&mut self) {
        if let Some(view) = self.view.take() {
            view.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn window_width(&self) -> f32 {
        // The empty and busy states are short messages; the list of groups needs room for paths
        if self.view.as_ref().is_some_and(|v| v.phase != Phase::Scanning && v.error.is_none() && !v.cancelled && !v.groups.is_empty()) { 780.0 } else { 460.0 }
    }

    fn window(&mut self, ui: &mut egui::Ui, theme: &SerpantinumTheme) -> bool {
        let Some(view) = self.view.as_mut() else { return false };
        let roots = view.roots.iter().map(|r| short(r)).collect::<Vec<_>>().join(", ");
        ui.label(egui::RichText::new(roots).size(material::FONT_CAPTION).color(theme.subtext1));
        ui.add_space(4.0);

        if view.phase == Phase::Scanning {
            ui.label(egui::RichText::new(tr("Looking for duplicate files…")).size(material::FONT_BODY).color(theme.text));
            material::hint_label(ui, tr("Progress is in the operations list. Closing this window stops the search."), theme);
            return true;
        }
        if let Some(error) = &view.error {
            ui.label(egui::RichText::new(error).size(material::FONT_BODY).color(theme.red));
            return true;
        }
        if view.cancelled {
            ui.label(egui::RichText::new(tr("Cancelled")).size(material::FONT_BODY).color(theme.subtext1));
            return true;
        }
        if view.groups.is_empty() {
            material::hint_label(ui, tr("No duplicate files found"), theme);
            return true;
        }

        let reclaimable: u64 = view.groups.iter().map(Group::reclaimable).sum();
        let summary = trf("{} groups of identical files, {} could be freed", &[&view.groups.len(), &format_bytes(reclaimable)]);
        ui.label(egui::RichText::new(summary).size(material::FONT_BODY).color(theme.text));
        ui.add_space(6.0);

        let mut rule: Option<Keep> = None;
        let mut clear = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(tr("Keep")).size(material::FONT_CAPTION).color(theme.subtext1));
            for (id, label, keep) in [("newest", tr("Newest"), Keep::Newest), ("oldest", tr("Oldest"), Keep::Oldest), ("short", tr("Shortest path"), Keep::ShortestPath)] {
                if material::button(ui, ("dup_keep", id), None, label, theme, material::ButtonKind::Tonal, true).clicked() {
                    rule = Some(keep);
                }
            }
            // With one folder every copy is inside it: the rule would only pick the first path
            for (i, root) in view.roots.iter().enumerate().filter(|_| view.roots.len() > 1) {
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string());
                if material::button(ui, ("dup_keep_in", i), Some(icons::FOLDER), &trf("In {}", &[&name]), theme, material::ButtonKind::Tonal, true).clicked() {
                    rule = Some(Keep::InFolder(root.clone()));
                }
            }
            if material::button(ui, "dup_clear", None, tr("Select none"), theme, material::ButtonKind::Text, !view.selected.is_empty()).clicked() {
                clear = true;
            }
        });
        if let Some(rule) = rule {
            view.select_all_but(&rule);
        }
        if clear {
            view.selected.clear();
        }
        ui.add_space(6.0);

        let mut toggled: Option<(PathBuf, bool)> = None;
        egui::ScrollArea::vertical().max_height(material::dialog_list_height(ui, 330.0)).auto_shrink([false, true]).show_rows(ui, ROW_HEIGHT, view.rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for row in &view.rows[range] {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), egui::Sense::hover());
                match *row {
                    Row::Group(g) => {
                        let group = &view.groups[g];
                        let text = trf("{} copies of {} each, {} could be freed", &[&group.files.len(), &format_bytes(group.size), &format_bytes(group.reclaimable())]);
                        material::paint_label(ui, rect.left() + 4.0, rect.center().y, &text, material::FONT_BODY, theme.subtext1, rect.width() - 8.0);
                    }
                    Row::File(g, f) => {
                        let member = &view.groups[g].files[f];
                        let on = view.selected.contains(&member.path);
                        let enabled = view.can_select(g, member);
                        let box_rect = egui::Rect::from_min_size(rect.min + egui::vec2(8.0, 0.0), egui::vec2(30.0, ROW_HEIGHT));
                        let clicked = ui.scope_builder(egui::UiBuilder::new().max_rect(box_rect), |ui| material::checkbox(ui, ("dup_pick", g, f), on, enabled, theme)).inner.clicked();
                        if clicked && enabled {
                            toggled = Some((member.path.clone(), !on));
                        }
                        let date = crate::fs::props::format_datetime(member.modified);
                        let date_w = 140.0;
                        material::paint_label(ui, rect.right() - date_w, rect.center().y, &date, material::FONT_CAPTION, theme.subtext1, date_w - 4.0);
                        let (name, folder) = (
                            member.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                            member.path.parent().map(short).unwrap_or_default(),
                        );
                        let x = rect.left() + 46.0;
                        let room = rect.width() - 46.0 - date_w - 8.0;
                        let name_w = ui.painter().layout_no_wrap(name.clone(), egui::FontId::proportional(material::FONT_BODY), theme.text).size().x.min(room);
                        material::paint_label(ui, x, rect.center().y, &name, material::FONT_BODY, if on { theme.red } else { theme.text }, name_w);
                        if room - name_w > 40.0 {
                            material::paint_label(ui, x + name_w + 10.0, rect.center().y, &folder, material::FONT_CAPTION, theme.subtext1, room - name_w - 10.0);
                        }
                    }
                }
            }
        });
        if let Some((path, on)) = toggled {
            if on {
                view.selected.insert(path);
            } else {
                view.selected.remove(&path);
            }
        }

        ui.add_space(8.0);
        let (count, bytes) = view.selected_size();
        let mut remove = false;
        ui.horizontal(|ui| {
            let label = if count == 0 { tr("Nothing selected").to_owned() } else { trf("{} selected, {}", &[&trn(count, "{} file", "{} files"), &format_bytes(bytes)]) };
            ui.label(egui::RichText::new(label).size(material::FONT_BODY).color(theme.subtext1));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                remove = material::button(ui, "dup_remove", Some(icons::TRASH), tr("Move to trash"), theme, material::ButtonKind::Danger, count > 0 && !view.removing).clicked() && count > 0 && !view.removing;
            });
        });
        if remove {
            self.start_removal();
        }
        true
    }

    fn messages(&mut self) -> Vec<String> {
        std::mem::take(&mut self.messages)
    }
}

