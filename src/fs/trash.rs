//! The freedesktop.org trash in the home data folder (`~/.local/share/Trash`): deleted items move
//! to `files/` and a `info/<name>.trashinfo` file remembers where they came from, so every
//! other file manager can restore them too.
use std::io;
use std::path::{Path, PathBuf};

use super::ops::{move_path, remove_tree, unique_path, unique_path_where};
use crate::i18n::tr;

pub fn root() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("Trash"))
}

/// The folder trashed items live in, as the file manager shows it.
pub fn files_dir() -> Option<PathBuf> {
    root().map(|r| r.join("files"))
}

/// Whether `path` is the trash folder itself or something inside it.
pub fn contains(path: &Path) -> bool {
    files_dir().is_some_and(|files| {
        let files = files.canonicalize().unwrap_or(files);
        path.starts_with(files)
    })
}

/// Whether the trash folder is the folder being looked at.
pub fn is_trash_folder(path: &Path) -> bool {
    files_dir().is_some_and(|files| files.canonicalize().unwrap_or(files) == path)
}

/// Whether `path` sits directly in the trash folder, where its record is.
pub fn is_top_level(path: &Path) -> bool {
    let home = files_dir().is_some_and(|files| path.parent().is_some_and(|parent| parent == files || files.canonicalize().is_ok_and(|f| f == parent)));
    home || path.parent().is_some_and(|p| p.file_name().is_some_and(|n| n == "files") && p.parent().is_some_and(|r| topdir_of(r).is_some() && r.join("info").is_dir()))
}

/// The trash folders on mounted drives that exist, where items deleted there wait: `<drive>/.Trash-$uid`
/// or `<drive>/.Trash/$uid`. Only drives and FUSE mounts are looked at, so a dead network share never stalls this.
pub fn drive_roots() -> Vec<PathBuf> {
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    let home = root();
    let mut found = Vec::new();
    for mount in mount_points() {
        for candidate in [mount.join(format!(".Trash-{uid}")), mount.join(".Trash").join(uid.to_string())] {
            if candidate.join("files").is_dir() && Some(&candidate) != home.as_ref() && !found.contains(&candidate) {
                found.push(candidate);
            }
        }
    }
    found
}

/// The `files` folders of `drive_roots`, listed next to the home trash in the trash view.
pub fn drive_files_dirs() -> Vec<PathBuf> {
    drive_roots().into_iter().map(|r| r.join("files")).collect()
}

/// Where block devices and FUSE filesystems are mounted, from `/proc/self/mounts`.
fn mount_points() -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mounts") else { return Vec::new() };
    let mut out: Vec<PathBuf> = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(mount), Some(kind)) = (fields.next(), fields.next(), fields.next()) else { continue };
        let network = matches!(kind, "fuse.sshfs" | "fuse.rclone" | "fuse.curlftpfs" | "fuse.davfs2");
        let browsable = source.starts_with("/dev/") || (kind.starts_with("fuse") && !network && !matches!(kind, "fuse.portal" | "fuse.gvfsd-fuse" | "fusectl"));
        // Spaces and the like are written as octal escapes
        let mount = PathBuf::from(unescape_mount(mount));
        if browsable && !out.contains(&mount) {
            out.push(mount);
        }
    }
    out
}

