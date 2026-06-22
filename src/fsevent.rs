//! macOS FSEvents backend.
//!
//! The FSEvents C API and `dispatch` are declared inline (stable system ABIs),
//! so the crate only needs `core-foundation-sys` for the CF collection/string types.
//!
//! Design: one `FSEventStream` over all recursive roots, delivered on a serial
//! `dispatch` queue (so no CFRunLoop thread is
//! needed). The stream is created *without* `kFSEventStreamCreateFlagFileEvents`,
//! so events are directory-granular — exactly what
//! `pending::Flags::NEEDS_NON_RECURSIVE_CRAWL` was added for: each event enqueues
//! a re-stat of the reported directory and the worker diffs it against the tree.
//! `kFSEventStreamEventFlagMustScanSubDirs` (coalescing/overflow) maps to a full
//! recrawl, mirroring inotify's `QUEUE_OVERFLOW`. The stream is rebuilt when a new
//! (uncovered) root is added, since FSEvents fixes its path list at creation; the
//! rebuilt stream resumes from the newest event id seen (`last_event_id`) so the
//! existing roots' events buffered across the teardown are replayed, not lost.

use std::ffi::{c_void, CStr};
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{self, AtomicBool, AtomicU64};
use std::sync::Arc;
use std::sync::Mutex;
use std::{io, ptr};

use core_foundation_sys::array::{kCFTypeArrayCallBacks, CFArrayCreate, CFArrayRef};
use core_foundation_sys::base::{kCFAllocatorDefault, CFRelease};
use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringCreateWithBytes, CFStringRef};

use crate::backend::Backend;
use crate::path::CanonicalPathBuf;
use crate::pending::{self, PendingChangesLock};
use crate::WatcherState;

// --- FSEvents FFI (CoreServices) ---------------------------------------------
type FSEventStreamRef = *mut c_void;
type ConstFSEventStreamRef = *const c_void;
type FSEventStreamEventFlags = u32;
type FSEventStreamEventId = u64;
type CFTimeInterval = f64;
type CFIndex = isize;

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

const K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER: u32 = 0x0000_0002;
const K_FS_EVENT_STREAM_CREATE_FLAG_WATCH_ROOT: u32 = 0x0000_0004;
const K_FS_EVENT_STREAM_EVENT_FLAG_MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;

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
    fn FSEventStreamStart(stream: FSEventStreamRef) -> bool;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
    /// The newest event id the system has issued. Used to anchor "now" when first
    /// building a stream so a later rebuild can resume from a concrete id even if
    /// no event has arrived yet.
    fn FSEventsGetCurrentEventId() -> FSEventStreamEventId;
}

// --- libdispatch FFI ---------------------------------------------------------
#[link(name = "System", kind = "dylib")]
unsafe extern "C" {
    fn dispatch_queue_create(label: *const i8, attr: *const c_void) -> *mut c_void;
    fn dispatch_release(object: *mut c_void);
}
// -----------------------------------------------------------------------------

struct Inner {
    roots: Vec<CanonicalPathBuf>,
    stream: FSEventStreamRef, // null when no roots are watched yet
}

pub(crate) struct FsEventWatcher {
    shutdown: AtomicBool,
    inner: Mutex<Inner>,
    queue: *mut c_void, // serial dispatch queue the callback runs on
    pub changes: Arc<PendingChangesLock>,
    state: Arc<WatcherState>,
    /// Newest FSEvents id seen so far, or 0 before the first stream is built.
    /// Each rebuild resumes the stream from here instead of `SinceNow`, so events
    /// buffered for the existing roots during the teardown/recreate window are
    /// replayed rather than lost -- no full recrawl needed. Updated by the callback
    /// (serialized on the dispatch queue) and read under `inner`'s lock at rebuild.
    last_event_id: Arc<AtomicU64>,
}

// The raw CF/dispatch pointers are only touched under `inner`'s lock or are
// themselves thread-safe (dispatch queues, FSEvent streams). The watcher is
// shared (Arc) between the worker thread and the dispatch queue.
unsafe impl Send for FsEventWatcher {}
unsafe impl Sync for FsEventWatcher {}

impl std::fmt::Debug for FsEventWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsEventWatcher").finish_non_exhaustive()
    }
}

