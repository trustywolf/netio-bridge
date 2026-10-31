//! Reads the bridge configuration: an iouyap-style ini file and the NETMAP it
//! points to.
//!
//! Only the subset Containerlab writes is accepted:
//!
//! ```text
//! [default]
//! base_port = 49000
//! netmap = /iol/NETMAP
//! [513:0/0]
//! eth_dev = eth0
//! ```
//!
//! and NETMAP lines of exactly two endpoints, one of them ours:
//!
//! ```text
//! 1:0/0 513:0/0
//! ```

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::netio;

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

#[derive(Debug)]
pub struct Config {
    pub ports: Vec<PortConfig>,
    /// Problems that do not stop the bridge.
    pub warnings: Vec<String>,
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
    if id == 0 || slot >= 16 || port >= 16 {
        return None;
    }
    Some(Endpoint { id, slot, port })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Ini {
    netmap: Option<String>,
    /// (slot, port, eth_dev) for every `[<id>:slot/port]` section of ours.
    ports: Vec<(u8, u8, String)>,
}

enum Section {
    None,
    Default,
    Own(usize),
    Other,
}

fn parse_ini(text: &str, id: u16) -> Result<Ini, ConfigError> {
    let mut netmap = None;
    let mut own: Vec<(u8, u8, Option<String>)> = Vec::new();
    let mut section = Section::None;
    for (n, raw) in text.lines().enumerate() {
        let n = n + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.trim();
            section = if name == "default" {
                Section::Default
            } else {
                let Some(ep) = parse_endpoint(name) else {
                    return err(format!("ini line {n}: bad section [{name}]"));
                };
                if ep.id != id {
                    Section::Other
                } else if own.iter().any(|(s, p, _)| (*s, *p) == (ep.slot, ep.port)) {
                    return err(format!("ini line {n}: duplicate section [{name}]"));
                } else {
                    own.push((ep.slot, ep.port, None));
                    Section::Own(own.len() - 1)
                }
            };
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return err(format!("ini line {n}: expected key = value"));
        };
        let (key, value) = (key.trim(), value.trim());
        match section {
            Section::None => return err(format!("ini line {n}: key outside any section")),
            Section::Default if key == "netmap" => netmap = Some(value.to_string()),
            Section::Own(i) if key == "eth_dev" => {
                if own[i].2.is_some() {
                    return err(format!("ini line {n}: duplicate eth_dev"));
                }
                if value.is_empty() {
                    return err(format!("ini line {n}: empty eth_dev"));
                }
                own[i].2 = Some(value.to_string());
            }
            _ => {}
        }
    }
    let mut ports = Vec::new();
    for (slot, port, dev) in own {
        let Some(dev) = dev else {
            return err(format!(
                "ini section [{id}:{slot}/{port}] has no eth_dev (only eth_dev ports are supported)"
            ));
        };
        if ports.iter().any(|(_, _, d)| *d == dev) {
            return err(format!("ini: {dev} is mapped to more than one port"));
        }
        ports.push((slot, port, dev));
    }
    Ok(Ini { netmap, ports })
}

/// A NETMAP link between one of our ports and an IOL port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Link {
    own: Endpoint,
    peer: Endpoint,
}

fn parse_netmap(text: &str, id: u16) -> Result<Vec<Link>, ConfigError> {
    let mut links: Vec<Link> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
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
        if links.iter().any(|l| l.own == own) {
            return err(format!(
                "NETMAP line {n}: {id}:{}/{} is mapped more than once",
                own.slot, own.port
            ));
        }
        links.push(Link { own, peer });
    }
    Ok(links)
}

