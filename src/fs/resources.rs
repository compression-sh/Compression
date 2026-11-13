//! What a job needs to stay there while it runs: the file systems and servers its paths are on. Disconnecting a server,
//! or unmounting or ejecting a drive, waits for the jobs that need it, and leaves the others alone.
//!
//! A local path is told by the file system it is on (`st_dev`), which the kernel gives every mounted file system of its
//! own; a path on a server by the mount it is under (the mount table and GVfs's folders, see `network::mount_root`),
//! since GVfs shows all its servers through one file system. A server is never asked: a path on one is told by its
//! name alone, so a server that hangs cannot hold the window up here.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Resource {
    /// A mounted file system, by its device number.
    FileSystem(u64),
    /// A server (or a phone or camera GVfs mounted), by the folder it is mounted on.
    Server(PathBuf),
    /// A whole drive, by its kernel name (`sdb`): every file system on it, also one mounted after it was held. What
    /// ejecting a drive holds; a job never needs it by this name, but by its file systems (see `overlaps`).
    Drive(String),
}

/// What `path` needs. A path that is not there yet (where a copy goes) is on the file system of the nearest folder above
/// it that is; a link is followed, as the job follows it to read or write through it.
pub fn of(path: &Path) -> Option<Resource> {
    if crate::fs::network::is_remote(path) {
        return crate::fs::network::mount_root(path).map(Resource::Server);
    }
    use std::os::unix::fs::MetadataExt;
    path.ancestors().find_map(|p| p.metadata().or_else(|_| p.symlink_metadata()).ok()).map(|m| Resource::FileSystem(m.dev()))
}

/// Whether something a job needs (`needed`) is among what is `held`: the same file system or server, or a file system
/// on a held drive. The drive of a file system is only looked up while a drive is held.
pub fn overlaps(held: &[Resource], needed: &[Resource]) -> bool {
    needed.iter().any(|n| {
        held.iter().any(|h| {
            h == n || matches!((h, n), (Resource::Drive(drive), Resource::FileSystem(dev)) if crate::fs::drives::drives_of_file_system(*dev).contains(drive))
        })
    })
}

/// What all of `paths` need, each once.
pub fn of_all<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Vec<Resource> {
    let mut out: Vec<Resource> = Vec::new();
    for resource in paths.into_iter().filter_map(|p| of(p)) {
        if !out.contains(&resource) {
            out.push(resource);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_told_by_its_file_system_and_a_new_one_by_the_folder_it_goes_in() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("compression_resources_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dev = dir.metadata().unwrap().dev();
        assert_eq!(of(&dir), Some(Resource::FileSystem(dev)));
        assert_eq!(of(&dir.join("not/there/yet.txt")), Some(Resource::FileSystem(dev)));
        // A second file system, where there is one: told apart by the number, not by the name
        if let Ok(shm) = std::fs::metadata("/dev/shm")
            && shm.dev() != dev
        {
            assert_eq!(of(Path::new("/dev/shm/x")), Some(Resource::FileSystem(shm.dev())));
            assert_eq!(of_all(&[dir.clone(), dir.join("a"), PathBuf::from("/dev/shm/x")]), vec![Resource::FileSystem(dev), Resource::FileSystem(shm.dev())]);
        }
        // A link into another file system is followed
        if std::fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != dev) {
            std::os::unix::fs::symlink("/dev/shm", dir.join("link")).unwrap();
            assert_eq!(of(&dir.join("link/x")), of(Path::new("/dev/shm/x")));
        }
        // A server folder is told by its mount, and never asked
        let server = dir.join("server");
        std::fs::create_dir_all(server.join("deep")).unwrap();
        crate::fs::network::declare_remote_for_tests(&server);
        assert_eq!(of(&server.join("deep/file")), Some(Resource::Server(server.clone())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_held_drive_covers_every_file_system_on_it_and_nothing_else() {
        use std::os::unix::fs::MetadataExt;
        let (Ok(shm), Ok(proc)) = (std::fs::metadata("/dev/shm"), std::fs::metadata("/proc")) else { return };
        // /dev/shm stands in for a partition of the drive "compression-test-res"; /proc is on no drive
        crate::fs::drives::declare_file_system_drive_for_tests(shm.dev(), "compression-test-res");
        let drive = [Resource::Drive("compression-test-res".into())];
        assert!(overlaps(&drive, &[Resource::FileSystem(shm.dev())]));
        assert!(!overlaps(&drive, &[Resource::FileSystem(proc.dev())]));
        assert!(!overlaps(&[Resource::Drive("another".into())], &[Resource::FileSystem(shm.dev())]));
        // The same resource is the same, held as itself
        assert!(overlaps(&[Resource::FileSystem(proc.dev())], &[Resource::FileSystem(proc.dev())]));
        assert!(overlaps(&[Resource::Server("/m".into())], &[Resource::Server("/m".into())]));
        assert!(!overlaps(&[Resource::Server("/m".into())], &[Resource::Server("/n".into())]));
    }
}