impl FsEventWatcher {
    pub fn new(#[cfg(test)] _slow: bool, state: Arc<WatcherState>) -> io::Result<Arc<Self>> {
        let queue = unsafe { dispatch_queue_create(c"filesentry.fsevent".as_ptr(), ptr::null()) };
        if queue.is_null() {
            return Err(io::Error::other("dispatch_queue_create failed"));
        }
        Ok(Arc::new(Self {
            shutdown: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                roots: Vec::new(),
                stream: ptr::null_mut(),
            }),
            queue,
            changes: Arc::new(PendingChangesLock::default()),
            state,
            last_event_id: Arc::new(AtomicU64::new(0)),
        }))
    }

    /// (Re)build the single stream covering all current roots. FSEvents fixes the
    /// path list at creation, so adding a root requires tearing down and
    /// recreating the stream.
    fn rebuild_stream(&self, inner: &mut Inner) -> io::Result<()> {
        unsafe {
            if !inner.stream.is_null() {
                FSEventStreamStop(inner.stream);
                FSEventStreamInvalidate(inner.stream);
                FSEventStreamRelease(inner.stream);
                inner.stream = ptr::null_mut();
            }
            if inner.roots.is_empty() {
                return Ok(());
            }
            let paths = cf_paths(&inner.roots);
            // Resume from the newest id seen so far rather than `SinceNow`, so events
            // buffered for the existing roots while the stream is torn down and
            // recreated are replayed instead of dropped. The first build has no id
            // yet; anchor it on the current system id so the *next* rebuild has a
            // concrete point to resume from even if no event arrived in between.
            let mut since_when = self.last_event_id.load(atomic::Ordering::Relaxed);
            if since_when == 0 {
                since_when = FSEventsGetCurrentEventId();
                self.last_event_id
                    .store(since_when, atomic::Ordering::Relaxed);
            }
            // The stream owns this heap `CallbackInfo` and frees it via
            // `release_callback` only after the last callback, so an in-flight
            // callback can never outlive the data it reads. See `CallbackInfo` for
            // why it holds `Arc` clones rather than `self`.
            let info = Box::into_raw(Box::new(CallbackInfo {
                state: self.state.clone(),
                changes: self.changes.clone(),
                last_event_id: self.last_event_id.clone(),
            }));
            let mut context = FSEventStreamContext {
                version: 0,
                info: info as *mut c_void,
                retain: None,
                release: Some(release_callback),
                copy_description: None,
            };
            let stream = FSEventStreamCreate(
                ptr::null(),
                callback,
                &mut context,
                paths,
                since_when,
                0.1, // latency seconds; the worker also debounces via settle_time
                K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER | K_FS_EVENT_STREAM_CREATE_FLAG_WATCH_ROOT,
            );
            CFRelease(paths as *const c_void);
            if stream.is_null() {
                // No stream took ownership of `info`, so free it here.
                drop(Box::from_raw(info));
                return Err(io::Error::other("FSEventStreamCreate failed"));
            }
            FSEventStreamSetDispatchQueue(stream, self.queue);
            if !FSEventStreamStart(stream) {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                return Err(io::Error::other("FSEventStreamStart failed"));
            }
            inner.stream = stream;
        }
        Ok(())
    }
}

impl crate::backend::Backend for FsEventWatcher {
    fn changes(&self) -> &PendingChangesLock {
        &self.changes
    }

    fn watch_dir(&self, path: CanonicalPathBuf, _recursive: bool) -> io::Result<()> {
        // FSEvents watches recursively, so only roots need a stream; a path
        // already under a watched root is a no-op (the worker calls this per
        // directory during the crawl, so most calls return here).
        let mut inner = self.inner.lock().unwrap();
        // Checked under `inner`'s lock: `shutdown` sets the flag *before* taking the
        // same lock, so once it has run we won't build a new stream that nothing would
        // ever release (Drop skips cleanup when already shut down) and whose callback
        // would hold a dangling `info` pointer to a dropped watcher.
        if self.is_shutdown() {
            return Ok(());
        }
        if inner
            .roots
            .iter()
            .any(|r| r.is_parent_of(&path) || *r == path)
        {
            return Ok(());
        }
        inner.roots.push(path);
        // Rebuilding tears down the existing stream (FSEvents fixes its path list at
        // creation). The replacement resumes from `last_event_id` (see
        // `rebuild_stream`), so events buffered for the existing roots during the
        // teardown/recreate window are replayed -- no recrawl needed to resync them.
        // The new root's own initial state is crawled separately by the worker.
        self.rebuild_stream(&mut inner)?;
        Ok(())
    }

    fn shutdown(&self) {
        self.shutdown.store(true, atomic::Ordering::Relaxed);
        let mut inner = self.inner.lock().unwrap();
        unsafe {
            if !inner.stream.is_null() {
                FSEventStreamStop(inner.stream);
                FSEventStreamInvalidate(inner.stream);
                FSEventStreamRelease(inner.stream);
                inner.stream = ptr::null_mut();
            }
        }
        drop(inner);
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
        // Ensure the stream is gone before the dispatch queue is released.
        if !self.is_shutdown() {
            self.shutdown();
        }
        unsafe { dispatch_release(self.queue) };
    }
}

