//! netio-bridge: connects the Ethernet ports of a Cisco IOL instance (NETIO
//! datagrams on Unix sockets) to Linux network interfaces, using the
//! iouyap-style `iouyap.ini` and `NETMAP` files that Containerlab writes.

#[cfg(not(target_os = "linux"))]
compile_error!("netio-bridge supports Linux only (AF_PACKET)");

mod config;
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

use port::Port;
use stats::UnixStats;

const USAGE: &str = "\
usage: netio-bridge <appl_id> [-q] [-c <ini file>] [-l <log file>]

Bridges IOL Ethernet ports to Linux interfaces, as mapped by an iouyap-style
ini file ([<appl_id>:<slot>/<port>] sections with eth_dev) and its NETMAP.

  <appl_id>      NETIO id of the bridge (Containerlab uses 513)
  -q             quiet: leave out start-up messages
  -c <ini file>  default /iol/iouyap.ini
  -l <log file>  default /tmp/netio-bridge.log
  -h, --help     show this help
  -V, --version  show the version

Send SIGUSR1 to write the traffic and drop counters to the log.";

struct Args {
    id: u16,
    quiet: bool,
    ini: PathBuf,
    log: PathBuf,
}

enum Cli {
    Run(Args),
    Exit(ExitCode),
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut id = None;
    let mut quiet = false;
    let mut ini = PathBuf::from("/iol/iouyap.ini");
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
            "-q" => quiet = true,
            "-c" => ini = args.next().ok_or("-c needs a file")?.into(),
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
        quiet,
        ini,
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
    let Err(e) = run(&args);
    log::error("fatal", &format!("fatal: {e}"));
    ExitCode::FAILURE
}

/// Returns only on a start-up error; once bridging, runs forever.
fn run(args: &Args) -> Result<Infallible, String> {
    let cfg = config::load(&args.ini, args.id).map_err(|e| e.to_string())?;
    log::info(&format!(
        "netio-bridge {} starting as {} with {}",
        env!("CARGO_PKG_VERSION"),
        args.id,
        args.ini.display()
    ));
    for w in &cfg.warnings {
        log::notice(w);
    }

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
    let sock = Arc::new(UnixDatagram::bind(&path).map_err(|e| format!("bind {path}: {e}"))?);

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

    let ports: Arc<Vec<Port>> = Arc::new(cfg.ports.into_iter().map(Port::new).collect());
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
        assert_eq!(a.id, 513);
        assert!(a.quiet);
        assert_eq!(a.ini, PathBuf::from("/iol/iouyap.ini"));
        assert_eq!(a.log, PathBuf::from("/tmp/netio-bridge.log"));
        let Ok(Cli::Run(a)) = parse(&["-c", "/x.ini", "513", "-l", "/y.log"]) else {
            panic!()
        };
        assert_eq!((a.id, a.quiet), (513, false));
        assert_eq!(a.ini, PathBuf::from("/x.ini"));
        assert_eq!(a.log, PathBuf::from("/y.log"));
    }

    #[test]
    fn bad_args() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["0"]).is_err());
        assert!(parse(&["70000"]).is_err());
        assert!(parse(&["513", "514"]).is_err());
        assert!(parse(&["513", "-x"]).is_err());
        assert!(parse(&["513", "-c"]).is_err());
    }
}
