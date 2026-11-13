//! Tags: coloured labels on files. The names are stored in the extended attribute
//! `user.xdg.tags` (comma separated), the one Dolphin reads, so tags show in both managers.
//! Colours are Compression's own (`~/.config/compression/tags.json`). Because there is no index, Compression remembers
//! the paths it has seen tagged (`~/.local/share/compression/tag-paths.json`) to answer `tag:work`.
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use eframe::egui;

use super::tag_reach::{self, Identity, Meta, Metas, Reach};
use super::{FileOpEvent, Mark, MenuItem, Plugin, PropertyGroup, Reaction, SidebarEntry, intern};
use crate::fs::file_item::FileItem;
use crate::fs::xattr;
use crate::i18n::{tr, trf, trn};
use crate::ui::icons;
use crate::ui::material;
use crate::ui::theme::SerpantinumTheme;

pub const ATTR: &str = "user.xdg.tags";
const MAX_NAME: usize = 64;
/// A folder scan for tags stops after this many entries.
const SCAN_LIMIT: usize = 200_000;

/// Colours offered in Settings; a tag without a choice gets one of these from a hash of its name.
pub const PALETTE: [[u8; 3]; 8] = [
    [0xe5, 0x57, 0x57], // red
    [0xf0, 0x92, 0x4a], // orange
    [0xe9, 0xc4, 0x4c], // yellow
    [0x5c, 0xb8, 0x5c], // green
    [0x3f, 0xb8, 0xaf], // teal
    [0x4a, 0x90, 0xe2], // blue
    [0x9b, 0x6b, 0xdf], // purple
    [0xe0, 0x6c, 0xb0], // pink
];

/// The names in an attribute value: comma separated, trimmed, without empty or repeated names.
pub fn parse(bytes: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in String::from_utf8_lossy(bytes).split(',') {
        let name = name.trim();
        if !name.is_empty() && !out.iter().any(|n| n == name) {
            out.push(name.to_owned());
        }
    }
    out
}

pub fn format(tags: &[String]) -> String {
    tags.join(",")
}

/// Tag names match without regard to case, in any script (the registry folds the same way).
fn same_tag(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// A name that fits the attribute: no comma (it separates names), no control characters, not empty.
pub fn clean_name(name: &str) -> Option<String> {
    let name = name.trim();
    let ok = !name.is_empty() && name.chars().count() <= MAX_NAME && !name.chars().any(|c| c == ',' || c.is_control());
    ok.then(|| name.to_owned())
}

fn hash(name: &str) -> usize {
    name.to_lowercase().bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3)) as usize
}

/// What reading or writing the tags of a file said, in words for the user.
fn describe(err: &std::io::Error, name: &str) -> String {
    if xattr::is_unsupported(err) {
        return tr("This disk can't store tags").to_owned();
    }
    match err.raw_os_error() {
        Some(libc::EPERM) => trf("Tags can't be set on {} (links and special files can't carry them)", &[&name]),
        Some(libc::EACCES) | Some(libc::EROFS) => trf("{} is read-only, so its tags can't change", &[&name]),
        _ => trf("Tags of {} failed: {}", &[&name, &crate::fs::describe::io_error(err)]),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

pub fn read_tags(path: &Path) -> std::io::Result<Vec<String>> {
    Ok(xattr::get(path, ATTR)?.map(|v| parse(&v)).unwrap_or_default())
}

pub fn write_tags(path: &Path, tags: &[String]) -> std::io::Result<()> {
    if tags.is_empty() { xattr::remove(path, ATTR) } else { xattr::set(path, ATTR, format(tags).as_bytes()) }
}

/// The paths Compression has seen tagged, with their tags.
#[derive(Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Registry(BTreeMap<PathBuf, Vec<String>>);

impl Registry {
    fn file() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("compression").join("tag-paths.json"))
    }

    /// Read as it is: where its files are now is found out on a thread (`Tags::start_check`), never here.
    fn load() -> Self {
        Self::file().map(|f| crate::fs::stored::load_json(&f)).unwrap_or_default()
    }

    fn save(&self) {
        // As administrator the data folder belongs to someone else's home, as for the settings
        if crate::fs::admin::is_admin() {
            return;
        }
        let Some(file) = Self::file() else { return };
        if let Ok(json) = serde_json::to_string(self) {
            let _ = crate::fs::stored::save(&file, json);
        }
    }

    /// Returns whether anything changed.
    pub fn record(&mut self, path: &Path, tags: &[String]) -> bool {
        if tags.is_empty() {
            return self.0.remove(path).is_some();
        }
        self.0.insert(path.to_path_buf(), tags.to_vec()).as_deref() != Some(tags)
    }

    /// `from` and everything inside it now lives at `to`.
    pub fn rename(&mut self, from: &Path, to: &Path) -> bool {
        let moved: Vec<PathBuf> = self.0.keys().filter(|p| p.starts_with(from)).cloned().collect();
        for old in &moved {
            if let Some(tags) = self.0.remove(old) {
                let new = to.join(old.strip_prefix(from).unwrap_or(Path::new("")));
                self.0.insert(new, tags);
            }
        }
        !moved.is_empty()
    }

    pub fn remove_tree(&mut self, path: &Path) -> bool {
        let before = self.0.len();
        self.0.retain(|p, _| !p.starts_with(path));
        self.0.len() != before
    }

    /// Every tag name in use, with the number of files that have it, ordered by name.
    pub fn names(&self) -> Vec<(String, usize)> {
        let mut counts: BTreeMap<String, (String, usize)> = BTreeMap::new();
        for tags in self.0.values() {
            for tag in tags {
                counts.entry(tag.to_lowercase()).or_insert_with(|| (tag.clone(), 0)).1 += 1;
            }
        }
        counts.into_values().collect()
    }

    pub fn with_tag(&self, name: &str) -> Vec<PathBuf> {
        let name = name.to_lowercase();
        self.0.iter().filter(|(_, tags)| tags.iter().any(|t| t.to_lowercase() == name)).map(|(p, _)| p.clone()).collect()
    }
}

#[derive(Clone)]
struct Entry {
    /// `(inode, ctime seconds, ctime nanoseconds)`: changing an attribute changes ctime, not mtime.
    key: (u64, i64, i64),
    tags: Vec<String>,
}

fn key_of(path: &Path) -> Option<(std::fs::Metadata, (u64, i64, i64))> {
    let meta = path.symlink_metadata().ok()?;
    let key = (meta.ino(), meta.ctime(), meta.ctime_nsec());
    Some((meta, key))
}

/// The tags of everything in `dir`, reusing `previous` where the file's ctime did not change.
fn read_folder(dir: &Path, previous: &HashMap<PathBuf, Entry>) -> HashMap<PathBuf, Entry> {
    let mut out = HashMap::new();
    let Ok(read) = std::fs::read_dir(dir) else { return out };
    for entry in read.flatten() {
        let path = entry.path();
        let Some((meta, key)) = key_of(&path) else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        let tags = match previous.get(&path) {
            Some(old) if old.key == key => old.tags.clone(),
            _ => read_tags(&path).unwrap_or_default(),
        };
        out.insert(path, Entry { key, tags });
    }
    out
}

