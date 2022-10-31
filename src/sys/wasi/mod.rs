//! # Notes
//!
//! The current implementation is somewhat limited. The `Waker` is not
//! implemented, as at the time of writing there is no way to support to wake-up
//! a thread from calling `poll_oneoff`.
//!
//! Furthermore the (re/de)register functions also don't work while concurrently
//! polling as both registering and polling requires a lock on the
//! `subscriptions`.
//!
//! Finally `Selector::try_clone`, required by `Registry::try_clone`, doesn't
//! work. However this could be implemented by use of an `Arc`.
//!
//! In summary, this only (barely) works using a single thread.

use std::cmp::min;
use std::io;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(target_vendor = "wasmer")]
use ::wasix as wasi;

use std::fs::File;
use std::os::wasi::io::FromRawFd;
use std::convert::TryInto;
use std::io::Write;

#[cfg(all(feature = "net", target_vendor = "unknown"))]
use crate::{Interest, Token};

#[cfg(target_vendor = "unknown")]
cfg_net! {
    pub(crate) mod tcp {
        use std::io;
        use std::net::{self, SocketAddr};

        pub(crate) fn accept(listener: &net::TcpListener) -> io::Result<(net::TcpStream, SocketAddr)> {
            let (stream, addr) = listener.accept()?;
            stream.set_nonblocking(true)?;
            Ok((stream, addr))
        }
    }
}

#[cfg(target_vendor = "wasmer")]
cfg_os_poll! {
    pub(crate) mod sourcefd;
    pub use self::sourcefd::SourceFd;
    
    pub(crate) mod waker;
    pub(crate) use self::waker::Waker;

    cfg_net! {
        mod net;
        pub(crate) mod tcp;
        pub(crate) mod udp;
        pub(crate) mod pipe;
    }
}

/// Unique id for use as `SelectorId`.
#[cfg(all(debug_assertions, feature = "net"))]
static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

pub struct Selector {
    #[cfg(all(debug_assertions, feature = "net"))]
    id: usize,
    /// Subscriptions (reads events) we're interested in.
    subscriptions: Arc<Mutex<Vec<wasi::Subscription>>>,
    #[cfg(debug_assertions)]
    has_waker: std::sync::atomic::AtomicBool,
    /// This file is used to wake up the poll selector
    /// and stall it while management actions are taken
    /// (like adding new subscriptions)
    stall: Arc<Mutex<File>>,
}

impl fmt::Debug
for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "selector")
    }
}

impl Selector {
    pub fn new() -> io::Result<Selector> {
        let fd = unsafe {
            wasi::fd_event(0, 0)
                .map_err(|errno| io::Error::from_raw_os_error(errno.raw() as i32))?
        };
        let fdstat = unsafe {
            wasi::fd_fdstat_get(fd)
                .map_err(|errno| io::Error::from_raw_os_error(errno.raw() as i32))?
        };
    
        let mut flags = fdstat.fs_flags;
        flags |= wasi::FDFLAGS_NONBLOCK;
        unsafe {
            wasi::fd_fdstat_set_flags(fd, flags)
                .map_err(|errno| io::Error::from_raw_os_error(errno.raw() as i32))?
        }
        let file = unsafe { File::from_raw_fd(fd.try_into().unwrap()) };

        let subscriptions = vec![
            wasi::Subscription {
                userdata: u64::MAX,
                u: wasi::SubscriptionU {
                    tag: wasi::EVENTTYPE_FD_READ.raw(),
                    u: wasi::SubscriptionUU {
                        fd_read: wasi::SubscriptionFdReadwrite {
                            file_descriptor: fd,
                        },
                    },
                },
            }
        ];
        Ok(Selector {
            #[cfg(all(debug_assertions, feature = "net"))]
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            subscriptions: Arc::new(Mutex::new(subscriptions)),
            #[cfg(debug_assertions)]
            has_waker: std::sync::atomic::AtomicBool::new(false),
            stall: Arc::new(Mutex::new(file)),
        })
    }

