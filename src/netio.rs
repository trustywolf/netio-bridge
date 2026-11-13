//! The IOL side: NETIO datagrams on a Unix socket under `/tmp/netio<uid>/`.
//!
//! Every datagram starts with an 8-byte header:
//!
//! ```text
//! dst_id (u16 BE) | src_id (u16 BE) | dst_port | src_port | msg_type | channel
//! ```
//!
//! A port byte encodes `Ethernet<slot>/<port>` as `(port << 4) | slot`.
//! Data frames carry `msg_type` 1 and channel 0, followed by the Ethernet frame.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::mem;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::log;
use crate::port::Port;
use crate::stats::UnixStats;

pub const HEADER_LEN: usize = 8;
pub const MSG_DATA: u8 = 1;

/// Largest datagram read from IOL.
const RX_SPACE: usize = 64 * 1024;
/// How long a port waits for room in IOL's receive queue before it drops the
/// frame. The queue holds only `net.unix.max_dgram_qlen` datagrams (10 in a
/// fresh network namespace), so a burst of broadcasts fills it while IOL is
/// still reading.
pub const SEND_TIMEOUT: Duration = Duration::from_millis(100);
/// How often [`wait_for_listener`] tries IOL's socket again.
const LISTEN_POLL: Duration = Duration::from_millis(50);

pub fn port_byte(slot: u8, port: u8) -> u8 {
    (port << 4) | slot
}

pub fn socket_dir() -> String {
    // SAFETY: getuid never fails.
    format!("/tmp/netio{}", unsafe { libc::getuid() })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub dst_id: u16,
    pub src_id: u16,
    pub dst_port: u8,
    pub src_port: u8,
    pub msg_type: u8,
    pub channel: u8,
}

impl Header {
    pub fn data(dst_id: u16, dst_port: u8, src_id: u16, src_port: u8) -> Self {
        Header {
            dst_id,
            src_id,
            dst_port,
            src_port,
            msg_type: MSG_DATA,
            channel: 0,
        }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let d = self.dst_id.to_be_bytes();
        let s = self.src_id.to_be_bytes();
        [
            d[0],
            d[1],
            s[0],
            s[1],
            self.dst_port,
            self.src_port,
            self.msg_type,
            self.channel,
        ]
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        Some(Header {
            dst_id: u16::from_be_bytes([buf[0], buf[1]]),
            src_id: u16::from_be_bytes([buf[2], buf[3]]),
            dst_port: buf[4],
            src_port: buf[5],
            msg_type: buf[6],
            channel: buf[7],
        })
    }
}

/// Takes the write lock on `<path>.lck` that keeps a second bridge with the same
/// id off the socket, and writes our PID into it. This is IOL's own convention
/// for its NETIO socket (`/tmp/netio0/1.lck`). The lock is held until the
/// returned file is closed or the process exits.
pub fn lock(path: &str) -> Result<File, String> {
    let name = format!("{path}.lck");
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&name)
        .map_err(|e| format!("open {name}: {e}"))?;
    // SAFETY: flock is plain old data; all-zero is a valid value.
    let mut fl: libc::flock = unsafe { mem::zeroed() };
    fl.l_type = libc::F_WRLCK as libc::c_short;
    fl.l_whence = libc::SEEK_SET as libc::c_short;
    // SAFETY: f is an open descriptor and fl is valid for the call.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETLK, &mut fl) } < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: as above; F_GETLK fills fl with the lock that blocks ours.
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETLK, &mut fl) } == 0 && fl.l_pid > 0 {
            return Err(format!(
                "{name} is locked by PID {}: another netio-bridge serves {path}",
                fl.l_pid
            ));
        }
        return Err(format!("lock {name}: {e}"));
    }
    f.set_len(0)
        .and_then(|()| writeln!(f, "{}", std::process::id()))
        .map_err(|e| format!("write {name}: {e}"))?;
    Ok(f)
}

/// A Unix datagram destination, prepared once and reused for every send.
pub struct UnixDest {
    addr: libc::sockaddr_un,
    len: libc::socklen_t,
    pub path: String,
}

