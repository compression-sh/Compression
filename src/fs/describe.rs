//! Words for the errors of the file system that people meet most, so a message does not read
//! "Permission denied (os error 13)" in English on a screen set to another language.
use std::io;

use crate::i18n::tr;

/// What went wrong, for a message. The usual errors of the system are put in the language of the
/// window; everything else (and an error made by Compression itself, which has its words already)
/// keeps its own text.
pub fn io_error(err: &io::Error) -> String {
    let words = match err.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => tr("Permission denied"),
        Some(libc::ENOENT) => tr("Not found"),
        Some(libc::EEXIST) => tr("Already exists"),
        Some(libc::ENOTEMPTY) => tr("The folder is not empty"),
        Some(libc::ENOSPC | libc::EDQUOT) => tr("Not enough space on the disk"),
        Some(libc::EROFS) => tr("The file system is read-only"),
        Some(libc::ENAMETOOLONG) => tr("The name is too long"),
        Some(libc::EBUSY) => tr("It is in use"),
        _ => return err.to_string(),
    };
    words.to_owned()
}
