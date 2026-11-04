//! Extended attributes through `libc`, without following symbolic links (a link cannot carry
//! `user.*` attributes). Tags live in `user.xdg.tags`; copying a file must carry its attributes along.
use std::ffi::{CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn c_name(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// The file system cannot store attributes (FAT, exFAT, some network shares).
pub fn is_unsupported(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::ENOTSUP)
}

/// The value of `name`, or `None` when the file has no such attribute.
pub fn get(path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    let (path, name) = (c_path(path)?, c_name(name)?);
    loop {
        // SAFETY: both pointers are NUL-terminated and a null buffer with size 0 only asks for the length.
        let size = unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return match io::Error::last_os_error() {
                e if e.raw_os_error() == Some(libc::ENODATA) => Ok(None),
                e => Err(e),
            };
        }
        let mut buffer = vec![0u8; size as usize];
        // SAFETY: the buffer holds `size` writable bytes.
        let read = unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if read >= 0 {
            buffer.truncate(read as usize);
            return Ok(Some(buffer));
        }
        let err = io::Error::last_os_error();
        // The value grew between the two calls: ask again
        if err.raw_os_error() != Some(libc::ERANGE) {
            return Err(err);
        }
    }
}

pub fn set(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    #[cfg(test)]
    if tests::refused(path) {
        return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
    }
    let (path, name) = (c_path(path)?, c_name(name)?);
    // SAFETY: the value pointer and length describe `value`.
    let result = unsafe { libc::lsetxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) };
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Removes `name`; a missing attribute is not an error.
pub fn remove(path: &Path, name: &str) -> io::Result<()> {
    let (path, name) = (c_path(path)?, c_name(name)?);
    // SAFETY: both pointers are NUL-terminated.
    let result = unsafe { libc::lremovexattr(path.as_ptr(), name.as_ptr()) };
    match io::Error::last_os_error() {
        _ if result == 0 => Ok(()),
        e if e.raw_os_error() == Some(libc::ENODATA) => Ok(()),
        e => Err(e),
    }
}

/// Names of all attributes of the file.
pub fn list(path: &Path) -> io::Result<Vec<String>> {
    let path = c_path(path)?;
    loop {
        // SAFETY: a null buffer with size 0 only asks for the length.
        let size = unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; size as usize];
        // SAFETY: the buffer holds `size` writable bytes.
        let read = unsafe { libc::llistxattr(path.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if read >= 0 {
            buffer.truncate(read as usize);
            return Ok(buffer.split(|b| *b == 0).filter(|n| !n.is_empty()).map(|n| OsStr::from_bytes(n).to_string_lossy().into_owned()).collect());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ERANGE) {
            return Err(err);
        }
    }
}

/// What copying the POSIX access control lists of a file or folder came to.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct AclCopy {
    /// The source has an access ACL (one that is more than its mode says) and it did not arrive: the group bits of the mode
    /// are then the ACL's mask, not a permission of the owning group, and must not be taken over.
    pub access_lost: bool,
    /// Something the source has, an access or default ACL, did not arrive.
    pub any_lost: bool,
}

/// Copies the POSIX ACLs of `src` to `dst` as they are (the bytes the kernel gives, nothing is read out of them): the
/// access ACL of anything but a link, and the default ACL, which only a folder has. Links have no ACL of their own,
/// so nothing is asked of them. Nothing else under `system.` is copied, and no `security.` or `trusted.` attribute.
/// Meant to run before the mode is set, since setting an ACL sets the group bits of the mode to its mask.
pub fn copy_acl(src: &Path, dst: &Path, folder: bool) -> AclCopy {
    let mut result = AclCopy::default();
    let names: &[&str] = if folder { &["system.posix_acl_access", "system.posix_acl_default"] } else { &["system.posix_acl_access"] };
    for name in names {
        match get(src, name) {
            Ok(Some(value)) => {
                if set(dst, name, &value).is_err() {
                    result.any_lost = true;
                    result.access_lost |= *name == "system.posix_acl_access";
                }
            }
            // No ACL: the mode is all there is
            Ok(None) => {}
            // The source cannot say (no ACLs on that file system): there is none to lose
            Err(_) => {}
        }
    }
    result
}

/// Copies the `user.*` attributes (tags, comments, ratings) from `src` to `dst`. A refusal does not stop the copy:
/// a copy onto a disk without attributes is still a good copy. Whether every attribute that `src` has arrived
/// is the result, so that the caller can say when it did not.
pub fn copy_user(src: &Path, dst: &Path) -> bool {
    let Ok(names) = list(src) else { return true };
    let mut all = true;
    for name in names.iter().filter(|n| n.starts_with("user.")) {
        match get(src, name) {
            Ok(Some(value)) => all &= set(dst, name, &value).is_ok(),
            Ok(None) => {}
            Err(_) => all = false,
        }
    }
    all
}


#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Folders in which the tests pretend the file system keeps no attributes.
    static REFUSING: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());

    pub(super) fn refused(path: &Path) -> bool {
        REFUSING.lock().unwrap().iter().any(|p| path.starts_with(p))
    }

    pub(crate) fn refuse_attributes_under(dir: &Path) {
        REFUSING.lock().unwrap().push(dir.to_path_buf());
    }
}