    pub fn try_clone(&self) -> io::Result<Selector> {
        Ok(
            Selector {
                #[cfg(all(debug_assertions, feature = "net"))]
                id: self.id,
                subscriptions: Arc::clone(&self.subscriptions),
                #[cfg(debug_assertions)]
                has_waker: std::sync::atomic::AtomicBool::new(self.has_waker.load(
                    std::sync::atomic::Ordering::Acquire)),
                stall: self.stall.clone(),
            }
        )
    }

    #[cfg(all(debug_assertions, feature = "net"))]
    pub fn id(&self) -> usize {
        self.id
    }

    pub fn select(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<()> {
        events.clear();

        loop
        {
            {
                let stall = self.stall.lock().unwrap();
                drop(stall);
            }

            let mut subscriptions = self.subscriptions.lock().unwrap();

            // If we want to a use a timeout in the `wasi_poll_oneoff()` function
            // we need another subscription to the list.
            if let Some(timeout) = timeout {
                subscriptions.push(timeout_subscription(timeout));
            }

            // `poll_oneoff` needs the same number of events as subscriptions.
            let length = subscriptions.len();
            events.reserve(length);

            debug_assert!(events.capacity() >= length);

            let res = unsafe { wasi::poll_oneoff(subscriptions.as_ptr(), events.as_mut_ptr(), length) };

            // If this is a stall event
            if let Ok(n_events) = res {
                if n_events == 1 {
                    unsafe { events.set_len(1) };
                    let evt = events[0];
                    if evt.userdata == u64::MAX {
                        // We release the lock and allow any stalled events to add new subscriptions
                        // (without triggering the poll itself)
                        continue;
                    }
                }
            }

            // Remove the timeout subscription we possibly added above.
            if timeout.is_some() {
                let timeout_sub = subscriptions.pop();
                debug_assert_eq!(
                    timeout_sub.unwrap().u.tag,
                    wasi::EVENTTYPE_CLOCK.raw(),
                    "failed to remove timeout subscription"
                );
            }

            drop(subscriptions); // Unlock.

            return match res {
                Ok(n_events) => {
                    // Safety: `poll_oneoff` initialises the `events` for us.
                    unsafe { events.set_len(n_events) };

                    // Remove the timeout event.
                    if timeout.is_some() {
                        if let Some(index) = events.iter().position(is_timeout_event) {
                            events.swap_remove(index);
                        }
                    }

                    check_errors(&events)
                }
                Err(err) => Err(io_err(err)),
            };
        }
    }

    fn stall<'a>(&'a self) -> io::Result<std::sync::MutexGuard<'a, File>> {
        // we stall the select and wake it up so that it can process
        // the new subscriptions
        let mut stall = self.stall.lock().unwrap();

        let buf: [u8; 8] = 1u64.to_ne_bytes();
        stall.write(&buf)?;

