//! Custom actions: your own commands in the context menu. Every `*.json` file and every
//! executable script in `~/.config/compression/actions/` is one entry.
//!
//! ```json
//! { "name": "Convert to WebP", "command": "for f in \"$@\"; do cwebp \"$f\" -o \"${f%.*}.webp\"; done",
//!   "extensions": ["jpg", "png"] }
//! ```
//!
//! `command` runs in `sh -c` inside the open folder with the selected files as arguments (`"$@"`).
//! Both kinds also get `COMPRESSION_SELECTED_FILES` (one path per line) and `COMPRESSION_CURRENT_DIR`,
//! and the same two as `SKI_SELECTED_FILES` and `SKI_CURRENT_DIR`, so scripts written for Ski keep working.
//! Optional filters: `extensions`, `dirs_only`, `files_only`, `single` (exactly one item).
//! Optional limits: `timeout` (seconds, 1 to 3600, default 30) after which the action is stopped, and
//! `max_output_mb` (MiB, 1 to 64, default 1) it may print on each of its two streams. A value outside
//! these ranges (or not a number that fits) is not used: the default stands.
//!
//! The folder is read again whenever a file in it changes, so edits show up without a restart.
//! Files that can't be used are listed on the plugin's Settings page with the reason.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use serde::Deserialize;

use super::{MenuItem, Plugin, Reaction, intern};
use crate::fs::file_item::FileItem;
use crate::fs::process::{self, Output, run_capped};
use crate::i18n::{tr, trf};
use crate::ui::icons;
use crate::ui::material;
use crate::ui::theme::SerpantinumTheme;

/// How often the folder is checked for changes while the window is being drawn.
const CHECK_EVERY: Duration = Duration::from_millis(1000);

const TEMPLATE: &str = "{\n  \"name\": \"New action\",\n  \"command\": \"echo \\\"$@\\\"\",\n  \"extensions\": []\n}\n";

#[derive(Debug, Deserialize, Default, Clone, PartialEq)]
#[serde(default)]
pub struct Definition {
    pub name: String,
    pub command: String,
    pub extensions: Vec<String>,
    pub dirs_only: bool,
    pub files_only: bool,
    pub single: bool,
    /// Seconds the action may run; see `Limits`.
    pub timeout: Option<f64>,
    /// MiB the action may print on each stream; see `Limits`.
    pub max_output_mb: Option<f64>,
}

struct Entry {
    def: Definition,
    /// Menu labels are `&'static str`; `intern` leaks each distinct text once, so reloading is free.
    label: &'static str,
    /// A script run by its own path instead of through `sh -c`.
    script: Option<PathBuf>,
}

/// A file in the actions folder that could not be used, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub file: PathBuf,
    pub reason: String,
}

/// The folder and every file in it with its modification time: when it differs, the folder is read again.
/// Each path with its modification time and mode: `chmod +x` changes only the mode.
type Stamp = Vec<(PathBuf, Option<SystemTime>, Option<u32>)>;

pub struct Actions {
    entries: Vec<Entry>,
    failures: Vec<Failure>,
    dir: Option<PathBuf>,
    stamp: Stamp,
    checked: Instant,
    messages: Vec<String>,
    done_tx: mpsc::Sender<String>,
    done_rx: mpsc::Receiver<String>,
    /// An action finished, and may have changed the files in the open folder.
    finished: Arc<AtomicBool>,
    running: Arc<AtomicUsize>,
}

/// The most an action may print on each of its two streams before it is stopped, unless it says otherwise.
const OUTPUT_LIMIT: usize = 1 << 20;
/// How long an action may run before it is stopped, unless it says otherwise.
const RUN_LIMIT: Duration = Duration::from_secs(30);
/// What an action may ask for: more than this is not granted.
const MAX_TIMEOUT_SECS: f64 = 3600.0;
const MAX_OUTPUT_MIB: f64 = 64.0;

/// How long an action may run and how much it may print on each stream.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Limits {
    run: Duration,
    output: usize,
}

impl Definition {
    /// The limits this action runs under: its own `timeout` and `max_output_mb` where they are
    /// sensible, the defaults (30 seconds, 1 MiB) where they are missing or out of range.
    fn limits(&self) -> Limits {
        let sensible = |value: Option<f64>, most: f64| value.filter(|v| v.is_finite() && (1.0..=most).contains(v));
        let run = sensible(self.timeout, MAX_TIMEOUT_SECS).map_or(RUN_LIMIT, Duration::from_secs_f64);
        let output = sensible(self.max_output_mb, MAX_OUTPUT_MIB).map_or(OUTPUT_LIMIT, |mib| (mib * (1 << 20) as f64) as usize);
        Limits { run, output }
    }
}

/// A limit as shown: `30`, `1.5`.
fn shown(value: f64) -> String {
    if value.fract() == 0.0 { format!("{}", value as u64) } else { format!("{value}") }
}

