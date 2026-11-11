use std::mem::{swap, take};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::path::CanonicalPathBuf;

#[derive(Debug, Default)]
pub struct PendingChangesLock {
    inner: Mutex<PendingChanges>,
    condvar: Condvar,
}

impl PendingChangesLock {
    /// Waits until there are changes or `exit` returns true, then swaps the
    /// queue into `dst`. Returns whether `timeout` (if any) elapsed first, in
    /// which case nothing is taken.
    pub fn take(
        &self,
        dst: &mut PendingChanges,
        timeout: Option<Duration>,
        exit: impl Fn() -> bool,
    ) -> bool {
        let mut guard = self.inner.lock().unwrap();
        let ready = |changes: &mut PendingChanges| !changes.is_empty() || exit();
        match timeout {
            None => guard = self.condvar.wait_while(guard, |c| !ready(c)).unwrap(),
            Some(timeout) => {
                let (g, res) = self
                    .condvar
                    .wait_timeout_while(guard, timeout, |c| !ready(c))
                    .unwrap();
                if res.timed_out() {
                    return true;
                }
                guard = g;
            }
        }
        swap(&mut *guard, dst);
        false
    }

    pub fn lock(&self) -> MutexGuard<'_, PendingChanges> {
        self.inner.lock().unwrap()
    }

    /// Wakes the waiter. It takes the mutex first, so that a waiter that has checked
    /// its predicate but not started waiting yet doesn't miss the wakeup. Must not
    /// be called while holding the guard from `lock()`.
    pub fn notify(&self) {
        drop(self.inner.lock().unwrap());
        self.condvar.notify_all();
    }
}

/// A path a backend reported as changed.
#[derive(Clone, Debug)]
pub struct PendingChange {
    pub path: CanonicalPathBuf,
    /// Set when the path was created or deleted, or is a directory whose contents
    /// changed in unknown ways. A directory with this set is crawled.
    pub crawl: bool,
}

#[derive(Debug, Default, Clone)]
pub struct PendingChanges {
    changes: Vec<PendingChange>,
    recrawl: bool,
}

impl PendingChanges {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() & !self.recrawl
    }

    pub fn recrawl(&mut self) {
        self.changes.clear();
        self.recrawl = true;
    }

    pub fn add(&mut self, path: CanonicalPathBuf, crawl: bool) {
        if !self.recrawl {
            self.changes.push(PendingChange { path, crawl });
        }
    }

    pub fn take_recrawl(&mut self) -> bool {
        take(&mut self.recrawl)
    }

    /// The changes in path order, one per path.
    pub fn drain(&mut self) -> impl Iterator<Item = PendingChange> + '_ {
        self.changes.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        self.changes.dedup_by(|cur, prev| {
            let same = cur.path == prev.path;
            prev.crawl |= same & cur.crawl;
            same
        });
        self.changes.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use super::{PendingChanges, PendingChangesLock};
    use crate::path::CanonicalPathBuf;

    #[test]
    fn drain_merges_equal_paths() {
        let path = |p: &str| CanonicalPathBuf::assert_canonicalized(std::path::Path::new(p));
        let mut pending = PendingChanges::default();
        pending.add(path("/b"), false);
        pending.add(path("/a"), false);
        pending.add(path("/b"), true);
        pending.add(path("/b"), false);
        let drained: Vec<_> = pending.drain().map(|c| (c.path, c.crawl)).collect();
        assert_eq!(drained, [(path("/a"), false), (path("/b"), true)]);
        assert!(pending.is_empty());
    }

    /// `notify` must synchronize with the mutex, or it can land between a
    /// waiter's predicate check and its wait.
    #[test]
    fn notify_waits_for_the_mutex() {
        let lock = Arc::new(PendingChangesLock::default());
        let done = Arc::new(AtomicBool::new(false));
        let guard = lock.lock();
        let handle = {
            let (lock, done) = (lock.clone(), done.clone());
            thread::spawn(move || {
                lock.notify();
                done.store(true, Ordering::SeqCst);
            })
        };
        thread::sleep(Duration::from_millis(200));
        assert!(
            !done.load(Ordering::SeqCst),
            "notify returned while the queue was locked"
        );
        drop(guard);
        handle.join().unwrap();
    }
}
