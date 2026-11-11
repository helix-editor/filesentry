//! Windows `ReadDirectoryChangesW` backend.
//!
//! One directory handle per watched root, all bound to a single I/O completion
//! port that one thread drains. Each root owns its `OVERLAPPED`, so the packet
//! the port returns *is* the root. Like inotify it reports per-file records:
//! create/delete/rename are queued with `crawl` set, modifications without, and a
//! failed or overflowed read becomes a full recrawl.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::ptr::{addr_of_mut, null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{io, thread};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
    FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, PostQueuedCompletionStatus,
    OVERLAPPED,
};

use crate::backend::Backend;
use crate::path::CanonicalPathBuf;
use crate::pending::PendingChangesLock;
use crate::{Filter, WatcherState};

const NOTIFY_FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_CREATION;

/// 64 KiB is the documented maximum for network drives. An overflow triggers a
/// recrawl, so the size is not a correctness limit.
const BUF_LEN: usize = 64 * 1024;

/// `ReadDirectoryChangesW` requires a DWORD-aligned buffer.
#[repr(C, align(4))]
struct Buffer([u8; BUF_LEN]);

/// The state of one watched directory.
///
/// Ownership: `watch_dir` allocates it, issues the first read while holding the
/// `watches` lock, and leaks it with `Box::into_raw`. From then on exactly one
/// read (or its queued completion packet) refers to it, and only the completion
/// thread may touch it: the kernel writes `overlapped` and `buffer` while the read
/// is pending, and the thread alone reads them, re-arms the read, and finally frees
/// the `Root` once it has dequeued the packet and decided not to re-arm. Any other
/// thread only sees the copy of `path`, `recursive` and `handle` in `watches`.
///
/// `overlapped` must stay the first field: the pointer the port returns is cast
/// back to `*mut Root`.
#[repr(C)]
struct Root {
    overlapped: OVERLAPPED,
    handle: HANDLE,
    buffer: Buffer,
    path: CanonicalPathBuf,
    recursive: bool,
    /// Whether the previous completion failed. Two failures in a row drop the root,
    /// so that a handle that can never be read again doesn't spin.
    failed_last: bool,
}

/// The part of a [`Root`] other threads may look at.
struct Watch {
    path: CanonicalPathBuf,
    recursive: bool,
    /// Identifies the root; it is removed from the list before its handle is closed.
    handle: HANDLE,
}

impl Watch {
    fn covers(&self, path: &CanonicalPathBuf, recursive: bool) -> bool {
        // A non-recursive watch of the same path does not cover a recursive request.
        (self.path == *path && (self.recursive || !recursive))
            || (self.recursive && self.path.is_parent_of(path))
    }
}

pub(crate) struct WindowsWatcher {
    shutdown: AtomicBool,
    iocp: HANDLE,
    /// Every live root. `shutdown` is set, and roots are registered, cancelled and
    /// removed, only while holding this lock, so neither side can miss a read the
    /// other is creating or cancelling.
    watches: Mutex<Vec<Watch>>,
    pub changes: PendingChangesLock,
    state: Arc<WatcherState>,
}

// SAFETY: the raw handles are plain kernel handles usable from any thread, and the
// `Root`s they lead to are only touched by the completion thread (see `Root`).
unsafe impl Send for WindowsWatcher {}
// SAFETY: as above; all shared mutable state is behind a `Mutex` or an atomic.
unsafe impl Sync for WindowsWatcher {}

impl std::fmt::Debug for WindowsWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsWatcher").finish_non_exhaustive()
    }
}

