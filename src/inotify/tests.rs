use std::os::fd::AsRawFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{fs, thread};

use tempfile::TempDir;

use super::sys::EventFlags;
use super::InotifyWatcher;
use crate::path::CanonicalPathBuf;
use crate::Watcher;

impl InotifyWatcher {
    /// Number of watches this watcher's inotify instance holds in the kernel.
    fn kernel_watch_count(&self) -> usize {
        let fd = rustix::fd::AsFd::as_fd(&self.notify).as_raw_fd();
        fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("inotify wd:"))
            .count()
    }
}

fn wait_for_watch_count(watcher: &Watcher, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while watcher.notify.kernel_watch_count() != expected && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(watcher.notify.kernel_watch_count(), expected);
}

/// Moving a directory out of the root must not leave watches on its subdirectories,
/// which count against `max_user_watches` and keep reporting.
#[test]
fn moved_out_dir_drops_its_watches() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let watcher = Watcher::new().unwrap();
    let _guard = watcher.shutdown_guard();
    let (tx, rx) = mpsc::sync_channel(1);
    let root = dir.path().canonicalize().unwrap();
    watcher
        .add_root(&root, true, move |ok| {
            let _ = tx.send(ok);
        })
        .unwrap();
    watcher.start();
    assert!(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(watcher.notify.kernel_watch_count(), 1);

    fs::create_dir_all(root.join("d/a/b/c")).unwrap();
    wait_for_watch_count(&watcher, 5);

    fs::rename(root.join("d"), outside.path().join("d")).unwrap();
    wait_for_watch_count(&watcher, 1);
}

#[test]
fn queue_overflow_recrawls() {
    let watcher = Watcher::new().unwrap();
    let _guard = watcher.shutdown_guard();
    let flags = EventFlags::QUEUE_OVERFLOW;
    watcher.notify.handle_event(-1, "".as_ref(), flags, &());
    assert!(watcher.notify.changes.lock().take_recrawl());
}

/// Removing the watch of a directory moved out produces an `IN_IGNORED`, which
/// must not look like a lost watch that needs a full recrawl.
#[test]
fn move_out_needs_no_recrawl() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir(root.join("d")).unwrap();
    let watcher = Watcher::new().unwrap();
    let _guard = watcher.shutdown_guard();
    for path in [&root, &root.join("d")] {
        let path = CanonicalPathBuf::assert_canonicalized(path);
        crate::backend::Backend::watch_dir(&*watcher.notify, path, true).unwrap();
    }
    fs::rename(root.join("d"), outside.path().join("d")).unwrap();
    wait_for_watch_count(&watcher, 1);
    // queued after the `IN_IGNORED`, so handled after it too
    let sentinel = root.join("sentinel");
    fs::write(&sentinel, "x").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut seen, mut recrawl) = (false, false);
    while !seen && Instant::now() < deadline {
        let mut changes = watcher.notify.changes.lock();
        recrawl |= changes.take_recrawl();
        seen = changes.drain().any(|change| change.path == sentinel);
        drop(changes);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(seen && !recrawl, "seen {seen}, recrawl {recrawl}");
}