/// The toast for an action that has ended, or was stopped.
fn message_for(label: &str, result: Result<Output, process::Failure>, limits: Limits) -> String {
    match result {
        Ok(out) if out.success => trf("Finished: {}", &[&label]),
        Ok(out) => {
            let error = String::from_utf8_lossy(&out.stderr);
            let error = error.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_owned();
            trf("{} failed: {}", &[&label, &error])
        }
        Err(process::Failure::TooMuchOutput) => trf("{} failed: {}", &[&label, &trf("it printed more than {} MiB", &[&shown(limits.output as f64 / (1 << 20) as f64)])]),
        Err(process::Failure::TimedOut) => trf("{} failed: {}", &[&label, &trf("it took longer than {} s", &[&shown(limits.run.as_secs_f64())])]),
        Err(process::Failure::Io(e)) => trf("{} failed: {}", &[&label, &e]),
        // Actions are never called off while they run
        Err(process::Failure::Cancelled) => trf("{} failed: {}", &[&label, &"cancelled"]),
    }
}

pub fn dir() -> Option<PathBuf> {
    crate::fs::admin::config_override().or_else(dirs::config_dir).map(|d| d.join("compression").join("actions"))
}

impl Actions {
    pub fn load() -> Self {
        Self::in_dir(dir())
    }

    fn in_dir(dir: Option<PathBuf>) -> Self {
        Self::in_dir_for(dir, crate::fs::admin::is_admin())
    }

    /// In an administrator session no action is read: the scripts and commands in that folder
    /// are the person's own to write, and would run as root.
    fn in_dir_for(dir: Option<PathBuf>, admin: bool) -> Self {
        let dir = dir.filter(|_| !admin);
        let (done_tx, done_rx) = mpsc::channel();
        let mut actions = Self {
            entries: Vec::new(),
            failures: Vec::new(),
            dir,
            stamp: Vec::new(),
            checked: Instant::now(),
            messages: Vec::new(),
            done_tx,
            done_rx,
            finished: Arc::default(),
            running: Arc::default(),
        };
        actions.reload();
        actions
    }

    fn reload(&mut self) {
        let Some(dir) = self.dir.clone() else { return };
        self.stamp = stamp(&dir);
        let (entries, failures) = read_dir(&dir);
        self.entries = entries;
        self.failures = failures;
    }

    /// Reads the folder again when a file in it was added, removed or edited. Returns whether it did.
    fn refresh(&mut self, force: bool) -> bool {
        if !force && self.checked.elapsed() < CHECK_EVERY {
            return false;
        }
        self.checked = Instant::now();
        let Some(dir) = self.dir.clone() else { return false };
        if stamp(&dir) == self.stamp {
            return false;
        }
        self.reload();
        true
    }

    /// Writes a starting point into the folder and returns its path.
    fn create_template(&mut self) -> std::io::Result<PathBuf> {
        let dir = self.dir.clone().ok_or_else(|| std::io::Error::other("no config folder"))?;
        std::fs::create_dir_all(&dir)?;
        let path = (1..).map(|n| dir.join(if n == 1 { "new-action.json".to_owned() } else { format!("new-action-{n}.json") })).find(|p| !p.exists()).expect("endless range");
        std::fs::write(&path, TEMPLATE)?;
        self.reload();
        Ok(path)
    }
}

fn stamp(dir: &Path) -> Stamp {
    let modified = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.modified().ok(), m.permissions().mode()));
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    files.sort();
    let split = |p: &Path| modified(p).map_or((None, None), |(time, mode)| (time, Some(mode)));
    let (time, mode) = split(dir);
    let mut stamp = vec![(dir.to_path_buf(), time, mode)];
    stamp.extend(files.into_iter().map(|f| {
        let (time, mode) = split(&f);
        (f, time, mode)
    }));
    stamp
}

fn read_dir(dir: &Path) -> (Vec<Entry>, Vec<Failure>) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    files.sort();
    let (mut entries, mut failures) = (Vec::new(), Vec::new());
    for path in files {
        match read_entry(&path) {
            Ok(Some(entry)) => entries.push(entry),
            Ok(None) => {}
            Err(reason) => failures.push(Failure { file: path, reason }),
        }
    }
    (entries, failures)
}

/// `Ok(None)` for files that are simply not actions (notes, folders, scripts that aren't executable).
fn read_entry(path: &Path) -> Result<Option<Entry>, String> {
    let Ok(meta) = std::fs::metadata(path) else { return Ok(None) };
    if !meta.is_file() {
        return Ok(None);
    }
    let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else { return Ok(None) };
    let is_json = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
    let (def, script) = if is_json {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let def: Definition = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if def.command.trim().is_empty() {
            return Err(tr("\"command\" is empty").to_owned());
        }
        (def, None)
    } else if meta.permissions().mode() & 0o111 != 0 {
        (Definition::default(), Some(path.to_path_buf()))
    } else {
        return Ok(None);
    };
    let name = if def.name.trim().is_empty() { stem } else { def.name.clone() };
    Ok(Some(Entry { label: intern(&name), def, script }))
}

