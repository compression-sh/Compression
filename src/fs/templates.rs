//! File templates for "New file": a few built-in ones, and every file found in the templates
//! folder (the desktop's `Templates` folder unless the settings name another).
use std::io;
use std::path::{Path, PathBuf};

use crate::i18n::{tr, trf};
use crate::ui::icons;

#[derive(Debug, Clone)]
pub enum Source {
    Builtin { content: &'static str, executable: bool },
    File(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Template {
    /// What the list shows.
    pub name: String,
    /// The name a new file starts with, extension included.
    pub file_name: String,
    pub icon: &'static str,
    pub source: Source,
}

impl Template {
    pub fn extension(&self) -> Option<&str> {
        // `archive.tar.gz` ends in two parts
        let lower = self.file_name.to_ascii_lowercase();
        for compound in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst"] {
            if lower.ends_with(compound) && self.file_name.len() > compound.len() {
                return Some(&self.file_name[self.file_name.len() - compound.len() + 1..]);
            }
        }
        self.file_name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()).map(|(_, ext)| ext)
    }
}

/// The folder user templates live in.
pub fn user_dir(custom: Option<&Path>) -> Option<PathBuf> {
    custom.map(Path::to_path_buf).or_else(dirs::template_dir).or_else(|| dirs::home_dir().map(|h| h.join("Templates")))
}

/// The built-in templates; nothing is read.
pub fn builtin() -> Vec<Template> {
    let make = |name: &str, file: &str, icon, content, executable| Template {
        name: name.to_owned(),
        file_name: file.to_owned(),
        icon,
        source: Source::Builtin { content, executable },
    };
    let untitled = tr("Untitled");
    vec![
        make(tr("Empty file"), untitled, icons::FILE, "", false),
        make(tr("Text document"), &format!("{untitled}.txt"), icons::TEXT, "", false),
        make(tr("Markdown document"), &format!("{untitled}.md"), icons::MARKDOWN, "# \n", false),
        make(tr("Shell script"), "script.sh", icons::SHELL, "#!/usr/bin/env bash\nset -euo pipefail\n\n", true),
        make(tr("Python script"), "script.py", icons::PYTHON, "#!/usr/bin/env python3\n\n\nif __name__ == \"__main__\":\n    pass\n", true),
        make(tr("HTML page"), "index.html", icons::HTML, "<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\">\n  <title></title>\n</head>\n<body>\n\n</body>\n</html>\n", false),
        make(tr("JSON file"), &format!("{untitled}.json"), icons::JSON, "{}\n", false),
    ]
}

/// The files of the templates folder (one level of subfolders is looked into), alphabetically. Reads that folder.
pub fn user(custom: Option<&Path>) -> Vec<Template> {
    let Some(dir) = user_dir(custom) else { return Vec::new() };
    let mut found = Vec::new();
    collect(&dir, 0, &mut found);
    found.sort_by(|a, b| super::scanner::natural_cmp(&a.name, &b.name));
    found
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<Template>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name.starts_with('.') || out.len() >= 200 {
            continue;
        }
        // Follows links; looks, opens nothing
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.is_dir() {
            if depth < 1 {
                collect(&path, depth + 1, out);
            }
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let icon = crate::fs::file_item::specific_icon(&file_name).map_or(icons::FILE, |(icon, _)| icon);
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| file_name.clone());
        out.push(Template { name, file_name, icon, source: Source::File(path) });
    }
}