impl WindowsWatcher {
    pub fn new(state: Arc<WatcherState>) -> io::Result<Arc<Self>> {
        // SAFETY: creating a fresh port takes no pointers.
        let iocp = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 0) };
        if iocp.is_null() {
            return Err(io::Error::last_os_error());
        }
        let watcher = Arc::new(Self {
            shutdown: AtomicBool::new(false),
            iocp,
            watches: Mutex::new(Vec::new()),
            changes: PendingChangesLock::default(),
            state,
        });
        let watcher_ = watcher.clone();
        let spawned = thread::Builder::new()
            .name("filesentry-rdcw".into())
            .spawn(move || watcher_.completion_loop());
        if let Err(err) = spawned {
            // SAFETY: the thread never started, so nothing else uses the port.
            unsafe { CloseHandle(iocp) };
            return Err(err);
        }
        Ok(watcher)
    }

    /// Runs until shutdown has been requested and every root has been released,
    /// then closes the port. The thread holds an `Arc<Self>`, so the watcher
    /// outlives it.
    fn completion_loop(&self) {
        loop {
            let mut bytes = 0u32;
            let mut key = 0usize;
            let mut overlapped: *mut OVERLAPPED = null_mut();
            // SAFETY: all out-pointers refer to live locals.
            let ok = unsafe {
                GetQueuedCompletionStatus(
                    self.iocp,
                    &mut bytes,
                    &mut key,
                    &mut overlapped,
                    u32::MAX,
                )
            };
            // SAFETY: reads the thread-local error of the call above.
            let err = if ok == 0 {
                unsafe { GetLastError() }
            } else {
                0
            };
            if overlapped.is_null() {
                if ok == 0 {
                    // The port itself failed. The roots are deliberately leaked: their
                    // reads may still be pending, so their buffers must not be freed.
                    log::error!("completion port failed: error {err}");
                    return;
                }
                // Otherwise: the wake-up packet from `shutdown`.
            } else {
                self.on_packet(overlapped.cast::<Root>(), ok != 0, bytes, err);
            }
            if self.is_shutdown() && self.watches.lock().unwrap().is_empty() {
                // SAFETY: no root is left, so no read can still complete on this port,
                // and `shutdown` has already posted its only packet.
                unsafe { CloseHandle(self.iocp) };
                return;
            }
        }
    }

    /// Handles the completion of the single read outstanding on `root`.
    fn on_packet(&self, root: *mut Root, ok: bool, bytes: u32, err: u32) {
        // SAFETY: the packet says the read has completed, so the kernel is done with
        // the root and this thread is its only user until it is re-armed or freed.
        let r = unsafe { &mut *root };
        let path = r.path.clone();
        // A failure is the root being deleted or renamed, `ERROR_NOTIFY_ENUM_DIR`, or
        // `ERROR_OPERATION_ABORTED` after `shutdown` cancelled the read.
        let give_up = !ok && r.failed_last;
        r.failed_last = !ok;
        let mut records = Vec::new();
        if ok && bytes > 0 {
            let len = (bytes as usize).min(BUF_LEN);
            records.extend_from_slice(&r.buffer.0[..len]);
        }
        let rearmed = !give_up && self.rearm(r).is_ok();
        if !rearmed {
            self.release(root);
        }
        if self.is_shutdown() {
            return;
        }
        if !ok {
            log::debug!("read on {path:?} failed with error {err}; recrawling");
        }
        if records.is_empty() {
            // Overflow or failure. The root is released first so that the recrawl's
            // `watch_dir` for a recreated directory is not mistaken for covered.
            self.changes.lock().recrawl();
        } else {
            let filter = self.state.config.lock().unwrap().filter.clone();
            let mut found = Vec::new();
            parse_records(&path, &records, &*filter, &mut found);
            let mut changes = self.changes.lock();
            for (path, crawl) in found {
                changes.add(path, crawl);
            }
        }
        self.changes.notify();
    }

    /// Issues the next read unless shutdown has begun. Checking under the lock
    /// means a read is never started after `shutdown` has cancelled the roots.
    fn rearm(&self, root: &mut Root) -> io::Result<()> {
        let _watches = self.watches.lock().unwrap();
        if self.is_shutdown() {
            return Err(io::Error::other("watcher is shut down"));
        }
        // SAFETY: the read on `root` has completed and the lock is held.
        unsafe { issue_read(root) }
    }

    /// Unregisters, closes and frees a root whose read has completed.
    fn release(&self, root: *mut Root) {
        // SAFETY: `root` came from `Box::into_raw`, no read is pending on it, and the
        // caller does not use it again.
        let root = unsafe { Box::from_raw(root) };
        self.watches
            .lock()
            .unwrap()
            .retain(|w| w.handle != root.handle);
        // SAFETY: the handle has no pending read and is no longer in `watches`.
        unsafe { CloseHandle(root.handle) };
    }
}

