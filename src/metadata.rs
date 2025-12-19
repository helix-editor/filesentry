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

        let mtime = if stat.st_mtime >= 0 {
            SystemTime::UNIX_EPOCH + Duration::new(stat.st_mtime as u64, stat.st_mtime_nsec as u32)
        } else {
            SystemTime::UNIX_EPOCH
                - Duration::new((-stat.st_mtime) as u64, stat.st_mtime_nsec as u32)
        };
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
    pub fn for_path(_path: &CannonicalPath) -> Option<Metadata> {
        unimplemented!("Windows file metadata not yet implemented")
    }
}
