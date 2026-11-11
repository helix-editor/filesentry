use std::io;

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
pub use rustix::fs::inotify::ReadFlags as EventFlags;
use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
use rustix::io::Errno;

#[derive(Debug)]
pub(super) struct Inotify {
    fd: OwnedFd,
}

impl Inotify {
    pub(super) fn new() -> io::Result<Inotify> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        Ok(Inotify { fd })
    }

    /// Returns the watch descriptor. Watching a directory that is already
    /// watched returns its existing descriptor.
    pub(super) fn add_directory_watch(&self, path: impl rustix::path::Arg) -> io::Result<i32> {
        inotify::add_watch(
            self.as_fd(),
            path,
            WatchFlags::ATTRIB
                | WatchFlags::CREATE
                | WatchFlags::DELETE
                | WatchFlags::DELETE_SELF
                | WatchFlags::MODIFY
                | WatchFlags::MOVE_SELF
                | WatchFlags::MOVE
                | WatchFlags::DONT_FOLLOW
                | WatchFlags::EXCL_UNLINK
                | WatchFlags::ONLYDIR,
        )
        .map_err(|err| {
            if err == Errno::NOSPC {
                io::Error::other("exhaused inotify max_user_watches, try increasing the setting or adding stricter glob filter")
            } else {
                err.into()
            }
        })
    }

    pub(super) fn remove_watch(&self, wd: i32) -> io::Result<()> {
        inotify::remove_watch(self.as_fd(), wd)?;
        Ok(())
    }
}

impl AsFd for Inotify {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