/// Translates `FILE_NOTIFY_INFORMATION` records into `(path, crawl)` pairs,
/// dropping those the filter ignores (including anything under an ignored
/// directory).
///
/// Parsed with bounds-checked byte reads: `buf` is a plain `Vec<u8>`, so
/// references to the DWORD-containing struct could be under-aligned, and a bad
/// offset or length must not read out of bounds. Layout: `NextEntryOffset: u32`,
/// `Action: u32`, `FileNameLength: u32` (bytes), `FileName: [u16]`.
fn parse_records(
    root: &CanonicalPathBuf,
    buf: &[u8],
    filter: &dyn Filter,
    out: &mut Vec<(CanonicalPathBuf, bool)>,
) {
    const HEADER: usize = 12;
    let read_u32 = |at: usize| u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
    let mut offset = 0usize;
    while offset + HEADER <= buf.len() {
        let next_entry = read_u32(offset) as usize;
        let action = read_u32(offset + 4);
        let name_len = read_u32(offset + 8) as usize;
        let name_start = offset + HEADER;
        let Some(name) = buf.get(name_start..name_start.saturating_add(name_len)) else {
            break;
        };
        let crawl = match action {
            FILE_ACTION_ADDED
            | FILE_ACTION_REMOVED
            | FILE_ACTION_RENAMED_OLD_NAME
            | FILE_ACTION_RENAMED_NEW_NAME => Some(true),
            FILE_ACTION_MODIFIED => Some(false),
            _ => None,
        };
        if let Some(crawl) = crawl {
            let units: Vec<u16> = name
                .chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect();
            let path = root.join(&OsString::from_wide(&units));
            if !filter.ignore_path_rec(path.as_std_path(), None) {
                out.push((path, crawl));
            }
        }
        if next_entry == 0 {
            break;
        }
        offset = offset.saturating_add(next_entry);
    }
}

