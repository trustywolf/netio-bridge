//! netio-bridge: connects the Ethernet ports of a Cisco IOL instance (NETIO
//! datagrams on Unix sockets) to Linux network interfaces, in a Containerlab
//! IOL node.

#[cfg(not(target_os = "linux"))]
compile_error!("netio-bridge supports Linux only (AF_PACKET)");

mod config;
mod link;
mod log;
mod netio;
mod port;
mod stats;

use std::convert::Infallible;
use std::fs;
use std::io;
use std::mem;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::process::ExitCode;
use std::ptr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use port::Port;
use stats::UnixStats;

const USAGE: &str = "\
usage: netio-bridge <appl_id> [--prepare] [-q] [-l <log file>]

Bridges the Ethernet ports of a Containerlab IOL node to its eth interfaces.
Run it with --prepare before IOL starts, then without, beside IOL.

  <appl_id>      NETIO id of the bridge (Containerlab uses 513)
  --prepare      wait for the links, write /iol/NETMAP with every port of
                 every slot and print the slot count (for iol.bin -e)
  -q             quiet: leave out start-up messages
  -l <log file>  default /tmp/netio-bridge.log
  -h, --help     show this help
  -V, --version  show the version

Send SIGUSR1 to write the traffic and drop counters to the log.";

/// How long --prepare waits for Containerlab to add the node's links.
const PREPARE_WAIT: Duration = Duration::from_secs(5);

struct Args {
    id: u16,
    prepare: bool,
    quiet: bool,
    log: PathBuf,
}

