//! macOS FSEvents backend.
//!
//! One `FSEventStream` per root, created when the root is added and never
//! rebuilt, all delivered on one serial `dispatch` queue (no CFRunLoop thread).
//! Events are directory-granular (no `FileEvents` flag): each queues a crawl of the
//! reported directory, and `MustScanSubDirs`/dropped events map to a full recrawl.
//! The FSEvents C API and `dispatch` are declared inline (stable system ABIs).

use std::ffi::{c_char, c_void, CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{Arc, Mutex};
use std::{io, ptr};

use core_foundation_sys::array::{kCFTypeArrayCallBacks, CFArrayCreate, CFArrayRef};
use core_foundation_sys::base::{kCFAllocatorDefault, CFRelease};
use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringCreateWithBytes, CFStringRef};

use crate::backend::Backend;
use crate::path::CanonicalPathBuf;
use crate::pending::PendingChangesLock;
use crate::WatcherState;

// --- FSEvents FFI (CoreServices) ---------------------------------------------
type FSEventStreamRef = *mut c_void;
type ConstFSEventStreamRef = *const c_void;
type FSEventStreamEventFlags = u32;
type FSEventStreamEventId = u64;
type CFTimeInterval = f64;
type CFIndex = isize;
type Boolean = u8;

#[repr(C)]
struct FSEventStreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> CFStringRef>,
}

type FSEventStreamCallback = extern "C" fn(
    stream: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void, // `char **` (no UseCFTypes flag)
    event_flags: *const FSEventStreamEventFlags,
    event_ids: *const FSEventStreamEventId,
);

const K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW: FSEventStreamEventId = u64::MAX;
const K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER: u32 = 0x0000_0002;
const K_FS_EVENT_STREAM_CREATE_FLAG_WATCH_ROOT: u32 = 0x0000_0004;
const K_FS_EVENT_STREAM_EVENT_FLAG_MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
const K_FS_EVENT_STREAM_EVENT_FLAG_USER_DROPPED: u32 = 0x0000_0002;
const K_FS_EVENT_STREAM_EVENT_FLAG_KERNEL_DROPPED: u32 = 0x0000_0004;
/// Any of these means events were coalesced or lost and the subtree must be rescanned.
const RESCAN_FLAGS: u32 = K_FS_EVENT_STREAM_EVENT_FLAG_MUST_SCAN_SUBDIRS
    | K_FS_EVENT_STREAM_EVENT_FLAG_USER_DROPPED
    | K_FS_EVENT_STREAM_EVENT_FLAG_KERNEL_DROPPED;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: *const c_void,
        callback: FSEventStreamCallback,
        context: *const FSEventStreamContext,
        paths_to_watch: CFArrayRef,
        since_when: FSEventStreamEventId,
        latency: CFTimeInterval,
        flags: FSEventStreamEventFlags,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, q: *mut c_void);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> Boolean;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
}

// --- libdispatch FFI ---------------------------------------------------------
#[link(name = "System", kind = "dylib")]
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_release(object: *mut c_void);
    fn dispatch_sync_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}
// -----------------------------------------------------------------------------

/// Heap payload handed to the FSEvents callback via the stream context's `info`.
/// Holds `Arc` clones rather than the watcher, which would be a reference cycle.
struct CallbackInfo {
    state: Arc<WatcherState>,
    changes: Arc<PendingChangesLock>,
}

/// An `FSEventStream` that is running on the watcher's dispatch queue.
struct Stream {
    stream: FSEventStreamRef,
    /// The serial queue the stream delivers on. It outlives every `Stream` (see
    /// `Drop for FsEventWatcher`).
    queue: *mut c_void,
    /// `FSEventStreamStop` is only valid on a started stream.
    started: bool,
}

// SAFETY: FSEventStream references are thread-safe, and the queue pointer is only
// used for `dispatch_sync_f`, which is too.
unsafe impl Send for Stream {}

extern "C" fn noop(_: *mut c_void) {}