/// The tagged items at `path`: itself, and everything below it when it is a folder.
fn tagged_in(path: &Path) -> Vec<Tagged> {
    let mut found = scan_tree(path);
    if let Ok(meta) = path.symlink_metadata()
        && !meta.file_type().is_symlink()
        && let Ok(tags) = read_tags(path)
        && !tags.is_empty()
    {
        found.push((path.to_path_buf(), tags));
    }
    found
}

/// Walks `dir` and returns every file with tags.
fn scan_tree(dir: &Path) -> Vec<Tagged> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0usize;
    while let Some(folder) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&folder) else { continue };
        for entry in read.flatten() {
            seen += 1;
            if seen > SCAN_LIMIT {
                return found;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            if let Ok(tags) = read_tags(&path)
                && !tags.is_empty()
            {
                found.push((path.clone(), tags));
            }
            if kind.is_dir() {
                stack.push(path);
            }
        }
    }
    found
}

/// A folder read on a worker: the generation that asked, the folder and what it holds.
type FolderRead = (u64, PathBuf, HashMap<PathBuf, Entry>);
type Tagged = (PathBuf, Vec<String>);

struct Prompt {
    /// Files that get the new tag; empty when editing `editing`.
    paths: Vec<PathBuf>,
    /// The tag being renamed or deleted, when the prompt edits one.
    editing: Option<String>,
    name: String,
    /// The colour picked in the dialog; None keeps the tag's current one.
    color: Option<[u8; 3]>,
    first_frame: bool,
}

pub struct Tags {
    registry: Registry,
    colors: BTreeMap<String, [u8; 3]>,
    cache: HashMap<PathBuf, Entry>,
    dir: Option<PathBuf>,
    generation: u64,
    folder_rx: Option<mpsc::Receiver<FolderRead>>,
    prompt: Option<Prompt>,
    messages: Vec<String>,
    /// Beside each remembered file: its file system, and whether its tags wait to be written to it.
    metas: Metas,
    /// Where each remembered file was found by the last check; one not checked yet counts as away.
    reach: HashMap<PathBuf, Reach>,
    /// The check that runs, and whether it forgets what it finds gone (the cleanup the person confirmed).
    check_rx: Option<mpsc::Receiver<(bool, Vec<Checked>)>>,
    identity: Identity,
    /// The person asked to forget the files that are gone and is asked to confirm.
    confirm_cleanup: bool,
    /// When the last check was started.
    last_check: Option<std::time::Instant>,
}

/// What a check found of one remembered file.
struct Checked {
    path: PathBuf,
    reach: Reach,
    /// The file system it is on, when it is there.
    fs: Option<String>,
    /// Its tags as read from it, when it is there and nothing waited to be written.
    read: Option<Vec<String>>,
    /// The tags that waited were written to it.
    wrote: bool,
}

/// Finds out, off the window's thread, where `entries` (path, tags in the index, what is kept beside them) are. A file
/// that is there gets the tags that waited for it written, or has its own tags read.
fn check(entries: Vec<(PathBuf, Vec<String>, Meta)>, identity: Identity, server_wait: std::time::Duration) -> Vec<Checked> {
    entries
        .into_iter()
        .map(|(path, tags, meta)| {
            let reach = tag_reach::classify(&path, Some(&meta), identity, server_wait);
            let (mut fs, mut read, mut wrote) = (None, None, false);
            if reach == Reach::Here {
                fs = identity(&path);
                if meta.pending {
                    wrote = write_tags(&path, &tags).is_ok();
                } else {
                    read = read_tags(&path).ok();
                }
            }
            Checked { path, reach, fs, read, wrote }
        })
        .collect()
}

fn colors_file() -> Option<PathBuf> {
    crate::fs::admin::config_override().or_else(dirs::config_dir).map(|d| d.join("compression").join("tags.json"))
}

impl Tags {
    pub fn load() -> Self {
        let colors = colors_file().map(|f| crate::fs::stored::load_json(&f)).unwrap_or_default();
        let mut tags = Self::with(Registry::load(), colors);
        tags.metas = Metas::load();
        tags.start_check(None, false);
        tags
    }

    fn with(registry: Registry, colors: BTreeMap<String, [u8; 3]>) -> Self {
        Self {
            registry,
            colors,
            cache: HashMap::new(),
            dir: None,
            generation: 0,
            folder_rx: None,
            prompt: None,
            messages: Vec::new(),
            metas: Metas::default(),
            reach: HashMap::new(),
            check_rx: None,
            identity: tag_reach::fs_identity,
            confirm_cleanup: false,
            last_check: None,
        }
    }

    /// Finds out on a thread where the remembered files are (`paths`, or all of them). With `forget`, the files found
    /// gone then are forgotten: only their tags in the index, never the files.
    fn start_check(&mut self, paths: Option<Vec<PathBuf>>, forget: bool) {
        let entries: Vec<(PathBuf, Vec<String>, Meta)> = self
            .registry
            .0
            .iter()
            .filter(|(p, _)| paths.as_ref().is_none_or(|only| only.contains(p)))
            .map(|(p, tags)| (p.clone(), tags.clone(), self.metas.0.get(p).cloned().unwrap_or_default()))
            .collect();
        let (tx, rx) = mpsc::channel();
        let identity = self.identity;
        std::thread::spawn(move || {
            let _ = tx.send((forget, check(entries, identity, tag_reach::SERVER_WAIT)));
        });
        self.check_rx = Some(rx);
        self.last_check = Some(std::time::Instant::now());
    }

