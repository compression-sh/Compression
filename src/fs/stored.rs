//! The small files Compression keeps for the person: pins, bookmarks, folder icons and glyphs, the tag index and tag
//! colors, "Open with" choices, the history of opened files. A file that cannot be used is never quietly replaced by an
//! empty one the next save would write over it:
//!
//! - one that is not there is simply empty;
//! - one that is there but cannot be read as what it should hold (broken, half edited by hand) is kept aside, its bytes
//!   as they are, as `<name>.broken` (or `.broken.1`, … when that is taken; nothing is written over), and the list
//!   starts empty;
//! - one that cannot be read at all (no permission), or that cannot be kept aside, stays where it is and is not
//!   written: a save to it is refused.
//!
//! Each of these, and every save that fails, is said once (`take_notes`, which the window shows), not again until the
//! same file has been saved well.
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::i18n::trf;

#[derive(Default)]
struct State {
    /// Files that must not be written, and why.
    held: HashMap<PathBuf, String>,
    /// The failure last said for a file, so a failure that repeats is not said again.
    said: HashMap<PathBuf, String>,
    /// What is still to be shown.
    notes: Vec<String>,
}

#[cfg(not(test))]
static STATE: std::sync::Mutex<Option<State>> = std::sync::Mutex::new(None);

// A test has its own state, so tests that run side by side cannot see each other's notes
#[cfg(test)]
thread_local! {
    static STATE: std::cell::RefCell<Option<State>> = const { std::cell::RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    #[cfg(not(test))]
    return f(STATE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(State::default));
    #[cfg(test)]
    return STATE.with(|s| f(s.borrow_mut().get_or_insert_with(State::default)));
}

fn name_of(file: &Path) -> String {
    file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| file.display().to_string())
}

/// What is to be said about the files since the last call, each once.
pub fn take_notes() -> Vec<String> {
    with_state(|s| std::mem::take(&mut s.notes))
}

/// Says `why` about `file`, unless that was the last thing said about it.
fn note(file: &Path, why: String) {
    with_state(|s| {
        if s.said.get(file) != Some(&why) {
            s.said.insert(file.to_path_buf(), why.clone());
            s.notes.push(why);
        }
    });
}

/// The file read by `parse`; `None` when it is not there or cannot be used (see the module: it is then kept aside, or
/// held, and said).
pub fn load_with<T>(file: &Path, parse: impl FnOnce(&[u8]) -> Option<T>) -> Option<T> {
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            let why = crate::fs::describe::io_error(&err);
            hold(file, trf("“{}” could not be read ({}). It is left as it is, and changes are not saved to it.", &[&name_of(file), &why]));
            return None;
        }
    };
    if let Some(value) = parse(&bytes) {
        return Some(value);
    }
    // As administrator the file belongs to someone else's home: it is not moved, and not written either
    if crate::fs::admin::is_admin() {
        hold(file, trf("“{}” could not be read. It is left as it is, and changes are not saved to it.", &[&name_of(file)]));
        return None;
    }
    match keep_aside(file) {
        Ok(kept) => note(file, trf("“{}” could not be read and was kept as “{}”. It starts empty.", &[&name_of(file), &name_of(&kept)])),
        Err(err) => {
            let why = crate::fs::describe::io_error(&err);
            hold(file, trf("“{}” could not be read, and could not be kept aside ({}). It is left as it is, and changes are not saved to it.", &[&name_of(file), &why]));
        }
    }
    None
}

/// `load_with` for a JSON file; the empty value when there is none that can be used.
pub fn load_json<T: serde::de::DeserializeOwned + Default>(file: &Path) -> T {
    load_with(file, |bytes| serde_json::from_slice(bytes).ok()).unwrap_or_default()
}

fn hold(file: &Path, why: String) {
    with_state(|s| {
        s.held.insert(file.to_path_buf(), why.clone());
    });
    note(file, why);
}