impl Stream {
    /// Create and start a stream for `path`, delivering events from now on.
    fn start(path: &CanonicalPathBuf, queue: *mut c_void, info: CallbackInfo) -> io::Result<Self> {
        // SAFETY: `path` is a valid byte slice; a null result is handled.
        let paths = unsafe { cf_path_array(path) };
        if paths.is_null() {
            return Err(io::Error::other(format!(
                "cannot watch non-UTF-8 path {path:?}"
            )));
        }
        // Until the stream owns it (`FSEventStreamCreate` succeeds) we free `info` ourselves.
        let info = Box::into_raw(Box::new(info));
        let context = FSEventStreamContext {
            version: 0,
            info: info as *mut c_void,
            retain: None,
            release: Some(release_callback),
            copy_description: None,
        };
        // SAFETY: `context` and `paths` are valid for the call (FSEvents copies both);
        // `callback` matches the declared signature.
        let stream = unsafe {
            FSEventStreamCreate(
                ptr::null(),
                callback,
                &context,
                paths,
                K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW,
                0.1, // latency seconds; the worker also debounces via settle_time
                K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER | K_FS_EVENT_STREAM_CREATE_FLAG_WATCH_ROOT,
            )
        };
        // SAFETY: we own the one reference `cf_path_array` returned.
        unsafe { CFRelease(paths as *const c_void) };
        if stream.is_null() {
            // SAFETY: no stream exists, so nothing else holds `info`.
            drop(unsafe { Box::from_raw(info) });
            return Err(io::Error::other("FSEventStreamCreate failed"));
        }
        // From here `Drop` owns teardown, including on the error path below.
        let mut stream = Stream {
            stream,
            queue,
            started: false,
        };
        // SAFETY: `stream` is a live, unstarted stream and `queue` a live dispatch queue.
        unsafe { FSEventStreamSetDispatchQueue(stream.stream, queue) };
        // SAFETY: as above, with the queue already set.
        if unsafe { FSEventStreamStart(stream.stream) } == 0 {
            return Err(io::Error::other("FSEventStreamStart failed"));
        }
        stream.started = true;
        Ok(stream)
    }
}

impl Drop for Stream {
    /// The only place a stream is torn down.
    ///
    /// Must not run on the dispatch queue: the barrier below would deadlock. The
    /// callback never drops a stream or the watcher, so that cannot happen.
    fn drop(&mut self) {
        // SAFETY: `self.stream` is live and released exactly once, here. After
        // `Invalidate` no new callback is scheduled, and the `dispatch_sync_f` on the
        // serial queue returns only after any callback already running on it has
        // finished, so the release callback frees `info` with no reader left.
        unsafe {
            if self.started {
                FSEventStreamStop(self.stream);
            }
            FSEventStreamInvalidate(self.stream);
            dispatch_sync_f(self.queue, ptr::null_mut(), noop);
            FSEventStreamRelease(self.stream);
        }
    }
}

pub(crate) struct FsEventWatcher {
    shutdown: AtomicBool,
    /// The serial queue every stream's callback runs on, which the barrier in
    /// `Stream::drop` relies on. A NULL `attr` makes it serial.
    queue: *mut c_void,
    pub changes: Arc<PendingChangesLock>,
    state: Arc<WatcherState>,
    /// One running stream per root.
    streams: Mutex<Vec<(CanonicalPathBuf, Stream)>>,
}

// SAFETY: `queue` is a thread-safe dispatch queue and everything else is `Send + Sync`.
unsafe impl Send for FsEventWatcher {}
unsafe impl Sync for FsEventWatcher {}

impl std::fmt::Debug for FsEventWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsEventWatcher").finish_non_exhaustive()
    }
}

impl FsEventWatcher {
    pub fn new(state: Arc<WatcherState>) -> io::Result<Arc<Self>> {
        // SAFETY: the label is a valid C string; a NULL attribute makes the queue serial.
        let queue = unsafe { dispatch_queue_create(c"filesentry.fsevent".as_ptr(), ptr::null()) };
        if queue.is_null() {
            return Err(io::Error::other("dispatch_queue_create failed"));
        }
        Ok(Arc::new(Self {
            shutdown: AtomicBool::new(false),
            queue,
            changes: Arc::new(PendingChangesLock::default()),
            state,
            streams: Mutex::new(Vec::new()),
        }))
    }
}

impl Backend for FsEventWatcher {
    fn changes(&self) -> &PendingChangesLock {
        &self.changes
    }

    fn watch_dir(&self, path: CanonicalPathBuf, _recursive: bool) -> io::Result<()> {
        let mut streams = self.streams.lock().unwrap();
        // Checked under the lock that `shutdown` takes after setting the flag, so no
        // stream can be added once `shutdown` has cleared the list.
        if self.is_shutdown() {
            return Ok(());
        }
        // FSEvents is recursive; the worker calls this per directory, so most calls end here.
        if streams
            .iter()
            .any(|(r, _)| *r == path || r.is_parent_of(&path))
        {
            return Ok(());
        }
        // A root added around existing roots keeps their streams: the overlap reports
        // some paths twice, which the pending queue merges.
        let stream = Stream::start(
            &path,
            self.queue,
            CallbackInfo {
                state: self.state.clone(),
                changes: self.changes.clone(),
            },
        )?;
        streams.push((path, stream));
        Ok(())
    }

