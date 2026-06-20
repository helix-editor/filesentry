//! Windows `ReadDirectoryChangesW` backend.
//!
//! Design: one directory handle per recursive root,
//! opened with `FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED`, associated
//! with an IOCP keyed by root index, and driven by `ReadDirectoryChangesW(..,
//! watch_subtree = TRUE, ..)` on a dedicated completion thread. Each root keeps a
//! heap-pinned read buffer + `OVERLAPPED`; on completion we parse the
//! `FILE_NOTIFY_INFORMATION` records and re-issue the read. It reports per-file
//! change records, so it feeds the *precise-path* path: create/delete/rename →
//! `NEEDS_RECURSIVE_CRAWL`, modify → empty flags. A zero-byte completion (buffer
//! overflow) maps to a recrawl, mirroring inotify's `QUEUE_OVERFLOW`.

use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{Arc, Mutex};
use std::{io, thread};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
    FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetOverlappedResult, GetQueuedCompletionStatus,
    PostQueuedCompletionStatus, OVERLAPPED,
};

use crate::backend::Backend;
use crate::path::CanonicalPathBuf;
use crate::pending::{self, PendingChangesLock};
use crate::WatcherState;

const NOTIFY_FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_CREATION;

/// 64 KiB is the documented max ReadDirectoryChangesW buffer for network drives;
/// it's a good local default too. On overflow we recrawl, so this is not a
/// correctness limit.
const BUF_LEN: usize = 64 * 1024;

/// Per-root watch state. Boxed so its `buffer`/`overlapped` have a stable address
/// while a read is in flight (the kernel writes into them asynchronously), even
/// as the `roots` Vec grows.
struct Root {
    path: CanonicalPathBuf,
    handle: HANDLE,
    buffer: Vec<u8>,
    overlapped: OVERLAPPED,
    /// Whether to watch the whole subtree (passed to `ReadDirectoryChangesW`).
    recursive: bool,
}

pub(crate) struct WindowsWatcher {
    shutdown: AtomicBool,
    iocp: HANDLE,
    roots: Mutex<Vec<Box<Root>>>,
    pub changes: PendingChangesLock,
    state: Arc<WatcherState>,
}

// Raw HANDLEs are touched only under `roots`'s lock / are kernel-thread-safe; the
// watcher is shared between the worker and the completion thread.
unsafe impl Send for WindowsWatcher {}
unsafe impl Sync for WindowsWatcher {}

impl std::fmt::Debug for WindowsWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsWatcher").finish_non_exhaustive()
    }
}

impl WindowsWatcher {
    pub fn new(#[cfg(test)] _slow: bool, state: Arc<WatcherState>) -> io::Result<Arc<Self>> {
        let iocp = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, 0 as HANDLE, 0, 0) };
        if iocp.is_null() {
            return Err(io::Error::last_os_error());
        }
        let watcher = Arc::new(Self {
            shutdown: AtomicBool::new(false),
            iocp,
            roots: Mutex::new(Vec::new()),
            changes: PendingChangesLock::default(),
            state,
        });
        let watcher_ = watcher.clone();
        thread::Builder::new()
            .name("filesentry-rdcw".into())
            .spawn(move || watcher_.completion_loop())?;
        Ok(watcher)
    }

    fn completion_loop(self: Arc<Self>) {
        loop {
            let mut bytes: u32 = 0;
            let mut key: usize = 0;
            let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
            let ok = unsafe {
                GetQueuedCompletionStatus(
                    self.iocp,
                    &mut bytes,
                    &mut key,
                    &mut overlapped,
                    u32::MAX, // INFINITE
                )
            };
            if self.is_shutdown() {
                break;
            }
            if ok == 0 {
                // A non-null `overlapped` means a queued read was dequeued but failed
                // (the wakeup post and a GQCS failure such as the shutdown close both
                // come through with `overlapped` null). A failed read most commonly
                // means the watched root was deleted or renamed, invalidating its
                // handle, so recrawl to re-stat the roots and report the removal --
                // inotify does the same on DELETE_SELF.
                if !overlapped.is_null() {
                    self.changes.lock().recrawl();
                    self.changes.notify();
                }
                continue;
            }
            let mut roots = self.roots.lock().unwrap();
            let Some(root) = roots.get_mut(key) else {
                continue;
            };
            if bytes == 0 {
                // Overflow: the buffer was too small for the batch — recrawl.
                self.changes.lock().recrawl();
                self.changes.notify();
            } else {
                // SAFETY: the kernel wrote `bytes` of FILE_NOTIFY_INFORMATION
                // records into `root.buffer`.
                let buf = root.buffer[..bytes as usize].to_vec();
                let path = root.path.clone();
                drop(roots); // don't hold the lock across change processing
                self.handle_records(&path, &buf);
                roots = self.roots.lock().unwrap();
                let Some(root) = roots.get_mut(key) else {
                    continue;
                };
                let _ = issue_read(root); // re-arm; errors drop this root's watch
                continue;
            }
            // re-arm after an overflow as well
            if let Some(root) = roots.get_mut(key) {
                let _ = issue_read(root);
            }
        }
    }

    /// Translate a buffer of `FILE_NOTIFY_INFORMATION` records into pending
    /// changes. Mirrors `InotifyWatcher::handle_event`'s flag mapping.
    ///
    /// Parsed with bounds-checked byte reads rather than by forming a
    /// `&FILE_NOTIFY_INFORMATION`: `buf` is a `Vec<u8>` (1-byte aligned), so a
    /// reference to the DWORD-containing struct could be under-aligned (UB), and a
    /// malformed `NextEntryOffset`/`FileNameLength` could otherwise read out of
    /// bounds. Layout: NextEntryOffset(u32), Action(u32), FileNameLength(u32, in
    /// bytes), FileName(WCHAR[]); the header is 12 bytes.
    fn handle_records(&self, root: &CanonicalPathBuf, buf: &[u8]) {
        const HEADER: usize = 12;
        let read_u32 = |b: &[u8]| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]);
        let filter = self.state.config.lock().unwrap().filter.clone();
        let mut changes = self.changes.lock();
        let mut offset = 0usize;
        while offset + HEADER <= buf.len() {
            let hdr = &buf[offset..offset + HEADER];
            let next_entry = read_u32(&hdr[0..4]) as usize;
            let action = read_u32(&hdr[4..8]);
            let name_len = read_u32(&hdr[8..12]) as usize; // in bytes
            let name_start = offset + HEADER;
            let name_end = name_start.saturating_add(name_len).min(buf.len());
            let name_u16: Vec<u16> = buf[name_start..name_end]
                .chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect();
            let name = String::from_utf16_lossy(&name_u16);
            let path = root.join(OsStr::new(&name));
            if !filter.ignore_path(path.as_std_path(), None) {
                match action {
                    FILE_ACTION_ADDED
                    | FILE_ACTION_REMOVED
                    | FILE_ACTION_RENAMED_OLD_NAME
                    | FILE_ACTION_RENAMED_NEW_NAME => {
                        changes.add_watcher(path, pending::Flags::NEEDS_RECURSIVE_CRAWL);
                    }
                    FILE_ACTION_MODIFIED => {
                        changes.add_watcher(path, pending::Flags::empty());
                    }
                    _ => {}
                }
            }
            if next_entry == 0 {
                break;
            }
            offset += next_entry;
        }
        drop(changes);
        self.changes.notify();
    }
}

