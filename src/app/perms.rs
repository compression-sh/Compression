//! Changing permissions from the properties dialog.
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::fs::props::{mode_string, Properties};

/// What the permission card asks for after a frame.
#[derive(Default)]
pub(super) struct PermAction {
    pub new_mode: Option<u32>,
    pub apply_to_contents: bool,
}

/// Bits of the owner, group and others classes, for read, write and execute.
const CLASSES: [(&str, u32); 3] = [("Owner", 0o100), ("Group", 0o010), ("Others", 0o001)];
const KINDS: [(&str, u32); 3] = [("Read", 4), ("Write", 2), ("Execute", 1)];

/// The "Permissions" card: a grid of tick boxes, a switch for running the file as a program, and
/// for folders a button that hands the same permissions to everything inside.
pub(super) fn permission_card(ui: &mut egui::Ui, theme: &SerpantinumTheme, full: f32, mode: u32, is_dir: bool, can_edit: bool) -> PermAction {
    let mut action = PermAction::default();
    let mut mode = mode & 0o7777;
    material::section_label(ui, tr("Permissions"), theme);
    egui::Frame::new().fill(theme.surface1).corner_radius(egui::CornerRadius::same(material::radius_container())).show(ui, |ui| {
        ui.set_width(full);
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.add_space(4.0);
        if !is_dir
            && material::switch_row(ui, "perm_exec", icons::EXECUTABLE, tr("Allow running as a program"), theme, mode & 0o111 != 0).clicked()
            && can_edit
        {
            mode = if mode & 0o111 != 0 { mode & !0o111 } else { mode | ((mode & 0o444) >> 2) | if mode & 0o444 == 0 { 0o100 } else { 0 } };
            action.new_mode = Some(mode);
        }

        let label_w = 92.0;
        let col_w = ((full - label_w - 16.0) / 3.0).max(40.0);
        let (head, _) = ui.allocate_exact_size(egui::vec2(full, 28.0), egui::Sense::hover());
        for (i, (label, _)) in KINDS.iter().enumerate() {
            let label = match *label {
                "Read" => tr("Read"),
                "Write" => tr("Write"),
                _ => if is_dir { tr("Open") } else { tr("Execute") },
            };
            ui.painter().text(egui::pos2(head.left() + label_w + col_w * (i as f32 + 0.5), head.center().y), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(material::FONT_CAPTION), theme.subtext1);
        }
        for (name, shift) in CLASSES {
            let (row, _) = ui.allocate_exact_size(egui::vec2(full, 38.0), egui::Sense::hover());
            let label = match name {
                "Owner" => tr("Owner"),
                "Group" => tr("Group"),
                _ => tr("Others"),
            };
            ui.painter().text(egui::pos2(row.left() + 16.0, row.center().y), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(material::FONT_BODY), theme.text);
            for (i, (_, bit)) in KINDS.iter().enumerate() {
                let mask = bit * shift;
                let cell = egui::Rect::from_center_size(egui::pos2(row.left() + label_w + col_w * (i as f32 + 0.5), row.center().y), egui::vec2(30.0, 30.0));
                let resp = ui.put(cell, |ui: &mut egui::Ui| material::checkbox(ui, ("perm", shift, bit), mode & mask != 0, can_edit, theme));
                if resp.clicked() && can_edit {
                    mode ^= mask;
                    action.new_mode = Some(mode);
                }
            }
        }
        ui.add_space(4.0);
        let octal = format!("{:03o}  ·  {}", mode & 0o777, mode_string(mode, is_dir));
        let (note, _) = ui.allocate_exact_size(egui::vec2(full, 24.0), egui::Sense::hover());
        let text = if can_edit { octal } else { format!("{octal}  ·  {}", tr("Only the owner can change these")) };
        ui.painter().text(egui::pos2(note.left() + 16.0, note.center().y), egui::Align2::LEFT_CENTER, text, egui::FontId::proportional(material::FONT_CAPTION), theme.subtext1);
        if !(is_dir && can_edit) {
            ui.add_space(8.0);
        }
        if is_dir && can_edit {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                if material::button(ui, "perm_tree", Some(icons::FOLDER_COPY), tr("Apply to enclosed items"), theme, ButtonKind::Tonal, true).clicked() {
                    action.apply_to_contents = true;
                }
            });
            ui.add_space(8.0);
        }
    });
    action
}