    /// Takes in what a check found; true when something changed.
    fn take_check(&mut self) -> bool {
        let Some(rx) = &self.check_rx else { return false };
        let (forget, found) = match rx.try_recv() {
            Ok(found) => found,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.check_rx = None;
                return false;
            }
        };
        self.check_rx = None;
        let (mut dirty, mut metas_dirty) = (false, false);
        for item in found {
            self.reach.insert(item.path.clone(), item.reach);
            match item.reach {
                Reach::Here => {
                    let meta = self.metas.0.entry(item.path.clone()).or_default();
                    if item.fs.is_some() && meta.fs != item.fs {
                        meta.fs = item.fs;
                        metas_dirty = true;
                    }
                    if item.wrote {
                        meta.pending = false;
                        metas_dirty = true;
                        // Written: a file left with no tag leaves the index now
                        if self.registry.0.get(&item.path).is_some_and(Vec::is_empty) {
                            dirty |= self.registry.record(&item.path, &[]);
                        }
                    }
                    // A file whose tags waited keeps those of the index, also when writing them failed this time
                    if let Some(read) = item.read.filter(|_| !meta.pending) {
                        dirty |= self.registry.record(&item.path, &read);
                    }
                }
                Reach::Gone if forget => dirty |= self.registry.record(&item.path, &[]),
                _ => {}
            }
        }
        // What the index forgot needs nothing kept beside it
        let before = self.metas.0.len();
        self.metas.0.retain(|p, _| self.registry.0.contains_key(p));
        metas_dirty |= self.metas.0.len() != before;
        self.reach.retain(|p, _| self.registry.0.contains_key(p));
        if dirty {
            self.registry.save();
        }
        if dirty || metas_dirty {
            self.metas.save();
        }
        true
    }

    /// Where `path` is, by the last check; a remembered file not checked yet counts as away, a file the index does not
    /// know (one on show in a folder) as here.
    fn reach_of(&self, path: &Path) -> Reach {
        match self.reach.get(path) {
            Some(reach) => *reach,
            None if self.registry.0.contains_key(path) => Reach::Away,
            None => Reach::Here,
        }
    }

    /// How many files have the tag `name`: there, away (or not known yet), and gone.
    fn counts(&self, name: &str) -> (usize, usize, usize) {
        self.registry.with_tag(name).iter().fold((0, 0, 0), |(here, away, gone), p| match self.reach_of(p) {
            Reach::Here => (here + 1, away, gone),
            Reach::Away => (here, away + 1, gone),
            Reach::Gone => (here, away, gone + 1),
        })
    }

    /// "3 files · 2 not available now · 1 no longer there", the parts that are not 0 (the first always).
    fn count_text(&self, name: &str) -> String {
        let (here, away, gone) = self.counts(name);
        let mut text = trn(here, "{} file", "{} files");
        if away > 0 {
            text.push_str(&format!(" · {}", trn(away, "{} not available now", "{} not available now")));
        }
        if gone > 0 {
            text.push_str(&format!(" · {}", trn(gone, "{} no longer there", "{} no longer there")));
        }
        text
    }

    /// The tagged files that are gone: said, and their tags forgotten only when the person confirms it. While the page is
    /// open the files are checked again now and then, on a thread, so what it says stays true.
    fn gone_files_section(&mut self, ui: &mut egui::Ui, theme: &SerpantinumTheme) {
        if self.check_rx.is_none() && self.last_check.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(10)) {
            self.start_check(None, false);
        }
        let gone = self.gone();
        if gone.is_empty() {
            self.confirm_cleanup = false;
            return;
        }
        let checking = self.check_rx.is_some();
        material::hint_label_inset(ui, &trn(gone.len(), "{} tagged file is no longer there: its drive is connected, and the file is not.", "{} tagged files are no longer there: their drive is connected, and the files are not."), theme, 14);
        if !self.confirm_cleanup {
            if material::button(ui, "tags_forget_gone", Some(icons::TAG), tr("Forget their tags…"), theme, material::ButtonKind::Tonal, !checking).clicked() {
                self.confirm_cleanup = true;
            }
            return;
        }
        material::hint_label_inset(ui, &trn(gone.len(), "The tags of {} file that is gone are forgotten. No file is touched.", "The tags of {} files that are gone are forgotten. No file is touched."), theme, 14);
        let (mut forget, mut cancel) = (false, false);
        ui.horizontal(|ui| {
            forget = material::button(ui, "tags_forget_confirm", Some(icons::DELETE_FOREVER), tr("Forget"), theme, material::ButtonKind::Danger, !checking).clicked();
            cancel = material::button(ui, "tags_forget_cancel", None, tr("Cancel"), theme, material::ButtonKind::Text, true).clicked();
        });
        if forget {
            self.forget_gone();
        } else if cancel {
            self.confirm_cleanup = false;
        }
    }

    /// The cleanup the person confirmed: each file found gone is looked at again first, and only what is still gone is
    /// forgotten (its tags in the index; the files are never touched).
    fn forget_gone(&mut self) {
        self.confirm_cleanup = false;
        let gone = self.gone();
        if !gone.is_empty() {
            self.start_check(Some(gone), true);
        }
    }

    /// The remembered files that are gone, by the last check.
    fn gone(&self) -> Vec<PathBuf> {
        self.reach.iter().filter(|(_, r)| **r == Reach::Gone).map(|(p, _)| p.clone()).collect()
    }

    pub fn color(&self, name: &str) -> [u8; 3] {
        self.colors.get(name).or_else(|| self.colors.get(&name.to_lowercase())).copied().unwrap_or(PALETTE[hash(name) % PALETTE.len()])
    }

    fn save_colors(&self) {
        let Some(file) = colors_file() else { return };
        if let Ok(json) = serde_json::to_string_pretty(&self.colors) {
            let _ = crate::fs::stored::save(&file, json);
        }
    }

    /// Every tag the user can pick: those in use and those with a colour of their own.
    fn known(&self) -> Vec<String> {
        let mut names: Vec<String> = self.registry.names().into_iter().map(|(n, _)| n).collect();
        for name in self.colors.keys() {
            if !names.iter().any(|n| same_tag(n, name)) {
                names.push(name.clone());
            }
        }
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    fn tags_of(&self, path: &Path) -> Vec<String> {
        match self.cache.get(path) {
            Some(entry) => entry.tags.clone(),
            None => read_tags(path).unwrap_or_default(),
        }
    }

    fn remember(&mut self, path: &Path, tags: Vec<String>) {
        if let Some((_, key)) = key_of(path) {
            self.cache.insert(path.to_path_buf(), Entry { key, tags: tags.clone() });
        }
        let changed = self.registry.record(path, &tags);
        // Written to the file just now: nothing waits for it, and the file system it is on is known
        self.reach.insert(path.to_path_buf(), Reach::Here);
        let fs = (self.identity)(path);
        let meta = self.metas.0.entry(path.to_path_buf()).or_default();
        let meta_changed = meta.pending || (fs.is_some() && meta.fs != fs);
        meta.pending = false;
        if fs.is_some() {
            meta.fs = fs;
        }
        if tags.is_empty() {
            self.metas.0.remove(path);
        }
        if changed {
            self.registry.save();
        }
        if changed || meta_changed {
            self.metas.save();
        }
    }

    /// Changes the tags of every path with `change`; the first failure is the one reported.
    /// A remembered file that cannot be reached now is changed in the index only, and its new tags wait to be written to
    /// it (`check` writes them once it is there), so that the old ones do not come back from the file. A file that
    /// fails is said, and the others are still changed.
    fn edit(&mut self, paths: &[PathBuf], change: impl Fn(&mut Vec<String>)) {
        let mut touched = 0;
        let mut waiting = false;
        for path in paths {
            if self.reach_of(path) != Reach::Here
                && let Some(known) = self.registry.0.get(path).cloned()
            {
                let mut tags = known.clone();
                change(&mut tags);
                if tags != known {
                    // Kept also when no tag is left: the file still has the old ones until they are written
                    self.registry.0.insert(path.clone(), tags);
                    self.metas.0.entry(path.clone()).or_default().pending = true;
                    waiting = true;
                    touched += 1;
                }
                continue;
            }
            let mut tags = match read_tags(path) {
                Ok(tags) => tags,
                Err(err) => {
                    self.messages.push(describe(&err, &file_name(path)));
                    continue;
                }
            };
            let before = tags.clone();
            change(&mut tags);
            if tags == before {
                continue;
            }
            if let Err(err) = write_tags(path, &tags) {
                self.messages.push(describe(&err, &file_name(path)));
                continue;
            }
            self.remember(path, tags);
            touched += 1;
        }
        if waiting {
            // A tag list that is now empty still waits to be written (the file keeps the old tags until then)
            self.registry.save();
            self.metas.save();
        }
        if touched == 0 && paths.len() > 1 {
            self.messages.push(tr("Nothing changed").to_owned());
        }
    }

    fn start_edit(&mut self, name: &str) {
        self.prompt = Some(Prompt { paths: Vec::new(), editing: Some(name.to_owned()), name: name.to_owned(), color: None, first_frame: true });
    }

    /// Gives every file with the tag `from` the name `to` instead (merging when it has both).
    fn rename_tag(&mut self, from: &str, to: &str) {
        let paths = self.registry.with_tag(from);
        self.edit(&paths, |tags| {
            tags.retain(|t| !same_tag(t, from));
            if !tags.iter().any(|t| same_tag(t, to)) {
                tags.push(to.to_owned());
            }
        });
        if let Some(key) = self.colors.keys().find(|k| same_tag(k, from)).cloned()
            && let Some(rgb) = self.colors.remove(&key)
        {
            self.colors.entry(to.to_owned()).or_insert(rgb);
            self.save_colors();
        }
    }

    fn set_color(&mut self, name: &str, rgb: Option<[u8; 3]>) {
        if let Some(rgb) = rgb {
            self.colors.retain(|k, _| !same_tag(k, name));
            self.colors.insert(name.to_owned(), rgb);
            self.save_colors();
        }
    }

    fn delete_tag(&mut self, name: &str) {
        let paths = self.registry.with_tag(name);
        self.edit(&paths, |tags| tags.retain(|t| !same_tag(t, name)));
        let before = self.colors.len();
        self.colors.retain(|k, _| !same_tag(k, name));
        if self.colors.len() != before {
            self.save_colors();
        }
    }

    fn start_prompt(&mut self, paths: &[PathBuf]) {
        self.prompt = Some(Prompt { paths: paths.to_vec(), editing: None, name: String::new(), color: None, first_frame: true });
    }
}