/// (Re)issues the overlapped `ReadDirectoryChangesW` for a root.
///
/// # Safety
///
/// No read may be pending on `root`, and the caller must be the only user of it.
unsafe fn issue_read(root: *mut Root) -> io::Result<()> {
    // SAFETY: per the contract, nothing else accesses the root.
    let ok = unsafe {
        (*root).overlapped = std::mem::zeroed();
        ReadDirectoryChangesW(
            (*root).handle,
            addr_of_mut!((*root).buffer).cast(),
            BUF_LEN as u32,
            (*root).recursive as i32,
            NOTIFY_FILTER,
            null_mut(),
            addr_of_mut!((*root).overlapped),
            None,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Backend for WindowsWatcher {
    fn changes(&self) -> &PendingChangesLock {
        &self.changes
    }

    /// A recursive request for a path that only has a non-recursive watch opens a
    /// second handle on the directory rather than replacing the first, because a
    /// root cannot be freed while its read is pending. Duplicate records collapse in
    /// the pending queue.
    fn watch_dir(&self, path: CanonicalPathBuf, recursive: bool) -> io::Result<()> {
        let mut watches = self.watches.lock().unwrap();
        if self.is_shutdown() {
            return Err(io::Error::other("watcher is shut down"));
        }
        if watches.iter().any(|w| w.covers(&path, recursive)) {
            return Ok(());
        }
        let wide = path.to_wide();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `handle` is a freshly opened overlapped directory handle and the
        // port is open (it is closed only after shutdown, which was ruled out above).
        if unsafe { CreateIoCompletionPort(handle, self.iocp, 0, 0) }.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: nothing else knows this handle yet.
            unsafe { CloseHandle(handle) };
            return Err(err);
        }
        let watch = Watch {
            path: path.clone(),
            recursive,
            handle,
        };
        let root = Box::into_raw(Box::new(Root {
            // SAFETY: an all-zero OVERLAPPED is its documented initial state.
            overlapped: unsafe { std::mem::zeroed() },
            handle,
            buffer: Buffer([0; BUF_LEN]),
            path,
            recursive,
            failed_last: false,
        }));
        // SAFETY: no read is pending on the new root and it is not shared yet.
        if let Err(err) = unsafe { issue_read(root) } {
            // SAFETY: the read was not queued, so nothing else refers to the root or
            // the handle.
            unsafe {
                drop(Box::from_raw(root));
                CloseHandle(handle);
            }
            return Err(err);
        }
        // The completion thread cannot release the root before this lock is dropped.
        watches.push(watch);
        Ok(())
    }

    /// Cancels every read and returns without waiting. The completion thread frees
    /// each root and closes the port once the cancelled reads have been dequeued, so
    /// a read cannot outlive its buffer. It doesn't join the thread, because a
    /// handler may call this while holding the config lock that the thread also
    /// takes. The thread keeps the watcher alive by itself.
    fn shutdown(&self) {
        let watches = self.watches.lock().unwrap();
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        for watch in watches.iter() {
            // SAFETY: the handle is in `watches`, so it is still open. Null cancels
            // every read on it; the call fails harmlessly if none is pending.
            unsafe { CancelIoEx(watch.handle, null()) };
        }
        // Posted while the lock is held: the thread cannot exit, and so cannot close
        // the port, before it has seen the list empty under this same lock.
        // SAFETY: the port is open until the thread exits, which needs this lock.
        unsafe { PostQueuedCompletionStatus(self.iocp, 0, 0, null()) };
        drop(watches);
        self.changes.notify();
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    fn refresh_config(&self) {}
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::*;
    use crate::config::Config;
    use crate::pending::PendingChanges;

    const WAIT: Duration = Duration::from_secs(20);

    /// Ignores any path whose last component is `name`.
    struct IgnoreName(&'static str);
    impl Filter for IgnoreName {
        fn ignore_path(&self, path: &Path, _is_dir: Option<bool>) -> bool {
            path.file_name().is_some_and(|n| n == self.0)
        }
    }

    fn watcher(filter: Arc<dyn Filter>) -> Arc<WindowsWatcher> {
        let state = Arc::new(WatcherState {
            config: Mutex::new(Config {
                filter,
                settle_time: Duration::from_millis(200),
                handlers: Vec::new(),
            }),
            new_roots: Mutex::new(Vec::new()),
            has_new_roots: AtomicBool::new(false),
            started: AtomicBool::new(false),
        });
        WindowsWatcher::new(state).unwrap()
    }

    fn canonical(dir: &TempDir) -> CanonicalPathBuf {
        CanonicalPathBuf::assert_canonicalized(&dir.path().canonicalize().unwrap())
    }

    /// Drains the queue until `done` accepts the changes seen so far or `WAIT`
    /// passes. Returns every path seen and whether a recrawl was requested.
    fn collect(
        w: &WindowsWatcher,
        done: impl Fn(&[CanonicalPathBuf], bool) -> bool,
    ) -> (Vec<CanonicalPathBuf>, bool) {
        let deadline = Instant::now() + WAIT;
        let (mut seen, mut recrawled) = (Vec::new(), false);
        while !done(&seen, recrawled) && Instant::now() < deadline {
            let mut batch = PendingChanges::default();
            if !w
                .changes
                .take(&mut batch, Some(Duration::from_millis(100)), || false)
            {
                recrawled |= batch.take_recrawl();
                seen.extend(batch.drain().map(|c| c.path));
            }
        }
        (seen, recrawled)
    }

    fn record(action: u32, name: &[u16], next: u32) -> Vec<u8> {
        let mut out = next.to_ne_bytes().to_vec();
        out.extend(action.to_ne_bytes());
        out.extend((name.len() as u32 * 2).to_ne_bytes());
        out.extend(name.iter().flat_map(|u| u.to_ne_bytes()));
        out
    }

    #[test]
    fn parse_keeps_lone_surrogates() {
        let root = CanonicalPathBuf::assert_canonicalized(Path::new(r"C:\root"));
        let name = [0x61, 0xD800, 0x62];
        let buf = record(FILE_ACTION_ADDED, &name, 0);
        let mut out = Vec::new();
        parse_records(&root, &buf, &(), &mut out);
        assert_eq!(out, [(root.join(&OsString::from_wide(&name)), true)]);
    }

    #[test]
    fn parse_survives_truncated_records() {
        let root = CanonicalPathBuf::assert_canonicalized(Path::new(r"C:\root"));
        let mut buf = record(FILE_ACTION_MODIFIED, &[0x61, 0x62], 0);
        buf.truncate(buf.len() - 1);
        let mut out = Vec::new();
        parse_records(&root, &buf, &(), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn parse_drops_ignored_directories() {
        let root = CanonicalPathBuf::assert_canonicalized(Path::new(r"C:\root"));
        let name: Vec<u16> = r"target\x.o".encode_utf16().collect();
        let buf = record(FILE_ACTION_MODIFIED, &name, 0);
        let mut out = Vec::new();
        parse_records(&root, &buf, &IgnoreName("target"), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn root_is_deleted_and_recreated() {
        let w = watcher(Arc::new(()));
        let dir = TempDir::new().unwrap();
        let sub = dir.path().canonicalize().unwrap().join("root");
        fs::create_dir(&sub).unwrap();
        let sub_path = CanonicalPathBuf::assert_canonicalized(&sub);
        w.watch_dir(sub_path.clone(), true).unwrap();

        fs::remove_dir(&sub).unwrap();
        let (_, recrawled) = collect(&w, |_, recrawled| recrawled);
        assert!(recrawled, "deleting the root requests a recrawl");
        assert!(
            w.watches.lock().unwrap().is_empty(),
            "the dead root is released before the recrawl is requested"
        );

        // The directory stays delete-pending until its last handle closes.
        let deadline = Instant::now() + WAIT;
        while fs::create_dir(&sub).is_err() {
            assert!(Instant::now() < deadline, "could not recreate the root");
            thread::sleep(Duration::from_millis(50));
        }
        w.watch_dir(sub_path, true).unwrap();
        fs::write(sub.join("again.txt"), "x").unwrap();
        let (seen, _) = collect(&w, |seen, _| {
            seen.iter().any(|p| p.as_std_path().ends_with("again.txt"))
        });
        assert!(
            seen.iter().any(|p| p.as_std_path().ends_with("again.txt")),
            "events resume after the root was recreated: {seen:?}"
        );
        w.shutdown();
    }

    #[test]
    fn ignored_directories_queue_nothing() {
        let w = watcher(Arc::new(IgnoreName("target")));
        let dir = TempDir::new().unwrap();
        let root = canonical(&dir);
        fs::create_dir(dir.path().join("target")).unwrap();
        w.watch_dir(root, true).unwrap();

        fs::write(dir.path().join("target").join("x.o"), "x").unwrap();
        fs::write(dir.path().join("visible.txt"), "x").unwrap();
        let (seen, _) = collect(&w, |seen, _| {
            seen.iter()
                .any(|p| p.as_std_path().ends_with("visible.txt"))
        });
        assert!(seen
            .iter()
            .any(|p| p.as_std_path().ends_with("visible.txt")));
        assert!(
            !seen.iter().any(|p| p
                .as_std_path()
                .components()
                .any(|c| c.as_os_str() == "target")),
            "nothing under `target` is queued: {seen:?}"
        );
        w.shutdown();
    }

    #[test]
    fn recursive_request_adds_a_watch() {
        let w = watcher(Arc::new(()));
        let dir = TempDir::new().unwrap();
        let root = canonical(&dir);
        fs::create_dir(dir.path().join("sub")).unwrap();

        w.watch_dir(root.clone(), false).unwrap();
        w.watch_dir(root.clone(), false).unwrap();
        assert_eq!(w.watches.lock().unwrap().len(), 1);
        w.watch_dir(root.clone(), true).unwrap();
        assert_eq!(w.watches.lock().unwrap().len(), 2);
        w.watch_dir(root.clone(), true).unwrap();
        w.watch_dir(root, false).unwrap();
        assert_eq!(w.watches.lock().unwrap().len(), 2);

        fs::write(dir.path().join("sub").join("f.txt"), "x").unwrap();
        let (seen, _) = collect(&w, |seen, _| {
            seen.iter().any(|p| p.as_std_path().ends_with("f.txt"))
        });
        assert!(
            seen.iter().any(|p| p.as_std_path().ends_with("f.txt")),
            "the subtree is watched: {seen:?}"
        );
        w.shutdown();
    }

    #[test]
    fn shutdown_releases_everything() {
        let w = watcher(Arc::new(()));
        let dir = TempDir::new().unwrap();
        w.watch_dir(canonical(&dir), true).unwrap();
        w.shutdown();
        w.shutdown();
        assert!(w.watch_dir(canonical(&dir), false).is_err());

        // The thread's own `Arc` is the last other reference, and it is dropped only
        // when the thread exits.
        let deadline = Instant::now() + WAIT;
        while Arc::strong_count(&w) > 1 {
            assert!(Instant::now() < deadline, "completion thread did not exit");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(w.watches.lock().unwrap().is_empty());
    }
}