/// Write the template's content to the new file `target`, which must not exist yet, and say what was left beside it.
///
/// The content goes into a hidden file of its own beside it (`.compression-new-…`) first. That file, with its
/// permissions, is synced (`fsync`), and only then does the whole file get the name, by a rename that never replaces
/// anything (`RENAME_NOREPLACE`) or, where the file system cannot do that, by a hard link, which never replaces anything
/// either. So a crash or kill never leaves an incomplete file under that name. Where neither way to give the name
/// exists, or the sync fails, nothing is made under the name and the sync is not retried.
///
/// Power loss: nothing here is promised. The sync asks for the content before the name exists, which helps only where
/// the file system and the device keep what `fsync` confirmed (not checked); the folder is not synced, so the new name
/// itself may be gone after a power loss.
///
/// Nothing is ever deleted here: a name cannot be removed without a moment in which it could be someone else's file,
/// so a hidden file that is left (after a failure, or as the second name of a file made by a hard link) is said
/// instead. `Ok(Some(note))` is such a note.
pub fn create(template: &Template, target: &Path) -> io::Result<Option<String>> {
    // Nothing to write: made directly, like a new empty file, which cannot be incomplete and works on every file system
    if let Source::Builtin { content: "", executable: false } = &template.source {
        return std::fs::OpenOptions::new().write(true).create_new(true).open(target).map(|_| None);
    }
    // A name that is taken, and a template that cannot be opened, are said before anything is made (a name taken
    // meanwhile is still never replaced below)
    if target.symlink_metadata().is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let (mut read, mode, len): (Box<dyn io::Read>, Option<std::fs::Permissions>, u64) = match &template.source {
        Source::Builtin { content, executable } => {
            use std::os::unix::fs::PermissionsExt;
            (Box::new(content.as_bytes()), executable.then(|| std::fs::Permissions::from_mode(0o755)), content.len() as u64)
        }
        Source::File(from) => {
            use std::io::Read;
            let (file, meta) = open_template(from)?;
            // What is appended while it is read is left out
            (Box::new(file.take(meta.len())), Some(meta.permissions()), meta.len())
        }
    };
    let parent = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let (staged, mut file) = staged_file(parent)?;
    let kept = staged.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let target_name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let left = |why: String| io::Error::other(format!("{why} · {}", trf("what was written is kept as “{}”; it can be deleted", &[&kept])));
    // Whole and synced before it gets the name; a failed sync is not retried (what reached the disk is unknown)
    let whole = |written: u64| if written < len { Err(io::Error::other(tr("the template became shorter while it was read"))) } else { Ok(()) };
    if let Err(err) = fill(&mut read, mode, &mut file).and_then(whole).and_then(|()| sync(&file)) {
        return Err(left(crate::fs::describe::io_error(&err)));
    }
    drop(file);
    let published = publish(&staged, target);
    #[cfg(test)]
    if published.is_ok() {
        tests::step("published");
    }
    match published {
        Ok(Published::Renamed) => Ok(None),
        Ok(Published::Linked) => Ok(Some(trf("a second, hidden name for it is left as “{}”; it can be deleted", &[&kept]))),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(left(trf("“{}” already exists", &[&target_name]))),
        Err(err) => Err(left(crate::fs::describe::io_error(&err))),
    }
}

/// `fsync` of the hidden file, with its content and permissions, before it gets the name.
fn sync(file: &std::fs::File) -> io::Result<()> {
    #[cfg(test)]
    if let Some(err) = tests::sync_step() {
        return Err(err);
    }
    file.sync_all()
}

/// How the whole file got its name.
#[derive(Debug, PartialEq, Eq)]
enum Published {
    /// Renamed: the hidden name is gone.
    Renamed,
    /// Hard-linked: the hidden name is still there, a second name of the same file.
    Linked,
}

/// Give the whole file `staged` the name `target`, never replacing anything there: a rename with `RENAME_NOREPLACE`,
/// or a hard link where the file system cannot do that. Where it can do neither, an error says so.
fn publish(staged: &Path, target: &Path) -> io::Result<Published> {
    match crate::compat::rename_noreplace(staged, target) {
        Ok(()) => Ok(Published::Renamed),
        Err(err) if err.kind() == io::ErrorKind::Unsupported => match hard_link(staged, target) {
            Ok(()) => Ok(Published::Linked),
            Err(err) if links_unsupported(&err) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                tr("This file system cannot make a file from a template without the risk of leaving an incomplete one, so nothing was made under that name"),
            )),
            Err(err) => Err(err),
        },
        Err(err) => Err(err),
    }
}

/// Whether a hard link failed because the file system cannot make one (`link(2)`: EPERM where it does not support them,
/// EOPNOTSUPP or ENOSYS from a file system of its own), as opposed to a real failure such as EEXIST or ENOSPC.
fn links_unsupported(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EPERM | libc::EOPNOTSUPP | libc::ENOSYS))
}

