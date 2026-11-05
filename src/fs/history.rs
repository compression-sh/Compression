//! Undo and redo. Every change that can be taken back is stored as the list of edits that
//! reverse it. Applying an entry returns the entry that reverses *that*, which goes on the
//! other stack, so one mechanism serves both directions.
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::{ops, trash};

/// One step that takes back (part of) a change.
#[derive(Debug, Clone)]
pub enum Edit {
    /// Move to the trash.
    Trash(PathBuf),
    /// Bring a trashed item back to where it came from.
    Restore(PathBuf),
    /// Move or rename to the exact path.
    Move { from: PathBuf, to: PathBuf },
    /// Rename several items at once; they may swap names.
    Renames(Vec<(PathBuf, PathBuf)>),
    Chmod { path: PathBuf, mode: u32 },
}

#[derive(Debug, Clone)]
pub struct UndoEntry {
    /// What the change was, for the toast and the menu: "Move of 3 items".
    pub label: String,
    pub edits: Vec<Edit>,
    /// How often the steps of this entry were tried again after they had failed (see `apply_entry`).
    retries: u8,
}

impl UndoEntry {
    pub fn new(label: impl Into<String>, edits: Vec<Edit>) -> Self {
        Self { label: label.into(), edits, retries: 0 }
    }
}

const LIMIT: usize = 100;

#[derive(Default)]
pub struct History {
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
}

impl History {
    /// A new change: it can be undone, and anything that was undone before can no longer be redone.
    pub fn record(&mut self, entry: UndoEntry) {
        if entry.edits.is_empty() {
            return;
        }
        self.redo.clear();
        self.push_undo(entry);
    }

    pub fn push_undo(&mut self, entry: UndoEntry) {
        self.undo.push(entry);
        if self.undo.len() > LIMIT {
            self.undo.remove(0);
        }
    }

    pub fn push_redo(&mut self, entry: UndoEntry) {
        self.redo.push(entry);
        if self.redo.len() > LIMIT {
            self.redo.remove(0);
        }
    }

    pub fn pop_undo(&mut self) -> Option<UndoEntry> {
        self.undo.pop()
    }

    pub fn pop_redo(&mut self) -> Option<UndoEntry> {
        self.redo.pop()
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|e| e.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|e| e.label.as_str())
    }
}

/// Carry out an entry. Returns the entry that reverses it, plus the first error if some edit
/// failed (the edits that worked are still reversible).
/// How often the steps that failed are kept for another try. After that they are given up on, or an entry that can
/// never work would stay at the top of the history for good and hide everything under it.
const RETRIES: u8 = 2;

/// What `apply_entry` did.
pub struct Applied {
    /// What takes back (or, for an undo, repeats) the steps that were done; `None` when none was.
    pub inverse: Option<UndoEntry>,
    /// The steps that failed, unchanged, for the next try (the same entry on the same side of the history); `None`
    /// when all went well, or when they were tried too often.
    pub left: Option<UndoEntry>,
    /// Why the first step went wrong.
    pub error: Option<String>,
    pub failed: usize,
    pub total: usize,
    /// The failed steps are not kept (tried `RETRIES` times).
    pub given_up: bool,
}

/// What one step came to.
enum Step {
    Done(Edit),
    /// The move was made across file systems and its copy is whole, but the original could not be removed (all):
    /// the step counts as done, with a warning.
    DoneWithLeftovers(Edit, io::Error),
    Failed(io::Error),
}

