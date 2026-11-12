//! Compatibility with Ski, which Compression was forked from: the user data of an existing Ski
//! install is copied once, and the legacy `SKI_*` environment variables are still read.
use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::i18n::tr;

const OLD: &str = "ski";
const NEW: &str = "compression";

/// `COMPRESSION_<name>`, or else the legacy `SKI_<name>`.
pub fn env_var(name: &str) -> Option<OsString> {
    lookup(name, |key| std::env::var_os(key))
}

fn lookup(name: &str, get: impl Fn(&str) -> Option<OsString>) -> Option<OsString> {
    get(&format!("COMPRESSION_{name}")).or_else(|| get(&format!("SKI_{name}")))
}

/// Copy the config, data and state folders of Ski to Compression's, where those are missing.
/// Nothing is ever deleted or overwritten, and the cache is left to be rebuilt. An administrator
/// session does not migrate: root would create files in the home of the person who started it.
pub fn migrate_user_data() {
    if crate::fs::admin::is_admin() {
        return;
    }
    for base in [dirs::config_dir(), dirs::data_dir(), dirs::state_dir()].into_iter().flatten() {
        let _ = migrate_dir(&base);
    }
}

/// `base/ski` -> `base/compression`. The copy is made under a name of this process, so that a
/// resident, a portal and a dialog starting together do not meet in one folder, and takes its
/// final name in one step that refuses to replace anything. A copy cut short never has that name.
fn migrate_dir(base: &Path) -> io::Result<()> {
    let (old, new) = (base.join(OLD), base.join(NEW));
    if new.symlink_metadata().is_ok() || !old.is_dir() {
        return Ok(());
    }
    let temp = base.join(format!("{NEW}.migrating.{}", std::process::id()));
    // Left by an earlier run of this process id that was cut short
    let _ = std::fs::remove_dir_all(&temp);
    let result = copy_tree(&old, &temp).and_then(|()| rename_noreplace(&temp, &new));
    if result.is_err() {
        // Another process finished first, or the copy failed: this attempt's folder is ours to remove
        let _ = std::fs::remove_dir_all(&temp);
    }
    result
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&source, &target)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&source)?, &target)?;
        } else if kind.is_file() {
            std::fs::copy(&source, &target)?;
        }
    }
    std::fs::set_permissions(to, from.metadata()?.permissions())
}

/// `rename`, but an existing `to` (even an empty folder) is an error instead of being replaced.
///
/// The kernel does this in one step (`renameat2` with `RENAME_NOREPLACE`). A file system that cannot (the kernel says
/// ENOSYS, EINVAL or EOPNOTSUPP) is refused, see `without_noreplace`.
///
/// A name that differs from the old one only in case, on a file system that does not tell case apart (FAT, exFAT, a
/// casefolded folder), is the same entry to the kernel, which answers EEXIST. Such a rename goes by a name of its own in
/// between, see `rename_by_own_name`.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    match rename_noreplace_once(from, to) {
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists && same_entry_in_same_folder(from, to) => rename_by_own_name(from, to, err),
        other => other,
    }
}

/// Whether `to` is the very entry `from` under another spelling: the same folder, the same file (device and inode),
/// another name. On a file system that tells case apart this is a hard link; `rename_by_own_name` then finds the name
/// still taken and changes nothing.
fn same_entry_in_same_folder(from: &Path, to: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    from != to
        && from.parent() == to.parent()
        && matches!((from.symlink_metadata(), to.symlink_metadata()), (Ok(a), Ok(b)) if (a.dev(), a.ino()) == (b.dev(), b.ino()))
}

