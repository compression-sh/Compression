//! File operations: copy, move, compress and extract. Everything here blocks, so the app runs
//! it on a worker thread through the engine (`fs::engine`).
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::process::{self, Failure};
use crate::i18n::{tr, trf};

/// How long one archive may take, to pack or to unpack, before it is stopped.
pub const ARCHIVE_TIME_LIMIT: Duration = Duration::from_secs(30 * 60);

/// What the person at the window decides about a long archive job while it runs: whether it goes
/// on, and where it has got to. `compress` and `extract` ask before every chunk of work and give up
/// with `ErrorKind::Interrupted` (cancelled) or `ErrorKind::TimedOut` (out of time).
pub struct Control<'a> {
    /// True once the job was cancelled.
    cancelled: &'a dyn Fn() -> bool,
    /// Told how many bytes of the archive at hand are done (read when unpacking, written when packing).
    progress: &'a dyn Fn(u64),
    deadline: Instant,
}

impl<'a> Control<'a> {
    pub fn new(cancelled: &'a dyn Fn() -> bool, progress: &'a dyn Fn(u64), limit: Duration) -> Self {
        Self { cancelled, progress, deadline: Instant::now() + limit }
    }

    /// No cancelling, no limit worth speaking of, no one watching the progress. For tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn unlimited() -> Control<'static> {
        Control::new(&|| false, &|_| {}, Duration::from_secs(24 * 3600))
    }

    /// Tells the progress by hand, as the files being read or written do. For tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn report(&self, done: u64) {
        (self.progress)(done);
    }

    /// `Ok` while the job may go on.
    pub fn check(&self) -> io::Result<()> {
        if (self.cancelled)() {
            Err(stopped())
        } else if Instant::now() >= self.deadline {
            Err(timed_out())
        } else {
            Ok(())
        }
    }
}

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

fn timed_out() -> io::Error {
    let minutes = (ARCHIVE_TIME_LIMIT.as_secs() / 60).to_string();
    io::Error::new(io::ErrorKind::TimedOut, trf("It took longer than {} minutes and was stopped", &[&minutes]))
}

/// A cancelled or timed out job: the work of the archive is to be taken back.
fn is_stop(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::Interrupted | io::ErrorKind::TimedOut)
}

/// A file or stream that asks the `Control` before every read and write, and counts the bytes that
/// pass. An archive library that copies by itself is stopped through the file it reads or writes.
struct Watched<'a, T> {
    inner: T,
    control: &'a Control<'a>,
    done: u64,
}

impl<'a, T> Watched<'a, T> {
    fn new(inner: T, control: &'a Control<'a>) -> Self {
        Self { inner, control, done: 0 }
    }

    fn count(&mut self, n: usize) {
        self.done += n as u64;
        (self.control.progress)(self.done);
    }
}

impl<T> Watched<'_, T> {
    /// A stop, as an error that the library on the other side passes on: `ErrorKind::Interrupted`
    /// is not one, since `write_all` and `read_exact` try again on it for ever. `compress` and
    /// `extract` turn it into the real stop afterwards (the stop still holds then).
    fn check(&self) -> io::Result<()> {
        self.control.check().map_err(io::Error::other)
    }
}

impl<T: Read> Read for Watched<'_, T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let n = self.inner.read(buf)?;
        self.count(n);
        Ok(n)
    }
}

impl<T: Write> Write for Watched<'_, T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.check()?;
        let n = self.inner.write(buf)?;
        self.count(n);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<T: Seek> Seek for Watched<'_, T> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// The bytes in `paths` and everything below them, links not followed: the size of the work of packing.
pub fn tree_bytes(paths: &[PathBuf]) -> u64 {
    fn add(path: &Path, total: &mut u64) {
        let Ok(meta) = path.symlink_metadata() else { return };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    add(&entry.path(), total);
                }
            }
        } else {
            *total += meta.len();
        }
    }
    let mut total = 0;
    for path in paths {
        add(path, &mut total);
    }
    total
}

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

