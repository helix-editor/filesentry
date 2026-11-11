use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{Arc, Mutex};
use std::{io, thread};

mod sys;

use rustix::event::{eventfd, poll, EventfdFlags, PollFd, PollFlags};
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::inotify::Reader;
use rustix::io::Errno;

use crate::backend::Backend;
use crate::inotify::sys::{EventFlags, Inotify};
use crate::path::CanonicalPathBuf;
use crate::pending::PendingChangesLock;
use crate::{Filter, WatcherState};

/// Room for the kernel's default queue of 16384 events at their maximum size (a
/// 16 byte header plus a name of up to `NAME_MAX + 1` bytes), so one read drains it.
const BUFFER_SIZE: usize = 16384 * (16 + 256);

#[derive(Debug, Clone)]
struct Watched {
    path: CanonicalPathBuf,
    /// (device, inode) of the directory when the watch was added.
    id: Option<(u64, u64)>,
}

impl Watched {
    /// Whether `path` still names the directory this watch is on.
    fn resolves(&self) -> bool {
        self.id.is_some() && dir_id(&self.path) == self.id
    }
}

fn dir_id(path: &CanonicalPathBuf) -> Option<(u64, u64)> {
    let meta = std::fs::symlink_metadata(path.as_std_path()).ok()?;
    Some((meta.dev(), meta.ino()))
}

#[derive(Debug)]
pub(crate) struct InotifyWatcher {
    wake_fd: OwnedFd,
    shutdown: AtomicBool,
    notify: Inotify,
    /// Never held across a syscall or while taking `changes`.
    watches: Mutex<HashMap<i32, Watched>>,
    changes: PendingChangesLock,
}

impl InotifyWatcher {
    pub fn new(state: Arc<WatcherState>) -> io::Result<Arc<Self>> {
        let watcher = Arc::new(Self {
            wake_fd: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?,
            notify: Inotify::new()?,
            watches: Mutex::new(HashMap::with_capacity(1024)),
            changes: PendingChangesLock::default(),
            shutdown: AtomicBool::new(false),
        });
        let watcher_ = watcher.clone();
        thread::spawn(move || {
            let result = watcher_.event_loop(&state);
            if let Err(err) = result {
                log::error!("inotify event loop failed: {err}");
                watcher_.changes.lock().recrawl();
                watcher_.changes.notify();
            }
        });
        Ok(watcher)
    }

    fn event_loop(&self, state: &WatcherState) -> io::Result<()> {
        let mut filter = state.config.lock().unwrap().filter.clone();
        let mut buf = Box::new_uninit_slice(BUFFER_SIZE);
        let mut reader = Reader::new(self.notify.as_fd(), &mut buf);
        loop {
            let mut fds = [
                PollFd::new(&self.notify, PollFlags::IN),
                PollFd::new(&self.wake_fd, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(err) => return Err(err.into()),
            }
            let readable = !fds[0].revents().is_empty();
            if !fds[1].revents().is_empty() {
                // Drain the counter so the level-triggered poll blocks again.
                let _ = rustix::io::read(&self.wake_fd, &mut [0u8; 8]);
                filter = state.config.lock().unwrap().filter.clone();
                if self.is_shutdown() {
                    return Ok(());
                }
            }
            if !readable {
                continue;
            }
            loop {
                let event = match reader.next() {
                    Err(Errno::AGAIN | Errno::INVAL) => break,
                    Ok(event) => event,
                    Err(err) => return Err(err.into()),
                };
                let child = event
                    .file_name()
                    .map_or(OsStr::new(""), |name| OsStr::from_bytes(name.to_bytes()));
                self.handle_event(event.wd(), child, event.events(), &*filter);
            }
            self.changes.notify();
        }
    }

    fn wake(&self) {
        // A full counter (EAGAIN) means the loop is already due to wake.
        let _ = rustix::io::write(&self.wake_fd, &1u64.to_ne_bytes());
    }

    /// Removes the watches of a directory that moved away from `dir` and of every
    /// directory mapped below it, which get no `IN_MOVE_SELF` of their own.
    ///
    /// Watches whose directory is still at its mapped path are kept, because the
    /// crawl has already re-added them under the new name. The mappings stay until
    /// the kernel's `IN_IGNORED` drops them, since forgetting them here would make
    /// that event trigger a full recrawl for an unknown watch.
    fn remove_moved_watches(&self, dir: &CanonicalPathBuf) {
        let candidates: Vec<(i32, Watched)> = {
            let watches = self.watches.lock().unwrap();
            watches
                .iter()
                .filter(|(_, w)| w.path == *dir || dir.is_parent_of(&w.path))
                .map(|(&wd, w)| {
                    (
                        wd,
                        Watched {
                            path: w.path.clone(),
                            id: w.id,
                        },
                    )
                })
                .collect()
        };
        for (wd, watched) in candidates {
            if !watched.resolves() {
                let _ = self.notify.remove_watch(wd);
            }
        }
    }

    fn handle_event(&self, wd: i32, child: &OsStr, flags: EventFlags, filter: &dyn Filter) {
        if flags.contains(EventFlags::QUEUE_OVERFLOW) {
            self.changes.lock().recrawl();
            return;
        }
        let dir = self
            .watches
            .lock()
            .unwrap()
            .get(&wd)
            .map(|w| w.path.clone());
        let Some(dir) = dir else {
            // lost track of a watch: resync everything
            if wd < 0 || flags.intersects(EventFlags::MOVE_SELF | EventFlags::IGNORED) {
                self.changes.lock().recrawl();
            }
            return;
        };
        let about_dir = EventFlags::IGNORED
            | EventFlags::MOVE_SELF
            | EventFlags::DELETE_SELF
            | EventFlags::UNMOUNT;
        if child.is_empty() || flags.intersects(about_dir) {
            if flags.contains(EventFlags::IGNORED) {
                self.watches.lock().unwrap().remove(&wd);
            } else if flags.contains(EventFlags::MOVE_SELF) {
                self.remove_moved_watches(&dir);
            }
            self.changes.lock().add(dir, true);
            return;
        }
        let path = dir.join(child);
        let is_dir = flags.contains(EventFlags::ISDIR);
        if !filter.ignore_path(path.as_std_path(), Some(is_dir)) {
            let crawl = flags.intersects(EventFlags::CREATE | EventFlags::DELETE);
            self.changes.lock().add(path, crawl);
        }
    }
}

impl Backend for InotifyWatcher {
    fn changes(&self) -> &PendingChangesLock {
        &self.changes
    }

    fn watch_dir(&self, path: CanonicalPathBuf, _recursive: bool) -> io::Result<()> {
        let wd = self.notify.add_directory_watch(&*path)?;
        let id = dir_id(&path);
        self.watches
            .lock()
            .unwrap()
            .insert(wd, Watched { path, id });
        Ok(())
    }

    fn shutdown(&self) {
        self.shutdown.store(true, atomic::Ordering::Relaxed);
        self.wake();
        self.changes.notify();
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(atomic::Ordering::Relaxed)
    }

    fn refresh_config(&self) {
        self.wake();
    }
}

#[cfg(test)]
mod tests;
