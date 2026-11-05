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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usual_errors_of_the_system_come_in_plain_words() {
        let words = |errno| io_error(&io::Error::from_raw_os_error(errno));
        assert_eq!(words(libc::EACCES), "Permission denied");
        assert_eq!(words(libc::EPERM), "Permission denied");
        assert_eq!(words(libc::ENOENT), "Not found");
        assert_eq!(words(libc::EEXIST), "Already exists");
        assert_eq!(words(libc::ENOTEMPTY), "The folder is not empty");
        assert_eq!(words(libc::ENOSPC), "Not enough space on the disk");
        assert_eq!(words(libc::EDQUOT), "Not enough space on the disk");
        assert_eq!(words(libc::EROFS), "The file system is read-only");
        assert_eq!(words(libc::ENAMETOOLONG), "The name is too long");
        assert_eq!(words(libc::EBUSY), "It is in use");
        for errno in [libc::EACCES, libc::ENOENT, libc::EEXIST, libc::ENOSPC, libc::EROFS] {
            assert!(!words(errno).contains("os error"), "{errno}");
        }
    }

    #[test]
    fn other_errors_keep_their_own_text() {
        // An error that Compression made itself has its words already, whatever its kind
        let own = io::Error::new(io::ErrorKind::PermissionDenied, "Administrator mode only starts a program that nobody else can change");
        assert_eq!(io_error(&own), "Administrator mode only starts a program that nobody else can change");
        assert_eq!(io_error(&io::Error::other("it broke")), "it broke");
        // A system error with no words of ours is shown as the system has it
        let other = io::Error::from_raw_os_error(libc::ENODEV);
        assert_eq!(io_error(&other), other.to_string());
        assert!(io_error(&other).contains("os error"));
    }
}
