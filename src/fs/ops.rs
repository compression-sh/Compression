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

/// Gives a copy the access and modification times of what it was copied from, through its open file. Not being able
/// to is no reason to fail the copy (some file systems and mounts refuse), so it is best effort.
pub(crate) fn keep_times(copy: &std::fs::File, original: &std::fs::Metadata) {
    let mut times = std::fs::FileTimes::new();
    if let Ok(accessed) = original.accessed() {
        times = times.set_accessed(accessed);
    }
    if let Ok(modified) = original.modified() {
        times = times.set_modified(modified);
    }
    let _ = copy.set_times(times);
}

/// The same for a folder, which has to be done after what is in it was made (that changes its time).
pub(crate) fn keep_folder_times(copy: &Path, original: &std::fs::Metadata) {
    if let Ok(folder) = std::fs::File::open(copy) {
        keep_times(&folder, original);
    }
}

/// The same for a symbolic link itself, not what it points to.
pub(crate) fn keep_link_times(copy: &Path, original: &std::fs::Metadata) {
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    let Ok(path) = std::ffi::CString::new(copy.as_os_str().as_bytes()) else { return };
    let times = [
        libc::timespec { tv_sec: original.atime(), tv_nsec: original.atime_nsec() },
        libc::timespec { tv_sec: original.mtime(), tv_nsec: original.mtime_nsec() },
    ];
    // SAFETY: `path` is NUL-terminated and `times` holds the two timespecs the call reads.
    unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
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
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
        keep_link_times(dst, &meta);
        Ok(())
    } else if meta.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        super::xattr::copy_user(src, dst);
        keep_folder_times(dst, &meta);
        keep_mode(std::fs::set_permissions(dst, meta.permissions()))
    } else if !meta.is_file() {
        // Opening a named pipe waits for a writer that may never come
        let name = src.file_name().map_or_else(|| src.display().to_string(), |n| n.to_string_lossy().into_owned());
        Err(io::Error::other(trf("Cannot copy {}: it is a named pipe, socket or device file", &[&name])))
    } else {
        // Not `std::fs::copy`: it fails the whole copy when the target refuses the permissions
        let mut reader = std::fs::File::open(src)?;
        // Not `File::create`: a file that appeared there meanwhile is not ours to cut short
        let mut writer = std::fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
        io::copy(&mut reader, &mut writer)?;
        super::xattr::copy_user(src, dst);
        keep_times(&writer, &meta);
        keep_mode(writer.set_permissions(meta.permissions()))
    }
}