    fn shutdown(&self) {
        self.shutdown.store(true, atomic::Ordering::Relaxed);
        self.streams.lock().unwrap().clear();
        self.changes.notify();
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(atomic::Ordering::Relaxed)
    }

    fn refresh_config(&self) {
        // The filter is re-read by the worker; nothing native to do.
    }
}

impl Drop for FsEventWatcher {
    fn drop(&mut self) {
        // Every stream must be gone before the queue is released.
        self.shutdown();
        // SAFETY: `queue` was created in `new` and no stream uses it any more.
        unsafe { dispatch_release(self.queue) };
    }
}

/// Build a retained one-element `CFArray` holding `path`; null if it is not UTF-8.
///
/// # Safety
/// Caller releases a non-null result with `CFRelease`.
unsafe fn cf_path_array(path: &CanonicalPathBuf) -> CFArrayRef {
    let bytes = path.as_std_path().as_os_str().as_bytes();
    let s = CFStringCreateWithBytes(
        kCFAllocatorDefault,
        bytes.as_ptr(),
        bytes.len() as CFIndex,
        kCFStringEncodingUTF8,
        false as u8,
    );
    if s.is_null() {
        return ptr::null();
    }
    let items = [s as *const c_void];
    let array = CFArrayCreate(
        kCFAllocatorDefault,
        items.as_ptr(),
        1,
        &kCFTypeArrayCallBacks, // the array retains the string…
    );
    CFRelease(s as *const c_void); // …so we drop our reference
    array
}

/// Called by FSEvents when the stream is released; frees the `CallbackInfo` box.
extern "C" fn release_callback(info: *const c_void) {
    // SAFETY: `info` is the box leaked in `Stream::start`, released exactly once.
    drop(unsafe { Box::from_raw(info as *mut CallbackInfo) });
}

/// FSEvents callback (runs on the dispatch queue).
extern "C" fn callback(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const FSEventStreamEventFlags,
    _event_ids: *const FSEventStreamEventId,
) {
    // A panic unwinding through this `extern "C"` frame would abort the host process.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // `from_raw_parts` needs a non-null pointer even for len 0.
        if num_events == 0 {
            return;
        }
        // SAFETY: `info` outlives every callback: `Stream::drop` waits for the running
        // callback before the stream's release callback frees it.
        let info = unsafe { &*(info as *const CallbackInfo) };
        let paths = event_paths as *const *const c_char;
        // SAFETY: FSEvents passes `num_events` flags.
        let flags = unsafe { std::slice::from_raw_parts(event_flags, num_events) };
        let events = (0..num_events).map(|i| {
            // SAFETY: FSEvents passes `num_events` NUL-terminated paths valid for this call.
            let cstr = unsafe { CStr::from_ptr(*paths.add(i)) };
            (Path::new(OsStr::from_bytes(cstr.to_bytes())), flags[i])
        });
        process_events(info, events);
    }));
    if result.is_err() {
        log::error!("filesentry: panic in FSEvents callback was contained");
    }
}

/// Strip a single trailing `/` from an FSEvents path (kept off the root `/`
/// itself), restoring `CanonicalPathBuf`'s "never ends in a separator" invariant.
fn strip_trailing_sep(path: &Path) -> &Path {
    let bytes = path.as_os_str().as_bytes();
    match bytes.strip_suffix(b"/") {
        Some(stripped) if !stripped.is_empty() => Path::new(OsStr::from_bytes(stripped)),
        _ => path,
    }
}

