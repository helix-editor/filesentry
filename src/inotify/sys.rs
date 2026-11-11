use std::ffi::{c_int, OsStr};
use std::mem::{align_of, size_of, MaybeUninit};
use std::os::fd::AsRawFd;
use std::{io, slice};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll};
use rustix::fd::{AsFd, OwnedFd};
pub use rustix::fs::inotify::ReadFlags as EventFlags;
use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
use rustix::io::Errno;

const INOTIFY: mio::Token = mio::Token(0);
pub const MESSAGE: mio::Token = mio::Token(1);

#[derive(Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub(super) struct Watch(c_int);

impl Watch {
    pub fn is_invalid(&self) -> bool {
        self.0 < 0
    }
}

pub struct Event<'a> {
    pub wd: Watch,
    pub child: &'a OsStr,
    pub flags: EventFlags,
}

#[derive(Debug)]
pub(super) struct Inotify {
    fd: OwnedFd,
}

impl Inotify {
    pub(super) fn new() -> io::Result<Inotify> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        Ok(Inotify { fd })
    }

    pub(super) fn add_directory_watch(&self, path: impl rustix::path::Arg) -> io::Result<Watch> {
        let watch = inotify::add_watch(
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
        })?;
        Ok(Watch(watch))
    }

    pub(super) fn remove_watch(&self, watch: &Watch) -> io::Result<()> {
        inotify::remove_watch(self.as_fd(), watch.0)?;
        Ok(())
    }

    pub(super) fn event_loop<T>(
        &self,
        poll: &mut Poll,
        state: &mut T,
        mut handle_event: impl FnMut(&mut T, Event<'_>),
        mut event_stream_done: impl FnMut(&mut T),
        mut handle_message: impl FnMut(&mut T) -> bool,
    ) -> io::Result<()> {
        let mut buf = vec![0u32; BUFFERSIZE].into_boxed_slice();
        let buf_bytes = unsafe {
            slice::from_raw_parts_mut(
                buf.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                buf.len() * size_of::<u32>(),
            )
        };
        let mut reader = inotify::Reader::new(&self.fd, buf_bytes);
        let raw_fd = self.fd.as_raw_fd();
        let mut fd = SourceFd(&raw_fd);
        poll.registry()
            .register(&mut fd, INOTIFY, Interest::READABLE)?;
        let mut events = Events::with_capacity(16);
        loop {
            // Wait for something to happen.
            match poll.poll(&mut events, None) {
                Err(ref e) if matches!(e.kind(), std::io::ErrorKind::Interrupted) => {
                    // System call was interrupted, we will retry
                    // TODO: Not covered by tests (to reproduce likely need to setup signal handlers)
                }
                Err(e) => return Err(e),
                Ok(()) => {}
            }

            let mut message = false;
            let mut inotify = false;
            for event in &events {
                match event.token() {
                    INOTIFY => inotify = true,
                    MESSAGE => message = true,
                    _ => unreachable!(),
                }
            }
            events.clear();
            if message && handle_message(state) {
                break;
            }
            if inotify {
                loop {
                    let event = match reader.next() {
                        Err(Errno::AGAIN | Errno::INVAL) => break,
                        Ok(read) => read,
                        Err(err) => return Err(err.into()),
                    };
                    handle_event(
                        state,
                        Event {
                            wd: Watch(event.wd()),
                            child: event.file_name().map_or(OsStr::new(""), |src| unsafe {
                                OsStr::from_encoded_bytes_unchecked(src.to_bytes())
                            }),
                            flags: event.events(),
                        },
                    );
                }
                event_stream_done(state)
            }
        }
        Ok(())
    }
}

impl AsFd for Inotify {
    fn as_fd(&self) -> rustix::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

const ALIGNED_MAX_EVENT_SIZE: usize = (4 * size_of::<u32>() + 256) / align_of::<u32>();
const BUFFERSIZE: usize = 16 * 1024 * ALIGNED_MAX_EVENT_SIZE;