/// Rename `src` to the exact path `dst`, copying across filesystems when it has to.
/// Never replaces anything: `dst` is a name the caller has found free, and one taken since is an error.
pub fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    match crate::compat::rename_noreplace(src, dst) {
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
    // `out` was free when its name was chosen. A file that is there now is not ours to fill, nor to
    // remove when the job fails, so the job ends before it touches anything
    if out.symlink_metadata().is_ok() {
        return Err(taken(out));
    }
    // Set when someone else made `out` meanwhile, in which case the clean-up below must leave it alone
    let foreign = std::cell::Cell::new(false);
    let create = || {
        std::fs::OpenOptions::new().write(true).create_new(true).open(out).inspect_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                foreign.set(true);
            }
        })
    };
    let result = match format {
        ArchiveFormat::Zip => create().and_then(|file| write_zip(&base, &names, file, control)),
        ArchiveFormat::TarGz => (|| {
            let file = create()?;
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
    if result.is_err() && !foreign.get() {
        let _ = std::fs::remove_file(out);
    }
    result
}

/// The error for a name that is already taken.
fn taken(path: &Path) -> io::Error {
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    io::Error::new(io::ErrorKind::AlreadyExists, trf("“{}” already exists", &[&name]))
}

/// Make a new file with `bytes` in it, named like `name` and not like anything already in `dir`. A name
/// that is taken between choosing and creating it moves on to the next one (up to five times); a file
/// that was not made here is never written to.
pub fn write_new(dir: &Path, name: &std::ffi::OsStr, suffix: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let mut last = None;
    for _ in 0..5 {
        let path = unique_path(dir, name, suffix);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                if let Err(err) = file.write_all(bytes) {
                    let _ = std::fs::remove_file(&path);
                    return Err(err);
                }
                return Ok(path);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last = Some(taken(&path)),
            Err(err) => return Err(err),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no free name")))
}

fn write_zip(base: &Path, names: &[std::ffi::OsString], file: std::fs::File, control: &Control) -> io::Result<()> {
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

    let mut zip = zip::ZipWriter::new(file);
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

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("compression_ops_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn moving_to_a_name_that_is_taken_is_an_error_and_changes_nothing() {
        let dir = dir("move_path");
        std::fs::write(dir.join("a"), "from").unwrap();
        std::fs::write(dir.join("b"), "taken").unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        for target in ["b", "empty"] {
            let err = move_path(&dir.join("a"), &dir.join(target)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{target}: {err}");
        }
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "from");
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "taken");
        move_path(&dir.join("a"), &dir.join("free")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("free")).unwrap(), "from");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copying_a_tree_does_not_cut_short_a_file_that_is_in_the_way() {
        let dir = dir("copy_tree");
        std::fs::write(dir.join("a"), "from").unwrap();
        std::fs::write(dir.join("b"), "taken").unwrap();
        let err = copy_tree(&dir.join("a"), &dir.join("b")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "taken");
        copy_tree(&dir.join("a"), &dir.join("c")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("c")).unwrap(), "from");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_noreplace_support_move_path_refuses_and_leaves_source_and_target_alone() {
        let dir = dir("move_path_refused");
        crate::compat::tests::without_support_under(&dir);
        std::fs::write(dir.join("a"), "from").unwrap();
        std::fs::write(dir.join("b"), "taken").unwrap();
        // A free name: not supported, not a conflict
        let err = move_path(&dir.join("a"), &dir.join("free")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        // A taken one: the conflict
        assert_eq!(move_path(&dir.join("a"), &dir.join("b")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "from");
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "taken");
        assert!(!dir.join("free").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_copy_used_across_file_systems_by_trash_and_undo_keeps_times_and_attributes_too() {
        use std::os::unix::fs::MetadataExt;
        let dir = dir("copy_tree_meta");
        std::fs::create_dir_all(dir.join("t/inner")).unwrap();
        std::fs::write(dir.join("t/inner/f"), "f").unwrap();
        std::os::unix::fs::symlink("inner/f", dir.join("t/l")).unwrap();
        let tagged = crate::fs::xattr::set(&dir.join("t/inner/f"), "user.xdg.tags", b"tag").is_ok();
        let age = |p: &Path, secs: i64| {
            let c = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str())).unwrap();
            let t = [libc::timespec { tv_sec: secs, tv_nsec: 5 }, libc::timespec { tv_sec: secs + 1, tv_nsec: 6 }];
            // SAFETY: NUL-terminated path, two timespecs.
            assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), t.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) }, 0);
        };
        for (p, s) in [("t/inner/f", 1_000_000_000), ("t/l", 1_000_000_100), ("t/inner", 1_000_000_200), ("t", 1_000_000_300)] {
            age(&dir.join(p), s);
        }
        copy_tree(&dir.join("t"), &dir.join("c")).unwrap();
        for (p, s) in [("inner/f", 1_000_000_000), ("l", 1_000_000_100), ("inner", 1_000_000_200), ("", 1_000_000_300)] {
            let m = dir.join("c").join(p).symlink_metadata().unwrap();
            assert_eq!((m.atime(), m.mtime(), m.mtime_nsec()), (s, s + 1, 6), "{p:?}");
        }
        assert_eq!(std::fs::read_link(dir.join("c/l")).unwrap(), Path::new("inner/f"));
        if tagged {
            assert_eq!(crate::fs::xattr::get(&dir.join("c/inner/f"), "user.xdg.tags").unwrap().as_deref(), Some(&b"tag"[..]));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
