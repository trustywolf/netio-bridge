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

use std::io;
use std::mem;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::log;
use crate::port::Port;
use crate::stats::UnixStats;

pub const HEADER_LEN: usize = 8;
pub const MSG_DATA: u8 = 1;

/// Largest datagram read from IOL.
const RX_SPACE: usize = 64 * 1024;

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

/// Sends one datagram without blocking.
pub fn send_to(sock: &UnixDatagram, buf: &[u8], dest: &UnixDest) -> io::Result<()> {
    // SAFETY: buf and dest.addr are valid for the lengths passed.
    let n = unsafe {
        libc::sendto(
            sock.as_raw_fd(),
            buf.as_ptr().cast(),
            buf.len(),
            libc::MSG_DONTWAIT,
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
}
