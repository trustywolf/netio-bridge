//! The port layout of a Containerlab IOL node.
//!
//! Containerlab starts the node with `IOL_PID` (IOL's NETIO id), optionally
//! `CLAB_IOL_NUM_SLOTS` and `CLAB_IOL_MGMT_INTF`, and writes `/iol/iouyap.ini`
//! and `/iol/NETMAP` for the ports that have links. IOL reads NETMAP once, at
//! boot, and uses only the ports listed there, so before IOL starts,
//! [`prepare`] rewrites NETMAP with every port of every slot:
//!
//! ```text
//! 1:0/0 513:0/0
//! 1:0/1 513:0/1
//! ```
//!
//! Port `<slot>/<port>` is bridged to `eth<4 x slot + port>`, except that the
//! management port and `Ethernet0/0` swap interfaces: `eth0` is always the
//! management port. [`load`] reads the NETMAP back, so a restarted bridge
//! serves exactly the ports IOL booted with.

use std::fmt;
use std::fs;
use std::io;

use crate::netio;

pub const NETMAP: &str = "/iol/NETMAP";
pub const IOUYAP_INI: &str = "/iol/iouyap.ini";
const SYS_NET: &str = "/sys/class/net";
const PORTS_PER_SLOT: u8 = 4;
const MAX_SLOTS: u8 = 16;

/// One bridged port: `Ethernet<slot>/<port>` of our id, the Linux interface
/// behind it and the IOL endpoint it is wired to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortConfig {
    pub slot: u8,
    pub port: u8,
    pub ifname: String,
    pub iol_id: u16,
    pub iol_port: u8,
}

impl PortConfig {
    pub fn port_byte(&self) -> u8 {
        netio::port_byte(self.slot, self.port)
    }

