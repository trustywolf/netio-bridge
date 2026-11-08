//! Link events: one rtnetlink socket wakes the port threads.
//!
//! The socket is subscribed to the link group of the network namespace, so the
//! kernel sends a message whenever an interface is added, renamed, changed or
//! deleted. Messages are not parsed: each one wakes every port, and each port
//! checks its own interface. Port threads therefore block with no timeout, and
//! an idle bridge does not wake up at all.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::log;
use crate::port::Port;

// From <linux/rtnetlink.h>, kept here like the packet socket constants.
const RTMGRP_LINK: u32 = 1;

/// An eventfd that wakes one port thread.
pub struct Wake(OwnedFd);

impl Wake {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain eventfd(2) call; the fd is owned right away.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh descriptor nobody else owns.
        Ok(Wake(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    pub fn notify(&self) {
        let one: u64 = 1;
        // SAFETY: writes the 8 bytes of one. It fails only when the counter
        // would overflow, and the port has been woken already then.
        unsafe { libc::write(self.fd(), (&one as *const u64).cast(), 8) };
    }

    /// Takes back every pending wake-up.
    pub fn clear(&self) {
        let mut n: u64 = 0;
        // SAFETY: reads 8 bytes into n; EAGAIN when nothing is pending.
        unsafe { libc::read(self.fd(), (&mut n as *mut u64).cast(), 8) };
    }

    /// Blocks until a wake-up is pending (or a signal interrupts the wait).
    pub fn wait(&self) {
        let mut p = libc::pollfd {
            fd: self.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: p is valid for the call.
        unsafe { libc::poll(&mut p, 1, -1) };
    }
}

/// The rtnetlink socket. It is bound before the port threads start, so a link
/// that appears while they start up is not missed.
pub struct Watcher(OwnedFd);

impl Watcher {
    pub fn open() -> io::Result<Self> {
        // SAFETY: plain socket(2) call; the fd is owned right away.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_ROUTE,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh descriptor nobody else owns.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: sockaddr_nl is plain old data; all-zero is a valid value.
        let mut addr: libc::sockaddr_nl = unsafe { mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_groups = RTMGRP_LINK;
        // SAFETY: addr is a valid sockaddr_nl of the size passed.
        let r = unsafe {
            libc::bind(
                fd,
                (&addr as *const libc::sockaddr_nl).cast(),
                mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Watcher(sock))
    }

    /// The watcher's thread: every message, or a lost one, wakes every port.
    pub fn run(self, ports: Arc<Vec<Port>>) {
        // Only the arrival matters; a longer message is truncated.
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: buf is valid for its length.
            let n =
                unsafe { libc::recv(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    // The queue overflowed and events were lost: every port
                    // checks, which covers whatever they were.
                    Some(libc::ENOBUFS) => {}
                    _ => {
                        log::error("netlink", &format!("recv on rtnetlink socket: {e}"));
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
            for p in ports.iter() {
                p.wake.notify();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(w: &Wake) -> bool {
        let mut p = libc::pollfd {
            fd: w.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: p is valid for the call.
        unsafe { libc::poll(&mut p, 1, 0) == 1 }
    }

    #[test]
    fn wake_up_and_clear() {
        let w = Arc::new(Wake::new().unwrap());
        assert!(!pending(&w));
        // Wake-ups add up; one clear takes them all back.
        w.notify();
        w.notify();
        assert!(pending(&w));
        w.clear();
        assert!(!pending(&w));
        // A waiter returns once notified.
        let n = {
            let w = w.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                w.notify();
            })
        };
        let t = std::time::Instant::now();
        w.wait();
        assert!(t.elapsed() >= Duration::from_millis(80));
        n.join().unwrap();
        // A wake-up sent before the wait is not lost.
        w.wait();
        w.clear();
    }

    #[test]
    fn watcher_opens_unprivileged() {
        // Joining the link group needs no capability.
        assert!(Watcher::open().is_ok());
    }
}
