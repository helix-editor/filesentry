use std::mem::take;
use std::sync::atomic;

use crate::backend::{Backend, NativeBackend};
use crate::path::CanonicalPathBuf;
use crate::pending::PendingChanges;
use crate::tree::FileTree;
use crate::{EventDebouncer, Watcher};

pub struct Worker {
    pending_changes: PendingChanges,
    events: EventDebouncer,
    tree: FileTree,
    watcher: Watcher,
}

impl Watcher {
    fn should_wakeup(&self) -> bool {
        self.state.has_new_roots.load(atomic::Ordering::Relaxed) || self.notify.is_shutdown()
    }
}

fn watch_dir(backend: &NativeBackend, path: CanonicalPathBuf) {
    if let Err(err) = backend.watch_dir(path.clone(), true) {
        log::error!("failed to watch {path:?}: {err}")
    }
}

impl Worker {
    pub fn new(watcher: Watcher) -> Self {
        Worker {
            pending_changes: PendingChanges::default(),
            events: EventDebouncer::new(),
            tree: FileTree::new(),
            watcher,
        }
    }

    /// Returns whether the settle time passed without new changes.
    fn wait_for_changes(&mut self) -> bool {
        let timeout = (!self.events.is_empty())
            .then(|| self.watcher.state.config.lock().unwrap().settle_time);
        self.watcher
            .notify
            .changes()
            .take(&mut self.pending_changes, timeout, || {
                self.watcher.should_wakeup()
            })
    }

    fn add_new_roots(&mut self) {
        let state = &self.watcher.state;
        if !state.has_new_roots.swap(false, atomic::Ordering::Relaxed) {
            return;
        }
        let new_roots = take(&mut *state.new_roots.lock().unwrap());
        let filter = state.config.lock().unwrap().filter.clone();
        let backend = &self.watcher.notify;
        for root in new_roots {
            // Watch before the tree records the root, so that a failed watch doesn't
            // leave a root that a retry would report as already watched.
            if let Err(err) = backend.watch_dir(root.path.clone(), root.recursive) {
                log::error!("failed to watch {:?}: {err}", root.path);
                (root.notify)(false);
                continue;
            }
            self.tree.track_root(
                root.path,
                root.recursive,
                &*filter,
                |path, ty| self.events.add(path, ty),
                |path| watch_dir(backend, path),
            );
            (root.notify)(true);
        }
    }

    pub fn run(mut self) {
        loop {
            let settled = self.wait_for_changes();
            if self.watcher.notify.is_shutdown() {
                break;
            }
            self.add_new_roots();
            if settled {
                let events = self.events.take();
                // Handlers run unlocked so they can call back into the `Watcher`.
                let config = &self.watcher.state.config;
                let mut handlers = take(&mut config.lock().unwrap().handlers);
                handlers.retain_mut(|handler| handler(events.clone()));
                let mut config = config.lock().unwrap();
                handlers.append(&mut config.handlers);
                config.handlers = handlers;
                continue;
            }
            let filter = self.watcher.state.config.lock().unwrap().filter.clone();
            let backend = &self.watcher.notify;
            let mut emit = |path, ty| self.events.add(path, ty);
            let mut add_watch = |path| watch_dir(backend, path);
            if self.pending_changes.take_recrawl() {
                self.tree.recrawl(&*filter, &mut emit, &mut add_watch);
            } else {
                self.tree.apply_transaction(
                    &mut self.pending_changes,
                    &*filter,
                    &mut emit,
                    &mut add_watch,
                );
            }
        }
    }
}