/// Do the steps of `entry` in order. A step that fails does not stop the others; it is handed back in `left`, so that
/// it is not lost from the history and can be tried again.
pub fn apply_entry(entry: UndoEntry) -> Applied {
    let UndoEntry { label, edits, retries } = entry;
    let total = edits.len();
    let (mut inverse, mut kept, mut error) = (Vec::new(), Vec::new(), None);
    for edit in edits {
        let again = edit.clone();
        match step(edit) {
            Step::Done(reverse) => inverse.push(reverse),
            Step::DoneWithLeftovers(reverse, err) => {
                inverse.push(reverse);
                error.get_or_insert(crate::fs::describe::io_error(&err));
            }
            Step::Failed(err) => {
                error.get_or_insert(crate::fs::describe::io_error(&err));
                kept.push(again);
            }
        }
    }
    inverse.reverse();
    let failed = kept.len();
    let given_up = failed > 0 && retries >= RETRIES;
    let left = (failed > 0 && !given_up).then(|| UndoEntry { label: label.clone(), edits: kept, retries: retries + 1 });
    let inverse = (!inverse.is_empty()).then(|| UndoEntry::new(label, inverse));
    Applied { inverse, left, error, failed, total, given_up }
}

/// `apply_entry` without what failed: the entry that takes back what was done, and the first error.
#[cfg(test)]
pub fn apply(entry: UndoEntry) -> (Option<UndoEntry>, Option<String>) {
    let applied = apply_entry(entry);
    (applied.inverse, applied.error)
}

fn step(edit: Edit) -> Step {
    match edit {
        Edit::Trash(path) => done(trash::trash(&path).map(Edit::Restore)),
        Edit::Restore(trashed) => done(trash::restore(&trashed).map(Edit::Trash)),
        Edit::Move { from, to } => {
            if to.symlink_metadata().is_ok() {
                Step::Failed(io::Error::other(format!("“{}” already exists", to.display())))
            } else {
                if let Some(parent) = to.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let reverse = Edit::Move { from: to.clone(), to: from.clone() };
                match ops::move_path(&from, &to) {
                    Ok(()) => Step::Done(reverse),
                    // The copy is whole where the move was meant to go: the step is done, what is left behind is said
                    Err(err) if ops::is_copied_not_removed(&err) => Step::DoneWithLeftovers(reverse, err),
                    Err(err) => Step::Failed(err),
                }
            }
        }
        Edit::Renames(pairs) => done(rename_many(&pairs).map(|_| Edit::Renames(pairs.into_iter().map(|(a, b)| (b, a)).collect()))),
        Edit::Chmod { path, mode } => done(std::fs::metadata(&path).and_then(|meta| {
            let old = meta.permissions().mode() & 0o7777;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).map(|_| Edit::Chmod { path, mode: old })
        })),
    }
}

fn done(result: io::Result<Edit>) -> Step {
    result.map_or_else(Step::Failed, Step::Done)
}

/// Rename `(from, to)` pairs together, going through temporary names so names can be swapped.
pub fn rename_many(pairs: &[(PathBuf, PathBuf)]) -> io::Result<()> {
    let mut staged: Vec<(&Path, PathBuf, &Path)> = Vec::new();
    for (i, (from, to)) in pairs.iter().enumerate() {
        let temp = from.with_file_name(format!(".compression-rename-{}-{i}", std::process::id()));
        if let Err(err) = std::fs::rename(from, &temp) {
            for (old, temp, _) in staged.into_iter().rev() {
                let _ = std::fs::rename(temp, old);
            }
            return Err(err);
        }
        staged.push((from, temp, to));
    }
    let mut failure = None;
    for (from, temp, to) in staged {
        // A name taken since the staging must not be replaced
        if let Err(err) = crate::compat::rename_noreplace(&temp, to) {
            let _ = std::fs::rename(&temp, from);
            failure.get_or_insert(err);
        }
    }
    failure.map_or(Ok(()), Err)
}