/// Body of the callback, split out so tests can run it without a live stream.
fn process_events<'a>(
    info: &CallbackInfo,
    events: impl Iterator<Item = (&'a Path, FSEventStreamEventFlags)>,
) {
    // A panicking handler poisons the config lock. Only the filter is read here, so
    // the poisoned value is still fine to use.
    let filter = info
        .state
        .config
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .filter
        .clone();
    // All fallible work happens here, so the pending-queue lock below is held only
    // for infallible pushes and cannot be poisoned by a panic in this function.
    let mut recrawl = false;
    let mut dirs = Vec::new();
    for (path, flags) in events {
        // The `HistoryDone` sentinel has an empty path.
        if path.as_os_str().is_empty() {
            continue;
        }
        if flags & RESCAN_FLAGS != 0 {
            recrawl = true;
            break;
        }
        // FSEvents reports directories with a trailing `/`, unlike the tree's `foo/bar`.
        let path = CanonicalPathBuf::assert_canonicalized(strip_trailing_sep(path));
        // FSEvents also reports writes under ignored directories, which inotify
        // never sees, so drop any path with an ignored ancestor.
        if !filter.ignore_path_rec(path.as_std_path(), Some(true)) {
            dirs.push(path);
        }
    }
    if !recrawl && dirs.is_empty() {
        return;
    }
    {
        let mut changes = info.changes.lock();
        if recrawl {
            changes.recrawl();
        } else {
            for dir in dirs {
                changes.add(dir, true);
            }
        }
    }
    info.changes.notify();
}

#[cfg(test)]
mod tests {
    use super::{process_events, CallbackInfo};
    use crate::path::CanonicalPathBuf;
    use crate::Watcher;
    use std::path::Path;

    /// A panicking handler poisons the config mutex; the callback must still record
    /// events rather than `.unwrap()` and abort across the `extern "C"` frame.
    #[test]
    fn callback_tolerates_poisoned_config() {
        let watcher = Watcher::new().unwrap();
        let info = CallbackInfo {
            state: watcher.state.clone(),
            changes: watcher.notify.changes.clone(),
        };

        // Poison the config mutex the way a panicking handler would.
        let state = watcher.state.clone();
        let _ = std::thread::spawn(move || {
            let _guard = state.config.lock().unwrap();
            panic!("simulated handler panic while holding the config lock");
        })
        .join();
        assert!(watcher.state.config.is_poisoned());

        // A poisoned config must not stop the callback from recording the change.
        process_events(
            &info,
            std::iter::once((Path::new("/tmp/filesentry-test/sub"), 0u32)),
        );
        assert!(
            !info.changes.lock().is_empty(),
            "the change should be recorded despite the poisoned config lock",
        );
    }

