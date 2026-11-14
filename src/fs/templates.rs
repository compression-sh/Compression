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

fn builtin() -> Vec<Template> {
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

/// Built-in templates first, then the files of the templates folder (one level of subfolders
/// is looked into), alphabetically.
pub fn load(custom: Option<&Path>) -> Vec<Template> {
    let mut all = builtin();
    let Some(dir) = user_dir(custom) else { return all };
    let mut found = Vec::new();
    collect(&dir, 0, &mut found);
    found.sort_by(|a, b| super::scanner::natural_cmp(&a.name, &b.name));
    all.extend(found);
    all
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<Template>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name.starts_with('.') || out.len() >= 200 {
            continue;
        }
        if path.is_dir() {
            if depth < 1 {
                collect(&path, depth + 1, out);
            }
            continue;
        }
        let icon = crate::fs::file_item::specific_icon(&file_name).map_or(icons::FILE, |(icon, _)| icon);
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| file_name.clone());
        out.push(Template { name, file_name, icon, source: Source::File(path) });
    }
}

/// Write the template's content to the new file `target`, which must not exist yet, and say what was left beside it.
///
/// The content goes into a hidden file of its own beside it (`.compression-new-…`) first, and only the whole file gets
/// the name, by a rename that never replaces anything (`RENAME_NOREPLACE`) or, where the file system cannot do that, by
/// a hard link, which never replaces anything either. So there is never an incomplete file under that name, also after
/// a hard stop. Where neither is possible, nothing is made under the name. Nothing is ever deleted here: a name cannot
/// be removed without a moment in which it could be someone else's file, so a hidden file that is left (after a
/// failure, or as the second name of a file made by a hard link) is said instead. `Ok(Some(note))` is such a note.
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
    let (mut read, mode): (Box<dyn io::Read>, Option<std::fs::Permissions>) = match &template.source {
        Source::Builtin { content, executable } => {
            use std::os::unix::fs::PermissionsExt;
            (Box::new(content.as_bytes()), executable.then(|| std::fs::Permissions::from_mode(0o755)))
        }
        Source::File(from) => {
            let file = std::fs::File::open(from)?;
            let mode = file.metadata()?.permissions();
            (Box::new(file), Some(mode))
        }
    };
    let parent = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let (staged, mut file) = staged_file(parent)?;
    let kept = staged.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let target_name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let left = |why: String| io::Error::other(format!("{why} · {}", trf("what was written is kept as “{}”; it can be deleted", &[&kept])));
    if let Err(err) = fill(&mut read, mode, &mut file) {
        return Err(left(crate::fs::describe::io_error(&err)));
    }
    drop(file);
    match publish(&staged, target) {
        Ok(Published::Renamed) => Ok(None),
        Ok(Published::Linked) => Ok(Some(trf("a second, hidden name for it is left as “{}”; it can be deleted", &[&kept]))),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(left(trf("“{}” already exists", &[&target_name]))),
        Err(err) => Err(left(crate::fs::describe::io_error(&err))),
    }
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
fn fill(read: &mut dyn io::Read, mode: Option<std::fs::Permissions>, file: &mut std::fs::File) -> io::Result<()> {
    #[cfg(test)]
    tests::copy_maybe_failing(read, file)?;
    #[cfg(not(test))]
    io::copy(read, file)?;
    if let Some(mode) = mode {
        super::ops::keep_mode(file.set_permissions(mode))?;
    }
    Ok(())
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
    }

    /// Folders in which the tests pretend the file system cannot make hard links.
    static NO_LINKS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(super) fn links_forced_off(path: &Path) -> bool {
        NO_LINKS.lock().unwrap().iter().any(|dir| path.starts_with(dir))
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

        let all = load(Some(&dir.join("tpl")));
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
