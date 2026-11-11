// use pretty_assertions::assert_eq;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, LazyLock, Mutex};
use std::time::Duration;

use tempfile::TempDir;

use crate::events::{EventDebouncer, EventType};
use crate::path::CanonicalPathBuf;
use crate::Watcher;

static TIMEOUT: LazyLock<Duration> =
    LazyLock::new(|| match std::env::var("FILESENTRY_TEST_TIMEOUT") {
        Ok(res) if !res.trim().is_empty() => Duration::from_secs(
            res.parse()
                .expect("invalid value for FILESENTRY_TEST_TIMEOUT expected an integer"),
        ),
        _ => Duration::from_secs(20),
    });

/// Asserts the net effect of a change rather than the raw event stream, which a
/// loaded runner may deliver in several batches.
struct Assertion {
    done: mpsc::Receiver<()>,
    events: Arc<Mutex<EventDebouncer>>,
    expected: Vec<(CanonicalPathBuf, EventType)>,
}

impl Assertion {
    pub fn new<'a>(
        watcher: &Watcher,
        dir: &Path,
        expected: impl IntoIterator<Item = (&'a str, EventType)>,
    ) -> Assertion {
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(path, ty)| (CanonicalPathBuf::assert_canonicalized(&sub(dir, path)), ty))
            .collect();
        let events = Arc::new(Mutex::new(EventDebouncer::new()));
        let (tx, rx) = mpsc::sync_channel(1);
        let assertion = Assertion {
            done: rx,
            events: events.clone(),
            expected: expected.clone(),
        };

        watcher.add_handler(move |batch| {
            if Arc::strong_count(&events) == 1 {
                return false;
            }
            let mut events = events.lock().unwrap();
            for event in batch.iter() {
                events.add(event.path.clone(), event.ty);
            }
            // Stop once every expected change is present with its expected type.
            // Unrelated extras (e.g. a parent directory's own `Delete`) are ignored.
            if expected
                .iter()
                .all(|(path, ty)| events.get(path) == Some(*ty))
            {
                let _ = tx.send(());
                false
            } else {
                true
            }
        });
        assertion
    }

    #[track_caller]
    pub fn check(self) {
        let timed_out = self.done.recv_timeout(*TIMEOUT).is_err();
        self.events.clear_poison();
        let events = self.events.lock().unwrap();
        for (path, ty) in &self.expected {
            assert_eq!(
                events.get(path),
                Some(*ty),
                "{}event for {path}",
                if timed_out {
                    "timed out waiting for "
                } else {
                    "wrong "
                }
            );
        }
    }
}

fn rm_dir(dst: &Path, path: &str) {
    fs::remove_dir(sub(dst, path)).unwrap();
}

fn rm_file(dst: &Path, path: &str) {
    fs::remove_file(sub(dst, path)).unwrap();
}

fn write(dst: &Path, path: &str, content: &str) {
    fs::write(sub(dst, path), content).unwrap();
}