fn unescape_mount(field: &str) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = (bytes[i] == b'\\').then(|| bytes.get(i + 1..i + 4)).flatten().and_then(|d| std::str::from_utf8(d).ok()).and_then(|d| u8::from_str_radix(d, 8).ok());
        match octal {
            Some(b) => {
                out.push(b);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    std::ffi::OsString::from_vec(out)
}

/// Every trash folder: the home one first.
fn all_roots() -> Vec<PathBuf> {
    root().into_iter().chain(drive_roots()).collect()
}

pub fn is_empty() -> bool {
    all_roots().iter().all(|r| std::fs::read_dir(r.join("files")).ok().is_none_or(|mut rd| rd.next().is_none()))
}

/// Move `path` to the trash, returning where it ended up. Items on another drive go to a trash
/// folder on that drive (as the freedesktop.org spec says), so deleting is a rename, not a copy.
pub fn trash(path: &Path) -> io::Result<PathBuf> {
    let home = root().ok_or_else(|| io::Error::other("no trash folder"))?;
    let original = absolute(path)?;
    match drive_trash(&home, &original) {
        Some((root, topdir)) => trash_in(&root, Some(&topdir), &original),
        None => trash_in(&home, None, &original),
    }
}

/// `path` made absolute and canonical; a symlink is trashed as the link itself, so only its folder is made canonical.
fn absolute(path: &Path) -> io::Result<PathBuf> {
    if path.is_symlink() {
        Ok(path.parent().unwrap_or(Path::new("/")).canonicalize()?.join(path.file_name().unwrap_or_default()))
    } else {
        path.canonicalize()
    }
}

/// The trash folder on the drive `original` lives on and the drive's top folder, or `None` when it
/// shares a filesystem with the home trash (or the drive has no usable trash folder).
fn drive_trash(home: &Path, original: &Path) -> Option<(PathBuf, PathBuf)> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::symlink_metadata(original).ok()?.dev();
    let home_dev = home.ancestors().find_map(|a| std::fs::metadata(a).ok())?.dev();
    if dev == home_dev {
        return None;
    }
    // The top folder is the highest ancestor still on the same filesystem: the mount point
    let mut topdir = original.parent()?;
    while let Some(up) = topdir.parent() {
        if std::fs::metadata(up).ok()?.dev() != dev {
            break;
        }
        topdir = up;
    }
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    let shared = topdir.join(".Trash");
    let sticky = std::fs::symlink_metadata(&shared).is_ok_and(|m| m.is_dir() && m.mode() & 0o1000 != 0);
    let make = |dir: PathBuf| {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).ok().map(|_| dir)
    };
    let root = sticky.then(|| make(shared.join(uid.to_string()))).flatten().or_else(|| make(topdir.join(format!(".Trash-{uid}"))))?;
    Some((root, topdir.to_path_buf()))
}

/// The top folder of the drive a trash folder is on; `None` for the home trash.
fn topdir_of(root: &Path) -> Option<&Path> {
    let name = root.file_name()?.to_string_lossy();
    if name.starts_with(".Trash-") {
        root.parent()
    } else if root.parent()?.file_name().is_some_and(|n| n == ".Trash") {
        root.parent()?.parent()
    } else {
        None
    }
}

/// Trash `original` (absolute) into `root`. On a drive, the record names the path from `topdir`.
fn trash_in(root: &Path, topdir: Option<&Path>, original: &Path) -> io::Result<PathBuf> {
    let path = original;
    let (files, info) = (root.join("files"), root.join("info"));
    std::fs::create_dir_all(&files)?;
    std::fs::create_dir_all(&info)?;

    let name = original.file_name().ok_or_else(|| io::Error::other("no file name"))?;
    // The record is made first and exclusively: it is what reserves the name, so two items with the
    // same name, trashed at the same moment (by two windows), cannot end up with one record or
    // replace each other in `files`.
    let record_of_name = |p: &Path| info.join(format!("{}.trashinfo", p.file_name().unwrap_or_default().to_string_lossy()));
    let (target, record, mut record_file) = loop {
        let target = unique_path_where(&files, name, "copy", |p| record_of_name(p).symlink_metadata().is_ok());
        let record = record_of_name(&target);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&record) {
            Ok(file) => break (target, record, file),
            // Someone took the name since it was chosen: choose again, now steering clear of that record
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    };
    let text = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", encode(topdir.and_then(|t| original.strip_prefix(t).ok()).unwrap_or(original)), now());
    if let Err(err) = io::Write::write_all(&mut record_file, text.as_bytes()) {
        let _ = std::fs::remove_file(&record);
        return Err(err);
    }
    drop(record_file);
    if let Err(err) = move_path(path, &target) {
        let _ = std::fs::remove_file(&record);
        return Err(err);
    }
    Ok(target)
}

/// Put a trashed item back where it came from (next to it under a new name if that is taken).
pub fn restore(trashed: &Path) -> io::Result<PathBuf> {
    if trashed.parent().and_then(Path::file_name).is_none_or(|n| n != "files") {
        return Err(io::Error::other(tr("The trash has no record of where this came from")));
    }
    let root = trashed.parent().and_then(Path::parent).ok_or_else(|| io::Error::other("not in the trash"))?;
    let record = record_of(root, trashed);
    let text = std::fs::read_to_string(&record).map_err(|_| io::Error::other(tr("The trash has no record of where this came from")))?;
    let original = text
        .lines()
        .find_map(|l| l.strip_prefix("Path="))
        .map(|p| PathBuf::from(decode(p.trim())))
        .ok_or_else(|| io::Error::other(tr("The trash has no record of where this came from")))?;
    // Records on a drive are relative to its top folder
    let original = match topdir_of(root) {
        Some(topdir) if original.is_relative() => topdir.join(original),
        _ => original,
    };
    let folder = original.parent().ok_or_else(|| io::Error::other("invalid original location"))?;
    std::fs::create_dir_all(folder)?;
    let target = unique_path(folder, original.file_name().unwrap_or_default(), "restored");
    move_path(trashed, &target)?;
    let _ = std::fs::remove_file(record);
    Ok(target)
}

