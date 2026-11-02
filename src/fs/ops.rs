//! File operations: copy, move, compress and extract. Everything here blocks, so the app runs
//! it on a worker thread through the engine (`fs::engine`).
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::i18n::{tr, trf};

/// Copies keep their permissions where the file system has them. Windows shares (SMB through
/// GVfs), FAT and exFAT drives and some FUSE mounts refuse to change them; the copy itself is
/// still good, so that refusal is not an error.
pub fn keep_mode(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if matches!(e.raw_os_error(), Some(libc::EOPNOTSUPP | libc::ENOSYS | libc::EPERM)) => Ok(()),
        other => other,
    }
}

/// A name for `name` inside `dir` that does not exist yet: `a.txt`, `a (copy).txt`, `a (copy 2).txt`, ...
pub fn unique_path(dir: &Path, name: &std::ffi::OsStr, suffix: &str) -> PathBuf {
    unique_path_where(dir, name, suffix, |_| false)
}

/// Like `unique_path`, and also steers clear of the paths `reserved` says are about to be used.
pub fn unique_path_where(dir: &Path, name: &std::ffi::OsStr, suffix: &str, reserved: impl Fn(&Path) -> bool) -> PathBuf {
    let free = |p: &Path| p.symlink_metadata().is_err() && !reserved(p);
    let direct = dir.join(name);
    if free(&direct) {
        return direct;
    }
    let name = name.to_string_lossy();
    let (stem, ext) = split_name(&name);
    for n in 1.. {
        let tag = if n == 1 { format!(" ({suffix})") } else { format!(" ({suffix} {n})") };
        let candidate = dir.join(format!("{stem}{tag}{ext}"));
        if free(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// `("archive", ".tar.gz")` / `("photo", ".png")` / `("Makefile", "")`.
fn split_name(name: &str) -> (&str, &str) {
    for double in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst"] {
        if let Some(stem) = name.strip_suffix(double)
            && !stem.is_empty() {
                return (stem, &name[stem.len()..]);
            }
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = src.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
    } else if meta.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        keep_mode(std::fs::set_permissions(dst, meta.permissions()))
    } else if !meta.is_file() {
        // Opening a named pipe waits for a writer that may never come
        let name = src.file_name().map_or_else(|| src.display().to_string(), |n| n.to_string_lossy().into_owned());
        Err(io::Error::other(trf("Cannot copy {}: it is a named pipe, socket or device file", &[&name])))
    } else {
        // Not `std::fs::copy`: it fails the whole copy when the target refuses the permissions
        let mut reader = std::fs::File::open(src)?;
        let mut writer = std::fs::File::create(dst)?;
        io::copy(&mut reader, &mut writer)?;
        keep_mode(writer.set_permissions(meta.permissions()))
    }
}

/// Rename `src` to the exact path `dst`, copying across filesystems when it has to.
pub fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        // Different filesystem: copy, then remove the original
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let existed = dst.symlink_metadata().is_ok();
            if let Err(err) = copy_tree(src, dst) {
                // A half-copied target is worse than none; one that was already there is not ours to remove
                if !existed {
                    let _ = remove_tree(dst);
                }
                return Err(err);
            }
            remove_tree(src)
        }
        Err(e) => Err(e),
    }
}

/// Delete a file, symlink or whole folder.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    if path.is_dir() && !path.is_symlink() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    TarGz,
    TarXz,
    TarBz2,
    TarZst,
    SevenZ,
}

impl ArchiveFormat {
    pub const ALL: [ArchiveFormat; 6] = [
        ArchiveFormat::Zip,
        ArchiveFormat::TarGz,
        ArchiveFormat::TarXz,
        ArchiveFormat::TarBz2,
        ArchiveFormat::TarZst,
        ArchiveFormat::SevenZ,
    ];

    pub fn extension(self) -> &'static str {
        match self {
            ArchiveFormat::Zip => ".zip",
            ArchiveFormat::TarGz => ".tar.gz",
            ArchiveFormat::TarXz => ".tar.xz",
            ArchiveFormat::TarBz2 => ".tar.bz2",
            ArchiveFormat::TarZst => ".tar.zst",
            ArchiveFormat::SevenZ => ".7z",
        }
    }

    /// Zip and tar.gz are built in; the rest need the system's `tar` / `7z` and their compressors.
    pub fn available(self) -> bool {
        match self {
            ArchiveFormat::Zip | ArchiveFormat::TarGz => true,
            ArchiveFormat::TarXz => have("xz") && have("tar"),
            ArchiveFormat::TarBz2 => have("bzip2") && have("tar"),
            ArchiveFormat::TarZst => have("zstd") && have("tar"),
            ArchiveFormat::SevenZ => have("7z") || have("7za"),
        }
    }
}

pub fn have(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

pub fn seven_zip() -> &'static str {
    if have("7z") { "7z" } else { "7za" }
}

