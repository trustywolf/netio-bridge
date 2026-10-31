//! Drop and traffic counters, written to the log on SIGUSR1.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

fn line(prefix: &str, fields: &[(&str, &Counter)]) -> String {
    let mut s = String::from(prefix);
    for (name, c) in fields {
        let _ = write!(s, " {name}={}", c.get());
    }
    s
}

/// Datagrams read from IOL.
#[derive(Default)]
pub struct UnixStats {
    pub rx: Counter,
    /// Shorter than the NETIO header.
    pub short: Counter,
    /// msg_type other than data.
    pub not_data: Counter,
    /// Addressed to another id.
    pub wrong_dst: Counter,
    /// Addressed to a port we do not bridge.
    pub unknown_port: Counter,
}

impl UnixStats {
    pub fn line(&self) -> String {
        line(
            "netio:",
            &[
                ("rx", &self.rx),
                ("short", &self.short),
                ("not_data", &self.not_data),
                ("wrong_dst", &self.wrong_dst),
                ("unknown_port", &self.unknown_port),
            ],
        )
    }
}

#[derive(Default)]
pub struct PortStats {
    /// IOL -> interface: frames sent.
    pub to_eth: Counter,
    /// IOL -> interface: dropped, no socket open.
    pub to_eth_down: Counter,
    /// IOL -> interface: dropped, send failed.
    pub to_eth_err: Counter,
    /// Interface -> IOL: frames sent.
    pub to_iol: Counter,
    /// Interface -> IOL: dropped, send failed.
    pub to_iol_err: Counter,
    /// Interface -> IOL: dropped, longer than the frame limit.
    pub oversize: Counter,
    /// Interface -> IOL: dropped, longer than the receive buffer.
    pub truncated: Counter,
    /// Interface -> IOL: dropped, shorter than an Ethernet header.
    pub runt: Counter,
    /// Interface -> IOL: tagged frames whose 802.1Q/802.1ad tag was put back.
    pub vlan_restored: Counter,
    /// Packet socket (re)opened.
    pub opens: Counter,
}

impl PortStats {
    pub fn line(&self, name: &str) -> String {
        line(
            &format!("port {name}:"),
            &[
                ("to_eth", &self.to_eth),
                ("to_eth_down", &self.to_eth_down),
                ("to_eth_err", &self.to_eth_err),
                ("to_iol", &self.to_iol),
                ("to_iol_err", &self.to_iol_err),
                ("oversize", &self.oversize),
                ("truncated", &self.truncated),
                ("runt", &self.runt),
                ("vlan_restored", &self.vlan_restored),
                ("opens", &self.opens),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_line() {
        let s = PortStats::default();
        s.to_eth.inc();
        s.to_eth.inc();
        s.oversize.inc();
        let l = s.line("0/1 eth1");
        assert!(l.starts_with("port 0/1 eth1: to_eth=2 "));
        assert!(l.contains(" oversize=1 "));
        assert!(l.ends_with(" opens=0"));
    }
}
