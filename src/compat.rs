//! Compatibility with Ski, which Compression was forked from: the user data of an existing Ski
//! install is copied once, and the legacy `SKI_*` environment variables are still read.
use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

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
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let path = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other);
    let (a, b) = (path(from)?, path(to)?);
    // SAFETY: both are NUL-terminated paths that outlive the call.
    let done = unsafe { libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), libc::RENAME_NOREPLACE) };
    if done == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // shortcut: a file system without RENAME_NOREPLACE gets a check first, which leaves a tiny race; upgrade if one shows up
        Some(libc::ENOSYS | libc::EINVAL) if to.symlink_metadata().is_err() => std::fs::rename(from, to),
        _ => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