/// Renames `from` to `to` by a name of its own in between: `from` steps aside, then takes `to`. When `to` is still taken
/// after that (a hard link, or something made meanwhile) it is not replaced: `from` gets its old name back and `taken`,
/// the first answer, is the error. A hard stop in between leaves a `.compression-rename-*` entry that holds the item.
fn rename_by_own_name(from: &Path, to: &Path, taken: io::Error) -> io::Result<()> {
    let aside = (0..100u32)
        .map(|n| from.with_file_name(format!(".compression-rename-{}-case-{n}", std::process::id())))
        .find(|p| p.symlink_metadata().is_err())
        .ok_or(taken)?;
    rename_noreplace_once(from, &aside)?;
    match rename_noreplace_once(&aside, to) {
        Ok(()) => Ok(()),
        Err(err) => match rename_noreplace_once(&aside, from) {
            Ok(()) => Err(err),
            Err(_) => Err(io::Error::other(crate::i18n::trf("Could not rename: the item is kept as “{}”", &[&aside.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()]))),
        },
    }
}

/// One `renameat2(RENAME_NOREPLACE)`, or the refusal of `without_noreplace` where the file system cannot do it.
fn rename_noreplace_once(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    if tests::forced(from, to) {
        return without_noreplace(from, to, io::Error::from_raw_os_error(libc::EINVAL));
    }
    let path = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other);
    let (a, b) = (path(from)?, path(to)?);
    // SAFETY: both are NUL-terminated paths that outlive the call.
    let done = unsafe { libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), libc::RENAME_NOREPLACE) };
    if done == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if lacks_noreplace(err.raw_os_error()) { without_noreplace(from, to, err) } else { Err(err) }
}

/// The error for a move that cannot be done safely on this file system: nothing is changed.
fn cannot_move_safely() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, tr("This file system does not support moving or renaming without the risk of replacing something, so nothing was changed"))
}

/// What a move does on a file system that cannot do RENAME_NOREPLACE: nothing. Every other way of moving is several steps
/// with a gap between them, in which something that someone else put there can be replaced or left behind, so none is
/// taken. The source and the target are not touched. A target that is there is said to be (that is a look, not a
/// change); otherwise the error says the operation is not supported here, which is not a conflict. Moving a folder
/// into itself, which the kernel also answers with EINVAL, keeps that error.
fn without_noreplace(from: &Path, to: &Path, err: io::Error) -> io::Result<()> {
    let meta = from.symlink_metadata()?;
    if meta.is_dir() && to.starts_with(from) {
        return Err(err);
    }
    if to.symlink_metadata().is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    Err(cannot_move_safely())
}