fn run(mut command: Command) -> io::Result<()> {
    let output = command.stdin(Stdio::null()).output()?;
    if output.status.success() {
        Ok(())
    } else {
        let text = String::from_utf8_lossy(if output.stderr.is_empty() { &output.stdout } else { &output.stderr });
        Err(io::Error::other(text.lines().last().unwrap_or("command failed").to_owned()))
    }
}

/// Whether `path` looks like an archive this module can unpack.
pub fn is_archive(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    [
        ".zip", ".jar", ".apk", ".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.bz2", ".tbz2", ".tar.zst", ".tzst",
        ".gz", ".xz", ".bz2", ".zst", ".7z", ".rar", ".iso", ".cab", ".deb", ".rpm",
    ]
    .iter()
    .any(|ext| name.ends_with(ext))
}

/// Folder name an archive extracts into: `photos.tar.gz` -> `photos`.
pub fn archive_stem(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for ext in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tgz", ".txz", ".tbz2", ".tzst"] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem.to_owned();
        }
    }
    split_name(&name).0.to_owned()
}

/// Pack `sources` (all in the same folder) into the archive `out`.
pub fn compress(sources: &[PathBuf], out: &Path, format: ArchiveFormat) -> io::Result<()> {
    let base = sources
        .first()
        .and_then(|p| p.parent())
        .ok_or_else(|| io::Error::other(tr("Nothing to compress")))?
        .to_path_buf();
    let names: Vec<_> = sources.iter().filter_map(|p| p.file_name().map(|n| n.to_owned())).collect();
    let result = match format {
        ArchiveFormat::Zip => write_zip(&base, &names, out),
        ArchiveFormat::TarGz => {
            let encoder = flate2::write::GzEncoder::new(std::fs::File::create(out)?, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            builder.follow_symlinks(false);
            for name in &names {
                let path = base.join(name);
                if path.is_dir() && !path.is_symlink() {
                    builder.append_dir_all(name, &path)?;
                } else {
                    builder.append_path_with_name(&path, name)?;
                }
            }
            builder.into_inner()?.finish().map(|_| ())
        }
        ArchiveFormat::TarXz | ArchiveFormat::TarBz2 | ArchiveFormat::TarZst => {
            let flag = match format {
                ArchiveFormat::TarXz => "--xz",
                ArchiveFormat::TarBz2 => "--bzip2",
                _ => "--zstd",
            };
            let mut cmd = Command::new("tar");
            cmd.arg("-c").arg(flag).arg("-f").arg(out).arg("-C").arg(&base).arg("--").args(&names);
            run(cmd)
        }
        ArchiveFormat::SevenZ => {
            let mut cmd = Command::new(seven_zip());
            cmd.current_dir(&base).arg("a").arg("-y").arg(out).args(&names);
            run(cmd)
        }
    };
    if result.is_err() {
        let _ = std::fs::remove_file(out);
    }
    result
}

fn write_zip(base: &Path, names: &[std::ffi::OsString], out: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use zip::write::SimpleFileOptions;

    fn add(zip: &mut zip::ZipWriter<std::fs::File>, path: &Path, name: &str) -> io::Result<()> {
        let meta = path.symlink_metadata()?;
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(meta.permissions().mode() & 0o7777)
            .large_file(meta.len() > u32::MAX as u64);
        if meta.is_dir() {
            zip.add_directory(format!("{name}/"), options).map_err(io::Error::other)?;
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                add(zip, &entry.path(), &format!("{name}/{}", entry.file_name().to_string_lossy()))?;
            }
        } else if meta.file_type().is_symlink() {
            zip.add_symlink(name, std::fs::read_link(path)?.to_string_lossy(), options).map_err(io::Error::other)?;
        } else {
            zip.start_file(name, options).map_err(io::Error::other)?;
            io::copy(&mut std::fs::File::open(path)?, zip)?;
        }
        Ok(())
    }

    let mut zip = zip::ZipWriter::new(std::fs::File::create(out)?);
    for name in names {
        add(&mut zip, &base.join(name), &name.to_string_lossy())?;
    }
    zip.finish().map(|_| ()).map_err(io::Error::other)
}

/// Unpack `archive` into the existing folder `dest`.
pub fn extract(archive: &Path, dest: &Path) -> io::Result<()> {
    let name = archive.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let ends = |exts: &[&str]| exts.iter().any(|e| name.ends_with(e));
    if ends(&[".zip", ".jar", ".apk"]) {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?).map_err(io::Error::other)?;
        zip.extract(dest).map_err(io::Error::other)
    } else if ends(&[".tar.gz", ".tgz"]) {
        tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(archive)?)).unpack(dest)
    } else if ends(&[".tar"]) {
        tar::Archive::new(std::fs::File::open(archive)?).unpack(dest)
    } else if ends(&[".tar.xz", ".txz", ".tar.bz2", ".tbz2", ".tar.zst", ".tzst"]) && have("tar") {
        let mut cmd = Command::new("tar");
        cmd.arg("-xf").arg(archive).arg("-C").arg(dest);
        run(cmd)
    } else if have("7z") || have("7za") {
        let mut cmd = Command::new(seven_zip());
        let mut out = std::ffi::OsString::from("-o");
        out.push(dest);
        cmd.arg("x").arg("-y").arg(out).arg(archive);
        run(cmd)
    } else {
        Err(io::Error::other(tr("No tool installed to unpack this kind of archive (install 7z)")))
    }
}

