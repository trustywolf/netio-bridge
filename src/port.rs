//! The Linux side: one AF_PACKET socket per bridged interface.
//!
//! Each port has its own thread that reads frames from the interface and sends
//! them to IOL, and that alone opens, checks and replaces the port's socket. The
//! NETIO reader only borrows the current socket to send, so a port that is down,
//! deleted or recreated never affects another port.

use std::ffi::CString;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::config::PortConfig;
use crate::log;
use crate::netio::{self, HEADER_LEN, UnixDest};
use crate::stats::PortStats;

/// Largest frame passed to IOL, tag included. Anything longer (such as a
/// GSO/GRO super-frame from the Linux stack) is dropped.
pub const MAX_FRAME: usize = 9220;
const TAG_LEN: usize = 4;
/// Receive space for one frame from the interface.
const RX_SPACE: usize = 64 * 1024;
/// Where a frame is received: room in front for the NETIO header and a tag.
const RX_OFFSET: usize = HEADER_LEN + TAG_LEN;
/// Destination and source MAC addresses.
const MACS_LEN: usize = 12;
const ETH_HLEN: usize = 14;

const RECV_TIMEOUT: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

// From <linux/if_packet.h>, kept here so the build does not depend on which of
// them the libc crate exports.
const PACKET_AUXDATA: libc::c_int = 8;
const PACKET_IGNORE_OUTGOING: libc::c_int = 23;
const PACKET_OUTGOING: u8 = 4;
const TP_STATUS_VLAN_VALID: u32 = 1 << 4;
const TP_STATUS_VLAN_TPID_VALID: u32 = 1 << 6;
const ETH_P_8021Q: u16 = 0x8100;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AuxData {
    pub tp_status: u32,
    pub tp_len: u32,
    pub tp_snaplen: u32,
    pub tp_mac: u16,
    pub tp_net: u16,
    pub tp_vlan_tci: u16,
    pub tp_vlan_tpid: u16,
}