    pub fn name(&self) -> String {
        format!("{}/{}", self.slot, self.port)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn err<T>(msg: String) -> Result<T, ConfigError> {
    Err(ConfigError(msg))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Endpoint {
    id: u16,
    slot: u8,
    port: u8,
}

/// Parses `id:slot/port`, with an optional `@host` suffix that is ignored.
fn parse_endpoint(s: &str) -> Option<Endpoint> {
    let s = s.split('@').next()?;
    let (id, sp) = s.split_once(':')?;
    let (slot, port) = sp.split_once('/')?;
    let id: u16 = id.trim().parse().ok()?;
    let slot: u8 = slot.trim().parse().ok()?;
    let port: u8 = port.trim().parse().ok()?;
    if id == 0 || slot >= MAX_SLOTS || port >= PORTS_PER_SLOT {
        return None;
    }
    Some(Endpoint { id, slot, port })
}

/// What Containerlab gives the node, besides its interfaces.
#[derive(Debug, Default)]
pub struct Clab {
    /// `IOL_PID`.
    pub iol_id: Option<String>,
    /// `CLAB_IOL_NUM_SLOTS`.
    pub num_slots: Option<String>,
    /// `CLAB_IOL_MGMT_INTF`, such as `Ethernet1/2`.
    pub mgmt_intf: Option<String>,
    /// Containerlab's `/iol/iouyap.ini`, empty if there is none.
    pub iouyap_ini: String,
}

impl Clab {
    pub fn from_env() -> Self {
        let var = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
        Clab {
            iol_id: var("IOL_PID"),
            num_slots: var("CLAB_IOL_NUM_SLOTS"),
            mgmt_intf: var("CLAB_IOL_MGMT_INTF"),
            iouyap_ini: fs::read_to_string(IOUYAP_INI).unwrap_or_default(),
        }
    }

    /// The management port: the one `CLAB_IOL_MGMT_INTF` names (Containerlab
    /// 0.83 and later), else the one Containerlab's `iouyap.ini` gives `eth0`,
    /// else `Ethernet0/0`.
    pub fn mgmt_port(&self, id: u16) -> Result<(u8, u8), ConfigError> {
        if let Some(m) = &self.mgmt_intf {
            return m
                .strip_prefix("Ethernet")
                .and_then(|sp| parse_endpoint(&format!("1:{sp}")))
                .map(|e| (e.slot, e.port))
                .ok_or_else(|| ConfigError(format!("bad CLAB_IOL_MGMT_INTF {m}")));
        }
        let mut section = None;
        for line in self.iouyap_ini.lines().map(str::trim) {
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = parse_endpoint(name).filter(|e| e.id == id);
            } else if let (Some(e), Some((k, v))) = (section, line.split_once('=')) {
                if k.trim() == "eth_dev" && v.trim() == "eth0" {
                    return Ok((e.slot, e.port));
                }
            }
        }
        Ok((0, 0))
    }
}

fn index(slot: u8, port: u8) -> usize {
    usize::from(slot) * usize::from(PORTS_PER_SLOT) + usize::from(port)
}

/// The slot count for IOL's `-e`: `CLAB_IOL_NUM_SLOTS` when set, else enough
/// for the highest of our ports in Containerlab's NETMAP (written before the
/// start, so it lists links that are not back yet after a crash) and for the
/// highest `eth<n>` interface present.
pub fn slot_count(
    clab: &Clab,
    id: u16,
    netmap: &str,
    ifnames: &[String],
) -> Result<u8, ConfigError> {
    let slots = match &clab.num_slots {
        Some(s) => match s.trim().parse::<u8>() {
            Ok(n) if (1..=MAX_SLOTS).contains(&n) => n,
            _ => return err(format!("bad CLAB_IOL_NUM_SLOTS {s}")),
        },
        None => {
            let from_netmap = netmap
                .split_whitespace()
                .filter_map(parse_endpoint)
                .filter(|e| e.id == id)
                .map(|e| e.slot + 1)
                .max()
                .unwrap_or(1);
            let from_eth = ifnames
                .iter()
                .filter_map(|n| n.strip_prefix("eth")?.parse::<usize>().ok())
                .map(|n| n / usize::from(PORTS_PER_SLOT) + 1)
                .max()
                .unwrap_or(1);
            from_netmap.max(from_eth.min(usize::from(MAX_SLOTS)) as u8)
        }
    };
    let (s, p) = clab.mgmt_port(id)?;
    if s >= slots {
        return err(format!(
            "management port Ethernet{s}/{p} is beyond the {slots} slot(s)"
        ));
    }
    Ok(slots)
}

/// NETMAP with every port of `slots` slots, each wired to the same port of IOL.
pub fn netmap_text(iol_id: u16, id: u16, slots: u8) -> String {
    let mut s = String::new();
    for slot in 0..slots {
        for port in 0..PORTS_PER_SLOT {
            s.push_str(&format!("{iol_id}:{slot}/{port} {id}:{slot}/{port}\n"));
        }
    }
    s
}

/// The ports a NETMAP gives us, each with its interface. Only point-to-point
/// lines between one of our ports and one IOL port are accepted.
pub fn ports(netmap: &str, id: u16, mgmt: (u8, u8)) -> Result<Vec<PortConfig>, ConfigError> {
    let mgmt = index(mgmt.0, mgmt.1);
    let mut ports: Vec<PortConfig> = Vec::new();
    for (n, raw) in netmap.lines().enumerate() {
        let n = n + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut eps = Vec::new();
        for tok in line.split_whitespace() {
            let Some(ep) = parse_endpoint(tok) else {
                return err(format!("NETMAP line {n}: bad endpoint {tok}"));
            };
            eps.push(ep);
        }
        let ours = eps.iter().filter(|e| e.id == id).count();
        if ours == 0 {
            continue;
        }
        if eps.len() != 2 || ours != 1 {
            return err(format!(
                "NETMAP line {n}: only point-to-point links between {id} and one IOL port are supported"
            ));
        }
        let (own, peer) = if eps[0].id == id {
            (eps[0], eps[1])
        } else {
            (eps[1], eps[0])
        };
        if ports
            .iter()
            .any(|p| (p.slot, p.port) == (own.slot, own.port))
        {
            return err(format!(
                "NETMAP line {n}: {id}:{}/{} is mapped more than once",
                own.slot, own.port
            ));
        }
        let i = index(own.slot, own.port);
        let eth = match i {
            0 => mgmt,
            _ if i == mgmt => 0,
            _ => i,
        };
        ports.push(PortConfig {
            slot: own.slot,
            port: own.port,
            ifname: format!("eth{eth}"),
            iol_id: peer.id,
            iol_port: netio::port_byte(peer.slot, peer.port),
        });
    }
    if ports.is_empty() {
        return err(format!("NETMAP has no port of {id}"));
    }
    Ok(ports)
}

fn read(path: &str) -> Result<String, ConfigError> {
    fs::read_to_string(path).or_else(|e| err(format!("{path}: {e}")))
}

/// Before IOL starts: picks the slot count, writes NETMAP with every port and
/// returns the count.
pub fn prepare(id: u16) -> Result<u8, ConfigError> {
    let clab = Clab::from_env();
    let Some(iol_id) = clab.iol_id.as_deref() else {
        return err("IOL_PID is not set".into());
    };
    let iol_id = match iol_id.parse::<u16>() {
        Ok(n) if n > 0 && n != id => n,
        _ => return err(format!("bad IOL_PID {iol_id}")),
    };
    let netmap = match fs::read_to_string(NETMAP) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return err(format!("{NETMAP}: {e}")),
    };
    let ifnames: Vec<String> = fs::read_dir(SYS_NET)
        .or_else(|e| err(format!("{SYS_NET}: {e}")))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    let slots = slot_count(&clab, id, &netmap, &ifnames)?;
    fs::write(NETMAP, netmap_text(iol_id, id, slots))
        .or_else(|e| err(format!("write {NETMAP}: {e}")))?;
    Ok(slots)
}

/// The ports to bridge, from the NETMAP [`prepare`] wrote.
pub fn load(id: u16) -> Result<Vec<PortConfig>, ConfigError> {
    let clab = Clab::from_env();
    ports(&read(NETMAP)?, id, clab.mgmt_port(id)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAB_INI: &str = "[default]\nbase_port = 49000\nnetmap = /iol/NETMAP\n\
        [513:0/0]\neth_dev = eth0\n[513:0/1]\neth_dev = eth1\n[513:1/0]\neth_dev = eth4\n";

    fn clab(slots: Option<&str>, mgmt: Option<&str>, ini: &str) -> Clab {
        Clab {
            iol_id: Some("1".into()),
            num_slots: slots.map(Into::into),
            mgmt_intf: mgmt.map(Into::into),
            iouyap_ini: ini.into(),
        }
    }

    fn eths(n: &[usize]) -> Vec<String> {
        let mut v: Vec<String> = n.iter().map(|i| format!("eth{i}")).collect();
        v.push("lo".into());
        v
    }

    #[test]
    fn endpoint() {
        assert_eq!(
            parse_endpoint("513:1/3"),
            Some(Endpoint {
                id: 513,
                slot: 1,
                port: 3
            })
        );
        assert_eq!(
            parse_endpoint("12:0/2@node1"),
            Some(Endpoint {
                id: 12,
                slot: 0,
                port: 2
            })
        );
        assert_eq!(parse_endpoint("513:16/0"), None);
        assert_eq!(parse_endpoint("513:0/4"), None);
        assert_eq!(parse_endpoint("0:0/0"), None);
        assert_eq!(parse_endpoint("513:0"), None);
        assert_eq!(parse_endpoint("x:0/0"), None);
    }

    #[test]
    fn mgmt_port_sources() {
        // Default: Ethernet0/0, with or without Containerlab's ini.
        assert_eq!(clab(None, None, "").mgmt_port(513), Ok((0, 0)));
        assert_eq!(clab(None, None, CLAB_INI).mgmt_port(513), Ok((0, 0)));
        // mgmt-intf: Ethernet1/2 in Containerlab's ini (before 0.83).
        let ini = "[default]\nnetmap = /iol/NETMAP\n[513:1/2]\neth_dev = eth0\n\
            [513:0/1]\neth_dev = eth1\n[514:0/0]\neth_dev = eth0\n";
        assert_eq!(clab(None, None, ini).mgmt_port(513), Ok((1, 2)));
        // Another id's eth0 does not count.
        assert_eq!(
            clab(None, None, "[514:1/1]\neth_dev = eth0\n").mgmt_port(513),
            Ok((0, 0))
        );
        // The variable wins over the ini (a recreate may leave a stale one).
        assert_eq!(
            clab(None, Some("Ethernet0/3"), ini).mgmt_port(513),
            Ok((0, 3))
        );
        assert!(clab(None, Some("Gi1"), "").mgmt_port(513).is_err());
        assert!(clab(None, Some("Ethernet0/4"), "").mgmt_port(513).is_err());
    }

    #[test]
    fn slots_from_netmap_and_interfaces() {
        let c = clab(None, None, CLAB_INI);
        let netmap = "1:0/0 513:0/0\n1:0/1 513:0/1\n";
        // eth0 to eth3: one slot.
        assert_eq!(slot_count(&c, 513, netmap, &eths(&[0, 1, 2, 3])), Ok(1));
        // eth4 is Ethernet1/0: two slots.
        assert_eq!(slot_count(&c, 513, netmap, &eths(&[0, 1, 4])), Ok(2));
        // A node started after a crash, before its links are back: NETMAP
        // still lists 1/1, so it keeps two slots with only eth0 present.
        let netmap = "1:0/0 513:0/0\n1:0/1 513:0/1\n1:1/0 513:1/0\n1:1/1 513:1/1\n";
        assert_eq!(slot_count(&c, 513, netmap, &eths(&[0])), Ok(2));
        // A config-drift recreate: NETMAP with eth0 only, the links present.
        assert_eq!(
            slot_count(&c, 513, "1:0/0 513:0/0\n", &eths(&[0, 1, 5])),
            Ok(2)
        );
        // Nothing at all still gives one slot.
        assert_eq!(slot_count(&c, 513, "", &[]), Ok(1));
        // Other ids in NETMAP do not count.
        assert_eq!(slot_count(&c, 513, "1:3/0 7:3/0\n", &eths(&[0])), Ok(1));
    }

    #[test]
    fn slots_pinned() {
        let c = clab(Some("3"), None, "");
        assert_eq!(slot_count(&c, 513, "", &eths(&[0, 1])), Ok(3));
        // Pinned below what the links need: the pin still wins.
        let c = clab(Some("1"), None, "");
        assert_eq!(slot_count(&c, 513, "", &eths(&[0, 4])), Ok(1));
        for bad in ["0", "17", "x", "-1"] {
            assert!(slot_count(&clab(Some(bad), None, ""), 513, "", &[]).is_err());
        }
        // The management port must exist.
        let c = clab(Some("1"), Some("Ethernet1/2"), "");
        assert!(slot_count(&c, 513, "", &eths(&[0])).is_err());
        let c = clab(None, Some("Ethernet1/2"), "");
        assert_eq!(slot_count(&c, 513, "1:1/2 513:1/2\n", &eths(&[0])), Ok(2));
    }

    #[test]
    fn full_netmap() {
        assert_eq!(
            netmap_text(1, 513, 1),
            "1:0/0 513:0/0\n1:0/1 513:0/1\n1:0/2 513:0/2\n1:0/3 513:0/3\n"
        );
        assert_eq!(netmap_text(7, 513, 2).lines().count(), 8);
        assert!(netmap_text(7, 513, 2).ends_with("7:1/3 513:1/3\n"));
    }

    #[test]
    fn ports_from_full_netmap() {
        let p = ports(&netmap_text(1, 513, 2), 513, (0, 0)).unwrap();
        assert_eq!(p.len(), 8);
        assert_eq!(
            p[5],
            PortConfig {
                slot: 1,
                port: 1,
                ifname: "eth5".into(),
                iol_id: 1,
                iol_port: 0x11
            }
        );
        for (i, port) in p.iter().enumerate() {
            assert_eq!(port.ifname, format!("eth{i}"));
            assert_eq!(port.port_byte(), port.iol_port);
        }
    }

    #[test]
    fn relocated_mgmt_port_swaps_with_0_0() {
        // mgmt-intf: Ethernet1/2 puts eth0 on 1/2 and eth6 on 0/0.
        let p = ports(&netmap_text(7, 513, 2), 513, (1, 2)).unwrap();
        let by = |s, q| p.iter().find(|x| (x.slot, x.port) == (s, q)).unwrap();
        assert_eq!(by(1, 2).ifname, "eth0");
        assert_eq!(by(1, 2).port_byte(), 0x21);
        assert_eq!(by(0, 0).ifname, "eth6");
        assert_eq!(by(0, 1).ifname, "eth1");
        assert_eq!(by(1, 3).ifname, "eth7");
        assert_eq!(by(0, 0).iol_id, 7);
    }

    #[test]
    fn netmap_comments_hosts_and_foreign_lines() {
        let netmap = "# comment\n\n1:0/0@a 513:0/0@a  # trailing\n2:0/0 3:0/0\n4:0/0 5:0/0 6:0/0\n\
            513:0/1 1:0/1\n1:1/0 513:1/0\n";
        let p = ports(netmap, 513, (0, 0)).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[1].iol_port, 0x10);
        assert_eq!(p[2].ifname, "eth4");
    }

    #[test]
    fn rejects_bad_netmap() {
        let bad = |t: &str| ports(t, 513, (0, 0)).is_err();
        // duplicate mapping of one of our ports
        assert!(bad("1:0/0 513:0/0\n2:0/0 513:0/0\n"));
        // segment with more than two members
        assert!(bad("1:0/0 2:0/0 513:0/0\n"));
        // a lone endpoint
        assert!(bad("513:0/0\n"));
        // both ends ours
        assert!(bad("513:0/1 513:0/0\n"));
        assert!(bad("1:0/0 513:x\n"));
        // none of ours
        assert!(bad("1:0/0 2:0/0\n"));
        assert!(bad(""));
    }
}