/// Moves `file` to `<name>.broken` (or the first free `.broken.N`), never over anything.
fn keep_aside(file: &Path) -> io::Result<PathBuf> {
    let name = name_of(file);
    for n in 0..100 {
        let kept = file.with_file_name(if n == 0 { format!("{name}.broken") } else { format!("{name}.broken.{n}") });
        if kept.symlink_metadata().is_ok() {
            continue;
        }
        match crate::compat::rename_noreplace(file, &kept) {
            Ok(()) => return Ok(kept),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

/// Writes `contents` to `file` whole (`atomic::write`), making its folder if needed. Refused for a file that is held;
/// a failure is said once (see the module) and returned.
pub fn save(file: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    if let Some(why) = with_state(|s| s.held.get(file).cloned()) {
        note(file, why.clone());
        return Err(io::Error::other(why));
    }
    let result = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| crate::fs::atomic::write(file, contents));
    match &result {
        Ok(()) => with_state(|s| {
            s.said.remove(file);
        }),
        Err(err) => report(file, err),
    }
    result
}

/// Says that `file` could not be saved (for saves that go their own way, as the servers' list does).
pub fn report(file: &Path, err: &io::Error) {
    note(file, trf("“{}” could not be saved: {}", &[&name_of(file), &crate::fs::describe::io_error(err)]));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("compression_stored_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn can_ignore_rights() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn a_file_that_is_not_there_is_empty_and_nothing_is_said() {
        let dir = dir("missing");
        let list: Vec<PathBuf> = load_json(&dir.join("pins.json"));
        assert!(list.is_empty());
        assert!(take_notes().is_empty());
        save(&dir.join("pins.json"), "[]").unwrap();
        assert!(take_notes().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_file_is_kept_aside_byte_for_byte_without_writing_over_an_earlier_one() {
        let dir = dir("broken");
        let file = dir.join("bookmarks.json");
        std::fs::write(dir.join("bookmarks.json.broken"), "an earlier one").unwrap();
        let bytes = b"[\"/home/me/Projects\", \xff oops".to_vec();
        std::fs::write(&file, &bytes).unwrap();
        let list: Vec<PathBuf> = load_json(&file);
        assert!(list.is_empty());
        assert_eq!(std::fs::read(dir.join("bookmarks.json.broken.1")).unwrap(), bytes, "the bytes as they were");
        assert_eq!(std::fs::read_to_string(dir.join("bookmarks.json.broken")).unwrap(), "an earlier one", "nothing written over");
        assert!(!file.exists());
        assert_eq!(take_notes(), vec!["“bookmarks.json” could not be read and was kept as “bookmarks.json.broken.1”. It starts empty.".to_owned()]);
        // The next save writes a new file, and the kept one stays
        save(&file, "[]").unwrap();
        assert_eq!(std::fs::read(dir.join("bookmarks.json.broken.1")).unwrap(), bytes);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_file_that_cannot_be_kept_aside_is_left_as_it_is_and_never_written() {
        if can_ignore_rights() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = dir("not_kept");
        let file = dir.join("tags.json");
        std::fs::write(&file, "{ broken").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let colors: HashMap<String, String> = load_json(&file);
        assert!(colors.is_empty());
        let err = save(&file, "{}").unwrap_err();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.to_string().contains("could not be kept aside"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{ broken", "the file was written over");
        // Said once, however often a save is tried
        let _ = save(&file, "{}");
        assert_eq!(take_notes().len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{ broken");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_cannot_be_read_is_not_moved_and_not_written() {
        if can_ignore_rights() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = dir("unreadable");
        let file = dir.join("open_with.json");
        std::fs::write(&file, "{\"/a\": \"x.desktop\"}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let map: HashMap<PathBuf, String> = load_json(&file);
        assert!(map.is_empty());
        assert!(save(&file, "{}").is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"/a\": \"x.desktop\"}");
        assert!(!dir.join("open_with.json.broken").exists(), "a file that could not be read was moved");
        let notes = take_notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("could not be read (Permission denied)"), "{notes:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_save_that_fails_is_said_once_until_one_works_again() {
        let dir = dir("save_fails");
        // Something that is not a folder is in the way of the folder the file goes in
        std::fs::write(dir.join("blocker"), "a file").unwrap();
        let file = dir.join("blocker/pins.json");
        assert!(save(&file, "[]").is_err());
        assert!(save(&file, "[]").is_err());
        let notes = take_notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("“pins.json” could not be saved: "), "{notes:?}");
        // Once a save worked, a new failure is said again
        std::fs::remove_file(dir.join("blocker")).unwrap();
        save(&file, "[]").unwrap();
        std::fs::remove_dir_all(dir.join("blocker")).unwrap();
        std::fs::write(dir.join("blocker"), "a file").unwrap();
        assert!(save(&file, "[]").is_err());
        assert_eq!(take_notes().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
