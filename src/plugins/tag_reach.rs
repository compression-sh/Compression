//! Where the files the tag index remembers are now. A file that is not there is not taken for gone: its drive may be
//! unplugged, its server may not answer, it may not be readable. It counts as gone only when the very file system it
//! was on is there and the file is not, and only a cleanup the person confirms forgets it then.
//!
//! A file system is known by its UUID (`/dev/disk/by-uuid`; on btrfs by the id btrfs makes of it), which another drive
//! mounted at the same place does not have. A file remembered before this was known has no UUID, and so is never taken for gone.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where a remembered file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// It is there.
    Here,
    /// Not there now, or it cannot be told: the drive or server is away, it cannot be read, its file system is unknown.
    Away,
    /// Its own file system is there, and the file is not.
    Gone,
}

/// What the index keeps beside the tags of a file, in a file of its own (`tag-paths-where.json`), so that the list of
/// tags stays as earlier versions read it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    /// The UUID of the file system the file was last seen on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<String>,
    /// The tags in the index were changed while the file could not be reached: they are written to it once it can.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}

#[derive(Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Metas(pub BTreeMap<PathBuf, Meta>);

impl Metas {
    fn file() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("compression").join("tag-paths-where.json"))
    }

    pub fn load() -> Self {
        Self::file().map(|f| crate::fs::stored::load_json(&f)).unwrap_or_default()
    }

    pub fn save(&self) {
        if crate::fs::admin::is_admin() {
            return;
        }
        let Some(file) = Self::file() else { return };
        if let Ok(json) = serde_json::to_string(self) {
            let _ = crate::fs::stored::save(&file, json);
        }
    }

    /// `from` and everything inside it now lives at `to`.
    pub fn rename(&mut self, from: &Path, to: &Path) -> bool {
        let moved: Vec<PathBuf> = self.0.keys().filter(|p| p.starts_with(from)).cloned().collect();
        for old in &moved {
            if let Some(meta) = self.0.remove(old) {
                self.0.insert(to.join(old.strip_prefix(from).unwrap_or(Path::new(""))), meta);
            }
        }
        !moved.is_empty()
    }

    pub fn remove_tree(&mut self, path: &Path) -> bool {
        let before = self.0.len();
        self.0.retain(|p, _| !p.starts_with(path));
        self.0.len() != before
    }
}

/// How the file system of a path is told: its UUID, or `None` when it has none that can be found.
pub type Identity = fn(&Path) -> Option<String>;

/// The UUID of the file system `path` (or the nearest folder above it that is there) is on, from `/dev/disk/by-uuid`;
/// on btrfs, whose numbers that listing does not name, the file system id btrfs makes of its UUID and the subvolume.
/// `None` on a server, and for a file system that has neither (tmpfs, …): there, a file is never taken for gone.
pub fn fs_identity(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    if crate::fs::network::is_remote(path) {
        return None;
    }
    let at = path.ancestors().find(|p| p.symlink_metadata().is_ok())?;
    let dev = at.symlink_metadata().ok()?.dev();
    let listed = std::fs::read_dir("/dev/disk/by-uuid").ok().and_then(|entries| {
        entries.flatten().find_map(|entry| {
            let target = std::fs::metadata(entry.path()).ok()?;
            (target.rdev() == dev).then(|| entry.file_name().to_string_lossy().into_owned())
        })
    });
    listed.or_else(|| btrfs_id(at))
}

/// The id btrfs gives `path`'s file system and subvolume, made of the file system's UUID, so it is the same at every
/// mount. Only btrfs: elsewhere the id may be made of the device number, which another drive in the same slot shares.
fn btrfs_id(path: &Path) -> Option<String> {
    const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
    let name = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).ok()?;
    // SAFETY: a NUL-terminated path and a zeroed struct for each call to fill
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(name.as_ptr(), &mut fs) } != 0 || fs.f_type as i64 != BTRFS_SUPER_MAGIC {
        return None;
    }
    // SAFETY: as above
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(name.as_ptr(), &mut vfs) } != 0 {
        return None;
    }
    Some(format!("btrfs:{:x}", vfs.f_fsid))
}

/// How long a server is given to say whether a remembered file on it is there.
pub const SERVER_WAIT: Duration = Duration::from_secs(2);

/// Where `path` is now, told by its own file system (`meta.fs`, through `identity`). Run off the window's thread.
pub fn classify(path: &Path, meta: Option<&Meta>, identity: Identity, server_wait: Duration) -> Reach {
    // A server is asked with a time limit, and a file on one is never taken for gone: there is no UUID to tell it by
    if crate::fs::network::is_remote(path) {
        let folder = path.parent().unwrap_or(path);
        return if crate::fs::mount_health::answers_within(folder, server_wait) && path.symlink_metadata().is_ok() { Reach::Here } else { Reach::Away };
    }
    match path.symlink_metadata() {
        Ok(_) => Reach::Here,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let Some(own) = meta.and_then(|m| m.fs.as_deref()) else { return Reach::Away };
            // The nearest folder above that is there must be on the file's own file system: then the file is not
            let above = path.ancestors().skip(1).find(|p| p.symlink_metadata().is_ok());
            match above.and_then(identity) {
                Some(found) if found == own => Reach::Gone,
                _ => Reach::Away,
            }
        }
        // No permission, an I/O error: it cannot be told
        Err(_) => Reach::Away,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_system_is_known_by_its_uuid_or_btrfs_id_and_one_without_either_by_nothing() {
        // What this computer has: each answer is either none, or the same every time it is asked
        for path in ["/", "/home", "/boot", "/tmp", "/dev/shm"] {
            let (first, again) = (fs_identity(Path::new(path)), fs_identity(Path::new(path)));
            assert_eq!(first, again, "{path}");
            eprintln!("{path}: {first:?}");
        }
        // A file that is not there is told by the folder above it
        assert_eq!(fs_identity(Path::new("/tmp/compression-not-there/at-all")), fs_identity(Path::new("/tmp")));
        // tmpfs has no UUID and is no btrfs: nothing is made up for it
        if std::fs::read_to_string("/proc/mounts").is_ok_and(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some("/dev/shm") && l.split_whitespace().nth(2) == Some("tmpfs"))) {
            assert_eq!(fs_identity(Path::new("/dev/shm")), None);
        }
    }
}