/// The most a packing or unpacking program may print before it is stopped; 7z names every file.
const TOOL_OUTPUT_LIMIT: usize = 64 << 20;

/// Runs an archive program to its end, in a process group of its own that is killed when the job is
/// cancelled or out of time. The program's last line of complaint is the error.
fn run_tool(command: Command, control: &Control) -> io::Result<()> {
    let left = control.deadline.saturating_duration_since(Instant::now());
    match process::run_capped_with(command, None, TOOL_OUTPUT_LIMIT, left, &|| (control.cancelled)()) {
        Ok(output) if output.success => Ok(()),
        Ok(output) => {
            let text = String::from_utf8_lossy(if output.stderr.is_empty() { &output.stdout } else { &output.stderr });
            Err(io::Error::other(text.lines().last().unwrap_or("command failed").to_owned()))
        }
        Err(Failure::Cancelled) => Err(stopped()),
        Err(Failure::TimedOut) => Err(timed_out()),
        Err(Failure::TooMuchOutput) => Err(io::Error::other(tr("The archive program printed too much"))),
        Err(Failure::Io(e)) => Err(e),
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

/// Pack `sources` (all in the same folder) into the archive `out`. A job that is cancelled or runs
/// out of time leaves no `out` behind: an archive that is not whole is no archive.
pub fn compress(sources: &[PathBuf], out: &Path, format: ArchiveFormat, control: &Control) -> io::Result<()> {
    let base = sources
        .first()
        .and_then(|p| p.parent())
        .ok_or_else(|| io::Error::other(tr("Nothing to compress")))?
        .to_path_buf();
    let names: Vec<_> = sources.iter().filter_map(|p| p.file_name().map(|n| n.to_owned())).collect();
    let result = match format {
        ArchiveFormat::Zip => write_zip(&base, &names, out, control),
        ArchiveFormat::TarGz => (|| {
            let file = std::fs::File::create(out)?;
            // The tar stream is watched where it enters the compressor, so the count is the size of the files
            let encoder = Watched::new(flate2::write::GzEncoder::new(file, flate2::Compression::default()), control);
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
            builder.into_inner()?.inner.finish().map(|_| ())
        })(),
        ArchiveFormat::TarXz | ArchiveFormat::TarBz2 | ArchiveFormat::TarZst => {
            let flag = match format {
                ArchiveFormat::TarXz => "--xz",
                ArchiveFormat::TarBz2 => "--bzip2",
                _ => "--zstd",
            };
            let mut cmd = Command::new("tar");
            cmd.arg("-c").arg(flag).arg("-f").arg(out).arg("-C").arg(&base).arg("--").args(&names);
            run_tool(cmd, control)
        }
        ArchiveFormat::SevenZ => {
            let mut cmd = Command::new(seven_zip());
            cmd.current_dir(&base).arg("a").arg("-y").arg(out).args(&names);
            run_tool(cmd, control)
        }
    };
    // An archive library may report the stop in its own words: a stop that holds now is the reason
    let result = result.map_err(|e| if is_stop(&e) { e } else { control.check().err().unwrap_or(e) });
    if result.is_err() {
        let _ = std::fs::remove_file(out);
    }
    result
}

fn write_zip(base: &Path, names: &[std::ffi::OsString], out: &Path, control: &Control) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use zip::write::SimpleFileOptions;

    fn add(zip: &mut zip::ZipWriter<std::fs::File>, path: &Path, name: &str, control: &Control, done: &mut u64) -> io::Result<()> {
        control.check()?;
        let meta = path.symlink_metadata()?;
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(meta.permissions().mode() & 0o7777)
            .large_file(meta.len() > u32::MAX as u64);
        if meta.is_dir() {
            zip.add_directory(format!("{name}/"), options).map_err(io::Error::other)?;
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                add(zip, &entry.path(), &format!("{name}/{}", entry.file_name().to_string_lossy()), control, done)?;
            }
        } else if meta.file_type().is_symlink() {
            zip.add_symlink(name, std::fs::read_link(path)?.to_string_lossy(), options).map_err(io::Error::other)?;
        } else {
            zip.start_file(name, options).map_err(io::Error::other)?;
            let mut file = std::fs::File::open(path)?;
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                control.check()?;
                let n = file.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                zip.write_all(&chunk[..n])?;
                *done += n as u64;
                (control.progress)(*done);
            }
        }
        Ok(())
    }

    let mut zip = zip::ZipWriter::new(std::fs::File::create(out)?);
    let mut done = 0;
    for name in names {
        add(&mut zip, &base.join(name), &name.to_string_lossy(), control, &mut done)?;
    }
    zip.finish().map(|_| ()).map_err(io::Error::other)
}