        Ok(stall)
    }

    #[cfg(any(feature = "net", feature = "os-poll"))]
    pub fn register(
        &self,
        fd: wasi::Fd,
        token: crate::Token,
        interests: crate::Interest,
    ) -> io::Result<()> {

        // we stall the select and wake it up so that it can process
        // the new subscriptions
        let _stall = self.stall()?;

        let mut subscriptions = self.subscriptions.lock().unwrap();

        log::trace!(
            "select::register: fd={:?}, token={:?}, interests={:?}",
            fd,
            token,
            interests
        );

        if interests.is_writable() {
            let subscription = wasi::Subscription {
                userdata: token.0 as wasi::Userdata,
                u: wasi::SubscriptionU {
                    tag: wasi::EVENTTYPE_FD_WRITE.raw(),
                    u: wasi::SubscriptionUU {
                        fd_write: wasi::SubscriptionFdReadwrite {
                            file_descriptor: fd,
                        },
                    },
                },
            };
            subscriptions.push(subscription);
        }

        if interests.is_readable() {
            let subscription = wasi::Subscription {
                userdata: token.0 as wasi::Userdata,
                u: wasi::SubscriptionU {
                    tag: wasi::EVENTTYPE_FD_READ.raw(),
                    u: wasi::SubscriptionUU {
                        fd_read: wasi::SubscriptionFdReadwrite {
                            file_descriptor: fd,
                        },
                    },
                },
            };
            subscriptions.push(subscription);
        }

        Ok(())
    }

    #[cfg(any(feature = "net", feature = "os-poll"))]
    pub fn reregister(
        &self,
        fd: wasi::Fd,
        token: crate::Token,
        interests: crate::Interest,
    ) -> io::Result<()> {
        log::trace!(
            "select::reregister: fd={:?}, token={:?}, interests={:?}",
            fd,
            token,
            interests
        );

        self.deregister(fd)
            .and_then(|()| self.register(fd, token, interests))
    }

    #[cfg(any(feature = "net", feature = "os-poll"))]
    pub fn deregister(&self, fd: wasi::Fd) -> io::Result<()> {
        log::trace!(
            "select::deregister: fd={:?}",
            fd,
        );

        // we stall the select and wake it up so that it can process
        // the new subscriptions
        let _stall = self.stall()?;

        let mut subscriptions = self.subscriptions.lock().unwrap();

        let predicate = |subscription: &wasi::Subscription| {
            // Safety: `subscription.u.tag` defines the type of the union in
            // `subscription.u.u`.
            match subscription.u.tag {
                t if t == wasi::EVENTTYPE_FD_WRITE.raw() => unsafe {
                    subscription.u.u.fd_write.file_descriptor == fd
                },
                t if t == wasi::EVENTTYPE_FD_READ.raw() => unsafe {
                    subscription.u.u.fd_read.file_descriptor == fd
                },
                _ => false,
            }
        };

        let mut ret = Err(io::ErrorKind::NotFound.into());

        while let Some(index) = subscriptions.iter().position(predicate) {
            subscriptions.swap_remove(index);
            ret = Ok(())
        }

        ret
    }

    #[cfg(debug_assertions)]
    pub fn register_waker(&self) -> bool {
        self.has_waker.swap(true, std::sync::atomic::Ordering::AcqRel)
    }
}

/// Token used to a add a timeout subscription, also used in removing it again.
const TIMEOUT_TOKEN: wasi::Userdata = wasi::Userdata::max_value();

/// Returns a `wasi::Subscription` for `timeout`.
fn timeout_subscription(timeout: Duration) -> wasi::Subscription {
    wasi::Subscription {
        userdata: TIMEOUT_TOKEN,
        u: wasi::SubscriptionU {
            tag: wasi::EVENTTYPE_CLOCK.raw(),
            u: wasi::SubscriptionUU {
                clock: wasi::SubscriptionClock {
                    id: wasi::CLOCKID_MONOTONIC,
                    // Timestamp is in nanoseconds.
                    timeout: min(wasi::Timestamp::MAX as u128, timeout.as_nanos())
                        as wasi::Timestamp,
                    // Give the implementation another millisecond to coalesce
                    // events.
                    precision: Duration::from_millis(1).as_nanos() as wasi::Timestamp,
                    // Zero means the `timeout` is considered relative to the
                    // current time.
                    flags: 0,
                },
            },
        },
    }
}

fn is_timeout_event(event: &wasi::Event) -> bool {
    event.type_ == wasi::EVENTTYPE_CLOCK && event.userdata == TIMEOUT_TOKEN
}

/// Check all events for possible errors, it returns the first error found.
fn check_errors(events: &[Event]) -> io::Result<()> {
    for event in events {
        if event.error != wasi::ERRNO_SUCCESS {
            return Err(io_err(event.error));
        }
    }
    Ok(())
}