/// Whether `errno` says the file system cannot do RENAME_NOREPLACE at all (the kernel answers EINVAL, a
/// file system of its own may answer EOPNOTSUPP), as opposed to a real failure such as EEXIST.
fn lacks_noreplace(errno: Option<i32>) -> bool {
    matches!(errno, Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Mutex;

    /// Folders in which the tests pretend the file system cannot do RENAME_NOREPLACE: a move from or to a path
    /// under one takes the way for such file systems (this is how the other modules' tests reach it too).
    static NO_SUPPORT: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());

    pub(super) fn forced(from: &Path, to: &Path) -> bool {
        NO_SUPPORT.lock().unwrap().iter().any(|p| from.starts_with(p) || to.starts_with(p))
    }

    /// Makes the file system under `dir` "lack" RENAME_NOREPLACE for the rest of the process.
    pub(crate) fn without_support_under(dir: &Path) {
        NO_SUPPORT.lock().unwrap().push(dir.to_path_buf());
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("compression_compat_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("taken_dir")).unwrap();
        std::fs::write(dir.join("a"), "from").unwrap();
        std::fs::write(dir.join("b"), "keep me").unwrap();
        dir
    }

    #[test]
    fn a_rename_without_replacing_never_touches_a_name_that_is_taken() {
        let dir = dir("noreplace");
        let err = rename_noreplace(&dir.join("a"), &dir.join("b")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "from");
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "keep me");
        let err = rename_noreplace(&dir.join("a"), &dir.join("taken_dir")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert!(dir.join("taken_dir").is_dir() && dir.join("a").exists());
        std::os::unix::fs::symlink("nowhere", dir.join("dangling")).unwrap();
        assert_eq!(rename_noreplace(&dir.join("a"), &dir.join("dangling")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        rename_noreplace(&dir.join("a"), &dir.join("c")).unwrap();
        assert!(!dir.join("a").exists() && std::fs::read_to_string(dir.join("c")).unwrap() == "from");
        assert_eq!(rename_noreplace(&dir.join("gone"), &dir.join("d")).unwrap_err().kind(), io::ErrorKind::NotFound);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_system_that_cannot_do_noreplace_is_told_from_a_real_failure() {
        for errno in [libc::ENOSYS, libc::EINVAL, libc::EOPNOTSUPP] {
            assert!(lacks_noreplace(Some(errno)), "{errno}");
        }
        for errno in [libc::EEXIST, libc::EACCES, libc::ENOENT, libc::ENOSPC, libc::EXDEV] {
            assert!(!lacks_noreplace(Some(errno)), "{errno}");
        }
        assert!(!lacks_noreplace(None));
    }

    /// Everything in `dir`, to see that nothing was made or removed.
    fn snapshot(dir: &Path) -> Vec<(String, u64, u64)> {
        let mut found: Vec<(String, u64, u64)> = std::fs::read_dir(dir).unwrap().map(|e| {
            let e = e.unwrap();
            let m = e.path().symlink_metadata().unwrap();
            (e.file_name().to_string_lossy().into_owned(), m.ino(), m.nlink())
        }).collect();
        found.sort();
        found
    }

    #[test]
    fn without_noreplace_support_a_move_is_refused_and_nothing_is_touched() {
        let dir = dir("refused");
        std::fs::create_dir_all(dir.join("f/inside")).unwrap();
        std::os::unix::fs::symlink("b", dir.join("l")).unwrap();
        let before = snapshot(&dir);
        let inside = snapshot(&dir.join("f"));
        without_support_under(&dir);
        // A file, a link and a folder, to a free name
        for (from, to) in [("a", "free"), ("l", "free"), ("f", "free")] {
            let err = rename_noreplace(&dir.join(from), &dir.join(to)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{from}");
            assert!(err.to_string().contains("does not support moving or renaming"), "{err}");
            assert!(!err.to_string().to_lowercase().contains("exists"), "{err}");
            assert_eq!(snapshot(&dir), before, "{from} -> free: nothing made, removed or changed");
        }
        assert_eq!(snapshot(&dir.join("f")), inside);
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "from");
        assert_eq!(std::fs::read_link(dir.join("l")).unwrap(), Path::new("b"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_noreplace_support_a_target_that_is_there_is_a_conflict_and_nothing_is_touched() {
        let dir = dir("refused_taken");
        std::fs::create_dir_all(dir.join("f/inside")).unwrap();
        std::os::unix::fs::symlink("nowhere", dir.join("dangling")).unwrap();
        let before = snapshot(&dir);
        without_support_under(&dir);
        for from in ["a", "f"] {
            for taken in ["b", "taken_dir", "dangling"] {
                let err = rename_noreplace(&dir.join(from), &dir.join(taken)).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{from} -> {taken}");
            }
        }
        assert_eq!(snapshot(&dir), before);
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "keep me");
        // A missing source and a folder into itself keep their own errors
        assert_eq!(rename_noreplace(&dir.join("gone"), &dir.join("z")).unwrap_err().kind(), io::ErrorKind::NotFound);
        let err = rename_noreplace(&dir.join("f"), &dir.join("f/inside/g")).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "{err}");
        assert_eq!(snapshot(&dir), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_supported_file_system_still_moves_and_refuses_a_taken_name_natively() {
        let dir = dir("native");
        let ino = dir.join("a").metadata().unwrap().ino();
        rename_noreplace(&dir.join("a"), &dir.join("c")).unwrap();
        assert_eq!(dir.join("c").metadata().unwrap().ino(), ino);
        assert_eq!(rename_noreplace(&dir.join("c"), &dir.join("b")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Runs `check` in a folder that does not tell case apart, the way FAT or exFAT do not: a casefolded tmpfs, mounted
    /// in a user namespace of its own over `/tmp`, so nothing of it is left afterwards. The test binary runs only `test`
    /// once more in there (`COMPRESSION_CASEFOLD_DIR` says where). Where the system allows no user namespaces or has no
    /// casefolded tmpfs, the test says so on stderr and checks nothing.
    pub(crate) fn in_casefold_folder(test: &str, check: impl FnOnce(&Path)) {
        if let Some(dir) = std::env::var_os("COMPRESSION_CASEFOLD_DIR") {
            check(Path::new(&dir));
            return;
        }
        const MOUNT: &str = "mount -t tmpfs -o casefold=utf8-12.1.0 none /tmp && mkdir /tmp/f && chattr +F /tmp/f";
        let probe = std::process::Command::new("unshare").args(["-rm", "sh", "-c", MOUNT]).output();
        if !probe.as_ref().is_ok_and(|o| o.status.success()) {
            eprintln!("{test}: no casefolded folder can be made here, nothing checked ({probe:?})");
            return;
        }
        let name = test.split_once("::").map_or(test, |(_, rest)| rest);
        let script = format!("{MOUNT} && COMPRESSION_CASEFOLD_DIR=/tmp/f exec \"$0\" --exact \"$1\" --nocapture --test-threads 1");
        let out = std::process::Command::new("unshare")
            .args(["-rm", "sh", "-c", &script])
            .arg(std::env::current_exe().unwrap())
            .arg(name)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && text.contains("1 passed"), "{text}{}", String::from_utf8_lossy(&out.stderr));
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn a_change_of_case_renames_on_a_file_system_that_does_not_tell_case_apart() {
        in_casefold_folder(concat!(module_path!(), "::a_change_of_case_renames_on_a_file_system_that_does_not_tell_case_apart"), |dir| {
            std::fs::write(dir.join("a.txt"), "content").unwrap();
            std::fs::create_dir(dir.join("Docs")).unwrap();
            std::fs::write(dir.join("Docs/inside"), "x").unwrap();
            std::fs::write(dir.join("b.txt"), "other").unwrap();
            // The kernel itself refuses: to it the new name is the old entry
            assert!(dir.join("A.TXT").exists(), "the folder does tell case apart");
            rename_noreplace(&dir.join("a.txt"), &dir.join("A.txt")).unwrap();
            rename_noreplace(&dir.join("Docs"), &dir.join("docs")).unwrap();
            assert_eq!(names_in(dir), ["A.txt", "b.txt", "docs"]);
            assert_eq!(std::fs::read_to_string(dir.join("A.txt")).unwrap(), "content");
            assert_eq!(std::fs::read_to_string(dir.join("docs/inside")).unwrap(), "x");
            // Another file under the name is still a name that is taken, and nothing changes
            assert_eq!(rename_noreplace(&dir.join("A.txt"), &dir.join("B.TXT")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(names_in(dir), ["A.txt", "b.txt", "docs"]);
            assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "other");
        });
    }

    #[test]
    fn a_hard_link_under_another_name_stays_a_taken_name_and_nothing_changes() {
        let dir = std::env::temp_dir().join(format!("compression_compat_link_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), "1").unwrap();
        std::fs::hard_link(dir.join("a"), dir.join("A")).unwrap();
        assert_eq!(rename_noreplace(&dir.join("a"), &dir.join("A")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(names_in(&dir), ["A", "a"], "both names are where they were, and nothing is left aside");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    use std::collections::HashMap;

    fn sandbox(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("compression_compat_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn legacy(base: &Path) {
        std::fs::create_dir_all(base.join("ski/themes")).unwrap();
        std::fs::write(base.join("ski/settings.json"), "old settings").unwrap();
        std::fs::write(base.join("ski/themes/mine.json"), "theme").unwrap();
        std::os::unix::fs::symlink("settings.json", base.join("ski/link")).unwrap();
    }

    #[test]
    fn user_data_is_copied_and_the_old_folder_stays_whole() {
        let base = sandbox("copy");
        legacy(&base);
        migrate_dir(&base).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("compression/settings.json")).unwrap(), "old settings");
        assert_eq!(std::fs::read_to_string(base.join("compression/themes/mine.json")).unwrap(), "theme");
        assert_eq!(std::fs::read_link(base.join("compression/link")).unwrap(), Path::new("settings.json"));
        assert_eq!(std::fs::read_to_string(base.join("ski/settings.json")).unwrap(), "old settings");
        assert!(base.join("ski/themes/mine.json").exists());
        let leftovers = std::fs::read_dir(&base).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains("migrating")).count();
        assert_eq!(leftovers, 0);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_existing_compression_folder_is_never_touched() {
        for existing in [true, false] {
            let base = sandbox(if existing { "keep_full" } else { "keep_empty" });
            legacy(&base);
            std::fs::create_dir(base.join("compression")).unwrap();
            if existing {
                std::fs::write(base.join("compression/settings.json"), "new settings").unwrap();
            }
            migrate_dir(&base).unwrap();
            if existing {
                assert_eq!(std::fs::read_to_string(base.join("compression/settings.json")).unwrap(), "new settings");
            }
            // Not even an empty folder is filled in or replaced
            assert_eq!(std::fs::read_dir(base.join("compression")).unwrap().count(), usize::from(existing));
            assert!(base.join("ski/settings.json").exists());
            std::fs::remove_dir_all(&base).unwrap();
        }
    }

    #[test]
    fn nothing_is_made_without_old_data() {
        let base = sandbox("none");
        migrate_dir(&base).unwrap();
        assert!(!base.join("compression").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_cut_short_migration_is_repeated_from_scratch() {
        let base = sandbox("retry");
        legacy(&base);
        // What an aborted run leaves: a half copy under this process's name and one under another's
        let mine = base.join(format!("compression.migrating.{}", std::process::id()));
        std::fs::create_dir_all(mine.join("junk")).unwrap();
        std::fs::write(mine.join("settings.json"), "half").unwrap();
        std::fs::create_dir_all(base.join("compression.migrating.1")).unwrap();
        migrate_dir(&base).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("compression/settings.json")).unwrap(), "old settings");
        assert!(!base.join("compression/junk").exists());
        assert!(!mine.exists());
        // The other process's folder is not ours to remove
        assert!(base.join("compression.migrating.1").exists());
        // And a second run changes nothing
        migrate_dir(&base).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("compression/settings.json")).unwrap(), "old settings");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_process_that_loses_the_race_leaves_the_winner_alone() {
        let base = sandbox("race");
        legacy(&base);
        let winner = base.join("compression");
        std::fs::create_dir_all(&winner).unwrap();
        std::fs::write(winner.join("x"), "winner").unwrap();
        let temp = base.join("compression.migrating.7");
        copy_tree(&base.join("ski"), &temp).unwrap();
        assert_eq!(rename_noreplace(&temp, &winner).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(winner.join("x")).unwrap(), "winner");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn compression_variables_win_over_the_legacy_ones() {
        let vars = |pairs: &[(&str, &str)]| -> HashMap<String, OsString> { pairs.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect() };
        let both = vars(&[("COMPRESSION_ENGINE", "podman"), ("SKI_ENGINE", "docker")]);
        assert_eq!(lookup("ENGINE", |k| both.get(k).cloned()), Some(OsString::from("podman")));
        let old = vars(&[("SKI_ENGINE", "docker")]);
        assert_eq!(lookup("ENGINE", |k| old.get(k).cloned()), Some(OsString::from("docker")));
        assert_eq!(lookup("ENGINE", |_| None), None);
    }
}