/// A new, empty hidden file in `dir`, made here (so it is known to be this one's own).
fn staged_file(dir: &Path) -> io::Result<(PathBuf, std::fs::File)> {
    for n in 0..1000u32 {
        let path = dir.join(format!(".compression-new-{}-t{n}", std::process::id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(io::ErrorKind::AlreadyExists.into())
}

/// `link(2)`: `target` becomes a second name of `staged`; a `target` that is there (a link too) is an error, never
/// replaced or followed.
fn hard_link(staged: &Path, target: &Path) -> io::Result<()> {
    #[cfg(test)]
    if tests::links_forced_off(staged) {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    std::fs::hard_link(staged, target)
}

/// Everything `read` gives into `file`, then the permissions `mode`.
fn fill(read: &mut dyn io::Read, mode: Option<std::fs::Permissions>, file: &mut std::fs::File) -> io::Result<u64> {
    #[cfg(test)]
    let written = tests::copy_maybe_failing(read, file)?;
    #[cfg(not(test))]
    let written = io::copy(read, file)?;
    if let Some(mode) = mode {
        super::ops::keep_mode(file.set_permissions(mode))?;
    }
    Ok(written)
}

/// Opens the template file `from` for reading only if it is a regular file (a link to one is followed), with what
/// `fstat` said of it then.
///
/// It is looked at through `O_PATH` first, which runs no driver's `open` and does not open a pipe for reading, so a
/// pipe, socket or device is never opened. Only a regular file is then opened for reading, through `/proc/self/fd`, so
/// it is the same file even when the name was swapped meanwhile, and the opened descriptor is checked to be that file.
/// Without a usable `/proc` the template is refused: opening it by its name instead would open whatever the name points
/// to by then. Built-in templates read no file and do not need `/proc`.
///
/// Not covered: looking it up or opening it may block on a hanging network or FUSE file system; and a change of the
/// content that keeps its length while it is read goes unnoticed (only a file that gets shorter is caught).
fn open_template(from: &Path) -> io::Result<(std::fs::File, std::fs::Metadata)> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let looked = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_PATH).open(from)?;
    let seen = looked.metadata()?;
    if !seen.is_file() {
        let name = from.file_name().map_or_else(|| from.display().to_string(), |n| n.to_string_lossy().into_owned());
        return Err(io::Error::other(trf("“{}” is not a regular file, so it cannot be a template", &[&name])));
    }
    let fds = proc_fds();
    #[cfg(test)]
    tests::at(tests::Stage::BetweenOpens(looked.as_raw_fd()), from);
    #[cfg(test)]
    tests::step("open");
    // The file looked at, whatever its name points to now; a regular file, so no flags for pipes or terminals are needed
    let file = std::fs::File::open(fds.join(looked.as_raw_fd().to_string()))
        .map_err(|err| if fds.is_dir() { err } else { io::Error::other(tr("This template cannot be opened safely here, because /proc is not available")) })?;
    let meta = file.metadata()?;
    // The same file as looked at, so a regular one
    if (meta.dev(), meta.ino()) != (seen.dev(), seen.ino()) {
        return Err(io::Error::other(tr("The template was replaced while it was opened")));
    }
    #[cfg(test)]
    tests::at(tests::Stage::Opened, from);
    Ok((file, meta))
}

/// Where this process's descriptors can be opened again.
fn proc_fds() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = tests::PROC_FDS.with(|d| d.borrow().clone()) {
        return dir;
    }
    PathBuf::from("/proc/self/fd")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::{Read, Write};
    use std::os::unix::fs::MetadataExt;
    use std::sync::Mutex;

    /// A failure to bring about while a template is written, after so many bytes.
    #[derive(Clone, Copy)]
    enum Fail {
        ReadAfter(u64),
        WriteAfter(u64),
    }

    thread_local! {
        static FAIL: Cell<Option<Fail>> = const { Cell::new(None) };
        /// The sync of the hidden file fails (EIO).
        static FAIL_SYNC: Cell<bool> = const { Cell::new(false) };
        /// What happened, in order: the sync and the publication.
        static STEPS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    pub(super) fn step(what: &str) {
        STEPS.with(|s| s.borrow_mut().push(what.to_owned()));
    }

    /// Notes the sync, and gives the failure to bring about, if any.
    pub(super) fn sync_step() -> Option<io::Error> {
        step("sync");
        FAIL_SYNC.with(Cell::get).then(|| io::Error::from_raw_os_error(libc::EIO))
    }

    /// Where `open_template` lets a test step in.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Stage {
        /// Looked at as a regular file through this `O_PATH` descriptor, not opened for reading yet.
        BetweenOpens(i32),
        /// Opened for reading, its length taken.
        Opened,
    }

    type Hook = Box<dyn FnMut(Stage, &Path)>;

    thread_local! {
        static HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
        /// Stands in for `/proc/self/fd`.
        pub(super) static PROC_FDS: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn at(stage: Stage, from: &Path) {
        HOOK.with(|h| {
            if let Some(hook) = h.borrow_mut().as_mut() {
                hook(stage, from);
            }
        });
    }

    /// Runs `f` with `hook` in `open_template`, and the descriptors at `proc_fds` if given.
    fn with_hook<T>(hook: impl FnMut(Stage, &Path) + 'static, proc_fds: Option<PathBuf>, f: impl FnOnce() -> T) -> T {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        PROC_FDS.with(|d| *d.borrow_mut() = proc_fds);
        let out = f();
        HOOK.with(|h| *h.borrow_mut() = None);
        PROC_FDS.with(|d| *d.borrow_mut() = None);
        out
    }

    fn mkfifo(path: &Path) {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid C string; mkfifo only makes a file system entry.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    }

    /// `f` on a thread of its own, given up on after 5 s: a template that blocks fails the test, not the run.
    fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("creating from the template blocked")
    }

    fn create_within(template: Template, target: PathBuf) -> (io::Result<Option<String>>, Vec<String>) {
        within(move || (create(&template, &target), take_steps()))
    }

    fn take_steps() -> Vec<String> {
        STEPS.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    /// Folders in which the tests pretend the file system cannot make hard links.
    static NO_LINKS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(super) fn links_forced_off(path: &Path) -> bool {
        NO_LINKS.lock().unwrap().iter().any(|dir| path.starts_with(dir))
    }

    /// Hard links count as missing under `dir` from now on.
    pub(crate) fn without_links_under(dir: &Path) {
        NO_LINKS.lock().unwrap().push(dir.to_path_buf());
    }

    struct FailingRead<'a> {
        inner: &'a mut dyn Read,
        left: u64,
    }

    impl Read for FailingRead<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.left == 0 {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            let most = buf.len().min(self.left as usize);
            let n = self.inner.read(&mut buf[..most])?;
            self.left -= n as u64;
            Ok(n)
        }
    }

    struct FailingWrite<'a> {
        inner: &'a mut std::fs::File,
        left: u64,
    }

    impl Write for FailingWrite<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.left == 0 {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            let n = self.inner.write(&buf[..buf.len().min(self.left as usize)])?;
            self.left -= n as u64;
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    pub(super) fn copy_maybe_failing(read: &mut dyn Read, file: &mut std::fs::File) -> io::Result<u64> {
        match FAIL.with(Cell::get) {
            None => io::copy(read, file),
            Some(Fail::ReadAfter(left)) => io::copy(&mut FailingRead { inner: read, left }, file),
            Some(Fail::WriteAfter(left)) => io::copy(read, &mut FailingWrite { inner: file, left }),
        }
    }

    fn work_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("compression_tpl_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    fn hidden(dir: &Path) -> Vec<PathBuf> {
        names(dir).into_iter().filter(|n| n.starts_with(".compression-new-")).map(|n| dir.join(n)).collect()
    }

    fn from_file(path: &Path) -> Template {
        Template { name: "t".to_owned(), file_name: "t".to_owned(), icon: icons::FILE, source: Source::File(path.to_path_buf()) }
    }

    /// 64 KiB that tell where they were cut.
    fn big_template(dir: &Path) -> (PathBuf, Vec<u8>) {
        let content: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("tpl"), &content).unwrap();
        (dir.join("tpl"), content)
    }

    #[test]
    fn a_read_or_write_that_fails_after_some_bytes_leaves_nothing_under_the_name_and_says_what_is_kept() {
        for (fail, label) in [(Fail::ReadAfter(4096), "read"), (Fail::WriteAfter(4096), "write")] {
            let dir = work_dir(&format!("fails_{label}"));
            let (tpl, content) = big_template(&dir);
            FAIL.with(|f| f.set(Some(fail)));
            let err = create(&from_file(&tpl), &dir.join("new.bin")).unwrap_err().to_string();
            FAIL.with(|f| f.set(None));
            assert!(!dir.join("new.bin").exists(), "{label}: nothing under the name");
            let kept = hidden(&dir);
            assert_eq!(kept.len(), 1, "{label}: one hidden file is left, not removed");
            // It already held what was written before the failure: the failure came after bytes, not before
            assert_eq!(std::fs::read(&kept[0]).unwrap(), &content[..4096], "{label}");
            let name = kept[0].file_name().unwrap().to_string_lossy().into_owned();
            assert!(err.contains(&name), "{label}: the message names what is kept: {err}");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn the_file_is_synced_before_it_gets_its_name_on_either_way() {
        for linked in [false, true] {
            let dir = work_dir(&format!("sync_order_{linked}"));
            let (tpl, content) = big_template(&dir);
            if linked {
                crate::compat::tests::without_support_under(&dir);
            }
            take_steps();
            let made = create(&from_file(&tpl), &dir.join("new.bin")).unwrap();
            assert_eq!(take_steps(), ["open", "sync", "published"], "linked: {linked}");
            assert_eq!(std::fs::read(dir.join("new.bin")).unwrap(), content);
            assert_eq!(made.is_some(), linked, "only the hard link says something: its hidden second name");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_file_that_cannot_be_synced_never_gets_its_name_and_the_hidden_file_is_said() {
        let dir = work_dir("sync_file_fails");
        let (tpl, content) = big_template(&dir);
        std::fs::write(dir.join("other.bin"), b"someone else's").unwrap();
        FAIL_SYNC.with(|f| f.set(true));
        take_steps();
        let err = create(&from_file(&tpl), &dir.join("new.bin")).unwrap_err().to_string();
        FAIL_SYNC.with(|f| f.set(false));
        assert_eq!(take_steps(), ["open", "sync"], "not published, no second try");
        assert!(!dir.join("new.bin").exists(), "nothing under the name");
        let kept = hidden(&dir);
        assert_eq!(kept.len(), 1, "the hidden file is left, not removed");
        assert_eq!(std::fs::read(&kept[0]).unwrap(), content, "it holds what was written");
        let name = kept[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(err.contains(&name), "the message names it: {err}");
        assert_eq!(std::fs::read(dir.join("other.bin")).unwrap(), b"someone else's");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_whole_file_gets_its_name_by_a_rename_and_nothing_hidden_stays() {
        let dir = work_dir("renamed");
        let (tpl, content) = big_template(&dir);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tpl, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(create(&from_file(&tpl), &dir.join("new.bin")).unwrap(), None);
        assert_eq!(std::fs::read(dir.join("new.bin")).unwrap(), content);
        assert_eq!(std::fs::metadata(dir.join("new.bin")).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(names(&dir), ["new.bin", "tpl"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_a_safe_rename_a_hard_link_gives_the_whole_file_its_name_and_the_hidden_second_name_is_said() {
        let dir = work_dir("linked");
        crate::compat::tests::without_support_under(&dir);
        let (tpl, content) = big_template(&dir);
        let note = create(&from_file(&tpl), &dir.join("new.bin")).unwrap().expect("the second name is said");
        let kept = hidden(&dir);
        assert_eq!(kept.len(), 1, "the hidden name is not removed");
        assert!(note.contains(&*kept[0].file_name().unwrap().to_string_lossy()), "{note}");
        // The very same, whole file under both names; nothing was emptied
        let (a, b) = (std::fs::metadata(dir.join("new.bin")).unwrap(), std::fs::metadata(&kept[0]).unwrap());
        assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
        assert_eq!(std::fs::read(dir.join("new.bin")).unwrap(), content);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_that_is_taken_also_by_a_link_is_never_replaced_or_followed_on_either_way() {
        for safe in [true, false] {
            let dir = work_dir(if safe { "taken" } else { "taken_linked" });
            if !safe {
                crate::compat::tests::without_support_under(&dir);
            }
            std::fs::write(dir.join("tpl"), "new content").unwrap();
            std::fs::write(dir.join("mine.txt"), "mine").unwrap();
            std::os::unix::fs::symlink("nowhere", dir.join("dangling")).unwrap();
            std::os::unix::fs::symlink("mine.txt", dir.join("to-mine")).unwrap();
            for taken in ["mine.txt", "dangling", "to-mine"] {
                let err = create(&from_file(&dir.join("tpl")), &dir.join(taken)).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{taken}, safe {safe}: {err}");
            }
            assert_eq!(std::fs::read_to_string(dir.join("mine.txt")).unwrap(), "mine");
            assert_eq!(std::fs::read_link(dir.join("dangling")).unwrap(), Path::new("nowhere"));
            assert_eq!(std::fs::read_link(dir.join("to-mine")).unwrap(), Path::new("mine.txt"));
            assert_eq!(names(&dir), ["dangling", "mine.txt", "to-mine", "tpl"], "safe {safe}: nothing was begun");
            // Taken only after the whole file was written: still not replaced, on either way, and the file is kept
            let (staged, mut file) = staged_file(&dir).unwrap();
            file.write_all(b"whole").unwrap();
            for taken in ["mine.txt", "dangling"] {
                assert_eq!(publish(&staged, &dir.join(taken)).unwrap_err().kind(), io::ErrorKind::AlreadyExists, "{taken}, safe {safe}");
            }
            assert_eq!(std::fs::read_to_string(dir.join("mine.txt")).unwrap(), "mine");
            assert!(dir.join("dangling").symlink_metadata().unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read_to_string(&staged).unwrap(), "whole");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn where_neither_way_exists_nothing_is_made_under_the_name_but_empty_files_still_are() {
        let dir = work_dir("neither");
        crate::compat::tests::without_support_under(&dir);
        NO_LINKS.lock().unwrap().push(dir.clone());
        let (tpl, content) = big_template(&dir);
        let err = create(&from_file(&tpl), &dir.join("new.bin")).unwrap_err().to_string();
        assert!(err.contains("cannot make a file from a template"), "{err}");
        assert!(!dir.join("new.bin").exists());
        let kept = hidden(&dir);
        assert_eq!(kept.len(), 1);
        assert!(err.contains(&*kept[0].file_name().unwrap().to_string_lossy()), "{err}");
        assert_eq!(std::fs::read(&kept[0]).unwrap(), content, "what was written is kept whole");
        // The built-in templates without content are made directly, as before
        for template in builtin().iter().filter(|t| matches!(t.source, Source::Builtin { content: "", .. })) {
            assert_eq!(create(template, &dir.join(&template.file_name)).unwrap(), None, "{}", template.name);
            assert_eq!(std::fs::metadata(dir.join(&template.file_name)).unwrap().len(), 0);
        }
        assert_eq!(hidden(&dir).len(), 1, "they leave nothing hidden");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_missing_kind_of_link_counts_as_no_hard_links() {
        for errno in [libc::EPERM, libc::EOPNOTSUPP, libc::ENOSYS] {
            assert!(links_unsupported(&io::Error::from_raw_os_error(errno)), "{errno}");
        }
        for errno in [libc::EEXIST, libc::EACCES, libc::ENOSPC, libc::EIO, libc::EMLINK, libc::EROFS] {
            assert!(!links_unsupported(&io::Error::from_raw_os_error(errno)), "{errno}");
        }
        assert!(!links_unsupported(&io::Error::other("not an errno")));
    }

    #[test]
    fn pipes_sockets_and_devices_are_never_opened_nor_listed_also_through_links() {
        let dir = work_dir("special");
        let tpl = dir.join("tpl");
        std::fs::create_dir(&tpl).unwrap();
        mkfifo(&tpl.join("pipe"));
        std::os::unix::fs::symlink(tpl.join("pipe"), tpl.join("to-pipe")).unwrap();
        // Looked at only (O_PATH): no driver's open runs
        std::os::unix::fs::symlink("/dev/null", tpl.join("to-null")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(tpl.join("socket")).unwrap();
        std::fs::write(tpl.join("Real.txt"), "real").unwrap();
        for name in ["pipe", "to-pipe", "to-null", "socket"] {
            take_steps();
            let (made, steps) = create_within(from_file(&tpl.join(name)), dir.join("new"));
            let err = made.unwrap_err().to_string();
            assert!(err.contains("is not a regular file"), "{name}: {err}");
            assert!(!steps.contains(&"open".to_owned()), "{name}: never opened for reading: {steps:?}");
            assert_eq!(names(&dir), ["tpl"], "{name}: nothing made, no hidden file");
        }
        let listed: Vec<String> = user(Some(&tpl)).into_iter().map(|t| t.file_name).collect();
        assert_eq!(listed, ["Real.txt"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_link_to_a_regular_file_is_listed_and_makes_the_whole_file() {
        let dir = work_dir("link");
        let tpl = dir.join("list");
        std::fs::create_dir(&tpl).unwrap();
        let (real, content) = big_template(&dir);
        std::os::unix::fs::symlink(&real, tpl.join("Linked.bin")).unwrap();
        let listed = user(Some(&tpl));
        assert_eq!(listed.iter().map(|t| t.file_name.as_str()).collect::<Vec<_>>(), ["Linked.bin"]);
        assert_eq!(create(&listed[0], &dir.join("new.bin")).unwrap(), None);
        assert_eq!(std::fs::read(dir.join("new.bin")).unwrap(), content);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_swapped_for_a_pipe_between_looking_and_opening_still_gives_the_file_that_was_looked_at() {
        let dir = work_dir("swap");
        let (tpl, content) = big_template(&dir);
        let (from, aside, template, target) = (tpl.clone(), dir.join("aside"), from_file(&tpl), dir.join("new.bin"));
        let made = within(move || {
            with_hook(
                move |stage, _| {
                    if matches!(stage, Stage::BetweenOpens(_)) {
                        std::fs::rename(&from, &aside).unwrap();
                        mkfifo(&from);
                    }
                },
                None,
                || create(&template, &target),
            )
        });
        assert_eq!(made.unwrap(), None);
        assert_eq!(std::fs::read(dir.join("new.bin")).unwrap(), content);
        use std::os::unix::fs::FileTypeExt;
        assert!(std::fs::symlink_metadata(&tpl).unwrap().file_type().is_fifo(), "the pipe was there, and not opened");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_proc_a_file_template_is_refused_before_anything_is_made_and_built_in_ones_still_work() {
        let dir = work_dir("no_proc");
        let (tpl, _) = big_template(&dir);
        let gone = dir.join("no-proc");
        let markdown = builtin().into_iter().find(|t| t.file_name.ends_with(".md")).unwrap();
        let (refused, built) = with_hook(|_, _| {}, Some(gone), || {
            (create(&from_file(&tpl), &dir.join("new.bin")), create(&markdown, &dir.join("new.md")))
        });
        let err = refused.unwrap_err().to_string();
        assert!(err.contains("/proc is not available"), "{err}");
        assert_eq!(built.unwrap(), None);
        assert_eq!(std::fs::read_to_string(dir.join("new.md")).unwrap(), "# \n");
        // Nothing for the file template: no name, no hidden file
        assert_eq!(names(&dir), ["new.md", "tpl"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_ordinary_failure_to_open_again_is_said_as_itself_and_not_as_a_missing_proc() {
        // The descriptors are there, but opening this one fails (a link to itself: ELOOP, also for root)
        let dir = work_dir("reopen_fails");
        let (tpl, _) = big_template(&dir);
        let fds = dir.join("fds");
        std::fs::create_dir(&fds).unwrap();
        let links = fds.clone();
        let made = with_hook(
            move |stage, _| {
                if let Stage::BetweenOpens(fd) = stage {
                    let own = links.join(fd.to_string());
                    std::os::unix::fs::symlink(&own, &own).unwrap();
                }
            },
            Some(fds),
            || create(&from_file(&tpl), &dir.join("new.bin")),
        );
        let err = made.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP), "the error itself: {err}");
        assert!(!err.to_string().contains("/proc"), "{err}");
        assert_eq!(names(&dir), ["fds", "tpl"], "no hidden file, nothing under the name");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_descriptor_that_is_not_the_file_looked_at_is_refused() {
        let dir = work_dir("identity");
        let (tpl, _) = big_template(&dir);
        std::fs::write(dir.join("other"), "other").unwrap();
        let fds = dir.join("fds");
        std::fs::create_dir(&fds).unwrap();
        let (links, other) = (fds.clone(), dir.join("other"));
        let made = with_hook(
            move |stage, _| {
                if let Stage::BetweenOpens(fd) = stage {
                    std::os::unix::fs::symlink(&other, links.join(fd.to_string())).unwrap();
                }
            },
            Some(fds),
            || create(&from_file(&tpl), &dir.join("new.bin")),
        );
        let err = made.unwrap_err().to_string();
        assert!(err.contains("was replaced while it was opened"), "{err}");
        assert_eq!(names(&dir), ["fds", "other", "tpl"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_template_that_shrinks_while_it_is_read_publishes_nothing_and_one_that_grows_is_cut_at_its_first_length() {
        let dir = work_dir("shrinks");
        let (tpl, _) = big_template(&dir);
        let from = tpl.clone();
        let made = with_hook(
            move |stage, _| {
                if stage == Stage::Opened {
                    std::fs::OpenOptions::new().write(true).open(&from).unwrap().set_len(100).unwrap();
                }
            },
            None,
            || create(&from_file(&tpl), &dir.join("new.bin")),
        );
        let err = made.unwrap_err().to_string();
        assert!(err.contains("became shorter while it was read"), "{err}");
        assert!(!dir.join("new.bin").exists());
        let kept = hidden(&dir);
        assert_eq!(kept.len(), 1);
        assert!(err.contains(&*kept[0].file_name().unwrap().to_string_lossy()), "the hidden file is said: {err}");
        std::fs::remove_dir_all(&dir).unwrap();

        let dir = work_dir("grows");
        let (tpl, content) = big_template(&dir);
        let from = tpl.clone();
        let made = with_hook(
            move |stage, _| {
                if stage == Stage::Opened {
                    std::fs::OpenOptions::new().append(true).open(&from).unwrap().write_all(&[7; 1000]).unwrap();
                }
            },
            None,
            || create(&from_file(&tpl), &dir.join("new.bin")),
        );
        assert_eq!(made.unwrap(), None);
        assert_eq!(std::fs::read(dir.join("new.bin")).unwrap(), content, "only the length it had when opened");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_template_that_cannot_be_opened_begins_nothing() {
        let dir = work_dir("missing");
        assert!(create(&from_file(&dir.join("gone")), &dir.join("new.bin")).is_err());
        assert!(names(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn builtin_and_user_templates_create_files() {
        let dir = std::env::temp_dir().join(format!("compression_tpl_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tpl/sub")).unwrap();
        std::fs::write(dir.join("tpl/Invoice.odt"), "odt").unwrap();
        std::fs::write(dir.join("tpl/sub/Letter.txt"), "dear").unwrap();
        std::fs::write(dir.join("tpl/.hidden"), "x").unwrap();

        let all: Vec<Template> = builtin().into_iter().chain(user(Some(&dir.join("tpl")))).collect();
        let names: Vec<&str> = all.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"Invoice") && names.contains(&"Letter") && !names.iter().any(|n| n.contains("hidden")));
        assert_eq!(all[0].file_name, "Untitled");

        let sh = all.iter().find(|t| t.file_name == "script.sh").unwrap();
        create(sh, &dir.join("a.sh")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.join("a.sh")).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(create(sh, &dir.join("a.sh")).is_err(), "an existing file is never replaced");

        let invoice = all.iter().find(|t| t.name == "Invoice").unwrap();
        create(invoice, &dir.join("new.odt")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("new.odt")).unwrap(), "odt");
        assert_eq!(invoice.extension(), Some("odt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
