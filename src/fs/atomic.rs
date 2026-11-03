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