enum Cli {
    Run(Args),
    Exit(ExitCode),
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut id = None;
    let mut prepare = false;
    let mut quiet = false;
    let mut log = PathBuf::from("/tmp/netio-bridge.log");
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(Cli::Exit(ExitCode::SUCCESS));
            }
            "-V" | "--version" => {
                println!("netio-bridge {}", env!("CARGO_PKG_VERSION"));
                return Ok(Cli::Exit(ExitCode::SUCCESS));
            }
            "--prepare" => prepare = true,
            "-q" => quiet = true,
            "-l" => log = args.next().ok_or("-l needs a file")?.into(),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s if id.is_none() => match s.parse::<u16>() {
                Ok(n) if n > 0 => id = Some(n),
                _ => return Err(format!("bad appl_id {s}")),
            },
            s => return Err(format!("unexpected argument {s}")),
        }
    }
    let id = id.ok_or("missing appl_id")?;
    Ok(Cli::Run(Args {
        id,
        prepare,
        quiet,
        log,
    }))
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Cli::Run(a)) => a,
        Ok(Cli::Exit(code)) => return code,
        Err(e) => {
            // Wrong command line only; at run time nothing is written here.
            eprintln!("netio-bridge: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    log::init(&args.log, args.quiet);
    if args.prepare {
        // Runs before IOL, from the entrypoint: stdout carries the slot count,
        // and stderr reaches the container log.
        thread::sleep(PREPARE_WAIT);
        return match config::prepare(args.id) {
            Ok(slots) => {
                log::info(&format!(
                    "netio-bridge {} prepared {} with {slots} slot(s)",
                    env!("CARGO_PKG_VERSION"),
                    config::NETMAP
                ));
                println!("{slots}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                log::error("fatal", &format!("prepare: {e}"));
                eprintln!("netio-bridge: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let Err(e) = run(&args);
    log::error("fatal", &format!("fatal: {e}"));
    ExitCode::FAILURE
}

/// Returns only on a start-up error; once bridging, runs forever.
fn run(args: &Args) -> Result<Infallible, String> {
    let ports = config::load(args.id).map_err(|e| e.to_string())?;
    log::info(&format!(
        "netio-bridge {} starting as {} with {}",
        env!("CARGO_PKG_VERSION"),
        args.id,
        config::NETMAP
    ));

    let dir = netio::socket_dir();
    match fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("mkdir {dir}: {e}")),
    }
    let path = format!("{dir}/{}", args.id);
    // Held for the life of the process: a second instance would otherwise
    // unlink the socket under this one and take it over.
    let _lock = netio::lock(&path)?;
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove stale {path}: {e}")),
    }
    let sock = UnixDatagram::bind(&path).map_err(|e| format!("bind {path}: {e}"))?;
    sock.set_write_timeout(Some(netio::SEND_TIMEOUT))
        .map_err(|e| format!("set send timeout on {path}: {e}"))?;
    let sock = Arc::new(sock);

    // SIGUSR1 is taken with sigwait() below; block it before any thread starts
    // so every thread inherits the mask.
    // SAFETY: sigset_t is initialised by sigemptyset before use.
    let set = unsafe {
        let mut set: libc::sigset_t = mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, ptr::null_mut());
        set
    };

    let mut iol_ids: Vec<u16> = ports.iter().map(|p| p.iol_id).collect();
    iol_ids.sort_unstable();
    iol_ids.dedup();
    for iol_id in iol_ids {
        let iol = format!("{dir}/{iol_id}");
        let waited = netio::wait_for_listener(&iol).map_err(|e| format!("probe {iol}: {e}"))?;
        log::info(&format!(
            "IOL {iol_id} listening on {iol} after {} ms",
            waited.as_millis()
        ));
    }

    let watcher = link::Watcher::open().map_err(|e| format!("rtnetlink socket: {e}"))?;
    let ports = ports
        .into_iter()
        .map(Port::new)
        .collect::<io::Result<Vec<_>>>()
        .map_err(|e| format!("eventfd: {e}"))?;
    let ports: Arc<Vec<Port>> = Arc::new(ports);
    let unix_stats = Arc::new(UnixStats::default());
    for i in 0..ports.len() {
        let (ports, sock, id) = (ports.clone(), sock.clone(), args.id);
        thread::Builder::new()
            .name(format!("port-{}", ports[i].cfg.ifname))
            .spawn(move || ports[i].run(&sock, id))
            .map_err(|e| format!("spawn port thread: {e}"))?;
    }
    {
        let (ports, sock, stats, id) = (ports.clone(), sock.clone(), unix_stats.clone(), args.id);
        thread::Builder::new()
            .name("netio".into())
            .spawn(move || netio::run_reader(sock, id, ports, stats))
            .map_err(|e| format!("spawn netio thread: {e}"))?;
    }
    {
        let ports = ports.clone();
        thread::Builder::new()
            .name("link".into())
            .spawn(move || watcher.run(ports))
            .map_err(|e| format!("spawn link thread: {e}"))?;
    }
    log::info(&format!("bridging {} ports on {path}", ports.len()));

    loop {
        let mut sig: libc::c_int = 0;
        // SAFETY: set and sig are valid for the call.
        if unsafe { libc::sigwait(&set, &mut sig) } == 0 && sig == libc::SIGUSR1 {
            log::notice(&unix_stats.line());
            for p in ports.iter() {
                log::notice(&p.stats.line(p.label()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: &[&str]) -> Result<Cli, String> {
        parse_args(v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn args() {
        let Ok(Cli::Run(a)) = parse(&["513", "-q"]) else {
            panic!()
        };
        assert_eq!((a.id, a.prepare, a.quiet), (513, false, true));
        assert_eq!(a.log, PathBuf::from("/tmp/netio-bridge.log"));
        let Ok(Cli::Run(a)) = parse(&["--prepare", "513", "-l", "/y.log"]) else {
            panic!()
        };
        assert_eq!((a.id, a.prepare, a.quiet), (513, true, false));
        assert_eq!(a.log, PathBuf::from("/y.log"));
    }

    #[test]
    fn bad_args() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["0"]).is_err());
        assert!(parse(&["70000"]).is_err());
        assert!(parse(&["513", "514"]).is_err());
        assert!(parse(&["513", "-x"]).is_err());
        assert!(parse(&["513", "-l"]).is_err());
        // The ini file option is gone: the layout comes from Containerlab.
        assert!(parse(&["513", "-c", "/x.ini"]).is_err());
    }
}