/// Extract into a fresh folder next to the archive (`<archive stem>`), returning that folder.
pub fn extract_beside(archive: &Path) -> io::Result<PathBuf> {
    let parent = archive.parent().ok_or_else(|| io::Error::other("no parent folder"))?;
    let dest = unique_path(parent, std::ffi::OsStr::new(&archive_stem(archive)), "extracted");
    std::fs::create_dir(&dest)?;
    match extract(archive, &dest) {
        Ok(()) => Ok(dest),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_mode_lets_copies_onto_shares_without_permissions_through() {
        // GVfs's SMB mount answers a chmod with "Operation not supported (os error 95)"
        assert!(keep_mode(Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))).is_ok());
        assert!(keep_mode(Err(io::Error::from_raw_os_error(libc::EPERM))).is_ok());
        assert!(keep_mode(Err(io::Error::from_raw_os_error(libc::ENOSPC))).is_err());
    }

    #[test]
    fn a_failed_move_across_file_systems_leaves_no_half_copied_target() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = std::env::temp_dir().join(format!("compression_ops_move_{}", std::process::id()));
        let shm = std::path::PathBuf::from("/dev/shm").join(format!("compression_ops_move_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&shm);
        std::fs::create_dir_all(root.join("tree/sub")).unwrap();
        // Only a copy between two file systems goes through the code under test; root can read everything
        if std::fs::create_dir_all(&shm).is_err() || shm.metadata().unwrap().dev() == root.metadata().unwrap().dev() || unsafe { libc::geteuid() } == 0 {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&shm);
            return;
        }
        std::fs::write(root.join("tree/a.txt"), "first").unwrap();
        std::fs::write(root.join("tree/sub/locked.txt"), "second").unwrap();
        std::fs::set_permissions(root.join("tree/sub/locked.txt"), std::fs::Permissions::from_mode(0o000)).unwrap();

        assert!(move_path(&root.join("tree"), &shm.join("tree")).is_err());
        assert!(!shm.join("tree").exists(), "the half-copied folder was left behind");
        assert_eq!(std::fs::read_to_string(root.join("tree/a.txt")).unwrap(), "first");

        // A target that was there before is not removed
        std::fs::create_dir_all(shm.join("there")).unwrap();
        std::fs::write(shm.join("there/mine.txt"), "mine").unwrap();
        assert!(move_path(&root.join("tree"), &shm.join("there")).is_err());
        assert_eq!(std::fs::read_to_string(shm.join("there/mine.txt")).unwrap(), "mine");

        std::fs::set_permissions(root.join("tree/sub/locked.txt"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&shm).unwrap();
    }

    #[test]
    fn the_copy_behind_trash_and_undo_refuses_a_named_pipe_instead_of_hanging() {
        let root = std::env::temp_dir().join(format!("compression_ops_fifo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tree")).unwrap();
        std::fs::write(root.join("tree/a.txt"), "a").unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("tree/link")).unwrap();
        let pipe = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(root.join("tree/pipe").as_os_str())).unwrap();
        // SAFETY: `pipe` is a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);

        // Run on a thread: a copy that opens the pipe would never return
        let (from, to) = (root.join("tree"), root.join("copy"));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(copy_tree(&from, &to)).unwrap());
        let err = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the copy hung on the pipe").unwrap_err();
        assert!(err.to_string().contains("named pipe"), "{err}");

        // Regular files and symlinks still copy
        std::fs::remove_file(root.join("tree/pipe")).unwrap();
        copy_tree(&root.join("tree"), &root.join("whole")).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("whole/a.txt")).unwrap(), "a");
        assert_eq!(std::fs::read_link(root.join("whole/link")).unwrap(), Path::new("a.txt"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn roundtrip() {
        let root = std::env::temp_dir().join(format!("compression_ops_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        std::fs::create_dir_all(src.join("d/e")).unwrap();
        std::fs::write(src.join("a.txt"), "hello").unwrap();
        std::fs::write(src.join("d/e/b.txt"), "world").unwrap();
        let srcs = vec![src.join("a.txt"), src.join("d")];
        for fmt in ArchiveFormat::ALL {
            if !fmt.available() { println!("skip {:?}", fmt); continue; }
            let out = src.join(format!("pack{}", fmt.extension()));
            compress(&srcs, &out, fmt).unwrap_or_else(|e| panic!("{fmt:?} compress: {e}"));
            let dest = extract_beside(&out).unwrap_or_else(|e| panic!("{fmt:?} extract: {e}"));
            assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "hello", "{fmt:?}");
            assert_eq!(std::fs::read_to_string(dest.join("d/e/b.txt")).unwrap(), "world", "{fmt:?}");
            println!("ok {:?} -> {}", fmt, dest.display());
        }
        assert!(unique_path(&src, std::ffi::OsStr::new("d"), "copy").ends_with("d (copy)"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