fn mk_write(dst: &Path, path: &str, content: &str) {
    let path = sub(dst, path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

// Rewrite separator to make assertions compatible across OSes.
fn sub(base: &Path, rel: &str) -> PathBuf {
    base.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
}

fn init_watcher() -> (TempDir, Watcher) {
    let _ = env_logger::builder().try_init();
    let dir = TempDir::new().unwrap();
    let watcher = Watcher::new().unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    watcher
        .add_root(dir.path(), true, move |success| {
            let _ = tx.send(success);
        })
        .unwrap();
    watcher.start();
    rx.recv_timeout(*TIMEOUT).expect("failed to start watcher");
    (dir, watcher)
}
// Events carry canonical paths (macOS `$TMPDIR` is under `/var`, a symlink to
// `/private/var`), so the expectations are built from the canonical temp dir.
fn with_watcher(f: impl FnOnce(&Path, &Watcher)) {
    let (dir, watcher) = init_watcher();
    // ReadDirectoryChangesW can split a create and a write across reads, and so
    // across batches. `Assertion` handles that, and the wider window makes it rarer.
    #[cfg(windows)]
    watcher.set_settle_time(std::time::Duration::from_secs(1));
    let shutdown_guard = watcher.shutdown_guard();
    let path = dir.path().canonicalize().unwrap();
    f(&path, &watcher);
    drop(shutdown_guard)
}

#[test]
fn delete() {
    with_watcher(|dir, watcher| {
        let assertion = Assertion::new(
            watcher,
            dir,
            [
                ("foo/baz", EventType::Create),
                ("foo/bar/baz", EventType::Create),
                ("baz", EventType::Create),
            ],
        );
        mk_write(dir, "baz", "foo");
        mk_write(dir, "foo/baz", "foo");
        mk_write(dir, "foo/bar/baz", "foo");
        assertion.check();
        let assertion = Assertion::new(watcher, dir, [("foo/bar/baz", EventType::Delete)]);
        rm_file(dir, "foo/bar/baz");
        rm_dir(dir, "foo/bar");
        assertion.check();
        let assertion = Assertion::new(watcher, dir, [("baz", EventType::Delete)]);
        rm_file(dir, "baz");
        assertion.check();
    });
}

#[test]
fn modify() {
    with_watcher(|dir, watcher| {
        let assertion = Assertion::new(
            watcher,
            dir,
            [
                ("foo/baz", EventType::Create),
                ("foo/bar/baz", EventType::Create),
                ("baz", EventType::Create),
            ],
        );
        mk_write(dir, "baz", "content1");
        mk_write(dir, "foo/baz", "content1");
        mk_write(dir, "foo/bar/baz", "content1");
        assertion.check();
        let assertion = Assertion::new(watcher, dir, [("foo/bar/baz", EventType::Modified)]);
        write(dir, "foo/bar/baz", "content2");
        assertion.check();
        let assertion = Assertion::new(watcher, dir, [("foo/baz", EventType::Modified)]);
        write(dir, "foo/baz", "content2");
        assertion.check();
        let assertion = Assertion::new(watcher, dir, [("baz", EventType::Modified)]);
        rm_file(dir, "baz");
        write(dir, "baz", "content3");
        assertion.check();
    });
}

/// A filter change with a recrawl picks up what the old filter hid.
#[test]
fn filter_change_recrawls() {
    struct IgnoreHidden;
    impl crate::Filter for IgnoreHidden {
        fn ignore_path(&self, path: &Path, _: Option<bool>) -> bool {
            path.file_name().is_some_and(|name| name == "hidden")
        }
    }
    with_watcher(|dir, watcher| {
        watcher.set_filter(Arc::new(IgnoreHidden), true);
        let assertion = Assertion::new(watcher, dir, [("seen", EventType::Create)]);
        mk_write(dir, "hidden/f", "x");
        mk_write(dir, "seen", "x");
        assertion.check();
        let assertion = Assertion::new(watcher, dir, [("hidden/f", EventType::Create)]);
        watcher.set_filter(Arc::new(()), true);
        assertion.check();
    });
}

/// helix calls `start` on every config reload. A second worker, with an empty tree,
/// would drop the changes it dequeued.
#[test]
fn start_twice_runs_one_worker() {
    let watcher = Watcher::new().unwrap();
    let _guard = watcher.shutdown_guard();
    watcher.start();
    // each worker holds a clone of the state
    let holders = Arc::strong_count(&watcher.state);
    watcher.start();
    assert_eq!(Arc::strong_count(&watcher.state), holders);
}

/// A caller waiting on `root_crawled` must hear back for an ignored root too.
#[test]
fn ignored_root_reports_back() {
    let dir = TempDir::new().unwrap();
    let ignored = dir.path().join(".git");
    fs::create_dir(&ignored).unwrap();
    let watcher = Watcher::new().unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    watcher
        .add_root(&ignored, true, move |watched| {
            let _ = tx.send(watched);
        })
        .unwrap();
    assert_eq!(rx.recv_timeout(*TIMEOUT), Ok(false));
}

/// Shut down while native callbacks may still be in flight. Under ASAN this
/// guards the FSEvents callback's `info` lifetime.
#[test]
fn teardown_with_pending_events_is_safe() {
    for _ in 0..16 {
        let dir = TempDir::new().unwrap();
        let watcher = Watcher::new().unwrap();
        watcher.add_handler(|_| true);
        let _ = watcher.add_root(dir.path(), true, |_| {});
        watcher.start();
        for i in 0..40 {
            let _ = fs::write(dir.path().join(format!("f{i}")), b"data");
        }
        watcher.shutdown();
    }
}

/// A root the backend cannot watch must be reported as failed every time, not
/// only the first.
#[test]
#[cfg(target_os = "linux")]
fn failed_watch_is_not_registered() {
    let dir = TempDir::new().unwrap();
    // inotify only watches directories
    let file = dir.path().join("file");
    fs::write(&file, b"x").unwrap();
    let watcher = Watcher::new().unwrap();
    watcher.start();
    for _ in 0..2 {
        let (tx, rx) = mpsc::sync_channel(1);
        watcher
            .add_root(&file, true, move |watched| {
                let _ = tx.send(watched);
            })
            .unwrap();
        assert_eq!(rx.recv_timeout(*TIMEOUT), Ok(false));
    }
    watcher.shutdown();
}

/// Handlers run without the config lock held, so they may call back into the
/// watcher.
#[test]
fn handler_may_reconfigure_the_watcher() {
    with_watcher(|dir, watcher| {
        let (tx, rx) = mpsc::sync_channel(1);
        let inner = watcher.clone();
        watcher.add_handler(move |_| {
            inner.add_handler(|_| false);
            inner.set_filter(Arc::new(()), false);
            inner.set_settle_time(Duration::from_millis(200));
            let _ = tx.try_send(());
            false
        });
        mk_write(dir, "f", "x");
        rx.recv_timeout(*TIMEOUT)
            .expect("handler deadlocked on the config lock");
    });
}
