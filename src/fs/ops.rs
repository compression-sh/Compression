//! File operations: copy, move, compress and extract. Everything here blocks, so the app runs
//! it on a worker thread through the engine (`fs::engine`).
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::process::{self, Failure};
use crate::i18n::{tr, trf};

/// What the person at the window decides about a long archive job while it runs: whether it goes
/// on, and where it has got to. `compress` and `extract` ask before every chunk of work and give up
/// with `ErrorKind::Interrupted` once it is cancelled. There is no time limit: packing or unpacking
/// many gigabytes onto a slow drive takes as long as it takes, and the person can stop it.
pub struct Control<'a> {
    /// True once the job was cancelled.
    cancelled: &'a dyn Fn() -> bool,
    /// Told how many bytes of the archive at hand are done (read when unpacking, written when packing).
    progress: &'a dyn Fn(u64),
}

impl<'a> Control<'a> {
    pub fn new(cancelled: &'a dyn Fn() -> bool, progress: &'a dyn Fn(u64)) -> Self {
        Self { cancelled, progress }
    }

    /// `Ok` while the job may go on.
    pub fn check(&self) -> io::Result<()> {
        if (self.cancelled)() { Err(stopped()) } else { Ok(()) }
    }
}

/// How long an archive program may run: as long as it takes (it is stopped by a cancel, see `Control`). The wait has to
/// be some length, and this one is far beyond any job.
const NO_TIME_LIMIT: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

#[cfg(test)]
impl Control<'_> {
    /// No cancelling, no one watching the progress.
    pub fn unlimited() -> Control<'static> {
        Control::new(&|| false, &|_| {})
    }

    /// Tells the progress by hand, as the files being read or written do.
    pub fn report(&self, done: u64) {
        (self.progress)(done);
    }
}

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

/// A cancelled job: the work of the archive is to be taken back.
fn is_stop(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted
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

/// What copying the tags and ACLs of one item came to.
pub(crate) struct Kept {
    /// The permissions the copy is to get. If the source has an access ACL and the copy could not be given it, the group
    /// bits of the mode are not the owning group's permission but the ACL's mask, so they are not taken over: the copy
    /// gets no group permissions, which is never more than the source gave anyone.
    pub permissions: std::fs::Permissions,
    /// Every `user.*` attribute arrived.
    pub tags: bool,
    pub acl: super::xattr::AclCopy,
}

/// The tags and ACLs of `src` onto its copy `dst`, and the permissions the copy is to get from `original`. Call it
/// before setting them.
pub(crate) fn keep_attributes(src: &Path, dst: &Path, original: &std::fs::Metadata) -> Kept {
    use std::os::unix::fs::PermissionsExt;
    let tags = super::xattr::copy_user(src, dst);
    let acl = super::xattr::copy_acl(src, dst, original.is_dir());
    let permissions = if acl.access_lost { std::fs::Permissions::from_mode(original.permissions().mode() & !0o070) } else { original.permissions() };
    Kept { permissions, tags, acl }
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

/// Whether a file has holes worth keeping: it uses less room than its size says. (Not a proof: a compressed file
/// does too; the copy then simply finds one data range as long as the file.)
pub(crate) fn may_have_holes(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.is_file() && meta.len() > 0 && meta.blocks().saturating_mul(512) < meta.len()
}

/// What `copy_holey` came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Holey {
    /// The data ranges were copied and the copy has the length of the original; the holes stayed holes (where the
    /// target file system has them).
    Copied,
    /// The source file system cannot tell data from holes (`SEEK_DATA`): nothing was written, copy it the plain way.
    Unavailable,
}

/// The most one `step` is asked to copy.
const HOLEY_STEP: u64 = 1 << 20;

fn seek_to(file: &std::fs::File, offset: u64, whence: libc::c_int) -> io::Result<u64> {
    use std::os::fd::AsRawFd;
    #[cfg(test)]
    if let Some(errno) = tests::seek_fails_for(file) {
        return Err(io::Error::from_raw_os_error(errno));
    }
    let offset = libc::off_t::try_from(offset).map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
    // SAFETY: an open descriptor; the call only moves its position
    let at = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if at < 0 { Err(io::Error::last_os_error()) } else { Ok(at as u64) }
}

/// Copies `reader` (of `len` bytes) into the new, empty file `writer` without writing its holes: only the data
/// ranges (found with `SEEK_DATA` and `SEEK_HOLE`) are copied, each by `step(most, first)` (which copies up to `most`
/// bytes at both files' positions and returns how many, 0 at the end), then the length is set. `check` is asked before
/// each step (to pause or stop), `progress` is told every byte passed, holes included, so a sparse file's progress still
/// ends at its size.
pub(crate) fn copy_holey(
    reader: &std::fs::File,
    writer: &std::fs::File,
    len: u64,
    check: &mut dyn FnMut() -> io::Result<()>,
    step: &mut dyn FnMut(u64, bool) -> io::Result<usize>,
    progress: &mut dyn FnMut(u64),
) -> io::Result<Holey> {
    let (mut reader_at, mut writer_at) = (reader, writer);
    let mut pos = 0;
    let mut asked = false;
    while pos < len {
        check()?;
        let data = match seek_to(reader, pos, libc::SEEK_DATA) {
            Ok(data) => data,
            // Nothing but a hole is left
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => break,
            // Not a file system that can tell (and nothing was written yet)
            Err(e) if !asked && matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)) => return Ok(Holey::Unavailable),
            Err(e) => return Err(e),
        };
        asked = true;
        if data >= len {
            break;
        }
        let end = seek_to(reader, data, libc::SEEK_HOLE).unwrap_or(len).min(len).max(data);
        progress(data - pos);
        reader_at.seek(SeekFrom::Start(data))?;
        writer_at.seek(SeekFrom::Start(data))?;
        let (mut left, mut first) = (end - data, true);
        while left > 0 {
            check()?;
            let n = step(left.min(HOLEY_STEP), first)? as u64;
            if n == 0 {
                // The file got shorter while it was copied: the copy ends where the data did
                return Ok(Holey::Copied);
            }
            first = false;
            left = left.saturating_sub(n);
            progress(n);
        }
        pos = end;
    }
    progress(len.saturating_sub(pos));
    writer.set_len(len)?;
    Ok(Holey::Copied)
}

/// What is in a folder, read without setting the folder's access time. Reading a folder sets it on a file system
/// mounted with relatime (the usual way), and the copy of a folder is to have the access time the original had before
/// the job looked at it. The folder is opened with `O_NOATIME`, which the system allows for the owner of the folder (and
/// root); for anyone else the folder is read the usual way and its access time may change, as with any reader.
pub(crate) struct DirPaths {
    dir: *mut libc::DIR,
    base: PathBuf,
}

pub(crate) fn read_dir_keeping_atime(path: &Path) -> io::Result<DirPaths> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: `c` is a NUL-terminated path that outlives the call.
    let mut fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_NOATIME) };
    if fd < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
        // SAFETY: as above.
        fd = unsafe { libc::open(c.as_ptr(), flags) };
    }
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is an open folder; on success the DIR owns it and `closedir` closes it.
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        let err = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so `fd` is still ours to close.
        unsafe { libc::close(fd) };
        return Err(err);
    }
    Ok(DirPaths { dir, base: path.to_path_buf() })
}

impl Iterator for DirPaths {
    type Item = io::Result<PathBuf>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::os::unix::ffi::OsStrExt;
        loop {
            // `readdir` leaves errno alone at the end and sets it on an error
            // SAFETY: `__errno_location` is the thread's errno; `self.dir` is an open DIR until `drop`.
            let entry = unsafe {
                *libc::__errno_location() = 0;
                libc::readdir(self.dir)
            };
            if entry.is_null() {
                let err = io::Error::last_os_error();
                return if err.raw_os_error() == Some(0) { None } else { Some(Err(err)) };
            }
            // SAFETY: a non-null entry from `readdir` has a NUL-terminated name, valid until the next call.
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                return Some(Ok(self.base.join(std::ffi::OsStr::from_bytes(name))));
            }
        }
    }
}

impl Drop for DirPaths {
    fn drop(&mut self) {
        // SAFETY: `self.dir` came from `fdopendir` and is closed only here.
        unsafe { libc::closedir(self.dir) };
    }
}