impl AuxData {
    /// The tag the kernel took off the frame, as (TPID, TCI). A priority-tagged
    /// frame (TCI 0) still counts when the kernel says the tag was valid.
    pub fn vlan(&self) -> Option<(u16, u16)> {
        if self.tp_status & TP_STATUS_VLAN_VALID == 0 && self.tp_vlan_tci == 0 {
            return None;
        }
        let tpid = if self.tp_status & TP_STATUS_VLAN_TPID_VALID != 0 {
            self.tp_vlan_tpid
        } else {
            ETH_P_8021Q
        };
        Some((tpid, self.tp_vlan_tci))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discard {
    Runt,
    Oversize,
}

/// Puts a frame received at `buf[RX_OFFSET..RX_OFFSET + len]` in its final
/// place, with the stripped tag (if any) back after the MAC addresses. Returns
/// the frame's offset and length; the offset is always at least `HEADER_LEN`.
pub fn place_frame(
    buf: &mut [u8],
    len: usize,
    vlan: Option<(u16, u16)>,
) -> Result<(usize, usize), Discard> {
    if len < ETH_HLEN {
        return Err(Discard::Runt);
    }
    let total = len + if vlan.is_some() { TAG_LEN } else { 0 };
    if total > MAX_FRAME {
        return Err(Discard::Oversize);
    }
    let Some((tpid, tci)) = vlan else {
        return Ok((RX_OFFSET, len));
    };
    let start = RX_OFFSET - TAG_LEN;
    buf.copy_within(RX_OFFSET..RX_OFFSET + MACS_LEN, start);
    buf[start + MACS_LEN..start + MACS_LEN + 2].copy_from_slice(&tpid.to_be_bytes());
    buf[start + MACS_LEN + 2..RX_OFFSET + MACS_LEN].copy_from_slice(&tci.to_be_bytes());
    Ok((start, total))
}

/// A bridged port, shared by its own thread and the NETIO reader.
pub struct Port {
    pub cfg: PortConfig,
    /// The current packet socket, `None` while the interface is missing. The
    /// reader clones the `Arc` under the lock and sends outside it; the fd is
    /// closed only when the last clone is dropped, so it cannot be reused under
    /// a sender.
    sock: Mutex<Option<Arc<OwnedFd>>>,
    pub stats: PortStats,
    label: String,
}

impl Port {
    pub fn new(cfg: PortConfig) -> Self {
        let label = format!("{} {}", cfg.name(), cfg.ifname);
        Port {
            cfg,
            sock: Mutex::new(None),
            stats: PortStats::default(),
            label,
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// IOL -> interface. Failures cost only this frame.
    pub fn send_frame(&self, frame: &[u8]) {
        let sock = self.sock.lock().unwrap().clone();
        let Some(sock) = sock else {
            self.stats.to_eth_down.inc();
            return;
        };
        // SAFETY: frame is valid for frame.len() bytes.
        let n = unsafe {
            libc::send(
                sock.as_raw_fd(),
                frame.as_ptr().cast(),
                frame.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if n < 0 {
            self.stats.to_eth_err.inc();
            let e = io::Error::last_os_error();
            log::error(
                &format!("to-eth {}", self.label),
                &format!("{}: send to interface: {e}", self.label),
            );
        } else {
            self.stats.to_eth.inc();
        }
    }

    fn set_sock(&self, sock: Option<Arc<OwnedFd>>) {
        *self.sock.lock().unwrap() = sock;
    }

    /// The port's thread: keep a socket open on the interface and forward what
    /// it receives to IOL.
    pub fn run(&self, unix: &UnixDatagram, my_id: u16) {
        let dest = match UnixDest::new(format!("{}/{}", netio::socket_dir(), self.cfg.iol_id)) {
            Ok(d) => d,
            Err(e) => {
                log::error(&self.label, &format!("{}: {e}; port disabled", self.label));
                return;
            }
        };
        let header = netio::Header::data(
            self.cfg.iol_id,
            self.cfg.iol_port,
            my_id,
            self.cfg.port_byte(),
        )
        .encode();
        let mut buf = vec![0u8; RX_OFFSET + RX_SPACE];
        let mut missing_logged = false;
        loop {
            let ifindex = if_index(&self.cfg.ifname);
            if ifindex == 0 {
                if !missing_logged {
                    log::notice(&format!("{}: interface not present, waiting", self.label));
                    missing_logged = true;
                }
                thread::sleep(RETRY_INTERVAL);
                continue;
            }
            let sock = match open(ifindex) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    log::error(
                        &format!("open {}", self.label),
                        &format!("{}: open packet socket: {e}", self.label),
                    );
                    thread::sleep(RETRY_INTERVAL);
                    continue;
                }
            };
            // A port that opens at start-up is routine; one that comes back
            // after a NOTICE (missing, or gone) gets a NOTICE too.
            let waited = std::mem::take(&mut missing_logged);
            self.stats.opens.inc();
            let msg = format!("{}: forwarding (ifindex {ifindex})", self.label);
            if self.stats.opens.get() == 1 && !waited {
                log::info(&msg);
            } else {
                log::notice(&msg);
            }
            self.set_sock(Some(sock.clone()));
            self.forward(&sock, ifindex, &mut buf, &header, unix, &dest);
            self.set_sock(None);
            log::notice(&format!(
                "{}: interface gone, closing its socket",
                self.label
            ));
        }
    }

    /// Reads frames until the socket no longer belongs to `ifname`.
    fn forward(
        &self,
        sock: &OwnedFd,
        ifindex: u32,
        buf: &mut [u8],
        header: &[u8; HEADER_LEN],
        unix: &UnixDatagram,
        dest: &UnixDest,
    ) {
        loop {
            let (len, aux) = match recv(sock.as_raw_fd(), buf) {
                Ok(Some(r)) => r,
                // Our own transmissions, when PACKET_IGNORE_OUTGOING is missing.
                Ok(None) => continue,
                Err(e) => {
                    match e.raw_os_error() {
                        // Timeout, or an interrupted read.
                        Some(libc::EAGAIN) | Some(libc::EINTR) => {}
                        // One-off error when the interface goes down; the
                        // socket starts receiving again once it comes back up.
                        Some(libc::ENETDOWN) => {}
                        _ => {
                            log::error(
                                &format!("recv {}", self.label),
                                &format!("{}: recv: {e}", self.label),
                            );
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                    if !still_bound(sock.as_raw_fd(), &self.cfg.ifname, ifindex) {
                        return;
                    }
                    continue;
                }
            };
            if len > RX_SPACE {
                self.stats.truncated.inc();
                continue;
            }
            let vlan = aux.and_then(|a| a.vlan());
            let (start, flen) = match place_frame(buf, len, vlan) {
                Ok(r) => r,
                Err(Discard::Runt) => {
                    self.stats.runt.inc();
                    continue;
                }
                Err(Discard::Oversize) => {
                    self.stats.oversize.inc();
                    continue;
                }
            };
            if vlan.is_some() {
                self.stats.vlan_restored.inc();
            }
            buf[start - HEADER_LEN..start].copy_from_slice(header);
            match netio::send_to(unix, &buf[start - HEADER_LEN..start + flen], dest) {
                Ok(()) => self.stats.to_iol.inc(),
                Err(e) => {
                    self.stats.to_iol_err.inc();
                    log::error(
                        &format!("to-iol {}", self.label),
                        &format!("{}: send to {}: {e}", self.label, dest.path),
                    );
                }
            }
        }
    }
}

fn if_index(name: &str) -> u32 {
    let Ok(c) = CString::new(name) else { return 0 };
    // SAFETY: c is a valid NUL-terminated string.
    unsafe { libc::if_nametoindex(c.as_ptr()) }
}

/// True while `ifname` exists with the ifindex the socket is bound to. The
/// kernel sets a packet socket's ifindex to -1 when its interface is deleted,
/// so a recreated interface that reuses the old ifindex is still noticed.
fn still_bound(fd: RawFd, ifname: &str, ifindex: u32) -> bool {
    if if_index(ifname) != ifindex {
        return false;
    }
    // SAFETY: sockaddr_ll is plain old data; the length passed matches it.
    unsafe {
        let mut sll: libc::sockaddr_ll = mem::zeroed();
        let mut len = mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
        if libc::getsockname(fd, (&mut sll as *mut libc::sockaddr_ll).cast(), &mut len) < 0 {
            return false;
        }
        sll.sll_ifindex == ifindex as libc::c_int
    }
}

fn check(r: libc::c_int) -> io::Result<()> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_opt<T>(fd: RawFd, level: libc::c_int, name: libc::c_int, val: &T) -> io::Result<()> {
    // SAFETY: val points to a T of the size passed.
    check(unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            (val as *const T).cast(),
            mem::size_of::<T>() as libc::socklen_t,
        )
    })
}

/// Opens a promiscuous packet socket that sees only `ifindex`'s frames.
fn open(ifindex: u32) -> io::Result<OwnedFd> {
    // Protocol 0: the socket receives nothing until it is bound to ifindex
    // below, so it never sees another interface's frames.
    // SAFETY: plain socket(2) call; the fd is owned right away.
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh descriptor nobody else owns.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let one: libc::c_int = 1;
    set_opt(fd, libc::SOL_PACKET, PACKET_AUXDATA, &one)?;
    // Kernels before 4.20 lack it; recv() then filters by packet type instead.
    let _ = set_opt(fd, libc::SOL_PACKET, PACKET_IGNORE_OUTGOING, &one);
    let tv = libc::timeval {
        tv_sec: RECV_TIMEOUT.as_secs() as libc::time_t,
        tv_usec: 0,
    };
    set_opt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv)?;

    // SAFETY: sockaddr_ll is plain old data; all-zero is a valid value.
    let mut sll: libc::sockaddr_ll = unsafe { mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
    sll.sll_ifindex = ifindex as libc::c_int;
    // SAFETY: sll is a valid sockaddr_ll of the size passed.
    check(unsafe {
        libc::bind(
            fd,
            (&sll as *const libc::sockaddr_ll).cast(),
            mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    })?;

    // SAFETY: packet_mreq is plain old data; all-zero is a valid value.
    let mut mreq: libc::packet_mreq = unsafe { mem::zeroed() };
    mreq.mr_ifindex = ifindex as libc::c_int;
    mreq.mr_type = libc::PACKET_MR_PROMISC as libc::c_ushort;
    set_opt(fd, libc::SOL_PACKET, libc::PACKET_ADD_MEMBERSHIP, &mreq)?;
    Ok(sock)
}

/// Receives one frame at `buf[RX_OFFSET..]`. Returns its full length (which may
/// exceed the buffer: MSG_TRUNC) and the auxiliary data, or `None` for a frame
/// we sent ourselves.
fn recv(fd: RawFd, buf: &mut [u8]) -> io::Result<Option<(usize, Option<AuxData>)>> {
    let space = buf.len() - RX_OFFSET;
    // SAFETY: every pointer in msg refers to a live local or into buf, with the
    // lengths given; the cmsg walk stays inside cbuf as the kernel filled it.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().add(RX_OFFSET).cast(),
            iov_len: space,
        };
        let mut sll: libc::sockaddr_ll = mem::zeroed();
        let mut cbuf = [0u64; 8];
        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_name = (&mut sll as *mut libc::sockaddr_ll).cast();
        msg.msg_namelen = mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = mem::size_of_val(&cbuf) as _;
        let n = libc::recvmsg(fd, &mut msg, libc::MSG_TRUNC);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if sll.sll_pkttype == PACKET_OUTGOING {
            return Ok(None);
        }
        let mut len = n as usize;
        if msg.msg_flags & libc::MSG_TRUNC != 0 {
            len = len.max(space + 1);
        }
        let mut aux = None;
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_PACKET
                && (*c).cmsg_type == PACKET_AUXDATA
                && (*c).cmsg_len as usize
                    >= libc::CMSG_LEN(mem::size_of::<AuxData>() as u32) as usize
            {
                aux = Some(ptr::read_unaligned(libc::CMSG_DATA(c) as *const AuxData));
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
        Ok(Some((len, aux)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len: usize) -> Vec<u8> {
        (0..len).map(|i| i as u8).collect()
    }

    /// A buffer with `f` received at RX_OFFSET.
    fn received(f: &[u8]) -> Vec<u8> {
        let mut buf = vec![0xee; RX_OFFSET + RX_SPACE];
        buf[RX_OFFSET..RX_OFFSET + f.len()].copy_from_slice(f);
        buf
    }

    fn aux(status: u32, tci: u16, tpid: u16) -> AuxData {
        AuxData {
            tp_status: status,
            tp_vlan_tci: tci,
            tp_vlan_tpid: tpid,
            ..Default::default()
        }
    }

    #[test]
    fn untagged_frame_stays_in_place() {
        let f = frame(60);
        let mut buf = received(&f);
        let (start, len) = place_frame(&mut buf, f.len(), None).unwrap();
        assert_eq!((start, len), (RX_OFFSET, 60));
        assert_eq!(&buf[start..start + len], &f[..]);
    }

    #[test]
    fn tag_reinserted_after_macs() {
        let f = frame(60);
        let mut buf = received(&f);
        let vlan = aux(TP_STATUS_VLAN_VALID, 0x0064, 0).vlan();
        assert_eq!(vlan, Some((0x8100, 100)));
        let (start, len) = place_frame(&mut buf, f.len(), vlan).unwrap();
        assert_eq!((start, len), (HEADER_LEN, 64));
        let out = &buf[start..start + len];
        assert_eq!(&out[..12], &f[..12]);
        assert_eq!(&out[12..16], &[0x81, 0x00, 0x00, 0x64]);
        assert_eq!(&out[16..], &f[12..]);
    }

    #[test]
    fn priority_tag_kept() {
        // TCI 0 (802.1p priority tag) with the valid bit set must not be lost.
        let vlan = aux(TP_STATUS_VLAN_VALID, 0, 0).vlan();
        assert_eq!(vlan, Some((0x8100, 0)));
        let f = frame(60);
        let mut buf = received(&f);
        let (start, len) = place_frame(&mut buf, f.len(), vlan).unwrap();
        assert_eq!(&buf[start + 12..start + 16], &[0x81, 0x00, 0x00, 0x00]);
        assert_eq!(len, 64);
    }

    #[test]
    fn tci_without_valid_bit_counts() {
        // Old kernels report the TCI without TP_STATUS_VLAN_VALID.
        assert_eq!(aux(0, 0x2005, 0).vlan(), Some((0x8100, 0x2005)));
        assert_eq!(aux(0, 0, 0).vlan(), None);
    }

    #[test]
    fn qinq_outer_tag_restored() {
        // 802.1ad outer tag stripped by the kernel, inner 802.1Q tag in the frame.
        let mut f = frame(64);
        f[12..16].copy_from_slice(&[0x81, 0x00, 0x00, 0x0a]);
        let vlan = aux(
            TP_STATUS_VLAN_VALID | TP_STATUS_VLAN_TPID_VALID,
            0x0014,
            0x88a8,
        )
        .vlan();
        assert_eq!(vlan, Some((0x88a8, 20)));
        let mut buf = received(&f);
        let (start, len) = place_frame(&mut buf, f.len(), vlan).unwrap();
        let out = &buf[start..start + len];
        assert_eq!(
            &out[12..20],
            &[0x88, 0xa8, 0x00, 0x14, 0x81, 0x00, 0x00, 0x0a]
        );
        assert_eq!(&out[20..], &f[16..]);
    }

    #[test]
    fn oversize_dropped() {
        let mut buf = received(&frame(100));
        assert_eq!(
            place_frame(&mut buf, MAX_FRAME, None),
            Ok((RX_OFFSET, MAX_FRAME))
        );
        assert_eq!(
            place_frame(&mut buf, MAX_FRAME + 1, None),
            Err(Discard::Oversize)
        );
        // The tag counts toward the limit.
        let vlan = Some((0x8100, 100));
        assert_eq!(
            place_frame(&mut buf, MAX_FRAME - TAG_LEN, vlan),
            Ok((HEADER_LEN, MAX_FRAME))
        );
        assert_eq!(
            place_frame(&mut buf, MAX_FRAME - TAG_LEN + 1, vlan),
            Err(Discard::Oversize)
        );
        // The 8000-byte tagged frame that crashed iouyap is forwarded intact.
        let f = frame(7996);
        let mut buf = received(&f);
        assert_eq!(place_frame(&mut buf, f.len(), vlan), Ok((HEADER_LEN, 8000)));
        assert_eq!(place_frame(&mut buf, 65_535, vlan), Err(Discard::Oversize));
    }

    #[test]
    fn runt_dropped() {
        let mut buf = received(&frame(13));
        assert_eq!(place_frame(&mut buf, 13, None), Err(Discard::Runt));
        assert_eq!(
            place_frame(&mut buf, 13, Some((0x8100, 1))),
            Err(Discard::Runt)
        );
        assert_eq!(place_frame(&mut buf, 14, None), Ok((RX_OFFSET, 14)));
    }

    #[test]
    fn auxdata_layout() {
        assert_eq!(mem::size_of::<AuxData>(), 20);
    }

    #[test]
    fn missing_interface() {
        assert_eq!(if_index("netio-test-nonexistent"), 0);
        assert_eq!(if_index("bad\0name"), 0);
        assert_eq!(if_index("lo"), 1);
    }
}