/// An I/O error inside a zip keeps its kind, so that a stop is still a stop.
fn zip_error(err: zip::result::ZipError) -> io::Error {
    match err {
        zip::result::ZipError::Io(e) => e,
        other => io::Error::other(other),
    }
}

/// Unpack `archive` into the existing folder `dest`. A job that is cancelled or runs out of time
/// takes back what it made in `dest`: the items that were not there when it began.
pub fn extract(archive: &Path, dest: &Path, control: &Control) -> io::Result<()> {
    let before = names_in(dest);
    let result = unpack(archive, dest, control).map_err(|e| if is_stop(&e) { e } else { control.check().err().unwrap_or(e) });
    if let (Err(err), Some(before)) = (&result, before)
        && is_stop(err)
    {
        for name in names_in(dest).unwrap_or_default().difference(&before) {
            let path = dest.join(name);
            let _ = if path.is_dir() && !path.is_symlink() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        }
    }
    result
}

fn names_in(dir: &Path) -> Option<std::collections::HashSet<std::ffi::OsString>> {
    Some(std::fs::read_dir(dir).ok()?.flatten().map(|e| e.file_name()).collect())
}

fn unpack(archive: &Path, dest: &Path, control: &Control) -> io::Result<()> {
    let name = archive.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let ends = |exts: &[&str]| exts.iter().any(|e| name.ends_with(e));
    // The archive is read through `Watched`: what is read is how far the job is, and a stop ends the read
    let source = || -> io::Result<Watched<std::fs::File>> { Ok(Watched::new(std::fs::File::open(archive)?, control)) };
    if ends(&[".zip", ".jar", ".apk"]) {
        let mut zip = zip::ZipArchive::new(source()?).map_err(zip_error)?;
        zip.extract(dest).map_err(zip_error)
    } else if ends(&[".tar.gz", ".tgz"]) {
        tar::Archive::new(flate2::read::GzDecoder::new(source()?)).unpack(dest)
    } else if ends(&[".tar"]) {
        tar::Archive::new(source()?).unpack(dest)
    } else if ends(&[".tar.xz", ".txz", ".tar.bz2", ".tbz2", ".tar.zst", ".tzst"]) && have("tar") {
        let mut cmd = Command::new("tar");
        cmd.arg("-xf").arg(archive).arg("-C").arg(dest);
        run_tool(cmd, control)
    } else if have("7z") || have("7za") {
        let mut cmd = Command::new(seven_zip());
        let mut out = std::ffi::OsString::from("-o");
        out.push(dest);
        cmd.arg("x").arg("-y").arg(out).arg(archive);
        run_tool(cmd, control)
    } else {
        Err(io::Error::other(tr("No tool installed to unpack this kind of archive (install 7z)")))
    }
}