#[cfg(test)]
impl DirPaths {
    /// The descriptor of the open folder, for a test to ask the system how it was opened.
    pub(crate) fn raw_fd(&self) -> i32 {
        // SAFETY: `self.dir` is an open DIR.
        unsafe { libc::dirfd(self.dir) }
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
    // Made of the bytes of the name, so a name that is not UTF-8 keeps them
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let (stem, ext) = split_bytes(name.as_bytes());
    for n in 1.. {
        let tag = if n == 1 { format!(" ({suffix})") } else { format!(" ({suffix} {n})") };
        let candidate = dir.join(std::ffi::OsString::from_vec([stem, tag.as_bytes(), ext].concat()));
        if free(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// `("archive", ".tar.gz")` / `("photo", ".png")` / `("Makefile", "")`.
fn split_name(name: &str) -> (&str, &str) {
    // The split is at an ASCII dot, so both halves are whole UTF-8
    let stem = split_bytes(name.as_bytes()).0.len();
    (&name[..stem], &name[stem..])
}

/// `split_name` for the bytes of a name, which need not be UTF-8.
fn split_bytes(name: &[u8]) -> (&[u8], &[u8]) {
    for double in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst"] {
        if let Some(stem) = name.strip_suffix(double.as_bytes())
            && !stem.is_empty() {
                return (stem, &name[stem.len()..]);
            }
    }
    match name.iter().rposition(|&b| b == b'.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, &[]),
    }
}

/// Open `path` for reading, as the regular file it was looked at as: a name that has become a pipe (or a link) since
/// must not hold the job up (opening a pipe waits for a writer that may never come) nor be followed.
pub(crate) fn open_regular_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(test)]
    tests::swap_before_open(path);
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(path)?;
    if !file.metadata()?.file_type().is_file() {
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        return Err(io::Error::other(trf("Cannot copy {}: it is a named pipe, socket or device file", &[&name])));
    }
    // The flag was for the opening; a file is read the usual way
    // SAFETY: the descriptor belongs to `file`, which is open; `fcntl` with these commands only reads and sets flags.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
    Ok(file)
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
        let kept = keep_attributes(src, dst, &meta);
        keep_folder_times(dst, &meta);
        keep_mode(std::fs::set_permissions(dst, kept.permissions))
    } else if !meta.is_file() {
        // Opening a named pipe waits for a writer that may never come
        let name = src.file_name().map_or_else(|| src.display().to_string(), |n| n.to_string_lossy().into_owned());
        Err(io::Error::other(trf("Cannot copy {}: it is a named pipe, socket or device file", &[&name])))
    } else {
        // Not `std::fs::copy`: it fails the whole copy when the target refuses the permissions
        let reader = open_regular_file(src)?;
        // Not `File::create`: a file that appeared there meanwhile is not ours to cut short
        let writer = std::fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
        copy_contents(&reader, &writer, &meta)?;
        let kept = keep_attributes(src, dst, &meta);
        keep_times(&writer, &meta);
        keep_mode(writer.set_permissions(kept.permissions))
    }
}

/// The contents of `reader` into the new file `writer`; a file with holes keeps them.
fn copy_contents(reader: &std::fs::File, writer: &std::fs::File, meta: &std::fs::Metadata) -> io::Result<()> {
    if may_have_holes(meta) {
        let (from, mut to) = (reader, writer);
        let mut step = |most: u64, _first: bool| io::copy(&mut from.take(most), &mut to).map(|n| n as usize);
        if copy_holey(reader, writer, meta.len(), &mut || Ok(()), &mut step, &mut |_| {})? == Holey::Copied {
            return Ok(());
        }
    }
    io::copy(&mut &*reader, &mut &*writer).map(drop)
}

/// The error of a move across file systems whose copy is whole but whose original could not be removed completely.
/// Nothing is lost: the copy stays where it was made, and so does what is left of the original. Callers that can
/// still make sense of the move (the trash, an undo) look for it with `is_copied_not_removed` and say so.
#[derive(Debug)]
pub struct CopiedNotRemoved {
    /// Where the whole copy is.
    pub copy: PathBuf,
    cause: io::Error,
}

impl std::fmt::Display for CopiedNotRemoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&trf("The copy is complete, but the original could not be fully removed: {}", &[&super::describe::io_error(&self.cause)]))
    }
}

impl std::error::Error for CopiedNotRemoved {}

impl CopiedNotRemoved {
    /// The error for a whole copy at `copy` whose original could not be removed because of `cause`.
    pub(crate) fn error(copy: &Path, cause: io::Error) -> io::Error {
        io::Error::other(Self { copy: copy.to_path_buf(), cause })
    }
}

/// How an item is moved to another place: `move_path`, or a move that a job can follow and stop
/// (`engine::move_item`). It is handed to the trash and to the undo, which move things on their own.
pub type Mover<'a> = &'a dyn Fn(&Path, &Path) -> io::Result<()>;

/// The details when `err` is the error of a move whose copy is whole and whose original is not (all) gone.
pub fn copied_not_removed(err: &io::Error) -> Option<&CopiedNotRemoved> {
    err.get_ref().and_then(|inner| inner.downcast_ref::<CopiedNotRemoved>())
}

/// Whether `err` is that error, see `CopiedNotRemoved`.
pub fn is_copied_not_removed(err: &io::Error) -> bool {
    copied_not_removed(err).is_some()
}

/// Rename `src` to the exact path `dst`, copying across filesystems when it has to.
/// Never replaces anything: `dst` is a name the caller has found free, and one taken since is an error.
/// When the copy is whole and the original cannot be removed (all of it), the error is `CopiedNotRemoved`.
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
            remove_tree(src).map_err(|cause| io::Error::other(CopiedNotRemoved { copy: dst.to_path_buf(), cause }))
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
/// cancelled. The program's last line of complaint is the error.
fn run_tool(command: Command, control: &Control) -> io::Result<()> {
    match process::run_capped_with(command, None, TOOL_OUTPUT_LIMIT, NO_TIME_LIMIT, &|| (control.cancelled)()) {
        Ok(output) if output.success => Ok(()),
        Ok(output) => {
            let text = String::from_utf8_lossy(if output.stderr.is_empty() { &output.stdout } else { &output.stderr });
            Err(io::Error::other(text.lines().last().unwrap_or(tr("The program failed")).to_owned()))
        }
        Err(Failure::Cancelled) => Err(stopped()),
        // Not within a hundred years
        Err(Failure::TimedOut) => Err(io::Error::from(io::ErrorKind::TimedOut)),
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
    // The names in the archive are those of the items in one folder; items of other folders would be taken for
    // items of this one (or be another file of the same name), so they are refused before anything is made
    if sources.iter().any(|p| p.parent() != Some(base.as_path())) {
        return Err(io::Error::other(tr("The selected items are in different folders. Open the folder first to compress them")));
    }
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
            // `--` ends the switches and the `@listfile` reading, `-spd` the wildcards (`*`, `?`): every name is a file
            cmd.current_dir(&base).arg("a").arg("-y").arg("-spd").arg("--").arg(out).args(&names);
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
    Err(last.unwrap_or_else(|| io::Error::other(tr("No free name was found"))))
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
        check_zip(&mut zip)?;
        zip.extract(dest).map_err(zip_error)
    } else if ends(&[".tar.gz", ".tgz"]) {
        unpack_tar(flate2::read::GzDecoder::new(source()?), dest)
    } else if ends(&[".tar"]) {
        unpack_tar(source()?, dest)
    } else if let Some(program) = decompressor(&name).filter(|p| have(p)) {
        // The program only turns the file back into a tar stream; what lands where is decided here, entry by entry
        unpack_compressed_tar(program, archive, dest, control)
    } else if have("7z") || have("7za") {
        check_listing(&seven_zip_listing(archive, control)?)?;
        let mut cmd = Command::new(seven_zip());
        let mut out = std::ffi::OsString::from("-o");
        out.push(dest);
        // `-spd`: the archive's name is a name, not a wildcard that could take in other archives beside it
        cmd.arg("x").arg("-y").arg("-spd").arg(out).arg("--").arg(archive);
        run_tool(cmd, control)
    } else {
        Err(io::Error::other(tr("No tool installed to unpack this kind of archive (install 7z)")))
    }
}

/// The entry `name` of an archive would be written outside the folder it is unpacked into.
fn outside(name: &Path) -> io::Error {
    io::Error::other(Outside(trf("The archive has an entry that would land outside the folder: {}", &[&name.display()])))
}

/// The error of `outside`, so that it can be told apart from what a program or a broken archive says.
#[derive(Debug)]
struct Outside(String);

