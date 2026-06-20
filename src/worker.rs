use std::mem::take;
use std::sync::atomic;

use crate::backend::Backend;
use crate::pending::PendingChanges;
use crate::tree::{FileTree, NodeId};
use crate::{EventDebouncer, Watcher};

pub struct Worker {
    pending_changes: PendingChanges,
    events: EventDebouncer,
    work_stack: Vec<(NodeId, usize)>,
    tree: FileTree,
    roots: Vec<(NodeId, bool)>,
    watcher: Watcher,
}

impl Watcher {
    fn should_wakeup(&self) -> bool {
        self.state.has_notifications.load(atomic::Ordering::Relaxed) || self.notify.is_shutdown()
    }
}

impl Worker {
    pub fn new(watcher: Watcher) -> Self {
        Worker {
            pending_changes: PendingChanges::default(),
            events: EventDebouncer::new(),
            work_stack: Vec::with_capacity(16),
            tree: FileTree::new(),
            roots: Vec::with_capacity(16),
            watcher,
        }
    }

    fn wait_for_changes(&mut self) -> bool {
        if self.events.is_empty() {
            self.watcher
                .notify
                .changes()
                .take(&mut self.pending_changes, || self.watcher.should_wakeup());
            false
        } else {
            self.watcher.notify.changes().take_timeout(
                &mut self.pending_changes,
                self.watcher.state.config.lock().unwrap().settle_time,
                || self.watcher.should_wakeup(),
            )
        }
    }

    fn process_notifications(&mut self) {
        let has_notifications = self
            .watcher
            .state
            .has_notifications
            .swap(false, atomic::Ordering::Relaxed);
        if has_notifications {
            let notifications = take(&mut *self.watcher.state.notifications.lock().unwrap());
            for root in notifications.roots {
                let Some(node) = self.tree.add_root(root.path.clone(), root.recursive) else {
                    (root.notify)(true);
                    continue;
                };
                if let Err(err) = self
                    .watcher
                    .notify
                    .watch_dir(root.path.clone(), root.recursive)
                {
                    log::error!("failed to watch {:?}: {err}", root.path);
                    (root.notify)(false);
                    continue;
                }
                let filter = self.watcher.state.config.lock().unwrap().filter.clone();
                self.tree
                    .crawl_root(node, root.recursive, &*filter, |path| {
                        if let Err(err) = self.watcher.notify.watch_dir(path.clone(), true) {
                            log::error!("failed to watch {path:?}: {err}")
                        }
                    });
                let i = self
                    .roots
                    .partition_point(|&(it, _)| self.tree[it].path < root.path);
                if root.recursive {
                    // for recursive roots remove any roots that are children
                    // and not ignored to avoid duplicate crawls
                    let mut end = i + self.roots[i..]
                        .iter()
                        .position(|&(it, _)| !root.path.is_parent_of(&self.tree[it].path))
                        .unwrap_or(self.roots.len() - i);
                    let mut j = i;
                    while j < end {
                        if filter.ignore_path_rec(
                            self.tree[self.roots[j].0].path.as_std_path(),
                            Some(true),
                        ) {
                            j += 1;
                        } else {
                            self.roots.remove(j);
                            end -= 1;
                        }
                    }
                };
                self.roots.insert(i, (node, root.recursive));
                (root.notify)(true);
            }
        }
    }

    pub fn run(mut self) {
        loop {
            let settled = self.wait_for_changes();
            if self.watcher.notify.is_shutdown() {
                break;
            }
            self.process_notifications();
            if settled {
                let events = self.events.take();
                self.watcher
                    .state
                    .config
                    .lock()
                    .unwrap()
                    .handlers
                    .retain_mut(|handler| handler(events.clone()));
                continue;
            }
            let filter = self.watcher.state.config.lock().unwrap().filter.clone();
            if self.pending_changes.take_recrawl() {
                #[cfg(test)]
                self.watcher
                    .state
                    .recrawls
                    .fetch_add(1, atomic::Ordering::Relaxed);

                for &(root, _) in &self.roots {
                    self.tree.crawl(
                        root,
                        &*filter,
                        &mut self.work_stack,
                        |path, ty| self.events.add(path, ty),
                        |path| {
                            if let Err(err) = self.watcher.notify.watch_dir(path.clone(), true) {
                                log::error!("failed to watch {path:?}: {err}")
                            }
                        },
                    );
                }
                continue;
            }
            self.tree.apply_transaction(
                &mut self.pending_changes,
                &*filter,
                |path, ty| self.events.add(path, ty),
                &mut self.work_stack,
                |path| {
                    if let Err(err) = self.watcher.notify.watch_dir(path.clone(), true) {
                        log::error!("failed to watch {path:?}: {err}")
                    }
                },
            );
        }
    }
}
