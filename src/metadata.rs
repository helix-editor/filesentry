use std::time::SystemTime;

use crate::path::CannonicalPath;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Metadata {
    pub is_dir: bool,
    pub mtime: SystemTime,
    pub size: usize,
    pub inode: u64,
}

impl Metadata {
    #[cfg(unix)]
    pub fn for_path(path: &CannonicalPath) -> Option<Metadata> {
        use std::time::Duration;

        use rustix::fs::{lstat, FileType};
        use rustix::io::Errno;

        let stat = match lstat(path) {
            Ok(stat) => stat,
            Err(Errno::NOTDIR | Errno::NOENT) => {
                return None;
            }
            Err(err) => {
                log::error!("failed to stat {path:?}: {err}");
                return None;
            }
        };

        let secs = Duration::from_secs(stat.st_mtime.unsigned_abs());
        let mtime = if stat.st_mtime >= 0 {
            SystemTime::UNIX_EPOCH + secs
        } else {
            SystemTime::UNIX_EPOCH - secs
        } + Duration::from_nanos(stat.st_mtime_nsec as u64);
        let is_dir = match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => false,
            FileType::Directory => true,
            _ => return None,
        };
        Some(Metadata {
            is_dir,
            mtime,
            size: stat.st_size as usize,
            inode: stat.st_ino,
        })
    }

    #[cfg(windows)]
    pub fn for_path(path: &CannonicalPath) -> Option<Metadata> {
        use std::ffi::c_void;

        use windows_sys::Win32::Storage::FileSystem::{
            GetFileAttributesExW, GetFileExInfoStandard, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, WIN32_FILE_ATTRIBUTE_DATA,
        };

        let wide = path.to_wide();
        let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
        // A single attribute query, like unix `lstat`, which doesn't open a handle
        // or follow reparse points. It returns 0 (e.g. FILE_NOT_FOUND) when the path
        // is gone, which maps to `None` like `ENOENT`/`ENOTDIR`.
        let ok = unsafe {
            GetFileAttributesExW(
                wide.as_ptr(),
                GetFileExInfoStandard,
                &mut data as *mut _ as *mut c_void,
            )
        };
        if ok == 0 {
            return None;
        }
        let attrs = data.dwFileAttributes;
        // Treat symlinks/junctions as unwatchable, mirroring unix `lstat` reporting
        // the link itself (a `FileType` the tree maps to `None`).
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return None;
        }
        let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
        let size = (((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64) as usize;
        Some(Metadata {
            is_dir,
            mtime: filetime_to_system_time(&data.ftLastWriteTime),
            size,
            // A stable file index requires opening a handle; ReadDirectoryChangesW
            // already delivers precise create/rename records, so the tree's
            // inode-change heuristic isn't needed here and a constant is fine.
            inode: 0,
        })
    }
}

/// Convert a Win32 `FILETIME` (100-ns ticks since 1601-01-01) to a `SystemTime`.
#[cfg(windows)]
fn filetime_to_system_time(ft: &windows_sys::Win32::Foundation::FILETIME) -> SystemTime {
    use std::time::Duration;

    // 100-ns ticks between the FILETIME epoch (1601) and the Unix epoch (1970).
    const EPOCH_DIFF_TICKS: u64 = 11_644_473_600 * 10_000_000;
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    if ticks >= EPOCH_DIFF_TICKS {
        SystemTime::UNIX_EPOCH + Duration::from_nanos((ticks - EPOCH_DIFF_TICKS) * 100)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_nanos((EPOCH_DIFF_TICKS - ticks) * 100)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::Metadata;
    use crate::path::CanonicalPathBuf;

    /// Before 1970 `st_mtime` is negative while `st_mtime_nsec` still counts
    /// forward from it.
    #[test]
    fn pre_epoch_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let mtime = SystemTime::UNIX_EPOCH - Duration::from_millis(1500);
        std::fs::File::create(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let meta = Metadata::for_path(&CanonicalPathBuf::assert_canonicalized(&path)).unwrap();
        assert_eq!(meta.mtime, mtime);
    }
}
