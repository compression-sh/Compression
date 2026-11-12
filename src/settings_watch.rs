//! Watches `settings.json` and `managed.json` (laid over it, see `settings::managed_file`), so an edit made by hand,
//! by another tool or by home-manager takes effect while Compression runs.
//!
//! inotify tells when a writer is finished: `IN_CLOSE_WRITE` for a file written in place and
//! `IN_MOVED_TO` for one renamed over the old, so the file is read once, complete, with no waiting.
//! Compression's own saves are such renames (`fs::atomic`); their temporary files have other names
//! and are not noticed.
use std::sync::mpsc::{channel, Receiver};

use eframe::egui;
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

pub struct SettingsWatch {
    _watcher: RecommendedWatcher,
    events: Receiver<()>,
}

impl SettingsWatch {
    pub fn new(ctx: &egui::Context) -> Option<Self> {
        let file = crate::settings::file()?;
        let dir = file.parent()?.to_path_buf();
        let names = [file.file_name()?.to_owned(), std::ffi::OsString::from("managed.json")];
        // As administrator the folder belongs to someone else's home: it is watched if it is there, never made
        if !dir.is_dir() && (crate::fs::admin::is_admin() || std::fs::create_dir_all(&dir).is_err()) {
            return None;
        }
        let (tx, events) = channel();
        let repaint = ctx.clone();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            // home-manager puts `managed.json` in place as a new link: made, not written
            let managed_made = matches!(event.kind, EventKind::Create(_)) && event.paths.iter().any(|p| p.file_name() == Some(names[1].as_os_str()));
            if (finished_writing(&event.kind) && event.paths.iter().any(|p| names.iter().any(|n| p.file_name() == Some(n.as_os_str()))) || managed_made) && tx.send(()).is_ok() {
                crate::app::wake_windows(&repaint);
            }
        })
        .ok()?;
        // The folder, not the file: editors save by renaming a new file over the old one
        watcher.watch(&dir, RecursiveMode::NonRecursive).ok()?;
        Some(Self { _watcher: watcher, events })
    }

    /// True when the file was written since the last call.
    pub fn changed(&self) -> bool {
        self.events.try_iter().count() > 0
    }
}

fn finished_writing(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(AccessKind::Close(AccessMode::Write))
            | EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Both | RenameMode::Any))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::CreateKind;

    #[test]
    fn only_a_finished_write_counts() {
        assert!(finished_writing(&EventKind::Access(AccessKind::Close(AccessMode::Write))));
        assert!(finished_writing(&EventKind::Modify(ModifyKind::Name(RenameMode::To))));
        assert!(finished_writing(&EventKind::Modify(ModifyKind::Name(RenameMode::Both))));
        // Half way: the file was only made, or its old name went away
        assert!(!finished_writing(&EventKind::Create(CreateKind::File)));
        assert!(!finished_writing(&EventKind::Modify(ModifyKind::Name(RenameMode::From))));
        assert!(!finished_writing(&EventKind::Access(AccessKind::Close(AccessMode::Read))));
    }
}
