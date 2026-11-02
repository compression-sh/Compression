//! Writing the files that hold the person's data (settings, pins, servers, tags, history). Several
//! Compression processes (the resident one, dialogs, the portal) share these files, and one can be
//! stopped at any moment, so a file is never written in place: the new content goes to a file of its
//! own next to it and replaces the old one in a single `rename`. A reader sees all of the old
//! content or all of the new, and a failed write leaves the old file as it was.
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Replace the file at `path` with `contents`, or make it.
pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    write_with(path, |file| file.write_all(contents.as_ref()))
}

/// `write`, with the content produced by `fill`. When `fill` fails, `path` is untouched.
pub fn write_with(path: &Path, fill: impl FnOnce(&mut std::fs::File) -> io::Result<()>) -> io::Result<()> {
    // A file that is a link (a dotfile manager's) stays a link: the link's target is replaced
    let target: PathBuf = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = target.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let dir = target.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    // A name of its own for every call of every process, so writers never share a temporary file
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let temp = dir.join(format!(".{}.{}.{}.tmp", name.to_string_lossy(), std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
        fill(&mut file)?;
        file.flush()?;
        // On disk before it replaces the old file, or a crash could leave an empty one
        file.sync_all()?;
        if let Ok(meta) = std::fs::metadata(&target) {
            // Keeps the mode the person gave the file (a private one stays private)
            let _ = file.set_permissions(meta.permissions());
        }
        drop(file);
        std::fs::rename(&temp, &target)
    })();
    match result {
        Ok(()) => {
            // The rename itself reaches the disk too, as far as the platform allows
            if let Ok(folder) = std::fs::File::open(dir) {
                let _ = folder.sync_all();
            }
            Ok(())
        }
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("compression_atomic_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn writes_new_files_and_replaces_old_ones_without_leftovers() {
        let dir = sandbox("replace");
        let file = dir.join("settings.json");
        write(&file, "one").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one");
        write(&file, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
        assert_eq!(files_in(&dir), ["settings.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_write_leaves_the_old_file_whole_and_no_temporary_file() {
        let dir = sandbox("failed");
        let file = dir.join("pins.json");
        std::fs::write(&file, "old content").unwrap();
        // The new content breaks off half way through
        let err = write_with(&file, |f| {
            f.write_all(b"new, but only half")?;
            Err(io::Error::other("disk full"))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "disk full");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old content");
        assert_eq!(files_in(&dir), ["pins.json"]);
        // Nor does a failure make a file that was not there
        let fresh = dir.join("fresh.json");
        assert!(write_with(&fresh, |_| Err(io::Error::other("no"))).is_err());
        assert!(!fresh.exists());
        assert_eq!(files_in(&dir), ["pins.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_target_that_cannot_be_replaced_is_left_alone() {
        let dir = sandbox("blocked");
        // A folder in the way: the rename fails after the content was written
        std::fs::create_dir_all(dir.join("servers.json")).unwrap();
        std::fs::write(dir.join("servers.json/keep"), "x").unwrap();
        assert!(write(&dir.join("servers.json"), "data").is_err());
        assert_eq!(std::fs::read_to_string(dir.join("servers.json/keep")).unwrap(), "x");
        assert_eq!(files_in(&dir), ["servers.json"]);
        // And a missing folder is an error, not a surprise
        assert!(write(&dir.join("missing/file.json"), "data").is_err());
        assert!(!dir.join("missing").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn links_and_modes_are_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = sandbox("link");
        let real = dir.join("real.json");
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write(&link, "new").unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(real.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn readers_never_see_a_half_written_file_while_several_writers_run() {
        let dir = sandbox("race");
        let file = dir.join("shared.json");
        let versions: Vec<String> = (0..4).map(|i| format!("{{\"writer\":{i},\"filler\":\"{}\"}}", "x".repeat(50_000))).collect();
        write(&file, &versions[0]).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = versions
            .iter()
            .cloned()
            .map(|text| {
                let file = file.clone();
                std::thread::spawn(move || {
                    for _ in 0..40 {
                        write(&file, &text).unwrap();
                    }
                })
            })
            .collect();
        let reader = {
            let (file, stop, versions) = (file.clone(), stop.clone(), versions.clone());
            std::thread::spawn(move || {
                let mut reads = 0;
                while !stop.load(Ordering::Relaxed) {
                    let text = std::fs::read_to_string(&file).unwrap();
                    assert!(versions.contains(&text), "a reader saw {} bytes that are no writer's whole content", text.len());
                    reads += 1;
                }
                reads
            })
        };
        for writer in writers {
            writer.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
        assert_eq!(files_in(&dir), ["shared.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