impl Plugin for Tags {
    fn id(&self) -> &'static str {
        "tags"
    }

    fn name(&self) -> &'static str {
        tr("Tags")
    }

    fn description(&self) -> &'static str {
        tr("Coloured labels on files, stored in the file system so other file managers see them too")
    }

    fn icon(&self) -> &'static str {
        icons::TAG
    }

    fn directory_changed(&mut self, dir: &Path) {
        if self.dir.as_deref() != Some(dir) {
            self.cache.retain(|p, _| p.parent() != self.dir.as_deref());
        }
        self.dir = Some(dir.to_path_buf());
        self.generation += 1;
        let (tx, rx) = mpsc::channel();
        self.folder_rx = Some(rx);
        let previous: HashMap<PathBuf, Entry> = self.cache.iter().filter(|(p, _)| p.parent() == Some(dir)).map(|(p, e)| (p.clone(), e.clone())).collect();
        let (dir, generation) = (dir.to_path_buf(), self.generation);
        std::thread::spawn(move || {
            let read = read_folder(&dir, &previous);
            let _ = tx.send((generation, dir, read));
        });
    }

    fn poll(&mut self) -> bool {
        let mut changed = self.take_check();
        if let Some(rx) = &self.folder_rx {
            match rx.try_recv() {
                Ok((generation, dir, read)) if generation == self.generation => {
                    self.folder_rx = None;
                    self.cache.retain(|p, _| p.parent() != Some(dir.as_path()));
                    let mut dirty = false;
                    let mut to_check = Vec::new();
                    for (path, entry) in &read {
                        // A file whose tags wait to be written keeps those of the index: the check writes them
                        if self.metas.0.get(path).is_some_and(|m| m.pending) {
                            to_check.push(path.clone());
                            continue;
                        }
                        dirty |= self.registry.record(path, &entry.tags);
                        self.reach.insert(path.clone(), Reach::Here);
                    }
                    // Remembered files of this folder that were not in it: not forgotten here (it may be another drive
                    // at the same place); the check tells away from gone
                    to_check.extend(self.registry.0.keys().filter(|p| p.parent() == Some(dir.as_path()) && !read.contains_key(*p)).cloned());
                    if dirty {
                        self.registry.save();
                    }
                    if !to_check.is_empty() && self.check_rx.is_none() {
                        self.start_check(Some(to_check), false);
                    }
                    self.cache.extend(read);
                    changed = true;
                }
                Ok(_) | Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.folder_rx = None,
            }
        }
        changed
    }

    fn busy(&self) -> bool {
        self.folder_rx.is_some() || self.check_rx.is_some()
    }

    fn marks(&self, item: &FileItem) -> Vec<Mark> {
        match self.cache.get(&item.path) {
            Some(entry) => entry.tags.iter().map(|t| Mark::Dot(self.color(t))).collect(),
            None => Vec::new(),
        }
    }

    fn menu_catalog(&self) -> Vec<MenuItem> {
        let group = ("tags", tr("Tags"));
        vec![
            MenuItem::new("toggle:*", tr("Each tag"), icons::TAG).group(group.0, group.1),
            MenuItem::new("new", tr("New tag…"), icons::ADD).group(group.0, group.1),
            MenuItem::new("clear", tr("Remove all tags"), icons::CLOSE).group(group.0, group.1),
        ]
    }

    fn menu_items(&self, selection: &[FileItem]) -> Vec<MenuItem> {
        // Links cannot carry attributes
        if selection.is_empty() || selection.iter().any(|i| i.is_symlink) {
            return Vec::new();
        }
        let per_file: Vec<Vec<String>> = selection.iter().map(|i| self.tags_of(&i.path)).collect();
        let group = ("tags", tr("Tags"));
        let mut items = Vec::new();
        for name in self.known() {
            let all = per_file.iter().all(|tags| tags.iter().any(|t| same_tag(t, &name)));
            items.push(MenuItem::new(format!("toggle:{name}"), intern(&name), if all { icons::CHECK } else { icons::TAG }).group(group.0, group.1));
        }
        items.push(MenuItem::new("new", tr("New tag…"), icons::ADD).group(group.0, group.1));
        if per_file.iter().any(|tags| !tags.is_empty()) {
            items.push(MenuItem::new("clear", tr("Remove all tags"), icons::CLOSE).group(group.0, group.1));
        }
        items
    }

    fn run(&mut self, id: &str, paths: &[PathBuf], _dir: &Path) -> Reaction {
        if let Some(name) = id.strip_prefix("toggle:") {
            let all = paths.iter().all(|p| self.tags_of(p).iter().any(|t| same_tag(t, name)));
            let name = name.to_owned();
            if all {
                self.edit(paths, |tags| tags.retain(|t| !same_tag(t, &name)));
            } else {
                self.edit(paths, |tags| {
                    if !tags.iter().any(|t| same_tag(t, &name)) {
                        tags.push(name.clone());
                    }
                });
            }
        } else if id == "new" {
            self.start_prompt(paths);
        } else if id == "clear" {
            self.edit(paths, |tags| tags.clear());
        }
        Reaction::None
    }

    fn window_title(&self) -> Option<&'static str> {
        self.prompt.as_ref().map(|p| if p.editing.is_some() { tr("Edit tag") } else { tr("New tag") })
    }

    fn close_window(&mut self) {
        self.prompt = None;
    }

    fn window_width(&self) -> f32 {
        420.0
    }

    fn window_subtitle(&self) -> Option<String> {
        let prompt = self.prompt.as_ref()?;
        match (&prompt.editing, prompt.paths.as_slice()) {
            (Some(name), _) => Some(self.count_text(name)),
            (None, [one]) => Some(file_name(one)),
            (None, paths) => Some(trn(paths.len(), "{} file", "{} files")),
        }
    }

    fn window(&mut self, ui: &mut egui::Ui, theme: &SerpantinumTheme) -> bool {
        let Some(mut prompt) = self.prompt.take() else { return false };
        let (mut add, mut delete) = (false, false);
        let valid = clean_name(&prompt.name).is_some();
        material::input_frame(theme).show(ui, |ui| {
            let out = egui::TextEdit::singleline(&mut prompt.name)
                .frame(egui::Frame::NONE)
                .desired_width(f32::INFINITY)
                .hint_text(tr("Tag name"))
                .font(egui::FontId::proportional(material::FONT_BODY));
            let response = ui.add(out);
            if std::mem::take(&mut prompt.first_frame) {
                response.request_focus();
                // Renaming starts with the name selected, so typing replaces it
                if prompt.editing.is_some()
                    && let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), response.id)
                {
                    let end = egui::text::CCursor::new(prompt.name.chars().count());
                    state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), end)));
                    state.store(ui.ctx(), response.id);
                }
            }
            add |= response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        });
        if !valid && !prompt.name.trim().is_empty() {
            let problem = if prompt.name.trim().chars().count() > MAX_NAME { trf("Names can't be longer than {} characters", &[&MAX_NAME]) } else { tr("Names can't contain commas").to_owned() };
            ui.label(egui::RichText::new(problem).size(material::FONT_CAPTION).color(theme.red));
        }
        // The colour follows the typed name until one is picked
        let shown = prompt.color.unwrap_or_else(|| self.color(prompt.editing.as_deref().filter(|_| prompt.name.trim().is_empty()).unwrap_or(prompt.name.trim())));
        ui.add_space(2.0);
        if let Some(rgb) = material::control_row(ui, theme, material::row_hint(tr("Color"), theme), |ui| material::swatch_choice(ui, "tag_dialog_color", &PALETTE, shown, theme)) {
            prompt.color = Some(rgb);
        }
        ui.add_space(2.0);
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 36.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = if prompt.editing.is_some() { tr("Save") } else { tr("Add tag") };
            add |= material::button(ui, "tag_add", Some(icons::CHECK), label, theme, material::ButtonKind::Filled, valid).clicked();
            if prompt.editing.is_some() {
                // The destructive choice sits at the far side, away from Save
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    delete = material::button(ui, "tag_delete", Some(icons::TRASH), tr("Delete tag"), theme, material::ButtonKind::Danger, true).clicked();
                });
            }
        });
        if let Some(old) = prompt.editing.clone() {
            if delete {
                self.delete_tag(&old);
                return false;
            }
            if add && let Some(name) = clean_name(&prompt.name) {
                if name != old {
                    self.rename_tag(&old, &name);
                }
                self.set_color(&name, prompt.color);
                return false;
            }
        } else if add && let Some(name) = clean_name(&prompt.name) {
            self.set_color(&name, prompt.color);
            self.edit(&prompt.paths, |tags| {
                if !tags.iter().any(|t| same_tag(t, &name)) {
                    tags.push(name.clone());
                }
            });
            return false;
        }
        self.prompt = Some(prompt);
        true
    }

    fn sidebar(&self) -> Vec<SidebarEntry> {
        self
            .registry
            .names()
            .into_iter()
            .map(|(name, _)| SidebarEntry { id: format!("tag:{name}"), label: name, icon: icons::TAG, editable: true })
            .collect()
    }

    fn sidebar_click(&mut self, id: &str) -> Reaction {
        Reaction::Search(id.to_owned())
    }

    fn sidebar_edit(&mut self, id: &str) {
        if let Some(name) = id.strip_prefix("tag:") {
            self.start_edit(name);
        }
    }

    fn file_op_done(&mut self, op: &FileOpEvent) {
        let mut dirty = false;
        match op {
            FileOpEvent::Moved(pairs) => {
                for (from, to) in pairs {
                    dirty |= self.registry.rename(from, to);
                    if self.metas.rename(from, to) {
                        self.metas.save();
                    }
                    let moved: Vec<PathBuf> = self.reach.keys().filter(|p| p.starts_with(from)).cloned().collect();
                    for old in moved {
                        if let Some(reach) = self.reach.remove(&old) {
                            self.reach.insert(to.join(old.strip_prefix(from).unwrap_or(Path::new(""))), reach);
                        }
                    }
                    // A moved folder takes everything cached below it along
                    let moved: Vec<PathBuf> = self.cache.keys().filter(|p| p.starts_with(from)).cloned().collect();
                    for old in moved {
                        if let Some(entry) = self.cache.remove(&old) {
                            let new = to.join(old.strip_prefix(from).unwrap_or(Path::new("")));
                            self.cache.insert(new, entry);
                        }
                    }
                }
            }
            FileOpEvent::Removed(paths) => {
                // Deleted by Compression itself (or moved to the trash, from where a restore reads the tags again)
                for path in paths {
                    dirty |= self.registry.remove_tree(path);
                    if self.metas.remove_tree(path) {
                        self.metas.save();
                    }
                    self.reach.retain(|p, _| !p.starts_with(path));
                    self.cache.retain(|p, _| !p.starts_with(path));
                }
            }
            FileOpEvent::Created(paths) => {
                for path in paths {
                    for (file, tags) in tagged_in(path) {
                        dirty |= self.registry.record(&file, &tags);
                    }
                }
            }
        }
        if dirty {
            self.registry.save();
        }
    }

    fn search_paths(&self, query: &str) -> Option<Vec<PathBuf>> {
        let name = query.trim().strip_prefix("tag:").or_else(|| query.trim().strip_prefix("Tag:"))?.trim();
        // Trust the attribute over the remembered list; drop what is gone or no longer tagged
        Some(self.registry.with_tag(name).into_iter().filter(|p| read_tags(p).map_or_else(|e| e.kind() != std::io::ErrorKind::NotFound, |tags| tags.iter().any(|t| same_tag(t, name)))).collect())
    }

    fn properties(&mut self, item: &FileItem) -> Vec<PropertyGroup> {
        if item.is_symlink {
            return Vec::new();
        }
        let tags = self.tags_of(&item.path);
        if tags.is_empty() {
            return Vec::new();
        }
        vec![PropertyGroup { title: tr("Tags"), rows: vec![(tr("Tags"), tags.join(", "))] }]
    }

    fn settings(&mut self, ui: &mut egui::Ui, theme: &SerpantinumTheme) {
        material::hint_label_inset(ui, tr("Pick a colour for each tag. Other file managers see the names only."), theme, 14);
        let names = self.known();
        if names.is_empty() {
            material::hint_label_inset(ui, tr("No tags yet. Right-click a file and choose Tags."), theme, 14);
        }
        let mut chosen: Option<(String, [u8; 3])> = None;
        for name in &names {
            let current = self.color(name);
            let pick = material::control_row(ui, theme, material::row_text(name, theme), |ui| material::swatch_choice(ui, ("tag_color", name), &PALETTE, current, theme));
            if let Some(rgb) = pick {
                chosen = Some((name.clone(), rgb));
            }
        }
        if let Some((name, rgb)) = chosen {
            self.colors.insert(name, rgb);
            self.save_colors();
        }
        self.gone_files_section(ui, theme);
        ui.add_space(4.0);
    }

    fn messages(&mut self) -> Vec<String> {
        std::mem::take(&mut self.messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::fs::xattr::{self, tests::scratch_file};

    /// A made-up drive: a folder whose `.fsid` file says the UUID of the file system it stands for.
    fn drive(at: &Path, id: &str) {
        std::fs::create_dir_all(at).unwrap();
        std::fs::write(at.join(".fsid"), id).unwrap();
    }

    /// The identity of what `path` is on, by the made-up drives (`drive`); anything else is the system's disk.
    fn made_up_identity(path: &Path) -> Option<String> {
        path.ancestors().find_map(|a| std::fs::read_to_string(a.join(".fsid")).ok()).or_else(|| Some("system".to_owned()))
    }

    /// A plugin on the made-up drives, with nothing remembered yet.
    fn tags_on_made_up_drives() -> Tags {
        let mut tags = Tags::with(Registry::default(), BTreeMap::new());
        tags.identity = made_up_identity;
        tags
    }

    /// The folder `dir` read as when it is shown, until the reading has been taken in.
    fn read_folder_of(tags: &mut Tags, dir: &Path) {
        tags.directory_changed(dir);
        for _ in 0..500 {
            tags.poll();
            if tags.folder_rx.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the folder was not read");
    }

    fn checked(tags: &mut Tags) {
        tags.start_check(None, false);
        settle(tags);
    }

    fn settle(tags: &mut Tags) {
        for _ in 0..500 {
            tags.poll();
            if tags.check_rx.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the check did not end");
    }

    /// A folder for the made-up drives, on a file system that keeps tags; `None` (said) where there is none.
    fn base(name: &str) -> Option<PathBuf> {
        let Some((dir, _)) = crate::fs::xattr::tests::scratch_file(name) else {
            eprintln!("SKIPPED: this file system keeps no tags");
            return None;
        };
        Some(dir)
    }

    #[test]
    fn the_tags_of_a_file_on_a_drive_that_is_unplugged_stay_through_a_save_and_are_there_when_it_is_back() {
        let Some(base) = base("tags_unplugged") else { return };
        let (stick, stash) = (base.join("mnt/STICK"), base.join("stash"));
        drive(&stick, "A");
        let file = stick.join("photo.jpg");
        std::fs::write(&file, "x").unwrap();
        write_tags(&file, &["holiday".to_owned()]).unwrap();
        let mut tags = tags_on_made_up_drives();
        tags.remember(&file, vec!["holiday".to_owned()]);
        assert_eq!(tags.metas.0.get(&file).and_then(|m| m.fs.as_deref()), Some("A"));
        // Unplugged: away, kept, and kept through a save and a new start
        std::fs::rename(&stick, &stash).unwrap();
        checked(&mut tags);
        assert_eq!(tags.reach_of(&file), Reach::Away);
        let (registry, metas) = (serde_json::to_string(&tags.registry).unwrap(), serde_json::to_string(&tags.metas).unwrap());
        let mut again = Tags::with(serde_json::from_str(&registry).unwrap(), BTreeMap::new());
        again.metas = serde_json::from_str(&metas).unwrap();
        again.identity = made_up_identity;
        checked(&mut again);
        assert_eq!(again.registry.with_tag("holiday"), vec![file.clone()]);
        // Plugged in again
        std::fs::create_dir_all(base.join("mnt")).unwrap();
        std::fs::rename(&stash, &stick).unwrap();
        checked(&mut again);
        assert_eq!(again.reach_of(&file), Reach::Here);
        assert_eq!(again.registry.with_tag("holiday"), vec![file]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn another_drive_at_the_same_place_does_not_make_a_file_gone_and_a_deleted_one_on_its_own_drive_is() {
        let Some(base) = base("tags_other_drive") else { return };
        let stick = base.join("mnt/STICK");
        drive(&stick, "A");
        let (kept, deleted) = (stick.join("kept.txt"), stick.join("deleted.txt"));
        for f in [&kept, &deleted] {
            std::fs::write(f, "x").unwrap();
            write_tags(f, &["work".to_owned()]).unwrap();
        }
        let mut tags = tags_on_made_up_drives();
        tags.remember(&kept, vec!["work".to_owned()]);
        tags.remember(&deleted, vec!["work".to_owned()]);
        // Its own drive is there, the file is not: gone
        std::fs::remove_file(&deleted).unwrap();
        checked(&mut tags);
        assert_eq!((tags.reach_of(&kept), tags.reach_of(&deleted)), (Reach::Here, Reach::Gone));
        // Another drive at the same place, without the files: away, never gone; a folder read there forgets nothing
        std::fs::remove_dir_all(&stick).unwrap();
        drive(&stick, "B");
        read_folder_of(&mut tags, &stick);
        checked(&mut tags);
        assert_eq!((tags.reach_of(&kept), tags.reach_of(&deleted)), (Reach::Away, Reach::Away));
        tags.forget_gone();
        settle(&mut tags);
        assert_eq!(tags.registry.with_tag("work").len(), 2, "something was forgotten on another drive");
        // A file remembered before the file systems were known is never taken for gone
        drive(&base.join("old"), "C");
        let old = base.join("old/note.txt");
        tags.registry.record(&old, &["work".to_owned()]);
        checked(&mut tags);
        assert_eq!(tags.reach_of(&old), Reach::Away);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_file_that_cannot_be_read_or_whose_server_does_not_answer_keeps_its_tags() {
        let dir = std::env::temp_dir().join(format!("compression_tags_unclear_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (locked, server) = (dir.join("locked"), dir.join("server"));
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::create_dir_all(&server).unwrap();
        let mut tags = tags_on_made_up_drives();
        tags.registry.record(&locked.join("a.txt"), &["x".to_owned()]);
        tags.registry.record(&server.join("b.txt"), &["x".to_owned()]);
        for p in [locked.join("a.txt"), server.join("b.txt")] {
            tags.metas.0.insert(p, Meta { fs: Some("system".to_owned()), pending: false });
        }
        crate::fs::network::declare_remote_for_tests(&server);
        crate::fs::mount_health::tests::hang_under(&server);
        let root = unsafe { libc::geteuid() } == 0;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        tags.start_check(None, false);
        settle(&mut tags);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::fs::mount_health::tests::answer_again(&server);
        if !root {
            assert_eq!(tags.reach_of(&locked.join("a.txt")), Reach::Away, "a file that cannot be looked at is not gone");
        }
        assert_eq!(tags.reach_of(&server.join("b.txt")), Reach::Away, "a server that does not answer is not a file that is gone");
        assert_eq!(tags.registry.with_tag("x").len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_the_gone_files_needs_a_yes_forgets_only_their_tags_and_looks_again_first() {
        let Some(base) = base("tags_cleanup") else { return };
        let (a, b) = (base.join("mnt/A"), base.join("mnt/B"));
        drive(&a, "A");
        drive(&b, "B");
        let (gone, back, away) = (a.join("gone.txt"), a.join("back.txt"), b.join("away.txt"));
        for f in [&gone, &back, &away] {
            std::fs::write(f, "x").unwrap();
            write_tags(f, &["t".to_owned()]).unwrap();
        }
        let mut tags = tags_on_made_up_drives();
        for f in [&gone, &back, &away] {
            tags.remember(f, vec!["t".to_owned()]);
        }
        std::fs::remove_file(&gone).unwrap();
        std::fs::remove_file(&back).unwrap();
        std::fs::rename(&b, base.join("stash")).unwrap();
        checked(&mut tags);
        assert_eq!(tags.gone().len(), 2);
        assert_eq!(tags.count_text("t"), "0 files · 1 not available now · 2 no longer there");
        // Nothing happens before the person confirms
        assert_eq!(tags.registry.with_tag("t").len(), 3);
        // One comes back (with its tag) before the cleanup runs: it is looked at again and stays
        std::fs::write(&back, "x").unwrap();
        write_tags(&back, &["t".to_owned()]).unwrap();
        tags.forget_gone();
        settle(&mut tags);
        let left = tags.registry.with_tag("t");
        assert!(!left.contains(&gone), "the gone file is still remembered");
        assert!(left.contains(&back), "a file that came back was forgotten");
        assert!(left.contains(&away), "a file that is only away was forgotten");
        assert!(base.join("stash/away.txt").exists() && back.exists(), "a file was touched");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_tag_renamed_or_deleted_while_a_file_is_away_is_written_to_it_when_it_is_back_and_never_comes_back() {
        let Some(base) = base("tags_offline_edit") else { return };
        let (stick, stash) = (base.join("mnt/STICK"), base.join("stash"));
        drive(&stick, "A");
        let (here, away) = (base.join("here.txt"), stick.join("away.txt"));
        for f in [&here, &away] {
            std::fs::write(f, "x").unwrap();
            write_tags(f, &["old".to_owned()]).unwrap();
        }
        let mut tags = tags_on_made_up_drives();
        tags.remember(&here, vec!["old".to_owned()]);
        tags.remember(&away, vec!["old".to_owned()]);
        std::fs::rename(&stick, &stash).unwrap();
        checked(&mut tags);
        // Renamed while the stick is away: the file that is here is changed at once, the other in the index only
        tags.rename_tag("old", "new");
        assert!(tags.messages.is_empty(), "{:?}", tags.messages);
        assert_eq!(read_tags(&here).unwrap(), vec!["new".to_owned()]);
        assert_eq!(read_tags(&stash.join("away.txt")).unwrap(), vec!["old".to_owned()], "a file that is away was written");
        assert_eq!(tags.registry.with_tag("new").len(), 2);
        assert!(tags.metas.0.get(&away).is_some_and(|m| m.pending));
        // Back: the new name is written to it, and the old one does not come back, not even from a folder read
        std::fs::create_dir_all(base.join("mnt")).unwrap();
        std::fs::rename(&stash, &stick).unwrap();
        read_folder_of(&mut tags, &stick);
        assert!(tags.registry.with_tag("old").is_empty(), "a folder read brought the old name back");
        settle(&mut tags);
        checked(&mut tags);
        assert_eq!(read_tags(&away).unwrap(), vec!["new".to_owned()]);
        assert!(tags.registry.with_tag("old").is_empty());
        // Deleted while away: after the return the file has no tag, and the index forgets it
        std::fs::rename(&stick, &stash).unwrap();
        checked(&mut tags);
        tags.delete_tag("new");
        assert!(read_tags(&here).unwrap().is_empty());
        std::fs::rename(&stash, &stick).unwrap();
        checked(&mut tags);
        assert!(read_tags(&away).unwrap().is_empty(), "the deleted tag came back");
        assert!(tags.registry.0.is_empty(), "{:?}", tags.registry);
        assert!(tags.metas.0.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn case_folding_covers_non_ascii_names() {
        assert!(same_tag("Übung", "übung"));
        assert!(same_tag("Работа", "работа"));
        assert!(!same_tag("a", "b"));
    }

    #[test]
    fn parses_and_formats_the_comma_list() {
        assert_eq!(parse("work, home,,work ,Ünï".as_bytes()), vec!["work", "home", "Ünï"]);
        assert_eq!(format(&["a".to_owned(), "b c".to_owned()]), "a,b c");
        assert!(parse(b"").is_empty());
    }

    #[test]
    fn names_with_commas_are_rejected() {
        assert_eq!(clean_name("  work  ").as_deref(), Some("work"));
        assert_eq!(clean_name("a,b"), None);
        assert_eq!(clean_name("   "), None);
        assert_eq!(clean_name("line\nbreak"), None);
        assert_eq!(clean_name(&"x".repeat(MAX_NAME + 1)), None);
    }

    #[test]
    fn colors_fall_back_to_a_stable_palette_entry() {
        let tags = Tags::with(Registry::default(), BTreeMap::from([("work".to_owned(), [1, 2, 3])]));
        assert_eq!(tags.color("work"), [1, 2, 3]);
        assert_eq!(tags.color("Home"), tags.color("home"));
        assert!(PALETTE.contains(&tags.color("home")));
    }

    #[test]
    fn tags_round_trip_through_the_attribute() {
        let Some((dir, file)) = scratch_file("tags") else { return };
        assert!(read_tags(&file).unwrap().is_empty());
        write_tags(&file, &["work".to_owned(), "home".to_owned()]).unwrap();
        assert_eq!(read_tags(&file).unwrap(), vec!["work", "home"]);
        assert_eq!(xattr::get(&file, ATTR).unwrap().as_deref(), Some(&b"work,home"[..]));
        write_tags(&file, &[]).unwrap();
        assert!(read_tags(&file).unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn toggling_through_the_plugin_tags_and_untags_files() {
        let Some((dir, file)) = scratch_file("toggle") else { return };
        let mut tags = Tags::with(Registry::default(), BTreeMap::new());
        tags.run("toggle:work", std::slice::from_ref(&file), &dir);
        assert_eq!(read_tags(&file).unwrap(), vec!["work"]);
        assert_eq!(tags.registry.with_tag("WORK"), vec![file.clone()]);
        assert_eq!(tags.search_paths("tag:work"), Some(vec![file.clone()]));
        assert_eq!(tags.search_paths("work"), None);
        tags.run("toggle:work", std::slice::from_ref(&file), &dir);
        assert!(read_tags(&file).unwrap().is_empty());
        assert!(tags.registry.with_tag("work").is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn renaming_and_deleting_a_tag_reaches_every_file() {
        let Some((dir, file)) = scratch_file("rename") else { return };
        let mut colors = BTreeMap::new();
        colors.insert("work".to_owned(), [1, 2, 3]);
        let mut tags = Tags::with(Registry::default(), colors);
        tags.run("toggle:work", std::slice::from_ref(&file), &dir);
        tags.run("toggle:home", std::slice::from_ref(&file), &dir);
        tags.rename_tag("work", "job");
        assert_eq!(read_tags(&file).unwrap(), vec!["home", "job"]);
        assert_eq!(tags.color("job"), [1, 2, 3]);
        // Renaming onto a tag the file already has merges the two
        tags.rename_tag("home", "JOB");
        assert_eq!(read_tags(&file).unwrap(), vec!["job"]);
        tags.delete_tag("job");
        assert!(read_tags(&file).unwrap().is_empty());
        assert!(tags.registry.names().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_disk_without_attributes_gets_one_clear_message() {
        let err = std::io::Error::from_raw_os_error(libc::ENOTSUP);
        assert_eq!(describe(&err, "a"), tr("This disk can't store tags"));
    }

    #[test]
    fn registry_forgets_what_it_is_told_to_and_follows_renames() {
        let dir = std::env::temp_dir().join(format!("compression-tag-registry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a"), "x").unwrap();
        let mut registry = Registry::default();
        registry.record(&dir.join("sub/a"), &["one".to_owned()]);
        registry.record(&dir.join("gone"), &["one".to_owned(), "two".to_owned()]);
        assert_eq!(registry.with_tag("one"), vec![dir.join("gone"), dir.join("sub/a")]);
        assert!(registry.remove_tree(&dir.join("gone")));
        assert!(registry.record(&dir.join("sub/a"), &[]));
        assert!(registry.names().is_empty());

        registry.record(&dir.join("sub/a"), &["one".to_owned()]);
        assert!(registry.rename(&dir.join("sub"), &dir.join("moved")));
        assert_eq!(registry.with_tag("one"), vec![dir.join("moved/a")]);
        assert!(registry.remove_tree(&dir.join("moved")));
        assert!(registry.names().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folder_reading_skips_links_and_reuses_unchanged_entries() {
        let Some((dir, file)) = scratch_file("folder") else { return };
        write_tags(&file, &["work".to_owned()]).unwrap();
        std::os::unix::fs::symlink(&file, dir.join("link")).unwrap();
        let first = read_folder(&dir, &HashMap::new());
        assert_eq!(first[&file].tags, vec!["work"]);
        assert!(!first.contains_key(&dir.join("link")));
        // A cached entry with the same key is trusted without reading the attribute
        let mut cached = first.clone();
        cached.get_mut(&file).unwrap().tags = vec!["cached".to_owned()];
        assert_eq!(read_folder(&dir, &cached)[&file].tags, vec!["cached"]);
        // Changing the attribute changes ctime, so the entry is read again
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_tags(&file, &["work".to_owned(), "new".to_owned()]).unwrap();
        assert_eq!(read_folder(&dir, &cached)[&file].tags, vec!["work", "new"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn created_items_enter_the_registry() {
        let Some((dir, file)) = scratch_file("created") else { return };
        write_tags(&file, &["work".to_owned()]).unwrap();
        let mut tags = Tags::with(Registry::default(), BTreeMap::new());
        tags.file_op_done(&FileOpEvent::Created(vec![file.clone(), dir.clone()]));
        assert_eq!(tags.registry.with_tag("work"), vec![file]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scanning_finds_tagged_files_in_subfolders() {
        let Some((dir, file)) = scratch_file("scan") else { return };
        std::fs::create_dir_all(dir.join("deep/er")).unwrap();
        let deep = dir.join("deep/er/b.txt");
        std::fs::write(&deep, "x").unwrap();
        write_tags(&deep, &["x".to_owned()]).unwrap();
        let found = scan_tree(&dir);
        assert_eq!(found, vec![(deep, vec!["x".to_owned()])]);
        let _ = file;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn menu_hides_for_links_and_marks_the_tags_all_files_share() {
        let Some((dir, file)) = scratch_file("menu") else { return };
        write_tags(&file, &["work".to_owned()]).unwrap();
        let tags = Tags::with(Registry(BTreeMap::from([(file.clone(), vec!["work".to_owned()])])), BTreeMap::new());
        let item = FileItem::from_path(&file).unwrap();
        let entries = tags.menu_items(std::slice::from_ref(&item));
        let toggle = entries.iter().find(|e| e.id == "toggle:work").unwrap();
        assert_eq!(toggle.icon, icons::CHECK);
        assert!(entries.iter().any(|e| e.id == "new") && entries.iter().any(|e| e.id == "clear"));
        let mut link = item.clone();
        link.is_symlink = true;
        assert!(tags.menu_items(&[link]).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_tag_error_with_no_words_of_ours_says_only_its_number() {
        let text = describe(&std::io::Error::from_raw_os_error(libc::ENOTBLK), "photo.png");
        assert_eq!(text, "Tags of photo.png failed: System error 15");
        assert_eq!(describe(&std::io::Error::from_raw_os_error(libc::EIO), "a"), "Tags of a failed: The disk or the connection reported an error");
    }
}