fn join(ini: Ini, links: &[Link], id: u16) -> Result<Config, ConfigError> {
    let mut ports = Vec::new();
    for (slot, port, ifname) in ini.ports {
        let Some(link) = links
            .iter()
            .find(|l| (l.own.slot, l.own.port) == (slot, port))
        else {
            return err(format!("{id}:{slot}/{port} ({ifname}) has no NETMAP entry"));
        };
        ports.push(PortConfig {
            slot,
            port,
            ifname,
            iol_id: link.peer.id,
            iol_port: netio::port_byte(link.peer.slot, link.peer.port),
        });
    }
    if ports.is_empty() {
        return err(format!("no [{id}:slot/port] sections in the ini file"));
    }
    let warnings = links
        .iter()
        .filter(|l| {
            !ports
                .iter()
                .any(|p| (p.slot, p.port) == (l.own.slot, l.own.port))
        })
        .map(|l| {
            format!(
                "NETMAP maps {id}:{}/{} but the ini file has no section for it; ignored",
                l.own.slot, l.own.port
            )
        })
        .collect();
    Ok(Config { ports, warnings })
}

/// Loads the ini file and its NETMAP. A relative `netmap` path is taken
/// relative to the ini file; without one, `NETMAP` next to the ini file is used.
pub fn load(ini_path: &Path, id: u16) -> Result<Config, ConfigError> {
    let text =
        fs::read_to_string(ini_path).or_else(|e| err(format!("{}: {e}", ini_path.display())))?;
    let ini = parse_ini(&text, id)?;
    let dir = ini_path.parent().unwrap_or(Path::new("."));
    let netmap_path: PathBuf = dir.join(ini.netmap.as_deref().unwrap_or("NETMAP"));
    let text = fs::read_to_string(&netmap_path)
        .or_else(|e| err(format!("{}: {e}", netmap_path.display())))?;
    let links = parse_netmap(&text, id)?;
    join(ini, &links, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAB_INI: &str = "[default]\nbase_port = 49000\nnetmap = /iol/NETMAP\n\
        [513:0/0]\neth_dev = eth0\n[513:0/1]\neth_dev = eth1\n[513:1/0]\neth_dev = eth4\n";
    const CLAB_NETMAP: &str = "1:0/0 513:0/0\n1:0/1 513:0/1\n1:1/0 513:1/0\n";

    fn parse(ini: &str, netmap: &str) -> Result<Config, ConfigError> {
        let ini = parse_ini(ini, 513)?;
        let links = parse_netmap(netmap, 513)?;
        join(ini, &links, 513)
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
        assert_eq!(parse_endpoint("513:0/16"), None);
        assert_eq!(parse_endpoint("0:0/0"), None);
        assert_eq!(parse_endpoint("513:0"), None);
        assert_eq!(parse_endpoint("x:0/0"), None);
    }

    #[test]
    fn containerlab_files() {
        let c = parse(CLAB_INI, CLAB_NETMAP).unwrap();
        assert!(c.warnings.is_empty());
        assert_eq!(
            c.ports,
            vec![
                PortConfig {
                    slot: 0,
                    port: 0,
                    ifname: "eth0".into(),
                    iol_id: 1,
                    iol_port: 0x00
                },
                PortConfig {
                    slot: 0,
                    port: 1,
                    ifname: "eth1".into(),
                    iol_id: 1,
                    iol_port: 0x10
                },
                PortConfig {
                    slot: 1,
                    port: 0,
                    ifname: "eth4".into(),
                    iol_id: 1,
                    iol_port: 0x01
                },
            ]
        );
        assert_eq!(
            parse_ini(CLAB_INI, 513).unwrap().netmap.as_deref(),
            Some("/iol/NETMAP")
        );
    }

    #[test]
    fn relocated_mgmt_port() {
        // mgmt-intf: Ethernet1/2 swaps eth0 and eth6.
        let ini = "[default]\nbase_port = 49000\nnetmap = /iol/NETMAP\n\
            [513:1/2]\neth_dev = eth0\n[513:0/1]\neth_dev = eth1\n[513:0/0]\neth_dev = eth6\n";
        let netmap = "7:1/2 513:1/2\n7:0/1 513:0/1\n7:0/0 513:0/0\n";
        let c = parse(ini, netmap).unwrap();
        assert_eq!(c.ports[0].ifname, "eth0");
        assert_eq!(c.ports[0].port_byte(), 0x21);
        assert_eq!(c.ports[0].iol_port, 0x21);
        assert_eq!(c.ports[0].iol_id, 7);
        assert_eq!(c.ports[2].ifname, "eth6");
        assert_eq!(c.ports[2].port_byte(), 0x00);
    }

    #[test]
    fn netmap_comments_hosts_and_foreign_lines() {
        let netmap = "# comment\n\n1:0/0@a 513:0/0@a  # trailing\n2:0/0 3:0/0\n4:0/0 5:0/0 6:0/0\n\
            513:0/1 1:0/1\n1:1/0 513:1/0\n";
        let c = parse(CLAB_INI, netmap).unwrap();
        assert_eq!(c.ports.len(), 3);
        assert_eq!(c.ports[1].iol_port, 0x10);
    }

    #[test]
    fn netmap_extra_own_port_is_a_warning() {
        let netmap = format!("{CLAB_NETMAP}1:2/0 513:2/0\n");
        let c = parse(CLAB_INI, &netmap).unwrap();
        assert_eq!(c.ports.len(), 3);
        assert_eq!(c.warnings.len(), 1);
    }

    #[test]
    fn other_ids_and_keys_ignored() {
        let ini = "[default]\nnetmap = N\nfoo = bar\n[514:0/0]\neth_dev = eth9\n\
            # comment\n; comment\n[513:0/0]\neth_dev = eth0\nbase_port = 1\n";
        let i = parse_ini(ini, 513).unwrap();
        assert_eq!(i.ports, vec![(0, 0, "eth0".to_string())]);
    }

    #[test]
    fn rejects_bad_ini() {
        assert!(parse_ini("eth_dev = eth0\n", 513).is_err());
        assert!(parse_ini("[513:0/0]\ntap_dev = tap0\n", 513).is_err());
        assert!(
            parse_ini(
                "[513:0/0]\neth_dev = eth0\n[513:0/0]\neth_dev = eth1\n",
                513
            )
            .is_err()
        );
        assert!(parse_ini("[513:0/0]\neth_dev = eth0\neth_dev = eth1\n", 513).is_err());
        assert!(
            parse_ini(
                "[513:0/0]\neth_dev = eth0\n[513:0/1]\neth_dev = eth0\n",
                513
            )
            .is_err()
        );
        assert!(parse_ini("[513:0/0]\neth_dev =\n", 513).is_err());
        assert!(parse_ini("[513:0/99]\neth_dev = eth0\n", 513).is_err());
        assert!(parse_ini("[513:0/0]\nnonsense\n", 513).is_err());
    }

    #[test]
    fn rejects_bad_netmap() {
        // duplicate mapping of one of our ports
        assert!(parse_netmap("1:0/0 513:0/0\n2:0/0 513:0/0\n", 513).is_err());
        // segment with more than two members
        assert!(parse_netmap("1:0/0 2:0/0 513:0/0\n", 513).is_err());
        // a lone endpoint
        assert!(parse_netmap("513:0/0\n", 513).is_err());
        // both ends ours
        assert!(parse_netmap("513:0/1 513:0/0\n", 513).is_err());
        assert!(parse_netmap("1:0/0 513:x\n", 513).is_err());
    }

    #[test]
    fn rejects_unmapped_ini_port() {
        assert!(parse(CLAB_INI, "1:0/0 513:0/0\n1:0/1 513:0/1\n").is_err());
        assert!(parse("[default]\n", "").is_err());
    }

    #[test]
    fn load_resolves_relative_netmap() {
        let dir = std::env::temp_dir().join(format!("netio-bridge-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("iouyap.ini"), "[513:0/0]\neth_dev = eth0\n").unwrap();
        fs::write(dir.join("NETMAP"), "1:0/0 513:0/0\n").unwrap();
        let c = load(&dir.join("iouyap.ini"), 513).unwrap();
        assert_eq!(c.ports.len(), 1);
        assert!(load(&dir.join("missing.ini"), 513).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