/// Convert `wasi::Errno` into an `io::Error`.
fn io_err(errno: wasi::Errno) -> io::Error {
    // TODO: check if this is valid.
    io::Error::from_raw_os_error(errno.raw() as i32)
}

pub(crate) type Events = Vec<Event>;

pub(crate) type Event = wasi::Event;

pub(crate) mod event {
    use std::fmt;
    #[cfg(target_vendor = "wasmer")]
    use ::wasix as wasi;

    use crate::sys::Event;
    use crate::Token;

    pub(crate) fn token(event: &Event) -> Token {
        Token(event.userdata as usize)
    }

    pub(crate) fn is_readable(event: &Event) -> bool {
        event.type_ == wasi::EVENTTYPE_FD_READ
    }

    pub(crate) fn is_writable(event: &Event) -> bool {
        event.type_ == wasi::EVENTTYPE_FD_WRITE
    }

    pub(crate) fn is_error(_: &Event) -> bool {
        // Not supported? It could be that `wasi::Event.error` could be used for
        // this, but the docs say `error that occurred while processing the
        // subscription request`, so it's checked in `Select::select` already.
        false
    }

    pub(crate) fn is_read_closed(event: &Event) -> bool {
        event.type_ == wasi::EVENTTYPE_FD_READ
            // Safety: checked the type of the union above.
            && (event.fd_readwrite.flags & wasi::EVENTRWFLAGS_FD_READWRITE_HANGUP) != 0
    }

    pub(crate) fn is_write_closed(event: &Event) -> bool {
        event.type_ == wasi::EVENTTYPE_FD_WRITE
            // Safety: checked the type of the union above.
            && (event.fd_readwrite.flags & wasi::EVENTRWFLAGS_FD_READWRITE_HANGUP) != 0
    }

    pub(crate) fn is_priority(_: &Event) -> bool {
        // Not supported.
        false
    }

    pub(crate) fn is_aio(_: &Event) -> bool {
        // Not supported.
        false
    }

    pub(crate) fn is_lio(_: &Event) -> bool {
        // Not supported.
        false
    }

    pub(crate) fn debug_details(f: &mut fmt::Formatter<'_>, event: &Event) -> fmt::Result {
        debug_detail!(
            TypeDetails(wasi::Eventtype),
            PartialEq::eq,
            wasi::EVENTTYPE_CLOCK,
            wasi::EVENTTYPE_FD_READ,
            wasi::EVENTTYPE_FD_WRITE,
        );

        #[allow(clippy::trivially_copy_pass_by_ref)]
        fn check_flag(got: &wasi::Eventrwflags, want: &wasi::Eventrwflags) -> bool {
            (got & want) != 0
        }
        debug_detail!(
            EventrwflagsDetails(wasi::Eventrwflags),
            check_flag,
            wasi::EVENTRWFLAGS_FD_READWRITE_HANGUP,
        );

        struct EventFdReadwriteDetails(wasi::EventFdReadwrite);

        impl fmt::Debug for EventFdReadwriteDetails {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct("EventFdReadwrite")
                    .field("nbytes", &self.0.nbytes)
                    .field("flags", &self.0.flags)
                    .finish()
            }
        }

        f.debug_struct("Event")
            .field("userdata", &event.userdata)
            .field("error", &event.error)
            .field("type", &TypeDetails(event.type_))
            .field("fd_readwrite", &EventFdReadwriteDetails(event.fd_readwrite))
            .finish()
    }
}

cfg_os_poll! {
    cfg_io_source! {
        pub(crate) struct IoSourceState;

        impl IoSourceState {
            pub(crate) fn new() -> IoSourceState {
                IoSourceState
            }

            pub(crate) fn do_io<T, F, R>(&self, f: F, io: &T) -> io::Result<R>
            where
                F: FnOnce(&T) -> io::Result<R>,
            {
                // We don't hold state, so we can just call the function and
                // return.
                f(io)
            }
        }
    }
}