/// Build a retained `CFArray` of `CFString` paths for `FSEventStreamCreate`.
unsafe fn cf_paths(paths: &[CanonicalPathBuf]) -> CFArrayRef {
    let cfstrings: Vec<*const c_void> = paths
        .iter()
        .filter_map(|p| {
            let bytes = p.as_std_path().as_os_str().as_bytes();
            let s = CFStringCreateWithBytes(
                kCFAllocatorDefault,
                bytes.as_ptr(),
                bytes.len() as CFIndex,
                kCFStringEncodingUTF8,
                false as u8,
            );
            // Returns NULL if the bytes aren't valid UTF-8 (a macOS path can be
            // arbitrary bytes). Skip it -- putting NULL into the array would crash
            // both CFArray's retain callback and the CFRelease loop below.
            if s.is_null() {
                log::error!("not watching root with non-UTF-8 path: {p:?}");
                None
            } else {
                Some(s as *const c_void)
            }
        })
        .collect();
    let array = CFArrayCreate(
        kCFAllocatorDefault,
        cfstrings.as_ptr(),
        cfstrings.len() as CFIndex,
        &kCFTypeArrayCallBacks, // the array retains the strings…
    );
    for s in cfstrings {
        CFRelease(s); // …so we drop our references
    }
    array
}

/// Heap payload handed to the FSEvents callback via the stream context's `info`.
/// Holds clones of the `Arc`s the callback touches (not the whole `FsEventWatcher`,
/// which would create a stream <-> watcher reference cycle). Freed by
/// `release_callback` when the stream is released.
struct CallbackInfo {
    state: Arc<WatcherState>,
    changes: Arc<PendingChangesLock>,
    last_event_id: Arc<AtomicU64>,
}

/// Called by FSEvents when the stream is released; frees the `CallbackInfo` box.
extern "C" fn release_callback(info: *const c_void) {
    drop(unsafe { Box::from_raw(info as *mut CallbackInfo) });
}

/// FSEvents callback (runs on the dispatch queue). Each entry is a changed
/// directory; we don't trust the coalesced detail and enqueue a non-recursive
/// crawl of each, or a full recrawl on `MustScanSubDirs`. Mirrors
/// `InotifyWatcher::handle_event`.
extern "C" fn callback(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const FSEventStreamEventFlags,
    event_ids: *const FSEventStreamEventId,
) {
    // This runs from libdispatch through C frames, where a panic unwinding across
    // an `extern "C"` fn aborts the whole host process (helix), not just the
    // watcher. Contain it: log and drop the batch.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // `from_raw_parts` requires a non-null, aligned pointer even for len 0, and
        // a 0-event callback has nothing to do anyway.
        if num_events == 0 {
            return;
        }
        // SAFETY: `info` is the `CallbackInfo` boxed at stream creation; the stream
        // keeps it alive until after the last callback (see `rebuild_stream`).
        // `event_paths` is a `char **` and `event_flags`/`event_ids` are arrays,
        // all of `num_events`.
        let info = unsafe { &*(info as *const CallbackInfo) };
        let paths = event_paths as *const *const i8;
        let flags = unsafe { std::slice::from_raw_parts(event_flags, num_events) };
        // Record the newest id so a stream rebuild resumes just after it. Ids are
        // monotonic within (and across) batches; the `HistoryDone` sentinel and
        // other synthetic events carry id 0, so skip those.
        let ids = unsafe { std::slice::from_raw_parts(event_ids, num_events) };
        if let Some(&id) = ids.iter().rev().find(|&&id| id != 0) {
            info.last_event_id.store(id, atomic::Ordering::Relaxed);
        }
        let events = (0..num_events).map(|i| {
            let cstr = unsafe { CStr::from_ptr(*paths.add(i)) };
            let os = std::ffi::OsStr::from_bytes(cstr.to_bytes());
            (std::path::Path::new(os), flags[i])
        });
        process_events(info, events);
    }));
    if result.is_err() {
        log::error!("filesentry: panic in FSEvents callback was contained");
    }
}

/// Strip a single trailing `/` from an FSEvents path (kept off the root `/`
/// itself), restoring `CanonicalPathBuf`'s "never ends in a separator" invariant.
fn strip_trailing_sep(path: &std::path::Path) -> &std::path::Path {
    let bytes = path.as_os_str().as_bytes();
    match bytes.strip_suffix(b"/") {
        Some(stripped) if !stripped.is_empty() => {
            std::path::Path::new(std::ffi::OsStr::from_bytes(stripped))
        }
        _ => path,
    }
}