    /// The trailing `/` of FSEvents' directory paths must be stripped.
    #[test]
    fn callback_strips_trailing_slash() {
        let watcher = Watcher::new().unwrap();
        let info = CallbackInfo {
            state: watcher.state.clone(),
            changes: watcher.notify.changes.clone(),
        };

        process_events(
            &info,
            std::iter::once((Path::new("/tmp/filesentry-test/sub/"), 0u32)),
        );

        let recorded: Vec<_> = info.changes.lock().drain().map(|c| c.path).collect();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].as_std_path(),
            Path::new("/tmp/filesentry-test/sub"),
            "the trailing slash must be stripped",
        );
    }

    /// A path under an ignored *ancestor* (not just a directly-ignored leaf) must be
    /// dropped. The default `()` filter ignores a `.git` component.
    #[test]
    fn callback_drops_events_under_ignored_ancestor() {
        let watcher = Watcher::new().unwrap();
        let info = CallbackInfo {
            state: watcher.state.clone(),
            changes: watcher.notify.changes.clone(),
        };

        // A directory *inside* an ignored `.git` -- the leaf itself is not `.git`.
        process_events(
            &info,
            std::iter::once((Path::new("/tmp/filesentry-test/.git/objects"), 0u32)),
        );
        assert!(
            info.changes.lock().is_empty(),
            "an event under an ignored `.git` ancestor must be dropped",
        );

        // A normal path under the same root is still recorded.
        process_events(
            &info,
            std::iter::once((Path::new("/tmp/filesentry-test/src"), 0u32)),
        );
        assert!(
            !info.changes.lock().is_empty(),
            "a non-ignored path must still be recorded",
        );
    }

    /// The `HistoryDone` sentinel (empty path) is ignored, not recorded as a root dir.
    #[test]
    fn callback_skips_empty_path() {
        let watcher = Watcher::new().unwrap();
        let info = CallbackInfo {
            state: watcher.state.clone(),
            changes: watcher.notify.changes.clone(),
        };
        process_events(&info, std::iter::once((Path::new(""), 0x10u32)));
        assert!(info.changes.lock().is_empty());
    }

    /// Dropped/coalesced events request a full recrawl.
    #[test]
    fn callback_recrawls_on_must_scan() {
        let watcher = Watcher::new().unwrap();
        let info = CallbackInfo {
            state: watcher.state.clone(),
            changes: watcher.notify.changes.clone(),
        };
        process_events(&info, std::iter::once((Path::new("/tmp/x"), 0x1u32)));
        assert!(info.changes.lock().take_recrawl());
    }

    fn canonical_tempdir() -> (tempfile::TempDir, std::path::PathBuf) {
        // Not `.tmp*`-prefixed (hidden), and canonical: /var is a symlink to /private/var,
        // which is what FSEvents reports.
        let dir = tempfile::Builder::new()
            .prefix("fsentry")
            .tempdir()
            .unwrap();
        let canon = dir.path().canonicalize().unwrap();
        (dir, canon)
    }

    /// Wait until a change under `dir` is pending.
    fn saw_change_under(fs: &super::FsEventWatcher, dir: &Path) -> bool {
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let mut changes = fs.changes.lock();
            if changes
                .drain()
                .any(|c| c.path.as_std_path().starts_with(dir))
            {
                return true;
            }
        }
        false
    }

    /// Each separately added root gets its own stream and reports writes.
    #[test]
    fn two_separate_roots_both_report() {
        use crate::backend::Backend;

        let watcher = Watcher::new().unwrap();
        let fs = &watcher.notify;
        let (_d1, p1) = canonical_tempdir();
        let (_d2, p2) = canonical_tempdir();
        fs.watch_dir(CanonicalPathBuf::assert_canonicalized(&p1), true)
            .unwrap();
        fs.watch_dir(CanonicalPathBuf::assert_canonicalized(&p2), true)
            .unwrap();
        assert!(
            !fs.changes.lock().take_recrawl(),
            "adding a root must not force a recrawl"
        );

        std::fs::write(p1.join("a"), b"1").unwrap();
        assert!(
            saw_change_under(fs, &p1),
            "no change reported for the first root"
        );
        std::fs::write(p2.join("b"), b"2").unwrap();
        assert!(
            saw_change_under(fs, &p2),
            "no change reported for the second root"
        );
    }

    /// A root whose stream cannot be created leaves existing streams running and is
    /// not recorded. A non-UTF-8 path fails deterministically (CFString creation).
    #[test]
    fn failed_root_keeps_existing_roots() {
        use crate::backend::Backend;
        use std::os::unix::ffi::OsStrExt;

        let watcher = Watcher::new().unwrap();
        let fs = &watcher.notify;
        let (_d1, p1) = canonical_tempdir();
        fs.watch_dir(CanonicalPathBuf::assert_canonicalized(&p1), true)
            .unwrap();

        let bad = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/filesentry-\xff"));
        assert!(fs
            .watch_dir(CanonicalPathBuf::assert_canonicalized(bad), true)
            .is_err());
        assert_eq!(fs.streams.lock().unwrap().len(), 1);

        std::fs::write(p1.join("a"), b"1").unwrap();
        assert!(
            saw_change_under(fs, &p1),
            "the first root must still report"
        );
    }

    /// A root inside an existing one adds nothing. A root around existing ones adds
    /// a stream and keeps the inner ones.
    #[test]
    fn nested_roots() {
        use crate::backend::Backend;

        let watcher = Watcher::new().unwrap();
        let fs = &watcher.notify;
        let (_d, p) = canonical_tempdir();
        let inner = p.join("outer").join("inner");
        std::fs::create_dir_all(&inner).unwrap();

        fs.watch_dir(CanonicalPathBuf::assert_canonicalized(&inner), true)
            .unwrap();
        fs.watch_dir(CanonicalPathBuf::assert_canonicalized(&inner), true)
            .unwrap();
        assert_eq!(fs.streams.lock().unwrap().len(), 1);
        fs.watch_dir(CanonicalPathBuf::assert_canonicalized(&p), true)
            .unwrap();
        assert_eq!(fs.streams.lock().unwrap().len(), 2);
        fs.watch_dir(CanonicalPathBuf::assert_canonicalized(&inner), true)
            .unwrap();
        assert_eq!(fs.streams.lock().unwrap().len(), 2);
    }

    /// After shutdown `watch_dir` adds nothing.
    #[test]
    fn no_streams_after_shutdown() {
        use crate::backend::Backend;

        let watcher = Watcher::new().unwrap();
        let fs = &watcher.notify;
        let (_d, p) = canonical_tempdir();
        fs.shutdown();
        fs.watch_dir(CanonicalPathBuf::assert_canonicalized(&p), true)
            .unwrap();
        assert!(fs.streams.lock().unwrap().is_empty());
    }
}