impl std::fmt::Display for Outside {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Outside {}

fn is_outside(err: &io::Error) -> bool {
    err.get_ref().is_some_and(|e| e.is::<Outside>())
}

/// `name` without `.` parts, as the entries of an archive are compared.
fn plain(name: &Path) -> PathBuf {
    name.components().filter(|c| matches!(c, std::path::Component::Normal(_))).collect()
}

/// Whether an entry `name` of an archive stays inside the folder it is unpacked into: it is not absolute, it has no
/// `..`, and it does not lie under an entry of the same archive that is a link (`links`): a file written through a
/// link could land anywhere. Checked before the entry is written, so nothing outside is ever touched.
fn check_member(name: &Path, links: &std::collections::HashSet<PathBuf>) -> io::Result<()> {
    let mut so_far = PathBuf::new();
    for part in name.components() {
        match part {
            std::path::Component::Normal(part) => {
                if !so_far.as_os_str().is_empty() && links.contains(&so_far) {
                    return Err(outside(name));
                }
                so_far.push(part);
            }
            std::path::Component::CurDir => {}
            _ => return Err(outside(name)),
        }
    }
    Ok(())
}

/// Unpack a tar stream into `dest`, checking every entry before it is written (see `check_member`); a hard link must
/// point inside too. A refused entry ends the unpacking with an error, and the caller takes the folder away. Like
/// `tar::Archive::unpack`, folders are made last, so that their permissions do not keep out what is in them.
fn unpack_tar<R: Read>(reader: R, dest: &Path) -> io::Result<()> {
    let dest = dest.canonicalize()?;
    let mut archive = tar::Archive::new(reader);
    let mut links = std::collections::HashSet::new();
    let mut folders = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.into_owned();
        check_member(&name, &links)?;
        let kind = entry.header().entry_type();
        if kind.is_hard_link() {
            let target = entry.link_name()?.map(|t| t.into_owned()).unwrap_or_default();
            check_member(&target, &links)?;
        }
        if kind.is_symlink() {
            links.insert(plain(&name));
        }
        if kind == tar::EntryType::Directory {
            folders.push(entry);
        } else if !entry.unpack_in(&dest)? {
            return Err(outside(&name));
        }
    }
    // Deepest first, so that a folder's permissions never keep out what goes into it; its time is set after its own
    // entry (the library sets the permissions of a folder, not its time), and making a folder below it does not change it
    folders.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut folder in folders {
        let name = folder.path()?.into_owned();
        let time = folder.header().mtime().ok();
        if !folder.unpack_in(&dest)? {
            return Err(outside(&name));
        }
        if let Some(seconds) = time {
            set_folder_time(&dest.join(plain(&name)), seconds);
        }
    }
    Ok(())
}

/// Gives the folder at `path` the time `seconds` of its archive entry. A file system that keeps no such time (FAT keeps
/// no folder times worth speaking of) or a folder that cannot be opened is no reason to fail the unpacking: the
/// contents are all there, only the time is the time of unpacking.
fn set_folder_time(path: &Path, seconds: u64) {
    let time = std::time::UNIX_EPOCH + Duration::from_secs(seconds);
    if let Err(err) = std::fs::File::open(path).and_then(|folder| folder.set_modified(time)) {
        eprintln!("compression: the time of {} could not be set: {}", path.display(), crate::fs::describe::io_error(&err));
    }
}

/// Every entry of a zip archive checked before anything is written: `enclosed_name` refuses absolute names and `..`,
/// and nothing may lie under an entry that is a link.
fn check_zip<R: Read + Seek>(zip: &mut zip::ZipArchive<R>) -> io::Result<()> {
    let mut links = std::collections::HashSet::new();
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i).map_err(zip_error)?;
        let Some(name) = entry.enclosed_name() else { return Err(outside(Path::new(&*entry.name().map_err(zip_error)?))) };
        check_member(&name, &links)?;
        if entry.is_symlink() {
            links.insert(plain(&name));
        }
    }
    Ok(())
}

/// The program that turns a compressed tar file back into a tar stream (`-dc` for all of them), by the file's name.
fn decompressor(name: &str) -> Option<&'static str> {
    let ends = |exts: &[&str]| exts.iter().any(|e| name.ends_with(e));
    if ends(&[".tar.xz", ".txz"]) {
        Some("xz")
    } else if ends(&[".tar.bz2", ".tbz2"]) {
        Some("bzip2")
    } else if ends(&[".tar.zst", ".tzst"]) {
        Some("zstd")
    } else {
        None
    }
}

/// The output of a program, read in turns of at most 50 ms, so that a stop or the end of the time is noticed even while
/// the program says nothing.
struct Polled<'a, T> {
    inner: T,
    control: &'a Control<'a>,
}

impl<T: Read + std::os::fd::AsRawFd> Read for Polled<'_, T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.control.check().map_err(io::Error::other)?;
            let mut pfd = libc::pollfd { fd: self.inner.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd for the duration of the call
            let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
            if ready < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if ready > 0 {
                return self.inner.read(buf);
            }
        }
    }
}

/// Unpack a compressed tar file: `program -dc` writes the tar stream, and `unpack_tar` puts the entries in place. The
/// program runs in a process group of its own and is killed when the unpacking ends early (a stop, a refused
/// entry); its error message is the reason when it fails.
fn unpack_compressed_tar(program: &str, archive: &Path, dest: &Path, control: &Control) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(program)
        .arg("-dc")
        .arg(archive)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| io::Error::other(tr("The program failed")))?;
    let mut stderr = child.stderr.take().ok_or_else(|| io::Error::other(tr("The program failed")))?;
    let said = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut text);
        String::from_utf8_lossy(&text).into_owned()
    });
    let mut reader = Watched::new(Polled { inner: stdout, control }, control);
    let mut result = unpack_tar(&mut reader, dest);
    if result.is_ok() {
        // The program writes on after the end of the archive (padding, and its own check of the data at the very end):
        // what follows is read to the end, so that it finishes and says whether the data were whole
        result = io::copy(&mut reader, &mut io::sink()).map(|_| ());
    }
    drop(reader);
    if result.is_err() {
        // SAFETY: a signal to the program's own process group
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    }
    let status = child.wait()?;
    let said = said.join().unwrap_or_default();
    let program_failed = || Err(io::Error::other(said.lines().last().map_or_else(|| tr("The program failed").to_owned(), str::to_owned)));
    // Only an exit code is the program's own failure: a signal is the kill above, or the pipe that was closed early
    let failed_itself = status.code().is_some_and(|code| code != 0);
    match result {
        // A refused entry, a stop and the end of the time are the reason, however the program ended
        Err(err) if is_outside(&err) || control.check().is_err() => Err(err),
        // A program that failed by itself explains a broken stream better than the tar reader does
        Err(_) if failed_itself => program_failed(),
        Ok(()) if !status.success() => program_failed(),
        other => other,
    }
}

/// The listing of an archive by 7z (`l -slt`), until the job is cancelled.
fn seven_zip_listing(archive: &Path, control: &Control) -> io::Result<String> {
    let mut cmd = Command::new(seven_zip());
    cmd.arg("l").arg("-slt").arg("-spd").arg("--").arg(archive);
    match process::run_capped_with(cmd, None, TOOL_OUTPUT_LIMIT, NO_TIME_LIMIT, &|| (control.cancelled)()) {
        Ok(output) if output.success => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        Ok(output) => Err(io::Error::other(String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or(tr("The program failed")).to_owned())),
        Err(Failure::Cancelled) => Err(stopped()),
        // Not within a hundred years
        Err(Failure::TimedOut) => Err(io::Error::from(io::ErrorKind::TimedOut)),
        Err(Failure::TooMuchOutput) => Err(io::Error::other(tr("The archive program printed too much"))),
        Err(Failure::Io(e)) => Err(e),
    }
}

/// The entries of a 7z listing (`l -slt`), each checked like the entries of a tar stream: the part after the line of
/// dashes has a block per entry with `Path = `, and `Symbolic Link = ` / `Hard Link = ` for links. A listing without
/// that line is not understood and refused.
fn check_listing(listing: &str) -> io::Result<()> {
    let Some((_, entries)) = listing.split_once("\n----------\n") else {
        return Err(io::Error::other(tr("The archive could not be listed")));
    };
    let mut links = std::collections::HashSet::new();
    for block in entries.split("\n\n") {
        let field = |key: &str| block.lines().find_map(|l| l.strip_prefix(key)).map(str::trim_end).filter(|v| !v.is_empty());
        let Some(name) = field("Path = ") else { continue };
        let name = Path::new(name);
        check_member(name, &links)?;
        if let Some(target) = field("Hard Link = ") {
            check_member(Path::new(target), &links)?;
        }
        if field("Symbolic Link = ").is_some() {
            links.insert(plain(name));
        }
    }
    Ok(())
}

/// Unpack `archive` into a new folder of its own inside `dest` and return it. Nothing that is in `dest` can be written
/// over this way; the caller puts the items where they go (and asks about names that are taken), then removes the
/// folder. A failure or a stop takes the folder away again.
pub fn extract_staged(archive: &Path, dest: &Path, control: &Control) -> io::Result<PathBuf> {
    let mut stage = None;
    for n in 0..1000 {
        let candidate = dest.join(format!(".compression-extract-{}-{n}", std::process::id()));
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                stage = Some(candidate);
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    let stage = stage.ok_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists))?;
    match extract(archive, &stage, control) {
        Ok(()) => Ok(stage),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            Err(e)
        }
    }
}

