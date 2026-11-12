//! Watches the files a theme can come from, so a color generator that rewrites them is followed
//! while Compression runs: the user's theme files and Serpantinum's `qs_colors.json`.
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use eframe::egui;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Writers truncate and then fill a file, so a change is read once things have been quiet this long.
const SETTLE: Duration = Duration::from_millis(150);

pub struct ThemeWatch {
    _watcher: RecommendedWatcher,
    events: Receiver<()>,
    due: Option<Instant>,
}

impl ThemeWatch {
    pub fn new(ctx: &egui::Context) -> Option<Self> {
        let (tx, events) = channel();
        let repaint = ctx.clone();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            let touched = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_));
            let ours = event.paths.iter().any(|p| p.extension().is_some_and(|e| e == "json"));
            if touched && ours && tx.send(()).is_ok() {
                crate::app::wake_windows(&repaint);
            }
        })
        .ok()?;
        // The folders, not the files: generators replace a file by renaming a new one over it
        let mut any = false;
        for dir in folders() {
            any |= watcher.watch(&dir, RecursiveMode::NonRecursive).is_ok();
        }
        any.then_some(Self { _watcher: watcher, events, due: None })
    }

    /// True once a watched file has changed and stopped changing; asks for another frame while waiting.
    pub fn changed(&mut self, ctx: &egui::Context) -> bool {
        if self.events.try_iter().count() > 0 {
            self.due = Some(Instant::now() + SETTLE);
        }
        match self.due {
            Some(at) if Instant::now() >= at => {
                self.due = None;
                true
            }
            Some(at) => {
                ctx.request_repaint_after(at - Instant::now());
                false
            }
            None => false,
        }
    }
}

fn folders() -> Vec<PathBuf> {
    let themes = crate::ui::theme::user_themes_dir();
    // The themes folder may not exist before the first theme is saved
    if let Some(dir) = &themes
        && !crate::fs::admin::is_admin()
    {
        let _ = std::fs::create_dir_all(dir);
    }
    folders_of(themes, &crate::ui::theme::serpantinum_dirs())
}

/// The folders that exist among the themes folder and Serpantinum's.
fn folders_of(themes: Option<PathBuf>, serpantinum: &crate::ui::theme::SerpantinumDirs) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = themes.into_iter().chain(serpantinum.all().cloned()).collect();
    out.retain(|d| Path::new(d).is_dir());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{serpantinum_dirs, SerpantinumDirs};

    #[test]
    fn the_watched_folders_are_the_ones_serpantinum_is_read_from() {
        let base = std::env::temp_dir().join(format!("compression_watch_folders_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (themes, xdg, fixed, absent) = (base.join("themes"), base.join("xdg/serpantinum"), base.join("home/.config/serpantinum"), base.join("nope"));
        for dir in [&themes, &xdg, &fixed] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let dirs = SerpantinumDirs { state: vec![absent.clone()], config: vec![xdg.clone(), fixed.clone()] };
        assert_eq!(folders_of(Some(themes.clone()), &dirs), vec![themes.clone(), xdg, fixed]);
        // A folder that is not there is not watched, and no themes folder is fine
        assert_eq!(folders_of(None, &SerpantinumDirs { state: vec![absent], config: Vec::new() }), Vec::<PathBuf>::new());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn serpantinums_folders_follow_the_xdg_variables_of_the_environment() {
        // The tests run with these set (see the isolated environment); the fixed folders come after
        let dirs = serpantinum_dirs();
        for (var, list) in [("XDG_STATE_HOME", &dirs.state), ("XDG_CONFIG_HOME", &dirs.config)] {
            if let Some(value) = std::env::var_os(var).filter(|v| !v.is_empty()) {
                assert_eq!(list.first(), Some(&Path::new(&value).join("serpantinum")), "{var}");
            }
        }
    }

    #[test]
    fn folders_watches_the_themes_folder_and_the_folders_serpantinum_is_read_from() {
        use crate::ui::theme::{tests::Reading, SERPANTINUM_DIRS_FOR_TESTS};
        let reading = Reading::new("watch_wiring");
        let serpantinum = reading.0.0.join("serpantinum");
        std::fs::create_dir_all(&serpantinum).unwrap();
        SERPANTINUM_DIRS_FOR_TESTS.with(|d| *d.borrow_mut() = Some(SerpantinumDirs { state: Vec::new(), config: vec![serpantinum.clone()] }));
        let watched = folders();
        SERPANTINUM_DIRS_FOR_TESTS.with(|d| *d.borrow_mut() = None);
        assert!(watched.contains(&serpantinum), "{watched:?}");
        assert!(watched.contains(&reading.0.0), "{watched:?}");
    }
}
