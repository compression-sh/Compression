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
        Err(process::Failure::Io(e)) => trf("{} failed: {}", &[&label, &crate::fs::describe::io_error(&e)]),
        // Actions are never called off while they run
        Err(process::Failure::Cancelled) => trf("{} failed: {}", &[&label, &tr("Cancelled")]),
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
        let dir = self.dir.clone().ok_or_else(|| std::io::Error::other(tr("There is no folder for your actions")))?;
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
        let text = std::fs::read_to_string(path).map_err(|e| crate::fs::describe::io_error(&e))?;
        let def: Definition = serde_json::from_str(&text).map_err(|e| crate::fs::describe::json_error(&e))?;
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
                Err(e) => self.messages.push(trf("Couldn't create the action: {}", &[&crate::fs::describe::io_error(&e)])),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, is_dir: bool) -> FileItem {
        let mut item = FileItem::from_path(Path::new("/")).unwrap();
        item.name = name.to_owned();
        item.path = PathBuf::from("/x").join(name);
        item.is_dir = is_dir;
        item
    }

    #[test]
    fn filters() {
        let webp = Definition { extensions: vec!["JPG".into(), ".png".into()], ..Default::default() };
        assert!(webp.applies(&[item("a.jpg", false), item("b.PNG", false)]));
        assert!(!webp.applies(&[item("a.jpg", false), item("b.txt", false)]));
        assert!(!webp.applies(&[]));
        let dirs = Definition { dirs_only: true, ..Default::default() };
        assert!(dirs.applies(&[item("d", true)]));
        assert!(!dirs.applies(&[item("f", false)]));
        let files = Definition { files_only: true, single: true, ..Default::default() };
        assert!(files.applies(&[item("f", false)]));
        assert!(!files.applies(&[item("f", false), item("g", false)]));
        assert!(!files.applies(&[item("d", true)]));
    }

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn an_action_with_endless_output_is_stopped_at_the_limit() {
        let started = std::time::Instant::now();
        let result = run_capped(sh("while :; do echo xxxxxxxxxxxxxxxxxxxxxxxx; done"), 64 * 1024, Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(10), "it was not stopped");
        let defaults = Definition::default().limits();
        assert_eq!(message_for("Loud", result, defaults), trf("{} failed: {}", &[&"Loud", &trf("it printed more than {} MiB", &[&"1"])]));
    }

    #[test]
    fn an_action_that_hangs_is_stopped_at_the_time_limit() {
        let started = std::time::Instant::now();
        let result = run_capped(sh("sleep 30"), OUTPUT_LIMIT, Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(5), "it was not stopped");
        let defaults = Definition::default().limits();
        assert_eq!(message_for("Stuck", result, defaults), trf("{} failed: {}", &[&"Stuck", &trf("it took longer than {} s", &[&"30"])]));
    }

    #[test]
    fn short_actions_still_report_what_happened() {
        let defaults = Definition::default().limits();
        assert_eq!(message_for("Quick", run_capped(sh("echo fine"), OUTPUT_LIMIT, RUN_LIMIT), defaults), trf("Finished: {}", &[&"Quick"]));
        assert_eq!(message_for("Bad", run_capped(sh("echo first >&2; echo it broke >&2; exit 1"), OUTPUT_LIMIT, RUN_LIMIT), defaults), trf("{} failed: {}", &[&"Bad", &"it broke"]));
        let missing = message_for("Gone", run_capped(Command::new("/nonexistent/compression-action"), OUTPUT_LIMIT, RUN_LIMIT), defaults);
        assert!(missing.starts_with("Gone failed: "), "{missing}");
    }

    /// The definition a JSON file holds, read the way the actions folder reads it.
    fn defined(json: &str) -> Result<Definition, String> {
        let dir = std::env::temp_dir().join(format!("compression-actions-limits-{}-{}", std::process::id(), json.len()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), json).unwrap();
        let read = read_entry(&dir.join("a.json")).map(|entry| entry.expect("an action").def);
        let _ = std::fs::remove_dir_all(&dir);
        read
    }

    const DEFAULT_LIMITS: Limits = Limits { run: Duration::from_secs(30), output: 1 << 20 };

    #[test]
    fn without_limits_in_the_definition_the_defaults_stand() {
        assert_eq!(Definition::default().limits(), DEFAULT_LIMITS);
        assert_eq!(defined(r#"{"command":"true"}"#).unwrap().limits(), DEFAULT_LIMITS);
        assert_eq!(defined(r#"{"command":"true","timeout":null,"max_output_mb":null}"#).unwrap().limits(), DEFAULT_LIMITS);
        // A script has no definition to put them in
        assert_eq!(Entry { def: Definition::default(), label: "s", script: Some(PathBuf::from("/x")) }.def.limits(), DEFAULT_LIMITS);
    }

    #[test]
    fn an_action_can_ask_for_its_own_time_and_output_limit() {
        let def = defined(r#"{"command":"true","timeout":120,"max_output_mb":8}"#).unwrap();
        assert_eq!(def.limits(), Limits { run: Duration::from_secs(120), output: 8 << 20 });
        // Either one alone; the other stays at its default
        assert_eq!(defined(r#"{"command":"true","timeout":600}"#).unwrap().limits(), Limits { run: Duration::from_secs(600), output: 1 << 20 });
        assert_eq!(defined(r#"{"command":"true","max_output_mb":2}"#).unwrap().limits(), Limits { run: Duration::from_secs(30), output: 2 << 20 });
        // Fractions are fine inside the range, and the ends of it are in
        assert_eq!(defined(r#"{"command":"true","timeout":1.5,"max_output_mb":1.5}"#).unwrap().limits(), Limits { run: Duration::from_millis(1500), output: 3 << 19 });
        assert_eq!(defined(r#"{"command":"true","timeout":3600,"max_output_mb":64}"#).unwrap().limits(), Limits { run: Duration::from_secs(3600), output: 64 << 20 });
        assert_eq!(defined(r#"{"command":"true","timeout":1,"max_output_mb":1}"#).unwrap().limits(), DEFAULT_LIMITS.with_run(1));
    }

    impl Limits {
        fn with_run(self, secs: u64) -> Self {
            Self { run: Duration::from_secs(secs), ..self }
        }
    }

    #[test]
    fn limits_that_make_no_sense_fall_back_to_the_defaults() {
        for bad in ["0", "-5", "0.5", "3601", "100000", "1e12"] {
            let def = defined(&format!(r#"{{"command":"true","timeout":{bad},"max_output_mb":{bad}}}"#)).unwrap();
            assert_eq!(def.limits(), DEFAULT_LIMITS, "{bad}");
        }
        // A good one beside a bad one is still used
        let def = defined(r#"{"command":"true","timeout":90,"max_output_mb":-1}"#).unwrap();
        assert_eq!(def.limits(), Limits { run: Duration::from_secs(90), output: 1 << 20 });
        let def = defined(r#"{"command":"true","timeout":0,"max_output_mb":4}"#).unwrap();
        assert_eq!(def.limits(), Limits { run: Duration::from_secs(30), output: 4 << 20 });
        // A number that is no number is not a definition, as with any other field of the wrong kind
        assert!(defined(r#"{"command":"true","timeout":"soon"}"#).is_err());
        // Not a finite number at all
        assert_eq!(Definition { timeout: Some(f64::NAN), max_output_mb: Some(f64::INFINITY), ..Default::default() }.limits(), DEFAULT_LIMITS);
    }

    #[test]
    fn the_messages_name_the_limit_that_was_hit() {
        let limits = Limits { run: Duration::from_secs(90), output: 3 << 19 };
        assert_eq!(message_for("A", Err(process::Failure::TimedOut), limits), trf("{} failed: {}", &[&"A", &trf("it took longer than {} s", &[&"90"])]));
        assert_eq!(message_for("A", Err(process::Failure::TooMuchOutput), limits), trf("{} failed: {}", &[&"A", &trf("it printed more than {} MiB", &[&"1.5"])]));
    }

    #[test]
    fn a_running_action_is_stopped_at_its_own_time_limit() {
        let dir = std::env::temp_dir().join(format!("compression-actions-own-limit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), r#"{"name":"Slow","command":"sleep 30","timeout":1}"#).unwrap();
        let mut actions = Actions::in_dir_for(Some(dir.clone()), false);
        let started = Instant::now();
        actions.run("0", &[PathBuf::from("x")], &dir);
        let message = actions.done_rx.recv_timeout(Duration::from_secs(20)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(15), "it ran for its 30 seconds");
        assert_eq!(message, trf("{} failed: {}", &[&"Slow", &trf("it took longer than {} s", &[&"1"])]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_administrator_session_loads_no_actions() {
        let dir = std::env::temp_dir().join(format!("compression-actions-admin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), r#"{"name":"List","command":"true"}"#).unwrap();
        std::fs::write(dir.join("tool.sh"), "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(dir.join("tool.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut admin = Actions::in_dir_for(Some(dir.clone()), true);
        assert!(admin.entries.is_empty() && admin.failures.is_empty());
        // Nor later, when the folder changes, and nothing is written into it
        assert!(!admin.refresh(true));
        assert!(admin.entries.is_empty());
        assert!(admin.create_template().is_err());
        assert!(!dir.join("new-action.json").exists());
        assert_eq!(Actions::in_dir_for(Some(dir.clone()), false).entries.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_json_and_scripts_and_runs_them() {
        let dir = std::env::temp_dir().join(format!("compression-actions-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), r#"{"name":"List","command":"printf '%s\n' \"$@\" > out.txt; echo \"$COMPRESSION_CURRENT_DIR\" \"$SKI_CURRENT_DIR\" >> out.txt"}"#).unwrap();
        std::fs::write(dir.join("broken.json"), "not json").unwrap();
        std::fs::write(dir.join("notes.txt"), "not executable").unwrap();
        std::fs::write(dir.join("tool.sh"), "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(dir.join("tool.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let (entries, failures) = read_dir(&dir);
        let names: Vec<_> = entries.iter().map(|e| e.label).collect();
        assert_eq!(names, ["List", "tool"]);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].file.ends_with("broken.json"));
        assert!(!failures[0].reason.is_empty());

        let mut actions = Actions::in_dir(Some(dir.clone()));
        assert_eq!(actions.entries.len(), 2);
        assert!(!actions.busy());
        actions.run("0", &[PathBuf::from("one"), PathBuf::from("two words")], &dir);
        let message = actions.done_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(message.contains("List"), "{message}");
        let out = std::fs::read_to_string(dir.join("out.txt")).unwrap();
        assert_eq!(out, format!("one\ntwo words\n{0} {0}\n", dir.display()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("compression-actions-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn labels(actions: &Actions) -> Vec<&'static str> {
        actions.entries.iter().map(|e| e.label).collect()
    }

    #[test]
    fn a_new_file_appears_and_an_edit_changes_the_label_without_leaking_twice() {
        let dir = scratch("reload");
        let mut actions = Actions::in_dir(Some(dir.clone()));
        assert!(actions.entries.is_empty());
        assert!(!actions.refresh(true), "nothing changed yet");

        std::fs::write(dir.join("a.json"), r#"{"name":"First","command":"true"}"#).unwrap();
        assert!(actions.refresh(true));
        assert_eq!(labels(&actions), ["First"]);

        // An edit that keeps the size and lands in the same second must still be seen: bump the mtime
        std::fs::write(dir.join("a.json"), r#"{"name":"Second","command":"true"}"#).unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(dir.join("a.json")).unwrap().set_modified(later).unwrap();
        assert!(actions.refresh(true));
        assert_eq!(labels(&actions), ["Second"]);

        // Going back to a text seen before reuses the same leaked string
        let first_again = intern("First");
        std::fs::write(dir.join("a.json"), r#"{"name":"First","command":"true"}"#).unwrap();
        std::fs::File::options().write(true).open(dir.join("a.json")).unwrap().set_modified(later + Duration::from_secs(5)).unwrap();
        assert!(actions.refresh(true));
        assert!(std::ptr::eq(labels(&actions)[0], first_again));

        std::fs::remove_file(dir.join("a.json")).unwrap();
        assert!(actions.refresh(true));
        assert!(actions.entries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_files_are_reported_with_the_reason() {
        let dir = scratch("broken");
        std::fs::write(dir.join("bad.json"), "{ nope").unwrap();
        std::fs::write(dir.join("empty.json"), r#"{"name":"Empty","command":"  "}"#).unwrap();
        std::fs::write(dir.join("ok.json"), r#"{"command":"true"}"#).unwrap();
        let actions = Actions::in_dir(Some(dir.clone()));
        assert_eq!(labels(&actions), ["ok"], "the name falls back to the file name");
        let broken: Vec<_> = actions.failures.iter().map(|f| f.file.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(broken, ["bad.json", "empty.json"]);
        assert!(actions.failures.iter().all(|f| !f.reason.is_empty()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_action_writes_a_working_template_without_overwriting() {
        let dir = scratch("template");
        let mut actions = Actions::in_dir(Some(dir.join("actions")));
        let first = actions.create_template().unwrap();
        let second = actions.create_template().unwrap();
        assert_ne!(first, second);
        assert!(actions.failures.is_empty(), "{:?}", actions.failures);
        assert_eq!(actions.entries.len(), 2);
        assert_eq!(actions.entries[0].def.command, "echo \"$@\"");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filters_are_summarised() {
        let def = Definition { extensions: vec!["jpg".into(), ".png".into()], single: true, ..Default::default() };
        assert_eq!(def.filters(), ".jpg .png, one item");
        assert_eq!(Definition::default().filters(), "any selection");
    }

    #[test]
    fn an_action_that_could_not_run_or_was_stopped_is_told_in_words() {
        let limits = Limits { run: std::time::Duration::from_secs(30), output: 1 << 20 };
        let io = |code| message_for("Backup", Err(process::Failure::Io(std::io::Error::from_raw_os_error(code))), limits);
        assert_eq!(io(libc::EACCES), "Backup failed: Permission denied");
        assert_eq!(io(libc::ENOTBLK), "Backup failed: System error 15");
        assert_eq!(message_for("Backup", Err(process::Failure::Cancelled), limits), "Backup failed: Cancelled");
        // What the script itself printed stays as it printed it
        let out = Output { success: false, stdout: Vec::new(), stderr: b"cannot open the thing\n".to_vec() };
        assert_eq!(message_for("Backup", Ok(out), limits), "Backup failed: cannot open the thing");
    }

    #[test]
    fn an_action_file_with_a_typo_is_told_by_its_line_and_not_in_serdes_english() {
        let dir = std::env::temp_dir().join(format!("compression_actions_json_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("broken.json");
        std::fs::write(&file, "{\n  \"command\": \"ls\",\n  oops\n}").unwrap();
        assert_eq!(read_entry(&file).err().as_deref(), Some("not valid JSON (line 3)"));
        std::fs::write(&file, "{ \"command\": 5 }").unwrap();
        assert_eq!(read_entry(&file).err().as_deref(), Some("a value has the wrong type (line 1)"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