/// Whether the person running Compression may change the permissions of this item.
pub(super) fn can_change(item: &crate::fs::file_item::FileItem) -> bool {
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    !item.is_symlink && (euid == 0 || euid == item.uid)
}

impl CompressionApp {
    /// Set the permission bits of one item and keep the way back.
    pub(super) fn set_mode(&mut self, props: &mut Properties, mode: u32) {
        let path = props.item.path.clone();
        let old = props.item.mode & 0o7777;
        match std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)) {
            Ok(()) => {
                props.item.mode = (props.item.mode & !0o7777) | mode;
                props.item.file_type = if !props.item.is_dir && mode & 0o111 != 0 && props.item.file_type != crate::fs::file_item::FileType::Executable {
                    crate::fs::file_item::FileType::Executable
                } else {
                    props.item.file_type
                };
                self.history.record(UndoEntry::new(trf("Permissions of {}", &[&props.item.name]), vec![Edit::Chmod { path, mode: old }]));
                self.reread_items();
            }
            Err(err) => self.set_toast(trf("Could not change permissions: {}", &[&crate::fs::describe::io_error(&err)])),
        }
    }

    /// Give everything inside a folder the permissions chosen for it: folders get the same bits,
    /// files the same read and write bits while keeping their own execute bits.
    pub(super) fn apply_mode_to_contents(&mut self, root: PathBuf, mode: u32) {
        let title = trf("Changing permissions in {}", &[&root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()]);
        let uses = [root.clone()];
        self.start_job(JobKind::Permissions, title, String::new(), None, &uses, move |w| {
            let mut edits = Vec::new();
            let mut skipped = Vec::new();
            let result = chmod_contents(&w.shared, &root, mode, &mut edits, &mut skipped);
            // Undoing goes through the list from the front, so it is the changes in reverse: the last one is undone first
            edits.reverse();
            chmod_outcome(result, edits, &skipped)
        });
    }
}

/// How the job ended. What was changed before a failure or a cancel stays changed, so it keeps its way back.
/// `skipped` are the items that were left alone (gone meanwhile, not ours to change, or a folder that cannot be read), and why.
fn chmod_outcome(result: std::io::Result<()>, edits: Vec<Edit>, skipped: &[(PathBuf, String)]) -> Outcome {
    let count = edits.len();
    let effect = if edits.is_empty() { Effect::None } else { Effect::Record(UndoEntry::new(trn(count, "Permissions of {} item", "Permissions of {} items"), edits)) };
    match result {
        Ok(()) => {
            let mut text = trn(count, "Changed permissions of {} item", "Changed permissions of {} items");
            if !skipped.is_empty() {
                text.push_str(" · ");
                text.push_str(&engine::left_out_note(skipped, trn(skipped.len(), "{} item could not be changed", "{} items could not be changed")));
            }
            Outcome::done(text).with(effect)
        }
        Err(err) if engine::is_cancel(&err) => Outcome::stopped().with(effect),
        Err(err) => Outcome::failed(crate::fs::describe::io_error(&err)).with(effect),
    }
}

