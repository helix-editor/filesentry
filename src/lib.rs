use std::io;
use std::path::Path;
#[cfg(not(watcher_disable))]
use std::sync::atomic::{self, AtomicBool};
#[cfg(not(watcher_disable))]
use std::sync::{Arc, Mutex, Weak};
#[cfg(not(watcher_disable))]
use std::time::Duration;

#[cfg(not(watcher_disable))]
use crate::backend::{Backend, NativeBackend};
#[cfg(not(watcher_disable))]
use crate::config::Config;
#[cfg(not(watcher_disable))]
use crate::events::EventDebouncer;
pub use crate::events::{Event, EventType, Events};
pub use crate::path::{CannonicalPath, CanonicalPathBuf};
pub use config::Filter;

mod config;
mod events;
mod metadata;
mod path;

// These modules require the full Watcher implementation
#[cfg(not(watcher_disable))]
mod backend;
#[cfg(all(not(watcher_disable), target_os = "macos"))]
mod fsevent;
#[cfg(all(not(watcher_disable), target_os = "linux"))]
mod inotify;
#[cfg(not(watcher_disable))]
mod pending;
#[cfg(not(watcher_disable))]
mod tree;
#[cfg(all(not(watcher_disable), target_os = "windows"))]
mod windows;
#[cfg(not(watcher_disable))]
mod worker;

#[cfg(all(test, not(watcher_disable)))]
mod tests;

#[cfg(not(watcher_disable))]
struct AddRoot {
    path: CanonicalPathBuf,
    recursive: bool,
    notify: Box<dyn FnOnce(bool) + Send>,
}

#[cfg(not(watcher_disable))]
impl std::fmt::Debug for AddRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddRoot").field("path", &self.path).finish()
    }
}

#[cfg(not(watcher_disable))]
#[derive(Debug)]
struct WatcherState {
    config: Mutex<Config>,
    /// roots for the worker to add
    new_roots: Mutex<Vec<AddRoot>>,
    has_new_roots: AtomicBool,
    started: AtomicBool,
}

#[cfg(not(watcher_disable))]
pub struct ShutdownOnDrop {
    watcher: Weak<NativeBackend>,
}

#[cfg(not(watcher_disable))]
impl ShutdownOnDrop {
    pub fn cancel(&mut self) {
        self.watcher = Weak::new()
    }
}

#[cfg(not(watcher_disable))]
impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        if let Some(watcher) = self.watcher.upgrade() {
            watcher.shutdown();
        }
    }
}

/// Stub ShutdownOnDrop when watcher is disabled
#[cfg(watcher_disable)]
pub struct ShutdownOnDrop;

#[cfg(watcher_disable)]
impl ShutdownOnDrop {
    pub fn cancel(&mut self) {}
}

#[cfg(not(watcher_disable))]
#[derive(Debug, Clone)]
pub struct Watcher {
    state: Arc<WatcherState>,
    notify: Arc<NativeBackend>,
}

#[cfg(not(watcher_disable))]
impl Watcher {
    pub fn shutdown(&self) {
        self.notify.shutdown();
    }

    pub fn shutdown_guard(&self) -> ShutdownOnDrop {
        ShutdownOnDrop {
            watcher: Arc::downgrade(&self.notify),
        }
    }

    pub fn add_root(
        &self,
        root: &Path,
        recursive: bool,
        root_crawled: impl FnOnce(bool) + 'static + Send,
    ) -> io::Result<()> {
        let root = root.canonicalize()?;
        if self
            .state
            .config
            .lock()
            .unwrap()
            .filter
            .ignore_path_rec(&root, None)
        {
            log::warn!("ignoring root {root:?} as it matches the ignore pattern");
            root_crawled(false);
            return Ok(());
        }
        self.state.new_roots.lock().unwrap().push(AddRoot {
            path: CanonicalPathBuf::assert_canonicalized(&root),
            recursive,
            notify: Box::new(root_crawled),
        });
        self.state
            .has_new_roots
            .store(true, atomic::Ordering::Relaxed);
        self.notify.changes().notify();
        Ok(())
    }

    pub fn set_filter(&self, filter: Arc<dyn Filter>, recrawl: bool) {
        self.state.config.lock().unwrap().filter = filter;
        self.notify.refresh_config();
        if recrawl {
            self.notify.changes().lock().recrawl();
            self.notify.changes().notify();
        }
    }

    pub fn set_settle_time(&self, settle_time: Duration) {
        self.state.config.lock().unwrap().settle_time = settle_time;
    }

    pub fn add_handler(&self, handler: impl FnMut(Events) -> bool + Send + 'static) {
        self.state
            .config
            .lock()
            .unwrap()
            .handlers
            .push(Box::new(handler));
    }

    pub fn new() -> io::Result<Self> {
        let state = Arc::new(WatcherState {
            config: Mutex::new(Config {
                filter: Arc::new(()),
                settle_time: Duration::from_millis(200),
                handlers: Vec::new(),
            }),
            new_roots: Mutex::new(Vec::new()),
            has_new_roots: AtomicBool::new(false),
            started: AtomicBool::new(false),
        });
        let watcher = NativeBackend::new(state.clone())?;

        Ok(Self {
            state,
            notify: watcher,
        })
    }

    /// Starts the worker thread; later calls do nothing.
    pub fn start(&self) {
        if self.state.started.swap(true, atomic::Ordering::Relaxed) {
            return;
        }
        let watcher = self.clone();
        std::thread::spawn(move || {
            let worker = worker::Worker::new(watcher);
            worker.run();
        });
    }
}

/// Stub Watcher when file watching is disabled.
/// File watching is not supported on this platform or has been disabled.
#[cfg(watcher_disable)]
#[derive(Debug, Clone)]
pub struct Watcher;

#[cfg(watcher_disable)]
impl Watcher {
    /// Creates a new Watcher. Returns an error when file watching is disabled.
    pub fn new() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file watching is disabled or not supported on this platform",
        ))
    }

    pub fn shutdown(&self) {}

    pub fn shutdown_guard(&self) -> ShutdownOnDrop {
        ShutdownOnDrop
    }

    pub fn add_root(
        &self,
        _root: &Path,
        _recursive: bool,
        _root_crawled: impl FnOnce(bool) + 'static + Send,
    ) -> io::Result<()> {
        Ok(())
    }

    pub fn set_filter(&self, _filter: std::sync::Arc<dyn Filter>, _recrawl: bool) {}

    pub fn set_settle_time(&self, _settle_time: std::time::Duration) {}

    pub fn add_handler(&self, _handler: impl FnMut(Events) -> bool + Send + 'static) {}

    pub fn start(&self) {}
}