#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("compression_history_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::compat::tests::without_support_under(&dir);
        dir
    }

    #[test]
    fn without_noreplace_support_renaming_several_is_refused_and_every_name_is_as_it_was() {
        let dir = dir("many");
        std::fs::write(dir.join("a"), "A").unwrap();
        std::fs::write(dir.join("b"), "B").unwrap();
        let err = rename_many(&[(dir.join("a"), dir.join("c")), (dir.join("b"), dir.join("d"))]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert_eq!((std::fs::read_to_string(dir.join("a")).unwrap(), std::fs::read_to_string(dir.join("b")).unwrap()), ("A".into(), "B".into()));
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"], "no staging name is left");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_noreplace_support_undoing_a_move_is_refused_with_the_reason_and_changes_nothing() {
        let dir = dir("undo");
        std::fs::write(dir.join("new_name"), "x").unwrap();
        let entry = UndoEntry::new("Renaming", vec![Edit::Move { from: dir.join("new_name"), to: dir.join("old_name") }]);
        let (inverse, error) = apply(entry);
        assert!(inverse.is_none());
        assert!(error.is_some_and(|e| e.contains("does not support moving or renaming")));
        assert_eq!(std::fs::read_to_string(dir.join("new_name")).unwrap(), "x");
        assert!(!dir.join("old_name").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rename_swap_and_chmod_round_trip() {
        let dir = std::env::temp_dir().join(format!("compression_history_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, "A").unwrap();
        std::fs::write(&b, "B").unwrap();

        let entry = UndoEntry::new("swap", vec![Edit::Renames(vec![(a.clone(), b.clone()), (b.clone(), a.clone())])]);
        let (inverse, error) = apply(entry);
        assert!(error.is_none());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "B");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "A");
        let (back, _) = apply(inverse.unwrap());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "A");
        assert!(back.is_some());

        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (inverse, _) = apply(UndoEntry::new("x", vec![Edit::Chmod { path: a.clone(), mode: 0o755 }]));
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o755);
        apply(inverse.unwrap());
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o644);

        let mut history = History::default();
        history.record(UndoEntry::new("one", vec![Edit::Trash(a.clone())]));
        let undone = history.pop_undo().unwrap();
        history.push_redo(undone);
        assert_eq!(history.redo_label(), Some("one"));
        history.record(UndoEntry::new("two", vec![Edit::Trash(b.clone())]));
        assert!(history.redo_label().is_none(), "a new change drops the redo stack");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn renaming_many_never_replaces_a_file_that_is_in_the_way() {
        let dir = std::env::temp_dir().join(format!("compression_many_noreplace_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (n, text) in [("a", "a"), ("b", "b"), ("taken", "keep me")] {
            std::fs::write(dir.join(n), text).unwrap();
        }
        let err = rename_many(&[(dir.join("a"), dir.join("taken"))]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(std::fs::read_to_string(dir.join("taken")).unwrap(), "keep me");
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "a", "put back under its name");
        assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".compression-rename")), "no staging name is left");

        // Names that trade places still do
        rename_many(&[(dir.join("a"), dir.join("b")), (dir.join("b"), dir.join("a"))]).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "b");
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "a");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_undo_that_meets_a_taken_name_says_so_in_plain_words() {
        let dir = std::env::temp_dir().join(format!("compression_history_words_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), "a").unwrap();
        std::fs::write(dir.join("taken"), "keep me").unwrap();
        let (inverse, error) = apply(UndoEntry::new("rename", vec![Edit::Renames(vec![(dir.join("a"), dir.join("taken"))])]));
        assert!(inverse.is_none());
        assert_eq!(error.as_deref(), Some("Already exists"));
        assert_eq!(std::fs::read_to_string(dir.join("taken")).unwrap(), "keep me");
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "a");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn plain_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("compression_history_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_step_that_fails_is_kept_for_another_try_and_the_other_steps_are_done() {
        let dir = plain_dir("kept");
        for (name, text) in [("a", "A"), ("b", "B"), ("c", "C"), ("taken", "keep me")] {
            std::fs::write(dir.join(name), text).unwrap();
        }
        let entry = UndoEntry::new(
            "three steps",
            vec![
                Edit::Move { from: dir.join("a"), to: dir.join("a2") },
                Edit::Move { from: dir.join("b"), to: dir.join("taken") },
                Edit::Chmod { path: dir.join("c"), mode: 0o600 },
            ],
        );
        let applied = apply_entry(entry);
        assert_eq!((applied.failed, applied.total, applied.given_up), (1, 3, false));
        assert!(applied.error.as_deref().is_some_and(|e| e.contains("already exists")), "{:?}", applied.error);
        // The first and the third step were done, the second was not
        assert!(dir.join("a2").exists() && !dir.join("a").exists());
        assert_eq!(std::fs::read_to_string(dir.join("taken")).unwrap(), "keep me", "nothing was replaced");
        let inverse = applied.inverse.expect("what was done can be taken back");
        assert_eq!(inverse.edits.len(), 2);
        assert_eq!(std::fs::metadata(dir.join("c")).unwrap().permissions().mode() & 0o777, 0o600);
        let left = applied.left.expect("the failed steps are kept");
        assert_eq!(left.edits.len(), 1);
        assert_eq!(left.label, "three steps");
        assert!(matches!(left.edits[0], Edit::Move { .. }), "in their order, unchanged");

        // The cause is put right, and the kept steps work
        std::fs::remove_file(dir.join("taken")).unwrap();
        let again = apply_entry(UndoEntry { edits: vec![left.edits[0].clone()], ..left });
        assert!(again.error.is_none() && again.left.is_none(), "{:?}", again.error);
        assert_eq!(std::fs::read_to_string(dir.join("taken")).unwrap(), "B");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn steps_that_keep_failing_are_given_up_on_after_a_few_tries() {
        let dir = plain_dir("given_up");
        let entry = || UndoEntry::new("never", vec![Edit::Restore(dir.join("not-in-the-trash"))]);
        let first = apply_entry(entry());
        let second = apply_entry(first.left.expect("kept the first time"));
        let third = apply_entry(second.left.expect("kept the second time"));
        assert!(third.left.is_none() && third.given_up, "after {RETRIES} tries it is dropped, or it would block the history for good");
        assert_eq!((third.failed, third.total), (1, 1));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_entry_that_goes_through_leaves_nothing_behind() {
        let dir = plain_dir("clean");
        std::fs::write(dir.join("a"), "A").unwrap();
        let applied = apply_entry(UndoEntry::new("one", vec![Edit::Move { from: dir.join("a"), to: dir.join("b") }]));
        assert!(applied.left.is_none() && applied.error.is_none() && applied.failed == 0 && !applied.given_up);
        assert_eq!(applied.inverse.unwrap().edits.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_undo_move_across_file_systems_with_leftovers_counts_as_done_and_says_what_is_left() {
        use std::os::unix::fs::PermissionsExt;
        let Some((root, shm)) = crate::fs::ops::tests::two_file_systems("history_leftover") else { return };
        std::fs::create_dir_all(root.join("tree/box")).unwrap();
        std::fs::write(root.join("tree/box/b.txt"), "two").unwrap();
        std::fs::set_permissions(root.join("tree/box"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let entry = UndoEntry::new("move", vec![Edit::Move { from: root.join("tree"), to: shm.join("tree") }]);
        let applied = apply_entry(entry);
        assert!(applied.error.as_deref().is_some_and(|e| e.contains("The copy is complete")), "{:?}", applied.error);
        assert_eq!(applied.failed, 0, "the step is done: the copy is whole where it was meant to go");
        assert!(applied.left.is_none(), "so it is not tried again");
        let inverse = applied.inverse.expect("and it can be taken back");
        assert!(matches!(&inverse.edits[0], Edit::Move { from, to } if *from == shm.join("tree") && *to == root.join("tree")));
        assert_eq!(std::fs::read_to_string(shm.join("tree/box/b.txt")).unwrap(), "two");
        crate::fs::ops::tests::unlock_and_remove(&root);
        crate::fs::ops::tests::unlock_and_remove(&shm);
    }
}
