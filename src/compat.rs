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
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
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