/// `apply_mode_to_contents` for one folder: what is inside gets the permissions. A folder that keeps the
/// owner's read, write and execute bits is changed before what is in it, so that one nobody could enter
/// (mode 000) can be reached; any other is changed after, since it could no longer be entered. Each change is
/// added to `edits` as it is made, the first one first. An item that is gone by now, or a folder that still
/// cannot be read, is left alone and goes to `skipped`; the rest carries on.
fn chmod_contents(shared: &engine::Shared, dir: &Path, mode: u32, edits: &mut Vec<Edit>, skipped: &mut Vec<(PathBuf, String)>) -> std::io::Result<()> {
    use std::io::ErrorKind::{NotFound, PermissionDenied};
    for entry in std::fs::read_dir(dir)? {
        shared.checkpoint()?;
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(err) if err.kind() == NotFound => continue,
            Err(err) => return Err(err),
        };
        let meta = match path.symlink_metadata() {
            Ok(meta) => meta,
            Err(err) if err.kind() == NotFound => continue,
            Err(err) => return Err(err),
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let old = meta.permissions().mode() & 0o7777;
        let new = if meta.is_dir() { mode } else { (mode & 0o666) | (old & 0o111) };
        let first = meta.is_dir() && new & 0o700 == 0o700;
        let change = |edits: &mut Vec<Edit>| -> std::io::Result<()> {
            if new != old {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(new))?;
                edits.push(Edit::Chmod { path: path.clone(), mode: old });
            }
            Ok(())
        };
        if first {
            match change(edits) {
                Err(err) if err.kind() == NotFound => continue,
                // Not ours to change: left alone, and so is what is in it
                Err(err) if err.kind() == PermissionDenied => {
                    skipped.push((path.clone(), crate::fs::describe::io_error(&err)));
                    continue;
                }
                other => other?,
            }
        }
        if meta.is_dir() {
            match chmod_contents(shared, &path, mode, edits, skipped) {
                Err(err) if matches!(err.kind(), NotFound | PermissionDenied) => skipped.push((path.clone(), crate::fs::describe::io_error(&err))),
                other => other?,
            }
        }
        if !first {
            match change(edits) {
                Err(err) if err.kind() == NotFound => {}
                // A file of somebody else's, say: left alone, the rest carries on
                Err(err) if err.kind() == PermissionDenied => skipped.push((path.clone(), crate::fs::describe::io_error(&err))),
                other => other?,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(path: &Path) -> u32 {
        path.symlink_metadata().unwrap().permissions().mode() & 0o7777
    }

    /// Gives a folder its mode back when the test ends, so a failed one leaves nothing that cannot be removed.
    struct Unlock(PathBuf);
    impl Drop for Unlock {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap().parent().unwrap());
        }
    }

    fn tree(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("compression_perm_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("tree");
        std::fs::create_dir_all(root.join("locked")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("other/f.txt"), "x").unwrap();
        std::fs::set_permissions(root.join("other/f.txt"), std::fs::Permissions::from_mode(0o600)).unwrap();
        root
    }

    #[test]
    fn a_folder_nobody_can_enter_is_fixed_with_the_rest_and_undone_with_it() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = tree("locked");
        std::fs::write(root.join("locked/in.txt"), "x").unwrap();
        std::fs::set_permissions(root.join("locked/in.txt"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let _unlock = Unlock(root.join("locked"));
        let (mut edits, mut skipped) = (Vec::new(), Vec::new());
        let result = chmod_contents(&engine::Shared::default(), &root, 0o755, &mut edits, &mut skipped);
        assert!(result.is_ok(), "{result:?}");
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(mode_of(&root.join("locked")), 0o755);
        assert_eq!(mode_of(&root.join("locked/in.txt")), 0o644);
        assert_eq!(mode_of(&root.join("other/f.txt")), 0o644);
        edits.reverse();
        let (inverse, error) = crate::fs::history::apply(UndoEntry::new("x", edits));
        assert_eq!(error, None);
        assert!(inverse.is_some());
        assert_eq!(mode_of(&root.join("other/f.txt")), 0o600);
        assert_eq!(mode_of(&root.join("locked")), 0o000);
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(mode_of(&root.join("locked/in.txt")), 0o600);
    }

    #[test]
    fn a_folder_that_stays_unreadable_is_named_and_the_rest_is_still_done() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = tree("skip");
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let _unlock = Unlock(root.join("locked"));
        let (mut edits, mut skipped) = (Vec::new(), Vec::new());
        let result = chmod_contents(&engine::Shared::default(), &root, 0o544, &mut edits, &mut skipped);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].0.ends_with("locked"));
        assert_eq!(mode_of(&root.join("other/f.txt")), 0o444);
        assert_eq!(mode_of(&root.join("other")), 0o544);
        std::fs::set_permissions(root.join("other"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let outcome = chmod_outcome(result, edits, &skipped);
        assert!(outcome.ok && matches!(outcome.effect, Effect::Record(_)));
        assert!(outcome.message.contains("1 item could not be changed") && outcome.message.contains("locked"), "{}", outcome.message);
    }

    #[test]
    fn a_file_that_is_a_dangling_link_is_left_alone_and_the_rest_is_done() {
        let root = tree("link");
        std::os::unix::fs::symlink("nowhere", root.join("other/dangling")).unwrap();
        let _unlock = Unlock(root.join("locked"));
        let (mut edits, mut skipped) = (Vec::new(), Vec::new());
        let result = chmod_contents(&engine::Shared::default(), &root, 0o755, &mut edits, &mut skipped);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(mode_of(&root.join("other/f.txt")), 0o644);
        assert!(root.join("other/dangling").symlink_metadata().unwrap().file_type().is_symlink(), "the link itself is kept");
    }

    #[test]
    fn permissions_without_the_execute_bit_can_be_given_to_a_whole_tree_and_taken_back() {
        // root may enter any folder, so there is nothing to show
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = std::env::temp_dir().join(format!("compression_perms_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::write(root.join("sub/deeper/f.txt"), "x").unwrap();
        std::fs::write(root.join("a.sh"), "x").unwrap();
        std::fs::set_permissions(root.join("a.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();

        // Read and write for everyone, no execute: the folders cannot be entered afterwards
        let mut edits = Vec::new();
        let result = chmod_contents(&engine::Shared::default(), &root, 0o666, &mut edits, &mut Vec::new());
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(mode_of(&root.join("sub")), 0o666);
        // (Nothing below `sub` can be looked at now, which is the point)
        assert!(root.join("sub/deeper").symlink_metadata().is_err());
        // A file keeps its own execute bits
        assert_eq!(mode_of(&root.join("a.sh")), 0o777);
        assert_eq!(edits.len(), 4, "{edits:?}");

        // Undo, as the history runs it: the list as the job recorded it
        edits.reverse();
        let (inverse, error) = crate::fs::history::apply(UndoEntry::new("x", edits));
        assert_eq!(error, None);
        assert!(inverse.is_some());
        assert_eq!(mode_of(&root.join("sub")), 0o755);
        assert_eq!(mode_of(&root.join("sub/deeper/f.txt")), 0o644);
        assert_eq!(mode_of(&root.join("a.sh")), 0o755);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_cancelled_permission_job_keeps_the_way_back_to_what_it_changed() {
        let edit = |name: &str| Edit::Chmod { path: PathBuf::from(name), mode: 0o644 };
        let cancelled = || Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
        // Stopped after two changes: a stop, not a failure, and the two can be undone
        let stopped = chmod_outcome(cancelled(), vec![edit("/a"), edit("/b")], &[]);
        assert!(stopped.cancelled && !stopped.ok);
        assert!(matches!(stopped.effect, Effect::Record(_)));
        // Stopped before anything was changed: nothing to undo
        let early = chmod_outcome(cancelled(), Vec::new(), &[]);
        assert!(early.cancelled && matches!(early.effect, Effect::None));
        // Done, and failed for another reason, are what they were
        let done = chmod_outcome(Ok(()), vec![edit("/a")], &[]);
        assert!(done.ok && !done.cancelled && matches!(done.effect, Effect::Record(_)));
        let failed = chmod_outcome(Err(std::io::Error::from_raw_os_error(13)), vec![edit("/a")], &[]);
        assert!(!failed.ok && !failed.cancelled && matches!(failed.effect, Effect::Record(_)));
    }

    #[test]
    fn the_walk_stops_at_a_cancel_and_leaves_everything_as_it_was() {
        let root = std::env::temp_dir().join(format!("compression_perm_cancel_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/f.txt"), "x").unwrap();
        let before = mode_of(&root.join("sub/f.txt"));
        let shared = engine::Shared::default();
        shared.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut edits = Vec::new();
        let err = chmod_contents(&shared, &root, 0o700, &mut edits, &mut Vec::new()).unwrap_err();
        assert!(engine::is_cancel(&err));
        assert!(edits.is_empty());
        assert_eq!(mode_of(&root.join("sub/f.txt")), before);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_permission_job_has_a_cancel_button_and_other_jobs_still_have_none() {
        let mut engine = Engine::new(&egui::Context::default());
        for (kind, expected) in [(JobKind::Permissions, true), (JobKind::Other, false)] {
            let id = engine.spawn(kind, "t".into(), String::new(), |_| Outcome::done("x".into()));
            assert_eq!(engine.jobs.iter().find(|j| j.id == id).unwrap().controllable(), expected, "{kind:?}");
        }
    }
}