impl UnixDest {
    pub fn new(path: String) -> io::Result<Self> {
        // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
        let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = path.as_bytes();
        if bytes.len() >= addr.sun_path.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("socket path too long: {path}"),
            ));
        }
        for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
            *dst = *src as libc::c_char;
        }
        let len =
            (mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
        Ok(UnixDest { addr, len, path })
    }
}

/// Sends one datagram. While IOL's receive queue is full the call waits, up to
/// the socket's send timeout ([`SEND_TIMEOUT`]); only the calling port's thread
/// waits.
pub fn send_to(sock: &UnixDatagram, buf: &[u8], dest: &UnixDest) -> io::Result<()> {
    // SAFETY: buf and dest.addr are valid for the lengths passed.
    let n = unsafe {
        libc::sendto(
            sock.as_raw_fd(),
            buf.as_ptr().cast(),
            buf.len(),
            0,
            (&dest.addr as *const libc::sockaddr_un).cast(),
            dest.len,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Waits until a socket is bound at `path` and returns how long that took.
/// The bridge and IOL start together, so a neighbour's frame can arrive before
/// IOL has opened its NETIO socket; ports start forwarding only after this.
/// A `connect()` reaches only a bound socket: it fails with `ENOENT` before IOL
/// creates the file and with `ECONNREFUSED` while a file left by an earlier
/// run is still there. Any other answer ends the wait: a socket is bound, or
/// the error is one that the ports' sends will report.
pub fn wait_for_listener(path: &str) -> io::Result<Duration> {
    let start = Instant::now();
    let probe = UnixDatagram::unbound()?;
    loop {
        match probe.connect(path) {
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ECONNREFUSED)) => {
                thread::sleep(LISTEN_POLL)
            }
            _ => return Ok(start.elapsed()),
        }
    }
}

/// Reads frames from IOL and hands each one to its port. Never touches port
/// state: a port that is down or gone only costs the frame.
pub fn run_reader(
    sock: Arc<UnixDatagram>,
    my_id: u16,
    ports: Arc<Vec<Port>>,
    stats: Arc<UnixStats>,
) {
    let mut by_byte: [Option<usize>; 256] = [None; 256];
    for (i, p) in ports.iter().enumerate() {
        by_byte[p.cfg.port_byte() as usize] = Some(i);
    }
    let mut buf = vec![0u8; RX_SPACE];
    loop {
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log::error("unix-recv", &format!("recv on NETIO socket: {e}"));
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        stats.rx.inc();
        let Some(h) = Header::decode(&buf[..n]) else {
            stats.short.inc();
            continue;
        };
        if h.msg_type != MSG_DATA {
            stats.not_data.inc();
            continue;
        }
        if h.dst_id != my_id {
            stats.wrong_dst.inc();
            continue;
        }
        let Some(i) = by_byte[h.dst_port as usize] else {
            stats.unknown_port.inc();
            continue;
        };
        ports[i].send_frame(&buf[HEADER_LEN..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_byte_encoding() {
        assert_eq!(port_byte(0, 0), 0x00);
        assert_eq!(port_byte(0, 1), 0x10);
        assert_eq!(port_byte(1, 0), 0x01);
        assert_eq!(port_byte(2, 3), 0x32);
        assert_eq!(port_byte(9, 3), 0x39);
    }

    #[test]
    fn header_round_trip() {
        let h = Header::data(1, port_byte(0, 2), 513, port_byte(0, 2));
        let b = h.encode();
        assert_eq!(b, [0x00, 0x01, 0x02, 0x01, 0x20, 0x20, 1, 0]);
        assert_eq!(Header::decode(&b), Some(h));
    }

    #[test]
    fn header_decode_big_endian_ids() {
        let b = [0x02, 0x01, 0x00, 0x0c, 0x13, 0x31, 1, 0, 0xff];
        let h = Header::decode(&b).unwrap();
        assert_eq!(h.dst_id, 513);
        assert_eq!(h.src_id, 12);
        assert_eq!(h.dst_port, port_byte(3, 1));
        assert_eq!(h.src_port, port_byte(1, 3));
        assert_eq!(h.msg_type, MSG_DATA);
    }

    #[test]
    fn header_decode_short() {
        assert_eq!(Header::decode(&[0; 7]), None);
        assert_eq!(Header::decode(&[]), None);
    }

    #[test]
    fn unix_dest_length() {
        let d = UnixDest::new("/tmp/netio0/1".into()).unwrap();
        let base = mem::offset_of!(libc::sockaddr_un, sun_path);
        assert_eq!(d.len as usize, base + "/tmp/netio0/1".len() + 1);
        assert!(UnixDest::new("x".repeat(200)).is_err());
    }

    #[test]
    fn lock_is_exclusive() {
        use std::io::Read;
        let path = std::env::temp_dir().join(format!("netio-lock-test-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let name = format!("{path}.lck");
        let held = lock(path).unwrap();
        // Closing any descriptor of the file would drop our POSIX lock, so the
        // checks below use one that stays open until the end.
        let mut other = File::options().read(true).write(true).open(&name).unwrap();
        let mut pid = String::new();
        other.read_to_string(&mut pid).unwrap();
        assert_eq!(pid, format!("{}\n", std::process::id()));
        // A POSIX lock never conflicts within one process, but an open file
        // description lock on another descriptor does.
        // SAFETY: flock is plain old data; all-zero is a valid value.
        let mut fl: libc::flock = unsafe { mem::zeroed() };
        fl.l_type = libc::F_WRLCK as libc::c_short;
        fl.l_whence = libc::SEEK_SET as libc::c_short;
        // SAFETY: other is open and fl is valid for the call.
        assert!(unsafe { libc::fcntl(other.as_raw_fd(), libc::F_OFD_SETLK, &mut fl) } < 0);
        drop(held);
        // SAFETY: as above.
        assert_eq!(
            unsafe { libc::fcntl(other.as_raw_fd(), libc::F_OFD_SETLK, &mut fl) },
            0
        );
        // Now ours is the one that is refused.
        assert!(lock(path).is_err());
        drop(other);
        assert!(lock(path).is_ok());
        let _ = std::fs::remove_file(&name);
    }

    #[test]
    fn send_waits_for_room() {
        let dir = std::env::temp_dir().join(format!("netio-send-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rx_path = dir.join("1");
        let _ = std::fs::remove_file(&rx_path);
        let rx = UnixDatagram::bind(&rx_path).unwrap();
        let tx = UnixDatagram::unbound().unwrap();
        tx.set_write_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let dest = UnixDest::new(rx_path.to_str().unwrap().into()).unwrap();
        // Nobody reads: once the queue is full, a send waits out the timeout.
        let mut sent = 0;
        let err = loop {
            let t = std::time::Instant::now();
            match send_to(&tx, &[0u8; 64], &dest) {
                Ok(()) => sent += 1,
                Err(e) => break (e, t.elapsed()),
            }
            assert!(sent < 100_000, "queue never filled");
        };
        assert_eq!(err.0.raw_os_error(), Some(libc::EAGAIN));
        assert!(
            err.1 >= Duration::from_millis(40),
            "gave up after {:?}",
            err.1
        );
        // A reader that drains the queue lets every later send through. The
        // longer timeout covers a reader that a busy machine keeps waiting.
        tx.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        let reader = thread::spawn(move || {
            let mut buf = [0u8; 64];
            for _ in 0..sent + 1000 {
                rx.recv(&mut buf).unwrap();
            }
        });
        for _ in 0..1000 {
            send_to(&tx, &[0u8; 64], &dest).unwrap();
        }
        reader.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn waits_for_a_bound_socket() {
        let dir = std::env::temp_dir().join(format!("netio-wait-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("1");
        let _ = std::fs::remove_file(&path);
        // A file left by an earlier run does not count, nor does a missing one.
        drop(UnixDatagram::bind(&path).unwrap());
        assert!(path.exists());
        let binder = {
            let path = path.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(200));
                std::fs::remove_file(&path).unwrap();
                thread::sleep(Duration::from_millis(200));
                UnixDatagram::bind(&path).unwrap()
            })
        };
        let waited = wait_for_listener(path.to_str().unwrap()).unwrap();
        let _rx = binder.join().unwrap();
        assert!(
            waited >= Duration::from_millis(400),
            "returned after {waited:?}"
        );
        // Once bound, it returns at once.
        assert!(wait_for_listener(path.to_str().unwrap()).unwrap() < LISTEN_POLL);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