/// Extract into a fresh folder next to the archive (`<archive stem>`), returning that folder.
pub fn extract_beside(archive: &Path, control: &Control) -> io::Result<PathBuf> {
    let parent = archive.parent().ok_or_else(|| io::Error::other(tr("It has no folder to go in")))?;
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
pub(crate) mod tests {
    use super::*;

    /// Two 7z archives side by side, made by 7z itself: `a*.7z` holding `one.txt`, and `aEVIL.7z` holding `evil.txt`,
    /// which a wildcard in the first one's name would match. `None` without 7z.
    fn archives_a_wildcard_would_mix(dir: &Path) -> Option<PathBuf> {
        let tool = ["7z", "7za"].into_iter().find(|t| have(t))?;
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("one.txt"), "one").unwrap();
        std::fs::write(src.join("evil.txt"), "evil").unwrap();
        for (archive, file) in [("a*.7z", "one.txt"), ("aEVIL.7z", "evil.txt")] {
            let made = std::process::Command::new(tool).current_dir(&src).args(["a", "-y", "-spd", "--"]).arg(dir.join(archive)).arg(file).output().unwrap();
            assert!(made.status.success());
        }
        Some(dir.join("a*.7z"))
    }

    #[test]
    fn unpacking_with_7z_takes_the_archive_name_as_it_is_and_not_as_a_wildcard() {
        let dir = dir("seven_zip_archive_name");
        let Some(archive) = archives_a_wildcard_would_mix(&dir) else {
            eprintln!("SKIPPED: no 7z installed, unpacking with 7z was not tested");
            return;
        };
        let dest = dir.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let stage = extract_staged(&archive, &dest, &Control::unlimited()).unwrap();
        let mut got: Vec<String> = std::fs::read_dir(&stage).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        got.sort();
        assert_eq!(got, ["one.txt"], "another archive was unpacked too");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The names of the entries of a 7z archive, byte for byte, and whether any is encrypted (`7z l -slt`).
    fn seven_zip_entries(archive: &Path) -> (Vec<Vec<u8>>, bool) {
        let out = std::process::Command::new(seven_zip()).args(["l", "-slt", "--"]).arg(archive).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let mut names = Vec::new();
        let mut encrypted = false;
        // The entries follow the line of dashes; the archive's own block comes before it
        let text = &out.stdout;
        let start = text.windows(12).position(|w| w == b"\n----------\n").map_or(0, |p| p + 12);
        for line in text[start..].split(|b| *b == b'\n') {
            if let Some(name) = line.strip_prefix(b"Path = ") {
                names.push(name.to_vec());
            }
            encrypted |= line == b"Encrypted = +";
        }
        names.sort();
        (names, encrypted)
    }

    #[test]
    fn packing_with_7z_takes_every_name_as_a_file_name_and_leaves_the_originals_alone() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        if !have("7z") && !have("7za") {
            eprintln!("SKIPPED: no 7z installed, packing with 7z was not tested");
            return;
        }
        let dir = dir("seven_zip_names");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // Names 7z would read as switches, a list file, wildcards; a space; bytes that are not UTF-8
        let chosen: Vec<std::ffi::OsString> = ["keep.txt", "-sdel", "-pSECRET", "-r", "@list", "*", "a?c", "a b.txt"]
            .iter()
            .map(std::ffi::OsString::from)
            .chain([std::ffi::OsString::from_vec(b"bad-\xe4.txt".to_vec())])
            .collect();
        for (n, name) in chosen.iter().enumerate() {
            std::fs::write(src.join(name), format!("content {n}")).unwrap();
        }
        // Not chosen: what a list file or a wildcard would pull in
        std::fs::write(src.join("list"), "secret.txt\n").unwrap();
        std::fs::write(src.join("secret.txt"), "not chosen").unwrap();
        std::fs::write(src.join("abc"), "not chosen").unwrap();
        let before: Vec<(std::ffi::OsString, Vec<u8>)> = std::fs::read_dir(&src).unwrap().map(|e| e.unwrap()).map(|e| (e.file_name(), std::fs::read(e.path()).unwrap())).collect();

        let out = dir.join("out.7z");
        let sources: Vec<PathBuf> = chosen.iter().map(|n| src.join(n)).collect();
        compress(&sources, &out, ArchiveFormat::SevenZ, &Control::unlimited()).unwrap();

        // Every original is there, as it was
        for (name, bytes) in &before {
            assert_eq!(std::fs::read(src.join(name)).ok().as_ref(), Some(bytes), "{name:?} was deleted or changed");
        }
        assert_eq!(std::fs::read_dir(&src).unwrap().count(), before.len(), "a file appeared or went");
        // The archive holds exactly the chosen files, under their own names, not encrypted
        let (names, encrypted) = seven_zip_entries(&out);
        let mut expected: Vec<Vec<u8>> = chosen.iter().map(|n| n.as_bytes().to_vec()).collect();
        expected.sort();
        assert_eq!(names, expected, "the archive holds other entries than the chosen files");
        assert!(!encrypted, "the archive was encrypted");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    use std::time::Instant;

    #[test]
    fn a_free_name_for_a_name_that_is_not_utf8_keeps_its_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let dir = dir("unique_bytes");
        // A name in Latin-1, and one with a double extension
        for (name, first, second) in [
            (&b"Bericht-\xe4.txt"[..], &b"Bericht-\xe4 (copy).txt"[..], &b"Bericht-\xe4 (copy 2).txt"[..]),
            (b"\xff\xfe.tar.gz", b"\xff\xfe (copy).tar.gz", b"\xff\xfe (copy 2).tar.gz"),
        ] {
            let name = std::ffi::OsString::from_vec(name.to_vec());
            std::fs::write(dir.join(&name), "x").unwrap();
            let free = unique_path(&dir, &name, "copy");
            assert_eq!(free.file_name().unwrap().as_bytes(), first);
            std::fs::write(&free, "x").unwrap();
            assert_eq!(unique_path(&dir, &name, "copy").file_name().unwrap().as_bytes(), second);
        }
        assert_eq!(split_name("photo.png"), ("photo", ".png"));
        assert_eq!(split_name("ä.tar.gz"), ("ä", ".tar.gz"));
        assert_eq!(split_name(".bashrc"), (".bashrc", ""));
        std::fs::remove_dir_all(&dir).unwrap();
    }

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

    #[test]
    fn the_copy_used_by_trash_and_undo_keeps_acls_and_never_widens_the_groups_permissions() {
        use std::os::unix::fs::MetadataExt;
        let acl = |args: &[&str], p: &Path| std::process::Command::new("setfacl").args(args).arg(p).status().is_ok_and(|s| s.success());
        let facl = |p: &Path| String::from_utf8_lossy(&std::process::Command::new("getfacl").args(["-n", "-c", "-p"]).arg(p).output().unwrap().stdout).into_owned();
        let dir = dir("copy_tree_acl");
        std::fs::create_dir_all(dir.join("t/inner")).unwrap();
        std::fs::write(dir.join("t/f"), "x").unwrap();
        std::fs::set_permissions(dir.join("t/f"), std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        if !acl(&["-m", "u:0:rw,g::r,m::rw"], &dir.join("t/f")) || !acl(&["-m", "u:0:rwx"], &dir.join("t/inner")) || !acl(&["-d", "-m", "u:0:rx"], &dir.join("t/inner")) {
            return;
        }
        copy_tree(&dir.join("t"), &dir.join("c")).unwrap();
        assert_eq!(facl(&dir.join("c/f")).replace("c/f", "t/f"), facl(&dir.join("t/f")).replace("c/f", "t/f"));
        assert_eq!(facl(&dir.join("c/inner")).replace("c/inner", "t/inner"), facl(&dir.join("t/inner")).replace("c/inner", "t/inner"));
        assert!(facl(&dir.join("c/f")).contains("group::r--"));
        // Where the target takes no ACL: whole copy, no group permission, never the mask's
        crate::fs::xattr::tests::refuse_attributes_under(&dir.join("refusing"));
        std::fs::create_dir_all(dir.join("refusing")).unwrap();
        copy_tree(&dir.join("t/f"), &dir.join("refusing/f")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("refusing/f")).unwrap(), "x");
        assert_eq!(dir.join("refusing/f").metadata().unwrap().mode() & 0o7777, 0o604);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file of `len` bytes that is a hole except for the pieces `data` says (offset, bytes).
    fn holey_file(path: &Path, len: u64, data: &[(u64, &[u8])]) {
        use std::os::unix::fs::FileExt;
        let file = std::fs::File::create(path).unwrap();
        file.set_len(len).unwrap();
        for (at, bytes) in data {
            file.write_all_at(bytes, *at).unwrap();
        }
        file.sync_all().unwrap();
    }

    /// Whether the file system keeps holes: the file uses less than its length (asked without the code under test).
    fn keeps_holes(path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let meta = path.metadata().unwrap();
        meta.blocks() * 512 < meta.len()
    }

    /// What the file uses on the disk; a hole costs nothing.
    fn allocated(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        path.metadata().unwrap().blocks() * 512
    }

    const MIB: u64 = 1 << 20;

    /// Files (by device and inode) whose `SEEK_DATA`/`SEEK_HOLE` fail with the given error, as on a file system that
    /// cannot tell data from holes. An entry lasts as long as the `SeekFails` that made it: a file system hands out
    /// the number of a deleted file again, and a later file must not be taken for this one.
    static SEEK_FAILS: std::sync::Mutex<Vec<((u64, u64), i32)>> = std::sync::Mutex::new(Vec::new());

    pub(super) fn seek_fails_for(file: &std::fs::File) -> Option<i32> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata().ok()?;
        SEEK_FAILS.lock().ok()?.iter().find(|(key, _)| *key == (meta.dev(), meta.ino())).map(|(_, errno)| *errno)
    }

    pub(crate) struct SeekFails((u64, u64));

    impl Drop for SeekFails {
        fn drop(&mut self) {
            if let Ok(mut fails) = SEEK_FAILS.lock() {
                fails.retain(|(key, _)| *key != self.0);
            }
        }
    }

    #[must_use = "the file stops failing when this is dropped"]
    pub(crate) fn make_seek_fail(path: &Path, errno: i32) -> SeekFails {
        use std::os::unix::fs::MetadataExt;
        let meta = path.metadata().unwrap();
        let key = (meta.dev(), meta.ino());
        SEEK_FAILS.lock().unwrap().push((key, errno));
        SeekFails(key)
    }

    #[test]
    fn a_file_with_holes_is_copied_without_them_and_the_progress_ends_at_its_size() {
        let dir = dir("holey");
        let len = 32 * MIB;
        holey_file(&dir.join("src"), len, &[(0, b"start"), (10 * MIB + 5, b"middle"), (len - 3, b"end")]);
        if !keeps_holes(&dir.join("src")) {
            return;
        }
        let (reader, writer) = (std::fs::File::open(dir.join("src")).unwrap(), std::fs::File::create(dir.join("dst")).unwrap());
        let (from, mut to) = (&reader, &writer);
        let (mut passed, mut steps) = (0, 0);
        let result = copy_holey(
            &reader,
            &writer,
            len,
            &mut || Ok(()),
            &mut |most, _| {
                steps += 1;
                io::copy(&mut from.take(most), &mut to).map(|n| n as usize)
            },
            &mut |n| passed += n,
        )
        .unwrap();
        assert_eq!(result, Holey::Copied);
        assert_eq!(passed, len, "holes count as passed, so the progress ends at the size");
        assert!(steps < 50, "{steps}: only the data was copied, in a few steps");
        assert_eq!(std::fs::read(dir.join("src")).unwrap(), std::fs::read(dir.join("dst")).unwrap());
        assert!(allocated(&dir.join("dst")) < MIB, "the copy uses {} bytes", allocated(&dir.join("dst")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hole_at_the_end_and_a_file_that_is_all_hole_keep_their_length() {
        let dir = dir("holey_ends");
        holey_file(&dir.join("tail"), 8 * MIB, &[(0, b"head")]);
        holey_file(&dir.join("all"), 8 * MIB, &[]);
        for name in ["tail", "all"] {
            let (reader, writer) = (std::fs::File::open(dir.join(name)).unwrap(), std::fs::File::create(dir.join(format!("{name}.copy"))).unwrap());
            let (from, mut to) = (&reader, &writer);
            let mut passed = 0;
            let result = copy_holey(&reader, &writer, 8 * MIB, &mut || Ok(()), &mut |most, _| io::copy(&mut from.take(most), &mut to).map(|n| n as usize), &mut |n| passed += n).unwrap();
            assert_eq!(result, Holey::Copied);
            assert_eq!(passed, 8 * MIB, "{name}: a hole at the end counts as passed too");
            assert_eq!(std::fs::read(dir.join(name)).unwrap(), std::fs::read(dir.join(format!("{name}.copy"))).unwrap(), "{name}");
        }
        assert_eq!(dir.join("all.copy").metadata().unwrap().len(), 8 * MIB);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_gets_shorter_while_it_is_copied_ends_the_copy_where_the_data_did() {
        let dir = dir("holey_shrinks");
        holey_file(&dir.join("src"), 8 * MIB, &[(MIB, &[7u8; 8192])]);
        let (reader, writer) = (std::fs::File::open(dir.join("src")).unwrap(), std::fs::File::create(dir.join("dst")).unwrap());
        let (from, mut to, mut calls) = (&reader, &writer, 0);
        let result = copy_holey(&reader, &writer, 8 * MIB, &mut || Ok(()), &mut |_, _| {
            calls += 1;
            if calls == 1 { io::copy(&mut from.take(100), &mut to).map(|n| n as usize) } else { Ok(0) }
        }, &mut |_| {})
        .unwrap();
        assert_eq!(result, Holey::Copied);
        assert_eq!(dir.join("dst").metadata().unwrap().len(), MIB + 100);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stopping_a_holey_copy_stops_it_and_a_file_system_without_holes_is_told_apart() {
        let dir = dir("holey_stops");
        holey_file(&dir.join("src"), 8 * MIB, &[(0, b"a"), (4 * MIB, b"b")]);
        let (reader, writer) = (std::fs::File::open(dir.join("src")).unwrap(), std::fs::File::create(dir.join("dst")).unwrap());
        let mut asked = 0;
        let err = copy_holey(&reader, &writer, 8 * MIB, &mut || { asked += 1; if asked > 3 { Err(io::ErrorKind::Interrupted.into()) } else { Ok(()) } }, &mut |_, _| Ok(1), &mut |_| {}).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        // A pipe cannot be asked at all, which is an error and not "no holes here"
        let (read_end, _write_end) = {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            use std::os::fd::FromRawFd;
            unsafe { (std::fs::File::from_raw_fd(fds[0]), std::fs::File::from_raw_fd(fds[1])) }
        };
        assert!(copy_holey(&read_end, &writer, 10, &mut || Ok(()), &mut |_, _| Ok(0), &mut |_| {}).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_copy_used_for_undo_and_the_trash_keeps_holes_too() {
        let dir = dir("holey_tree");
        let len = 16 * MIB;
        holey_file(&dir.join("src"), len, &[(0, b"x"), (len - 1, b"y")]);
        if !keeps_holes(&dir.join("src")) {
            return;
        }
        copy_tree(&dir.join("src"), &dir.join("dst")).unwrap();
        assert_eq!(std::fs::read(dir.join("src")).unwrap(), std::fs::read(dir.join("dst")).unwrap());
        assert!(allocated(&dir.join("dst")) < MIB);
        // A plain file is copied as before
        std::fs::write(dir.join("plain"), "text").unwrap();
        copy_tree(&dir.join("plain"), &dir.join("plain2")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("plain2")).unwrap(), "text");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_system_that_cannot_tell_holes_is_copied_the_plain_way_and_nothing_is_written_before() {
        for (n, errno) in [libc::EINVAL, libc::ENOSYS, libc::EOPNOTSUPP].into_iter().enumerate() {
            let dir = dir(&format!("holey_unavailable_{n}"));
            holey_file(&dir.join("src"), 8 * MIB, &[(0, b"a"), (4 * MIB, b"b")]);
            let seek_fails = make_seek_fail(&dir.join("src"), errno);
            let (reader, writer) = (std::fs::File::open(dir.join("src")).unwrap(), std::fs::File::create(dir.join("dst")).unwrap());
            let mut passed = 0;
            let result = copy_holey(&reader, &writer, 8 * MIB, &mut || Ok(()), &mut |_, _| panic!("nothing is copied when holes cannot be told"), &mut |n| passed += n).unwrap();
            assert_eq!(result, Holey::Unavailable, "errno {errno}");
            assert_eq!((passed, writer.metadata().unwrap().len()), (0, 0), "nothing was counted or written before giving up");
            // The caller then copies the plain way: whole, as long, the same
            copy_tree(&dir.join("src"), &dir.join("plain")).unwrap();
            assert_eq!(std::fs::read(dir.join("src")).unwrap(), std::fs::read(dir.join("plain")).unwrap(), "errno {errno}");
            // Before the file goes: its number may be given to the next file
            drop(seek_fails);
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn any_other_error_from_telling_holes_is_an_error_and_not_a_silent_plain_copy() {
        let dir = dir("holey_io_error");
        holey_file(&dir.join("src"), 8 * MIB, &[(0, b"a")]);
        let seek_fails = make_seek_fail(&dir.join("src"), libc::EIO);
        let (reader, writer) = (std::fs::File::open(dir.join("src")).unwrap(), std::fs::File::create(dir.join("dst")).unwrap());
        let err = copy_holey(&reader, &writer, 8 * MIB, &mut || Ok(()), &mut |_, _| Ok(0), &mut |_| {}).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
        drop(seek_fails);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_stops_failing_to_tell_holes_when_its_guard_is_dropped() {
        let dir = dir("seek_guard");
        std::fs::write(dir.join("a"), "x").unwrap();
        let file = std::fs::File::open(dir.join("a")).unwrap();
        let guard = make_seek_fail(&dir.join("a"), libc::EINVAL);
        assert_eq!(seek_fails_for(&file), Some(libc::EINVAL));
        drop(guard);
        assert_eq!(seek_fails_for(&file), None, "the number of this file may be given to the next one");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_is_read_without_setting_its_access_time_and_lists_the_same_as_the_usual_way() {
        let dir = dir("read_dir_atime");
        std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join(".hidden"), "h").unwrap();
        let entries = read_dir_keeping_atime(&dir).unwrap();
        // The system says how the folder was opened: with O_NOATIME, which it allows for our own folder
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", entries.raw_fd())).unwrap();
        let flags = info.lines().find_map(|l| l.strip_prefix("flags:")).map(|f| i32::from_str_radix(f.trim(), 8).unwrap()).unwrap();
        assert_ne!(flags & libc::O_NOATIME, 0, "{info}");
        let mut ours: Vec<PathBuf> = entries.map(Result::unwrap).collect();
        let mut usual: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        ours.sort();
        usual.sort();
        assert_eq!(ours, usual, "the same paths, without . and ..");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_of_someone_else_and_a_folder_that_is_not_there_are_read_or_refused_as_usual() {
        // The system does not allow O_NOATIME on a folder of another user (unless root): then it is read the usual way
        let root: Vec<PathBuf> = read_dir_keeping_atime(Path::new("/")).unwrap().map(Result::unwrap).collect();
        assert!(root.contains(&PathBuf::from("/usr")), "{root:?}");
        assert_eq!(read_dir_keeping_atime(Path::new("/definitely/not/here")).err().map(|e| e.kind()), Some(io::ErrorKind::NotFound));
        let file = dir("read_dir_file").join("f");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(read_dir_keeping_atime(&file).err().and_then(|e| e.raw_os_error()), Some(libc::ENOTDIR));
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

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

    /// A folder on this disk and one on another file system (`/dev/shm`); `None` where that cannot be had, or as root,
    /// who can remove what no one else can.
    /// Paths that the tests turn into something else just before they are opened, as a program might between the
    /// moment a copy looks at a file and the moment it opens it: into a pipe (`None`) or into a link to a file.
    static SWAPS: std::sync::Mutex<Vec<(PathBuf, Option<PathBuf>)>> = std::sync::Mutex::new(Vec::new());

    pub(crate) fn swap_before_open_of(path: &Path, link_to: Option<PathBuf>) {
        SWAPS.lock().unwrap().push((path.to_path_buf(), link_to));
    }

    pub(super) fn swap_before_open(path: &Path) {
        let swap = SWAPS.lock().unwrap().iter().find(|(p, _)| p == path).cloned();
        let Some((_, link)) = swap else { return };
        let _ = std::fs::remove_file(path);
        match link {
            Some(target) => std::os::unix::fs::symlink(target, path).unwrap(),
            None => {
                let name = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).unwrap();
                // SAFETY: `name` is a valid C string; `mkfifo` only creates the file.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
        }
    }

    #[test]
    fn the_copy_of_the_trash_and_of_an_undo_does_not_wait_for_a_file_that_became_a_pipe_nor_follow_a_link() {
        let dir = std::env::temp_dir().join(format!("compression_ops_swap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (secret, to_pipe, to_link) = (dir.join("secret"), dir.join("to_pipe"), dir.join("to_link"));
        for file in [&secret, &to_pipe, &to_link] {
            std::fs::write(file, "contents").unwrap();
        }
        swap_before_open_of(&to_pipe, None);
        swap_before_open_of(&to_link, Some(secret.clone()));
        // Run on a thread so that a copy that waits for the pipe ends this test instead of hanging it
        let (tx, rx) = std::sync::mpsc::channel();
        let (a, b, c, d, e) = (to_pipe.clone(), dir.join("pipe_copy"), to_link.clone(), dir.join("link_copy"), dir.join("plain_copy"));
        let plain = secret.clone();
        std::thread::spawn(move || {
            let results = (copy_tree(&a, &b), copy_tree(&c, &d), copy_tree(&plain, &e));
            let _ = tx.send(results);
        });
        let (pipe, link, plain) = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the copy waited for a pipe");
        assert!(pipe.unwrap_err().to_string().contains("named pipe"));
        assert!(link.is_err());
        assert!(!dir.join("pipe_copy").exists() && !dir.join("link_copy").exists());
        plain.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("plain_copy")).unwrap(), "contents");
        let _ = std::fs::remove_dir_all(&dir);
    }

    pub(crate) fn two_file_systems(name: &str) -> Option<(PathBuf, PathBuf)> {
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir().join(format!("compression_{name}_{}", std::process::id()));
        let shm = PathBuf::from("/dev/shm").join(format!("compression_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&shm);
        std::fs::create_dir_all(&root).unwrap();
        if std::fs::create_dir_all(&shm).is_err() || shm.metadata().unwrap().dev() == root.metadata().unwrap().dev() || unsafe { libc::geteuid() } == 0 {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&shm);
            return None;
        }
        Some((root, shm))
    }

    /// Makes `dir` and everything in it removable again, then removes it.
    pub(crate) fn unlock_and_remove(dir: &Path) {
        let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(dir).status();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_move_across_file_systems_whose_original_cannot_be_removed_keeps_the_whole_copy_and_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let Some((root, shm)) = two_file_systems("ops_leftover") else { return };
        std::fs::create_dir_all(root.join("tree/box")).unwrap();
        std::fs::write(root.join("tree/a.txt"), "one").unwrap();
        std::fs::write(root.join("tree/box/b.txt"), "two").unwrap();
        // A folder that can be read but not emptied: the copy is whole, the original cannot be removed
        std::fs::set_permissions(root.join("tree/box"), std::fs::Permissions::from_mode(0o555)).unwrap();

        let err = move_path(&root.join("tree"), &shm.join("tree")).unwrap_err();
        assert!(is_copied_not_removed(&err), "{err}");
        assert_eq!(copied_not_removed(&err).unwrap().copy, shm.join("tree"));
        assert!(err.to_string().contains("The copy is complete, but the original could not be fully removed"), "{err}");
        assert_eq!(std::fs::read_to_string(shm.join("tree/a.txt")).unwrap(), "one", "the copy is whole and stays");
        assert_eq!(std::fs::read_to_string(shm.join("tree/box/b.txt")).unwrap(), "two");
        assert!(root.join("tree/box/b.txt").exists(), "what is left of the original stays too");

        // Other failures are not that error
        assert!(!is_copied_not_removed(&io::Error::from(io::ErrorKind::PermissionDenied)));
        unlock_and_remove(&root);
        unlock_and_remove(&shm);
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
            let control = Control::new(&|| false, &progress);
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
        let control = Control::new(&|| false, &progress);
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
            let control = Control::new(&stop, &progress);
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
            let control = Control::new(&stop, &progress);
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
        let control = Control::new(&stop, &|_| {});
        assert!(extract_beside(&out, &control).is_err());
        assert!(!root.join("pack").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_program_that_packs_is_killed_when_cancelled_and_not_for_taking_its_time() {
        let sleeper = || {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg("sleep 30");
            cmd
        };
        // A program that takes its time is not stopped for it
        let mut slow = Command::new("sh");
        slow.arg("-c").arg("sleep 1.5; echo packed");
        assert!(run_tool(slow, &Control::unlimited()).is_ok());
        // Cancelled while it runs
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let err = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                cancel.store(true, Relaxed);
            });
            run_tool(sleeper(), &Control::new(&|| cancel.load(Relaxed), &|_| {})).unwrap_err()
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

    #[test]
    fn a_new_archive_never_touches_a_file_that_is_in_the_way() {
        let root = std::env::temp_dir().join(format!("compression_ops_taken_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        let mut tried = 0;
        for fmt in ArchiveFormat::ALL {
            if !fmt.available() {
                continue;
            }
            tried += 1;
            let out = root.join(format!("mine{}", fmt.extension()));
            std::fs::write(&out, "not an archive, and not the job's").unwrap();
            let err = compress(&[root.join("a.txt")], &out, fmt, &Control::unlimited()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{fmt:?}: {err}");
            assert!(err.to_string().contains("already exists"), "{fmt:?}: {err}");
            assert_eq!(std::fs::read_to_string(&out).unwrap(), "not an archive, and not the job's", "{fmt:?}");
            // A free name still works afterwards
            let free = root.join(format!("free{}", fmt.extension()));
            compress(&[root.join("a.txt")], &free, fmt, &Control::unlimited()).unwrap_or_else(|e| panic!("{fmt:?}: {e}"));
            assert!(free.is_file(), "{fmt:?}");
        }
        assert!(tried >= 2, "zip and tar.gz are always there");
        // A dangling link counts as taken too
        std::os::unix::fs::symlink("nowhere", root.join("dangling.zip")).unwrap();
        let err = compress(&[root.join("a.txt")], &root.join("dangling.zip"), ArchiveFormat::Zip, &Control::unlimited()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(std::fs::symlink_metadata(root.join("dangling.zip")).unwrap().file_type().is_symlink());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_job_still_removes_the_archive_it_made() {
        let root = std::env::temp_dir().join(format!("compression_ops_madeit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        for fmt in [ArchiveFormat::Zip, ArchiveFormat::TarGz] {
            let out = root.join(format!("cancelled{}", fmt.extension()));
            let cancelled = || true;
            let control = Control::new(&cancelled, &|_| {});
            assert!(compress(&[root.join("a.txt")], &out, fmt, &control).is_err());
            assert!(!out.exists(), "{fmt:?}: the half-made archive is gone");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_new_file_takes_the_next_free_name_and_keeps_the_others() {
        let root = std::env::temp_dir().join(format!("compression_ops_writenew_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let name = std::ffi::OsStr::new("Pasted image.png");
        let first = write_new(&root, name, "copy", b"one").unwrap();
        let second = write_new(&root, name, "copy", b"two").unwrap();
        let third = write_new(&root, name, "copy", b"three").unwrap();
        assert_eq!(first, root.join("Pasted image.png"));
        assert_eq!(second, root.join("Pasted image (copy).png"));
        assert_eq!(third, root.join("Pasted image (copy 2).png"));
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        assert_eq!(std::fs::read(&third).unwrap(), b"three");
        // A folder that is not there is an error of its own, and leaves nothing behind
        assert_eq!(write_new(&root.join("nope"), name, "copy", b"x").unwrap_err().kind(), io::ErrorKind::NotFound);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_errors_ops_makes_itself_are_in_words() {
        let control = Control::unlimited();
        let err = extract_beside(Path::new("/"), &control).unwrap_err();
        assert_eq!(crate::fs::describe::io_error(&err), "It has no folder to go in");
    }

    #[test]
    fn items_of_two_folders_are_refused_before_anything_is_made_and_one_folder_is_as_before() {
        let root = std::env::temp_dir().join(format!("compression_compress_folders_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        // The same name in another folder: it must never stand in for the first one
        std::fs::write(root.join("sub/a.txt"), "other a").unwrap();
        let listing = |dir: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            names
        };
        let before = listing(&root);
        for format in [ArchiveFormat::Zip, ArchiveFormat::TarGz] {
            let out = root.join(format!("both{}", format.extension()));
            let err = compress(&[root.join("a.txt"), root.join("sub/a.txt")], &out, format, &Control::unlimited()).unwrap_err();
            assert_eq!(crate::fs::describe::io_error(&err), "The selected items are in different folders. Open the folder first to compress them");
            assert!(!out.exists(), "no archive is made");
            assert_eq!(listing(&root), before, "nothing is left behind");
            // One folder: as always
            compress(&[root.join("a.txt"), root.join("b.txt")], &out, format, &Control::unlimited()).unwrap();
            assert!(out.is_file());
            std::fs::remove_file(&out).unwrap();
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    // --- Unpacking never writes outside the folder it unpacks into

    /// A tar archive whose entries are written as given, without the checks of `tar::Builder` (that is the point):
    /// `kind` is 'f' (file "x"), 'd' (folder), 'l' (symlink to `target`) or 'h' (hard link to `target`).
    fn raw_tar(entries: &[(&str, char, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind, target) in entries {
            let mut header = tar::Header::new_gnu();
            let gnu = header.as_gnu_mut().unwrap();
            gnu.name[..name.len()].copy_from_slice(name.as_bytes());
            gnu.linkname[..target.len()].copy_from_slice(target.as_bytes());
            header.set_mode(if *kind == 'd' { 0o755 } else { 0o644 });
            let (kind, data): (tar::EntryType, &[u8]) = match kind {
                'f' => (tar::EntryType::Regular, b"x"),
                'd' => (tar::EntryType::Directory, b""),
                'l' => (tar::EntryType::Symlink, b""),
                _ => (tar::EntryType::Link, b""),
            };
            header.set_entry_type(kind);
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder.append(&header, data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// A folder with `in/` (where the archive lies and is unpacked) and `outside/` (with a file `secret`).
    fn evil_root(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = dir(name);
        let (inside, outside) = (root.join("in"), root.join("outside"));
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "secret").unwrap();
        (root, inside, outside)
    }

    /// Writes `tar` as `name` in `inside`, compressed as the name says (with the system's program for xz).
    fn archive_as(inside: &Path, name: &str, tar: &[u8]) -> Option<PathBuf> {
        let path = inside.join(name);
        if name.ends_with(".tar.gz") {
            let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(&path).unwrap(), flate2::Compression::fast());
            gz.write_all(tar).unwrap();
            gz.finish().unwrap();
        } else if name.ends_with(".tar.xz") {
            if !have("xz") {
                return None;
            }
            let mut child = Command::new("xz").arg("-c").stdin(std::process::Stdio::piped()).stdout(std::fs::File::create(&path).unwrap()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(tar).unwrap();
            assert!(child.wait().unwrap().success());
        } else {
            // `.tar`, and a tar under another name for 7z (it tells the format by the content)
            if !name.ends_with(".tar") && !(have("7z") || have("7za")) {
                return None;
            }
            std::fs::write(&path, tar).unwrap();
        }
        Some(path)
    }

    /// What is in `dir`, by name.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    /// An entry for `raw_tar`, owned: name, kind, target.
    type Entry = (String, char, String);

    #[test]
    fn an_archive_with_an_entry_that_would_land_outside_is_refused_and_nothing_is_written() {
        for format in ["evil.tar", "evil.tar.gz", "evil.tar.xz", "evil.cb7"] {
            let cases: Vec<(&str, Vec<Entry>)> = vec![
                ("dot-dot", vec![("ok".into(), 'f', String::new()), ("../../outside/escape".into(), 'f', String::new())]),
                ("absolute", vec![("ok".into(), 'f', String::new()), ("{outside}/escape".into(), 'f', String::new())]),
                ("through a link", vec![("ok".into(), 'f', String::new()), ("l".into(), 'l', "{outside}".into()), ("l/escape".into(), 'f', String::new())]),
                ("hard link out", vec![("ok".into(), 'f', String::new()), ("h".into(), 'h', "../../outside/secret".into())]),
            ];
            for (case, entries) in cases {
                let (root, inside, outside) = evil_root(&format!("evil_{}_{}", format.replace('.', "_"), case.replace(' ', "_")));
                let fill = |s: &str| s.replace("{outside}", &outside.display().to_string());
                let entries: Vec<Entry> = entries.iter().map(|(n, k, t)| (fill(n), *k, fill(t))).collect();
                let borrowed: Vec<(&str, char, &str)> = entries.iter().map(|(n, k, t)| (n.as_str(), *k, t.as_str())).collect();
                let Some(archive) = archive_as(&inside, format, &raw_tar(&borrowed)) else { continue };
                let result = extract_beside(&archive, &Control::unlimited());
                let err = result.expect_err(&format!("{format}, {case}: unpacked"));
                assert!(err.to_string().contains("outside the folder") || format == "evil.cb7" && case == "hard link out", "{format}, {case}: {err}");
                // Nothing outside changed, and no half-unpacked folder is left
                assert_eq!(names(&outside), ["secret"], "{format}, {case}");
                assert_eq!(std::fs::read_to_string(outside.join("secret")).unwrap(), "secret", "{format}, {case}");
                assert_eq!(names(&inside), [format], "{format}, {case}");
                let _ = std::fs::remove_dir_all(&root);
            }
        }
    }

    #[test]
    fn a_zip_with_an_entry_that_would_land_outside_is_refused_and_nothing_is_written() {
        use zip::write::SimpleFileOptions;
        for case in ["dot-dot", "through a link"] {
            let (root, inside, outside) = evil_root(&format!("evil_zip_{}", case.replace(' ', "_")));
            let archive = inside.join("evil.zip");
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
            zip.start_file("ok", SimpleFileOptions::default()).unwrap();
            zip.write_all(b"x").unwrap();
            if case == "dot-dot" {
                zip.start_file("../../outside/escape", SimpleFileOptions::default()).unwrap();
                zip.write_all(b"x").unwrap();
            } else {
                zip.add_symlink("l", outside.display().to_string(), SimpleFileOptions::default()).unwrap();
                zip.start_file("l/escape", SimpleFileOptions::default()).unwrap();
                zip.write_all(b"x").unwrap();
            }
            zip.finish().unwrap();
            let err = extract_beside(&archive, &Control::unlimited()).expect_err(case);
            assert!(err.to_string().contains("outside the folder"), "{case}: {err}");
            assert_eq!(names(&outside), ["secret"], "{case}");
            assert_eq!(names(&inside), ["evil.zip"], "{case}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn links_that_stay_inside_or_point_to_a_system_file_are_unpacked_as_they_are() {
        for format in ["good.tar", "good.tar.gz", "good.tar.xz", "good.cb7"] {
            let (root, inside, outside) = evil_root(&format!("good_{}", format.replace('.', "_")));
            let tar = raw_tar(&[("d/", 'd', ""), ("d/a", 'f', ""), ("rel", 'l', "d/a"), ("abs", 'l', "/etc/hostname"), ("h", 'h', "d/a")]);
            let Some(archive) = archive_as(&inside, format, &tar) else { continue };
            let out = extract_beside(&archive, &Control::unlimited()).unwrap_or_else(|e| panic!("{format}: {e}"));
            assert_eq!(std::fs::read_to_string(out.join("d/a")).unwrap(), "x", "{format}");
            assert_eq!(std::fs::read_link(out.join("rel")).unwrap(), Path::new("d/a"), "{format}");
            assert_eq!(std::fs::read_to_string(out.join("h")).unwrap(), "x", "{format}");
            // A link to a file of the system is kept as a link (as every unpacker does); it is not followed while unpacking
            assert!(std::fs::symlink_metadata(out.join("abs")).unwrap().file_type().is_symlink(), "{format}");
            assert_eq!(names(&outside), ["secret"], "{format}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn a_compressed_tar_that_is_broken_or_stopped_leaves_nothing_and_says_why() {
        if !have("xz") {
            return;
        }
        let (root, inside, _) = evil_root("xz_broken");
        let broken = inside.join("broken.tar.xz");
        std::fs::write(&broken, b"this is not xz").unwrap();
        let err = extract_beside(&broken, &Control::unlimited()).unwrap_err();
        assert!(!err.to_string().is_empty() && !err.to_string().contains("outside"), "{err}");
        assert_eq!(names(&inside), ["broken.tar.xz"]);
        // A stop while the program writes: the job ends as a stop and the folder goes
        let good = archive_as(&inside, "big.tar.xz", &raw_tar(&[("a", 'f', ""), ("b", 'f', "")])).unwrap();
        let err = extract_beside(&good, &Control::new(&|| true, &|_| {})).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted, "{err}");
        assert_eq!(names(&inside), ["big.tar.xz", "broken.tar.xz"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_7z_listing_is_read_entry_by_entry_and_one_it_cannot_read_is_refused() {
        let listing = |entries: &str| format!("7-Zip\n\nPath = /tmp/a.7z\nType = 7z\n\n----------\n{entries}");
        assert!(check_listing(&listing("Path = a\nSymbolic Link = \n\nPath = d/b\nHard Link = \n")).is_ok());
        assert!(check_listing(&listing("Path = ../x\n")).is_err());
        assert!(check_listing(&listing("Path = /etc/x\n")).is_err());
        assert!(check_listing(&listing("Path = l\nSymbolic Link = /etc\n\nPath = l/x\n")).is_err());
        assert!(check_listing(&listing("Path = h\nHard Link = ../../x\n")).is_err());
        // The archive's own (absolute) path in the head is not an entry
        assert!(check_listing(&listing("")).is_ok());
        assert_eq!(check_listing("Path = a\n").unwrap_err().to_string(), "The archive could not be listed");
    }


    #[test]
    fn the_output_of_a_program_that_says_nothing_still_notices_a_cancel() {
        let mut child = Command::new("sleep").arg("30").stdout(std::process::Stdio::piped()).spawn().unwrap();
        let cancel = AtomicBool::new(false);
        let stop = || cancel.load(Relaxed);
        let control = Control::new(&stop, &|_| {});
        let started = Instant::now();
        let err = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                cancel.store(true, Relaxed);
            });
            let mut polled = Polled { inner: child.stdout.take().unwrap(), control: &control };
            polled.read(&mut [0u8; 16]).unwrap_err()
        });
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        // Wrapped, as the reader does with every stop: the library that reads would try an Interrupted again for good
        assert_eq!(err.to_string(), "cancelled");
        let _ = child.kill();
        let _ = child.wait();
    }


    #[test]
    fn a_compressed_tar_is_read_to_its_end_so_padding_does_not_fail_it_and_a_broken_end_is_told() {
        if !have("xz") {
            return;
        }
        let (root, inside, _) = evil_root("xz_tail");
        // Far more after the end of the archive than a pipe holds: the program is still writing when the tar ends
        let mut tar = raw_tar(&[("a", 'f', "")]);
        tar.extend(std::iter::repeat_n(0u8, 512 * 1024));
        let padded = archive_as(&inside, "padded.tar.xz", &tar).unwrap();
        let out = extract_beside(&padded, &Control::unlimited()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(std::fs::read_to_string(out.join("a")).unwrap(), "x");
        // The data are whole up to the archive's end, but the program's own check at the very end fails: that is told
        let mut bytes = std::fs::read(&padded).unwrap();
        let at = bytes.len() - 20;
        bytes[at] ^= 0xff;
        let broken = inside.join("broken-end.tar.xz");
        std::fs::write(&broken, bytes).unwrap();
        let before = names(&inside);
        assert!(extract_beside(&broken, &Control::unlimited()).is_err(), "a broken stream counts as unpacked");
        assert_eq!(names(&inside), before, "the half-unpacked folder is left");
        let _ = std::fs::remove_dir_all(&root);
    }


    #[test]
    fn a_refused_entry_is_the_reason_even_when_the_program_is_still_writing() {
        if !have("xz") {
            return;
        }
        // Far more data after the refused entry than a pipe holds: the program is surely still writing when the
        // unpacking stops, and is killed
        let (root, inside, outside) = evil_root("xz_refused_busy");
        let mut tar = raw_tar(&[("../../outside/escape", 'f', "")]);
        tar.extend(std::iter::repeat_n(0u8, 2 << 20));
        let archive = archive_as(&inside, "busy.tar.xz", &tar).unwrap();
        let err = extract_beside(&archive, &Control::unlimited()).unwrap_err();
        assert!(is_outside(&err) && err.to_string().contains("outside the folder"), "{err}");
        assert_eq!(names(&outside), ["secret"]);
        assert_eq!(names(&inside), ["busy.tar.xz"]);
        let _ = std::fs::remove_dir_all(&root);
    }


    #[test]
    fn folder_entries_of_an_archive_get_their_permissions_and_times() {
        use std::os::unix::fs::PermissionsExt;
        // A folder entry with its own mode and time, and a file in it that comes before the folder entry: the folder
        // exists by then, and still takes the mode and time of its own entry (they are applied last)
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind, mode) in [("kept/a", tar::EntryType::Regular, 0o644), ("kept/", tar::EntryType::Directory, 0o750)] {
            let mut header = tar::Header::new_gnu();
            header.set_path(name).unwrap();
            header.set_entry_type(kind);
            header.set_mode(mode);
            header.set_mtime(1_000_000_000);
            let data: &[u8] = if kind == tar::EntryType::Regular { b"x" } else { b"" };
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder.append(&header, data).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        for format in ["folders.tar", "folders.tar.gz", "folders.tar.xz"] {
            let (root, inside, _) = evil_root(&format!("folder_meta_{}", format.replace('.', "_")));
            let Some(archive) = archive_as(&inside, format, &tar) else { continue };
            let out = extract_beside(&archive, &Control::unlimited()).unwrap_or_else(|e| panic!("{format}: {e}"));
            let meta = std::fs::metadata(out.join("kept")).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o750, "{format}: the folder's mode");
            let mtime = meta.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            assert_eq!(mtime, 1_000_000_000, "{format}: the folder's time");
            assert_eq!(std::fs::read_to_string(out.join("kept/a")).unwrap(), "x", "{format}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

}
