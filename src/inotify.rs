use std::sync::atomic::{self, AtomicBool};
use std::sync::Arc;
use std::{io, thread};

mod sys;

use hashbrown::DefaultHashBuilder;
use mio::{Poll, Waker};
use papaya::HashMap;

use crate::backend::Backend;
use crate::inotify::sys::{Event, EventFlags, Inotify, Watch};
use crate::path::CanonicalPathBuf;
use crate::pending::PendingChangesLock;
use crate::{Filter, WatcherState};

#[derive(Debug)]
pub(crate) struct InotifyWatcher {
    waker: Waker,
    shutdown: AtomicBool,
    notify: Inotify,
    watches: HashMap<Watch, CanonicalPathBuf, DefaultHashBuilder>,
    changes: PendingChangesLock,
}

impl InotifyWatcher {
    pub fn new(state: Arc<WatcherState>) -> io::Result<Arc<Self>> {
        let mut poll = Poll::new()?;
        let waker = Waker::new(poll.registry(), sys::MESSAGE)?;
        let watcher = Arc::new(Self {
            waker,
            notify: Inotify::new()?,
            watches: HashMap::with_capacity_and_hasher(1024, DefaultHashBuilder::default()),
            changes: PendingChangesLock::default(),
            shutdown: AtomicBool::new(false),
        });
        let mut filter = state.config.lock().unwrap().filter.clone();

        let watcher_ = watcher.clone();
        thread::spawn(move || {
            watcher_.notify.event_loop(
                &mut poll,
                &mut filter,
                |filter, event| watcher_.handle_event(event, &**filter),
                |_| watcher_.changes.notify(),
                |filter| {
                    *filter = state.config.lock().unwrap().filter.clone();
                    watcher_.is_shutdown()
                },
            )
        });
        Ok(watcher)
    }

    fn handle_event(&self, event: Event, filter: &dyn Filter) {
        if event.flags.contains(EventFlags::QUEUE_OVERFLOW) {
            self.changes.lock().recrawl();
            return;
        }
        let watches = self.watches.pin();
        let Some(dir) = watches.get(&event.wd) else {
            // lost track of a watch: resync everything
            if event.wd.is_invalid()
                || event
                    .flags
                    .intersects(EventFlags::MOVE_SELF | EventFlags::IGNORED)
            {
                self.changes.lock().recrawl();
            }
            return;
        };
        let about_dir = EventFlags::IGNORED
            | EventFlags::MOVE_SELF
            | EventFlags::DELETE_SELF
            | EventFlags::UNMOUNT;
        if event.child.is_empty() || event.flags.intersects(about_dir) {
            if event.flags.contains(EventFlags::IGNORED) {
                watches.remove(&event.wd);
            } else if event.flags.contains(EventFlags::MOVE_SELF) {
                // A moved directory keeps its watch, which would go on reporting
                // under the old path. Remove it; the kernel's IGNORED for it then
                // drops the mapping above (forgetting it here instead would turn
                // that IGNORED into an unknown-watch recrawl).
                let _ = self.notify.remove_watch(&event.wd);
            }
            self.changes.lock().add(dir.clone(), true);
            return;
        }
        let path = dir.join(event.child);
        let is_dir = event.flags.contains(EventFlags::ISDIR);
        if !filter.ignore_path(path.as_std_path(), Some(is_dir)) {
            let crawl = event
                .flags
                .intersects(EventFlags::CREATE | EventFlags::DELETE);
            self.changes.lock().add(path, crawl);
        }
    }
}

impl Backend for InotifyWatcher {
    fn changes(&self) -> &PendingChangesLock {
        &self.changes
    }

    fn watch_dir(&self, path: CanonicalPathBuf, _recursive: bool) -> io::Result<()> {
        let watch = self.notify.add_directory_watch(&*path)?;
        self.watches.pin().insert(watch, path);
        Ok(())
    }

    fn shutdown(&self) {
        self.shutdown.store(true, atomic::Ordering::Relaxed);
        let _ = self.waker.wake();
        self.changes.notify();
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(atomic::Ordering::Relaxed)
    }

    fn refresh_config(&self) {
        let _ = self.waker.wake();
    }
}