/// Body of the FSEvents callback, split out so it can be unit-tested without a
/// live stream and so the panic-containing wrapper in [`callback`] stays tiny.
fn process_events<'a>(
    info: &CallbackInfo,
    events: impl Iterator<Item = (&'a std::path::Path, FSEventStreamEventFlags)>,
) {
    // Tolerate a poisoned config lock rather than unwrap: a panicking handler
    // poisons it (it's held across handlers in `worker::Worker::run`), and we only
    // read the filter here, where a stale-but-valid value is fine.
    let filter = info
        .state
        .config
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .filter
        .clone();
    let mut changes = info.changes.lock();
    for (path, flag) in events {
        if flag & K_FS_EVENT_STREAM_EVENT_FLAG_MUST_SCAN_SUBDIRS != 0 {
            changes.recrawl();
            break;
        }
        // FSEvents reports directory paths with a trailing `/`, but
        // `CanonicalPathBuf` requires paths to never end in a separator: a
        // trailing slash makes `foo/bar/` hash and compare distinct from the
        // crawl-built `foo/bar`, spawning a duplicate tree node that a later
        // crawl then reports as spuriously deleted. Strip it so the path matches
        // the canonical node.
        let path = strip_trailing_sep(path);
        let path = CanonicalPathBuf::assert_canonicalized(path);
        // `ignore_path_rec`, not `ignore_path`: FSEvents has no per-directory
        // exclusion, so it reports writes under an ignored directory too. inotify
        // never sees those (it adds no watch there); to match, drop any path with an
        // ignored *ancestor*, not just a directly-ignored leaf.
        if filter.ignore_path_rec(path.as_std_path(), Some(true)) {
            continue;
        }
        changes.add_watcher(path, pending::Flags::NEEDS_NON_RECURSIVE_CRAWL);
    }
    drop(changes);
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
        let watcher = Watcher::new_impl(false).unwrap();
        let info = CallbackInfo {
            state: watcher.state.clone(),
            changes: watcher.notify.changes.clone(),
            last_event_id: watcher.notify.last_event_id.clone(),
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

    /// FSEvents hands directory paths to the callback with a trailing `/`. The
    /// recorded change must drop it so it matches the crawl-built canonical node
    /// (`foo/bar`, not `foo/bar/`); otherwise a duplicate tree node is created and
    /// later reported as a spurious delete.
    #[test]
    fn callback_strips_trailing_slash() {
        let watcher = Watcher::new_impl(false).unwrap();
        let info = CallbackInfo {
            state: watcher.state.clone(),
            changes: watcher.notify.changes.clone(),
            last_event_id: watcher.notify.last_event_id.clone(),
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
        let watcher = Watcher::new_impl(false).unwrap();
        let info = CallbackInfo {
            state: watcher.state.clone(),
            changes: watcher.notify.changes.clone(),
            last_event_id: watcher.notify.last_event_id.clone(),
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

    /// Adding a second root rebuilds the single FSEvents stream, tearing the old one
    /// down. The replacement resumes from `last_event_id` (anchored on the current
    /// system id at the first build), so events for the already-watched roots are
    /// replayed rather than lost -- no recrawl is forced, unlike the old `SinceNow`
    /// rebuild which had to recrawl to recover them.
    #[test]
    fn adding_a_root_resumes_without_recrawl() {
        use crate::backend::Backend;
        use std::sync::atomic::Ordering;

        let watcher = Watcher::new_impl(false).unwrap();
        let fs = &watcher.notify;
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();

        // First root builds the initial stream and anchors `last_event_id` on the
        // current system id, giving later rebuilds a concrete point to resume from.
        fs.watch_dir(CanonicalPathBuf::assert_canonicalized(d1.path()), true)
            .unwrap();
        assert!(!fs.changes.lock().take_recrawl());
        assert_ne!(
            fs.last_event_id.load(Ordering::Relaxed),
            0,
            "the first build must anchor last_event_id so rebuilds can resume",
        );

        // A second, unrelated root rebuilds the stream. Because it resumes from
        // last_event_id, the existing root's buffered events are replayed and no
        // recrawl is needed.
        fs.watch_dir(CanonicalPathBuf::assert_canonicalized(d2.path()), true)
            .unwrap();
        assert!(
            !fs.changes.lock().take_recrawl(),
            "resuming from last_event_id must not force a recrawl",
        );
    }
}
