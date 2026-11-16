pub mod atomic;
pub mod process;
pub mod file_item;
pub mod places;
pub mod scanner;
pub mod search;
pub mod props;
pub mod recent;
pub mod apps;
pub mod describe;
pub mod ops;
pub mod trash;
pub mod batch_rename;
pub mod engine;
pub mod history;
pub mod terminal;
pub mod admin;
pub mod glob;
pub mod templates;
pub mod drives;
pub mod xattr;
pub mod watch;
pub mod dirsize;
pub mod archive_list;
pub mod complete;
pub mod network;
pub mod mount_health;
pub mod secrets;
pub mod resources;
pub mod stored;

/// Starts `work` on a thread of its own. When no thread can be started, that is an error, and `work` (with all it
/// holds) is dropped right away.
pub(crate) fn spawn(work: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_SPAWN.with(std::cell::Cell::get) {
        drop(work);
        return Err(std::io::Error::from_raw_os_error(libc::EAGAIN));
    }
    std::thread::Builder::new().spawn(work).map(drop)
}

#[cfg(test)]
thread_local! {
    /// On this thread, `spawn` fails as if no thread could be started (EAGAIN).
    pub(crate) static FAIL_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