/// Delete an item for good; for trashed items this also drops their record.
pub fn delete_forever(path: &Path) -> io::Result<()> {
    remove_tree(path)?;
    // A trashed item sits in `<trash>/files`, its record in `<trash>/info`: in the home trash or one on a drive
    if let Some(root) = path.parent().filter(|p| p.file_name().is_some_and(|n| n == "files")).and_then(Path::parent)
        && root.join("info").is_dir() {
            let _ = std::fs::remove_file(record_of(root, path));
        }
    Ok(())
}

pub fn empty() -> io::Result<()> {
    for root in all_roots() {
        for sub in ["files", "info"] {
            let Ok(entries) = std::fs::read_dir(root.join(sub)) else { continue };
            for entry in entries.flatten() {
                remove_tree(&entry.path())?;
            }
        }
    }
    Ok(())
}

fn record_of(root: &Path, trashed: &Path) -> PathBuf {
    root.join("info").join(format!("{}.trashinfo", trashed.file_name().unwrap_or_default().to_string_lossy()))
}

fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    // SAFETY: `tm` is plain data and `localtime_r` only writes into it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Percent-encode a path for a `.trashinfo` file, keeping `/` readable.
fn encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(s: &str) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    std::ffi::OsString::from_vec(out)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_noreplace_support_trashing_is_refused_and_the_item_and_its_trash_stay_untouched() {
        assert!(dirs::data_dir().is_some_and(|d| d.starts_with(std::env::temp_dir())), "run the tests with an empty HOME and XDG folders, as CI does");
        let dir = std::env::temp_dir().join(format!("compression_trash_fb_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("trash_fb_folder")).unwrap();
        crate::compat::tests::without_support_under(&dir);
        std::fs::write(dir.join("trash_fb_file"), "data").unwrap();
        for name in ["trash_fb_file", "trash_fb_folder"] {
            let err = trash(&dir.join(name)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{name}");
            assert!(dir.join(name).exists());
        }
        assert_eq!(std::fs::read_to_string(dir.join("trash_fb_file")).unwrap(), "data");
        // No record was left behind for what did not go (the trash is shared with the other tests: their names are
        // not these)
        let info = dirs::data_dir().unwrap().join("Trash/info");
        for name in ["trash_fb_file", "trash_fb_folder"] {
            assert!(!info.join(format!("{name}.trashinfo")).exists(), "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn items_with_one_name_get_a_record_each_and_a_record_in_the_way_is_never_replaced() {
        let base = std::env::temp_dir().join(format!("compression_trash_record_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, work) = (base.join("Trash"), base.join("work"));
        std::fs::create_dir_all(&work).unwrap();
        let record = |kept: &Path| record_of(&root, kept);

        // One after the other
        std::fs::write(work.join("a.txt"), "first").unwrap();
        let first = trash_in(&root, None, &absolute(&work.join("a.txt")).unwrap()).unwrap();
        std::fs::write(work.join("a.txt"), "second").unwrap();
        let second = trash_in(&root, None, &absolute(&work.join("a.txt")).unwrap()).unwrap();
        assert_ne!(first, second);
        for (kept, content) in [(&first, "first"), (&second, "second")] {
            assert_eq!(std::fs::read_to_string(kept).unwrap(), content);
            assert!(std::fs::read_to_string(record(kept)).unwrap().contains(&format!("Path={}", encode(&work.join("a.txt")))));
        }

        // A record already in the way, as another window's trashing would have left it, keeps its content
        let reserved = root.join("info").join("b.txt.trashinfo");
        std::fs::write(&reserved, "[Trash Info]\nPath=/somewhere/else/b.txt\n").unwrap();
        std::fs::write(work.join("b.txt"), "mine").unwrap();
        let kept = trash_in(&root, None, &absolute(&work.join("b.txt")).unwrap()).unwrap();
        assert_ne!(kept.file_name().unwrap(), "b.txt", "the reserved name is not used");
        assert_eq!(std::fs::read_to_string(&reserved).unwrap(), "[Trash Info]\nPath=/somewhere/else/b.txt\n");
        assert!(std::fs::read_to_string(record(&kept)).unwrap().contains("Path="));
        assert_eq!(restore(&kept).unwrap(), work.join("b.txt"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn many_items_with_one_name_trashed_at_once_each_keep_their_own_record() {
        let base = std::env::temp_dir().join(format!("compression_trash_race_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("Trash");
        let sources: Vec<PathBuf> = (0..12).map(|i| base.join(format!("w{i}")).join("same.txt")).collect();
        for (i, source) in sources.iter().enumerate() {
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::write(source, format!("content {i}")).unwrap();
        }
        let start = std::sync::Arc::new(std::sync::Barrier::new(sources.len()));
        let threads: Vec<_> = sources
            .iter()
            .cloned()
            .map(|source| {
                let (root, start) = (root.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    trash_in(&root, None, &absolute(&source).unwrap()).unwrap()
                })
            })
            .collect();
        let kept: Vec<PathBuf> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let distinct: std::collections::HashSet<_> = kept.iter().collect();
        assert_eq!(distinct.len(), sources.len(), "two items got one name");
        for (i, (source, kept)) in sources.iter().zip(&kept).enumerate() {
            assert_eq!(std::fs::read_to_string(kept).unwrap(), format!("content {i}"));
            let text = std::fs::read_to_string(record_of(&root, kept)).unwrap();
            assert!(text.contains(&format!("Path={}", encode(source))), "{text}");
        }
        assert_eq!(std::fs::read_dir(root.join("info")).unwrap().count(), sources.len());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn mount_fields_unescape_and_drive_trash_folders_are_recognised() {
        assert_eq!(unescape_mount("/run/media/me/My\\040Stick"), std::ffi::OsString::from("/run/media/me/My Stick"));
        assert!(topdir_of(Path::new("/mnt/d/.Trash-1000")).is_some());
        assert!(topdir_of(Path::new("/mnt/d/.Trash/1000")).is_some());
        assert!(topdir_of(Path::new("/home/me/.local/share/Trash")).is_none());
    }

    #[test]
    fn trash_and_restore() {
        let base = std::env::temp_dir().join(format!("compression_trash_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, work) = (base.join("Trash"), base.join("work dir"));
        std::fs::create_dir_all(work.join("sub")).unwrap();
        std::fs::write(work.join("a b.txt"), "x").unwrap();
        std::fs::write(work.join("sub/in.txt"), "y").unwrap();

        let t1 = trash_in(&root, None, &absolute(&work.join("a b.txt")).unwrap()).unwrap();
        assert!(t1.exists() && !work.join("a b.txt").exists());
        std::fs::write(work.join("a b.txt"), "second").unwrap();
        let t2 = trash_in(&root, None, &absolute(&work.join("a b.txt")).unwrap()).unwrap();
        assert_ne!(t1, t2, "clashing names get a fresh name");
        let t3 = trash_in(&root, None, &absolute(&work.join("sub")).unwrap()).unwrap();

        // Restore puts items back, and moves aside what is in the way
        std::fs::write(work.join("a b.txt"), "occupied").unwrap();
        let back = restore(&t1).unwrap();
        assert_eq!(std::fs::read_to_string(&back).unwrap(), "x");
        assert_ne!(back, work.join("a b.txt"));
        let back = restore(&t3).unwrap();
        assert_eq!(back, work.join("sub"));
        assert!(work.join("sub/in.txt").exists());
        assert!(!root.join("info").join(format!("{}.trashinfo", t1.file_name().unwrap().to_string_lossy())).exists());

        // Something inside a trashed folder has no record of its own and can't be restored alone
        let again = trash_in(&root, None, &absolute(&work.join("sub")).unwrap()).unwrap();
        assert!(restore(&again.join("in.txt")).is_err());
        assert!(again.join("in.txt").exists());

        // On another drive the record is relative to the drive's top folder, and restoring joins it back
        let (drive, stray) = (base.join("drive"), base.join("drive/docs/x.txt"));
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(&stray, "z").unwrap();
        let drive = drive.canonicalize().unwrap();
        let drive_root = drive.join(".Trash-1000");
        let kept = trash_in(&drive_root, Some(&drive), &absolute(&stray).unwrap()).unwrap();
        assert_eq!(kept.parent().unwrap(), drive_root.join("files"));
        assert!(std::fs::read_to_string(record_of(&drive_root, &kept)).unwrap().contains("Path=docs/x.txt"));
        assert_eq!(topdir_of(&drive_root), Some(drive.as_path()));
        assert_eq!(restore(&kept).unwrap(), drive.join("docs/x.txt"));

        assert_eq!(decode(&encode(Path::new("/a b/ü%x"))), std::ffi::OsString::from("/a b/ü%x"));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