impl Definition {
    /// The filters as one line for the settings page.
    fn filters(&self) -> String {
        let mut parts = Vec::new();
        if !self.extensions.is_empty() {
            parts.push(self.extensions.iter().map(|e| format!(".{}", e.trim_start_matches('.'))).collect::<Vec<_>>().join(" "));
        }
        if self.dirs_only {
            parts.push(tr("folders only").to_owned());
        }
        if self.files_only {
            parts.push(tr("files only").to_owned());
        }
        if self.single {
            parts.push(tr("one item").to_owned());
        }
        if parts.is_empty() { tr("any selection").to_owned() } else { parts.join(", ") }
    }
}

impl Definition {
    pub fn applies(&self, selection: &[FileItem]) -> bool {
        if selection.is_empty() || (self.single && selection.len() != 1) {
            return false;
        }
        selection.iter().all(|item| {
            if self.dirs_only && !item.is_dir || self.files_only && item.is_dir {
                return false;
            }
            self.extensions.is_empty()
                || item.path.extension().is_some_and(|ext| self.extensions.iter().any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext.to_string_lossy())))
        })
    }
}

impl Plugin for Actions {
    fn id(&self) -> &'static str {
        "actions"
    }

    fn name(&self) -> &'static str {
        tr("Custom actions")
    }

    fn description(&self) -> &'static str {
        tr("Adds your scripts and commands from ~/.config/compression/actions to the context menu")
    }

    fn icon(&self) -> &'static str {
        icons::TERMINAL
    }

    fn directory_changed(&mut self, _dir: &Path) {
        self.refresh(true);
    }

    fn poll(&mut self) -> bool {
        self.refresh(false)
    }

    fn settings(&mut self, ui: &mut egui::Ui, theme: &SerpantinumTheme) {
        if self.entries.is_empty() && self.failures.is_empty() {
            material::hint_label_inset(ui, tr("No actions yet. Choose New action to start from a template."), theme, 14);
        }
        for entry in &self.entries {
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.label(egui::RichText::new(entry.label).size(material::FONT_BODY).color(theme.text));
                ui.label(egui::RichText::new(entry.def.filters()).size(material::FONT_BODY - 1.0).color(theme.subtext1));
            });
        }
        if !self.failures.is_empty() {
            ui.add_space(6.0);
            material::hint_label_inset(ui, tr("These files can't be used:"), theme, 14);
            for failure in &self.failures {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(14.0);
                    let name = failure.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    ui.label(egui::RichText::new(name).size(material::FONT_BODY).color(theme.red));
                    ui.label(egui::RichText::new(&failure.reason).size(material::FONT_BODY - 1.0).color(theme.subtext1));
                });
            }
        }
        ui.add_space(6.0);
        let create = material::control_row(ui, theme, material::row_hint("", theme), |ui| {
            material::button(ui, "new_action", Some(icons::ADD), tr("New action"), theme, material::ButtonKind::Tonal, self.dir.is_some()).clicked()
        });
        if create {
            match self.create_template() {
                Ok(path) => self.messages.push(trf("Created {}", &[&path.display()])),
                Err(e) => self.messages.push(trf("Couldn't create the action: {}", &[&e])),
            }
        }
        ui.add_space(4.0);
    }

    fn menu_items(&self, selection: &[FileItem]) -> Vec<MenuItem> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.def.applies(selection))
            .map(|(i, e)| MenuItem::new(i.to_string(), e.label, icons::TERMINAL).group(tr("Actions")))
            .collect()
    }

    fn run(&mut self, id: &str, paths: &[PathBuf], dir: &Path) -> Reaction {
        let Some(entry) = id.parse::<usize>().ok().and_then(|i| self.entries.get(i)) else { return Reaction::None };
        let mut command = match &entry.script {
            Some(script) => {
                let mut c = Command::new(script);
                c.args(paths);
                c
            }
            None => {
                let mut c = Command::new("sh");
                c.arg("-c").arg(&entry.def.command).arg("compression-action").args(paths);
                c
            }
        };
        let selected = paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("\n");
        command.current_dir(dir).env("COMPRESSION_SELECTED_FILES", &selected).env("COMPRESSION_CURRENT_DIR", dir).env("SKI_SELECTED_FILES", &selected).env("SKI_CURRENT_DIR", dir).stdin(std::process::Stdio::null());
        let (tx, label, running, finished) = (self.done_tx.clone(), entry.label, Arc::clone(&self.running), Arc::clone(&self.finished));
        let limits = entry.def.limits();
        running.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let message = message_for(label, run_capped(command, limits.output, limits.run), limits);
            let _ = tx.send(message);
            finished.store(true, Ordering::Relaxed);
            running.fetch_sub(1, Ordering::Relaxed);
        });
        Reaction::None
    }

    fn reaction(&mut self) -> Reaction {
        if self.finished.swap(false, Ordering::Relaxed) { Reaction::Refresh } else { Reaction::None }
    }

    fn busy(&self) -> bool {
        self.running.load(Ordering::Relaxed) > 0
    }

    fn messages(&mut self) -> Vec<String> {
        let mut out = std::mem::take(&mut self.messages);
        out.extend(self.done_rx.try_iter());
        out
    }
}

