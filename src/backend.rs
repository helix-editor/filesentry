//! The native watcher interface. A backend pushes raw change notifications into
//! its [`PendingChangesLock`] and signals it; the worker drains it, re-stats the
//! paths and diffs them against the tree.

use std::io;

use crate::path::CanonicalPathBuf;
use crate::pending::PendingChangesLock;

pub(crate) trait Backend: Send + Sync + 'static {
    /// The queue the worker drains. The backend pushes into it and calls
    /// `changes().notify()` when new notifications arrive.
    fn changes(&self) -> &PendingChangesLock;

    /// Start watching a directory. `recursive` asks the backend to cover the whole
    /// subtree. Backends that watch subtrees (`ReadDirectoryChangesW`) honor it,
    /// while per-directory backends (inotify) ignore it and rely on the worker
    /// calling `watch_dir` for each directory. Recursive backends do nothing for a
    /// path an ancestor already covers.
    fn watch_dir(&self, path: CanonicalPathBuf, recursive: bool) -> io::Result<()>;

    /// Request shutdown of the backend's worker thread(s) and wake the worker.
    fn shutdown(&self);
    fn is_shutdown(&self) -> bool;

    /// Re-reads the configuration, such as the filter, and wakes the event loop.
    fn refresh_config(&self);
}

// Other targets compile the watcher out (see build.rs).
#[cfg(target_os = "linux")]
pub(crate) type NativeBackend = crate::inotify::InotifyWatcher;
#[cfg(target_os = "macos")]
pub(crate) type NativeBackend = crate::fsevent::FsEventWatcher;
#[cfg(target_os = "windows")]
pub(crate) type NativeBackend = crate::windows::WindowsWatcher;