impl Drop for WindowsWatcher {
    fn drop(&mut self) {
        if !self.is_shutdown() {
            self.shutdown();
        }
    }
}

/// (Re)issue the overlapped `ReadDirectoryChangesW` for a root.
fn issue_read(root: &mut Root) -> io::Result<()> {
    root.overlapped = unsafe { std::mem::zeroed() };
    let len = root.buffer.len() as u32;
    let ptr = root.buffer.as_mut_ptr() as *mut c_void;
    let ok = unsafe {
        ReadDirectoryChangesW(
            root.handle,
            ptr,
            len,
            root.recursive as i32, // watch_subtree
            NOTIFY_FILTER,
            std::ptr::null_mut(),
            &mut root.overlapped,
            None,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl crate::backend::Backend for WindowsWatcher {
    fn changes(&self) -> &PendingChangesLock {
        &self.changes
    }

    fn watch_dir(&self, path: CanonicalPathBuf, recursive: bool) -> io::Result<()> {
        // watch_subtree covers descendants, so only roots open a handle.
        let mut roots = self.roots.lock().unwrap();
        // Skip only if an existing watch already covers `path`: the exact directory,
        // or a *recursive* ancestor (a non-recursive ancestor does not watch this
        // path's subtree, so it must still get its own watch).
        if roots
            .iter()
            .any(|r| r.path == path || (r.recursive && r.path.is_parent_of(&path)))
        {
            return Ok(());
        }
        let wide: Vec<u16> = path
            .as_std_path()
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                0 as HANDLE,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let key = roots.len();
        if unsafe { CreateIoCompletionPort(handle, self.iocp, key, 0) }.is_null() {
            let err = io::Error::last_os_error();
            unsafe { CloseHandle(handle) };
            return Err(err);
        }
        let mut root = Box::new(Root {
            path,
            handle,
            buffer: vec![0u8; BUF_LEN],
            overlapped: unsafe { std::mem::zeroed() },
            recursive,
        });
        if let Err(err) = issue_read(&mut root) {
            // `Root` has no `Drop` and we never pushed it into `roots`, so close the
            // directory handle here or it (and its IOCP association) would leak.
            unsafe { CloseHandle(root.handle) };
            return Err(err);
        }
        roots.push(root);
        Ok(())
    }

    fn shutdown(&self) {
        if self.shutdown.swap(true, atomic::Ordering::Relaxed) {
            // Already shut down; the handles are closed once. A second call must not
            // run again or it would double-`CloseHandle` the IOCP (and root handles),
            // which can close an unrelated handle that reused the value.
            return;
        }
        // Wake the completion thread (it re-checks is_shutdown before touching `roots`).
        unsafe { PostQueuedCompletionStatus(self.iocp, 0, 0, std::ptr::null_mut()) };
        {
            let mut roots = self.roots.lock().unwrap();
            for root in roots.iter() {
                unsafe {
                    // A `ReadDirectoryChangesW` may still be in flight with the kernel
                    // holding pointers into `root.buffer`/`root.overlapped`. Cancel it and
                    // wait (GetOverlappedResult bWait=TRUE, which blocks on the file handle
                    // independently of the IOCP) before the `Box<Root>` is dropped, or the
                    // kernel could write into freed memory. `CancelIoEx` returns 0 when no
                    // read is pending — nothing to wait for then.
                    if CancelIoEx(root.handle, &root.overlapped) != 0 {
                        let mut transferred = 0u32;
                        GetOverlappedResult(
                            root.handle,
                            &root.overlapped,
                            &mut transferred,
                            1, // bWait = TRUE
                        );
                    }
                    CloseHandle(root.handle);
                }
            }
            // The kernel is done with every buffer/OVERLAPPED, so freeing the Boxes is safe.
            roots.clear();
        }
        unsafe { CloseHandle(self.iocp) };
        self.changes.notify();
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(atomic::Ordering::Relaxed)
    }

    fn refresh_config(&self) {}
}