/// Extract into a fresh folder next to the archive (`<archive stem>`), returning that folder.
pub fn extract_beside(archive: &Path, control: &Control) -> io::Result<PathBuf> {
    let parent = archive.parent().ok_or_else(|| io::Error::other("no parent folder"))?;
    let dest = unique_path(parent, std::ffi::OsStr::new(&archive_stem(archive)), "extracted");
    std::fs::create_dir(&dest)?;
    match extract(archive, &dest, control) {
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
            compress(&srcs, &out, fmt, &Control::unlimited()).unwrap_or_else(|e| panic!("{fmt:?} compress: {e}"));
            let dest = extract_beside(&out, &Control::unlimited()).unwrap_or_else(|e| panic!("{fmt:?} extract: {e}"));
            assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "hello", "{fmt:?}");
            assert_eq!(std::fs::read_to_string(dest.join("d/e/b.txt")).unwrap(), "world", "{fmt:?}");
            println!("ok {:?} -> {}", fmt, dest.display());
        }
        assert!(unique_path(&src, std::ffi::OsStr::new("d"), "copy").ends_with("d (copy)"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    // ---- Cancelling, the time limit and the progress of archive jobs ----

    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

    /// A folder of its own with `src/` holding incompressible files, so archives are about as big as their contents.
    fn archive_sandbox(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("compression_ops_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/sub")).unwrap();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut noise = |len: usize| -> Vec<u8> {
            (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state >> 24) as u8
                })
                .collect()
        };
        std::fs::write(root.join("src/one.bin"), noise(1 << 20)).unwrap();
        std::fs::write(root.join("src/two.bin"), noise(1 << 20)).unwrap();
        std::fs::write(root.join("src/sub/three.bin"), noise(1 << 20)).unwrap();
        std::fs::write(root.join("src/sub/four.bin"), noise(1 << 20)).unwrap();
        root
    }

    fn sources(root: &Path) -> Vec<PathBuf> {
        vec![root.join("src/one.bin"), root.join("src/two.bin"), root.join("src/sub")]
    }

    #[test]
    fn the_size_of_the_work_of_packing_is_the_bytes_below_the_sources() {
        let root = archive_sandbox("size");
        assert_eq!(tree_bytes(&sources(&root)), 4 << 20);
        assert_eq!(tree_bytes(&[root.join("missing")]), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn packing_reports_the_bytes_it_has_done() {
        let root = archive_sandbox("pack_progress");
        let total = tree_bytes(&sources(&root));
        for format in [ArchiveFormat::Zip, ArchiveFormat::TarGz] {
            let (last, calls) = (AtomicU64::new(0), AtomicU64::new(0));
            let progress = |done: u64| {
                // Never backwards
                assert!(done >= last.swap(done, Relaxed));
                calls.fetch_add(1, Relaxed);
            };
            let control = Control::new(&|| false, &progress, Duration::from_secs(60));
            let out = root.join(format!("pack{}", format.extension()));
            compress(&sources(&root), &out, format, &control).unwrap();
            // The zip counts the files' bytes; the tar stream adds its headers and padding
            let done = last.load(Relaxed);
            assert!(done >= total && done < total + 64 * 1024, "{format:?}: {done} of {total}");
            assert!(calls.load(Relaxed) > 4, "{format:?}: progress comes in steps, not once at the end");
            assert!(out.is_file());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unpacking_reports_how_much_of_the_archive_it_has_read() {
        let root = archive_sandbox("unpack_progress");
        let out = root.join("pack.zip");
        compress(&sources(&root), &out, ArchiveFormat::Zip, &Control::unlimited()).unwrap();
        let (last, calls) = (AtomicU64::new(0), AtomicU64::new(0));
        let progress = |done: u64| {
            assert!(done >= last.swap(done, Relaxed));
            calls.fetch_add(1, Relaxed);
        };
        let control = Control::new(&|| false, &progress, Duration::from_secs(60));
        let dest = extract_beside(&out, &control).unwrap();
        assert!(dest.join("sub/four.bin").is_file());
        assert!(last.load(Relaxed) >= out.metadata().unwrap().len() / 2, "it read most of the archive");
        assert!(calls.load(Relaxed) > 4);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_cancelled_pack_stops_half_way_and_leaves_no_archive() {
        let root = archive_sandbox("pack_cancel");
        for format in [ArchiveFormat::Zip, ArchiveFormat::TarGz] {
            let cancel = AtomicBool::new(false);
            // The person presses Cancel once about a megabyte is done
            let progress = |done: u64| {
                if done >= 1 << 20 {
                    cancel.store(true, Relaxed);
                }
            };
            let stop = || cancel.load(Relaxed);
            let control = Control::new(&stop, &progress, Duration::from_secs(60));
            let out = root.join(format!("pack{}", format.extension()));
            let err = compress(&sources(&root), &out, format, &control).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Interrupted, "{format:?}: {err}");
            assert!(!out.exists(), "{format:?}: an unfinished archive was left behind");
        }
        // The sources are untouched
        assert_eq!(tree_bytes(&sources(&root)), 4 << 20);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_cancelled_unpack_takes_back_what_it_made_and_keeps_what_was_there() {
        let root = archive_sandbox("unpack_cancel");
        for (format, ext) in [(ArchiveFormat::Zip, "zip"), (ArchiveFormat::TarGz, "tar.gz")] {
            let out = root.join(format!("pack.{ext}"));
            compress(&sources(&root), &out, format, &Control::unlimited()).unwrap();
            let dest = root.join(format!("into_{ext}"));
            std::fs::create_dir_all(dest.join("old")).unwrap();
            std::fs::write(dest.join("keep.txt"), "mine").unwrap();
            let cancel = AtomicBool::new(false);
            let progress = |done: u64| {
                if done >= 2 << 20 {
                    cancel.store(true, Relaxed);
                }
            };
            let stop = || cancel.load(Relaxed);
            let control = Control::new(&stop, &progress, Duration::from_secs(60));
            let err = extract(&out, &dest, &control).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Interrupted, "{ext}: {err}");
            let mut left: Vec<String> = std::fs::read_dir(&dest).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            left.sort();
            assert_eq!(left, ["keep.txt", "old"], "{ext}");
            assert_eq!(std::fs::read_to_string(dest.join("keep.txt")).unwrap(), "mine");
        }
        // Next to the archive, the whole new folder goes
        let out = root.join("pack.zip");
        let cancel = AtomicBool::new(true);
        let stop = || cancel.load(Relaxed);
        let control = Control::new(&stop, &|_| {}, Duration::from_secs(60));
        assert!(extract_beside(&out, &control).is_err());
        assert!(!root.join("pack").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_job_out_of_time_stops_like_a_cancelled_one_but_says_why() {
        let root = archive_sandbox("timeout");
        let out = root.join("pack.zip");
        let none = Control::new(&|| false, &|_| {}, Duration::ZERO);
        let err = compress(&sources(&root), &out, ArchiveFormat::Zip, &none).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("30"), "{err}");
        assert!(!out.exists());

        compress(&sources(&root), &out, ArchiveFormat::Zip, &Control::unlimited()).unwrap();
        let dest = root.join("into");
        std::fs::create_dir_all(&dest).unwrap();
        let err = extract(&out, &dest, &none).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0);

        // The same through the libraries that copy by themselves
        let tgz = root.join("pack.tar.gz");
        let err = compress(&sources(&root), &tgz, ArchiveFormat::TarGz, &none).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(!tgz.exists());
        compress(&sources(&root), &tgz, ArchiveFormat::TarGz, &Control::unlimited()).unwrap();
        let err = extract(&tgz, &dest, &none).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_program_that_packs_is_killed_when_cancelled_or_out_of_time() {
        let sleeper = || {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg("sleep 30");
            cmd
        };
        // Out of time
        let started = Instant::now();
        let control = Control::new(&|| false, &|_| {}, Duration::from_millis(200));
        let err = run_tool(sleeper(), &control).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
        // Cancelled while it runs
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let err = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                cancel.store(true, Relaxed);
            });
            run_tool(sleeper(), &Control::new(&|| cancel.load(Relaxed), &|_| {}, Duration::from_secs(60))).unwrap_err()
        });
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(10));
        // A program that ends normally, and one that complains
        let ok = Control::unlimited();
        let mut fine = Command::new("sh");
        fine.arg("-c").arg("echo packed");
        assert!(run_tool(fine, &ok).is_ok());
        let mut bad = Command::new("sh");
        bad.arg("-c").arg("echo first >&2; echo cannot open >&2; exit 2");
        assert_eq!(run_tool(bad, &ok).unwrap_err().to_string(), "cannot open");
    }
}
