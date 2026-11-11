//! The native watcher interface. A backend pushes raw change notifications into
//! its [`PendingChangesLock`] and signals it; the worker drains it, re-stats the
//! paths and diffs them against the tree.

use std::io;

use crate::path::CanonicalPathBuf;
use crate::pending::PendingChangesLock;

pub(crate) trait Backend: Send + Sync + 'static {
    /// The queue the worker drains; the backend pushes into it and signals its
    /// condvar (`changes().notify()`) when new notifications arrive.
    fn changes(&self) -> &PendingChangesLock;

    /// Start watching a directory. `recursive` asks the backend to cover the whole
    /// subtree: subtree backends (`ReadDirectoryChangesW`) honor it; per-directory
    /// backends (inotify) ignore it and rely on the worker calling `watch_dir` per
    /// directory. Recursive backends no-op a path already covered by an ancestor.
    fn watch_dir(&self, path: CanonicalPathBuf, recursive: bool) -> io::Result<()>;

    /// Request shutdown of the backend's worker thread(s) and wake the worker.
    fn shutdown(&self);
    fn is_shutdown(&self) -> bool;

    /// Re-read live configuration (e.g. the filter); wake the event loop.
    fn refresh_config(&self);
}

// Other targets compile the watcher out (see build.rs).
#[cfg(target_os = "linux")]
pub(crate) type NativeBackend = crate::inotify::InotifyWatcher;
#[cfg(target_os = "macos")]
pub(crate) type NativeBackend = crate::fsevent::FsEventWatcher;
#[cfg(target_os = "windows")]
pub(crate) type NativeBackend = crate::windows::WindowsWatcher;
