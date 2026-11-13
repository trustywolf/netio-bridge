# netio-bridge

netio-bridge connects the Ethernet ports of a Cisco IOL (IOS on Linux) instance
to Linux network interfaces. It is a small replacement for `iouyap` in
[Containerlab](https://containerlab.dev) IOL images, and works only there: it
takes the port layout from what Containerlab gives the node and bridges one
Linux interface per IOL port.

It is an independent implementation written in Rust and contains no iouyap
code.

## Why

`iouyap` 0.97.5, the bridge of Containerlab's own IOL images until 2026-11-02,
has four failure modes that take IOL ports down for good:

| Event | iouyap 0.97.5 | netio-bridge |
| --- | --- | --- |
| A peer's veth disappears (peer container stopped or removed) | The thread that serves every port exits on the first failed write; the whole node, management port included, goes dark ([containerlab#3422](https://github.com/srl-labs/containerlab/issues/3422)) | Only that frame is lost; other ports are untouched |
| The container's own interface goes down and up | The port's reader exits on `ENETDOWN` and never reopens | The port resumes when the interface is up again |
| The interface is deleted and recreated (new ifindex) | The port stays dead | The port reopens on the new interface as soon as it appears |
| An 802.1Q-tagged frame longer than 4092 bytes arrives | Stack buffer overflow; the process is killed | Frames up to 9220 bytes are forwarded; longer ones are counted and dropped |

Those images now run [iouyap-go](https://github.com/clab-labs/iouyap-go)
([vrnetlab#561](https://github.com/srl-labs/vrnetlab/pull/561)).

## How it works

- One thread reads NETIO datagrams from IOL on `/tmp/netio<uid>/<appl_id>` and
  sends each frame to its port's packet socket with `MSG_DONTWAIT`. A send error
  costs that frame only; this thread never changes port state.
- Each port has its own thread with an `AF_PACKET` socket bound to the
  interface's ifindex (promiscuous, `PACKET_AUXDATA`, `PACKET_IGNORE_OUTGOING`).
  Frames are sent to IOL with an 8-byte NETIO header.
  When IOL's receive queue is full, the port's thread waits up to 100 ms for
  room before it drops the frame. The queue holds `net.unix.max_dgram_qlen`
  datagrams, only 10 in a container's fresh network namespace, so a burst of
  broadcasts (an ARP sweep of the management network) fills it while IOL is
  still reading. Raising the sysctl in the container (Containerlab: `sysctls:`
  `net.unix.max_dgram_qlen: 512`) also helps the other direction.
- The bridge and IOL start together, so a neighbour's frame can arrive before
  IOL has opened its NETIO socket. The port threads start only once a
  `connect()` to IOL's socket no longer fails with `ENOENT` (no file yet) or
  `ECONNREFUSED` (a file left by an earlier run, as after a container restart):
  until then IOL is not listening, and the bridge tries again every 50 ms. A
  frame that arrives before then is lost either way, as IOL is not reading; the
  wait keeps it out of the error log.
- The kernel strips the outer VLAN tag before a packet socket sees a frame.
  netio-bridge puts it back from `PACKET_AUXDATA`: 802.1Q or 802.1ad TPID as
  reported, priority-tagged frames (VID 0) included.
- One more thread holds an rtnetlink socket in the link group. Every link
  message (an interface added, renamed, changed or deleted in the namespace)
  wakes every port thread through its eventfd. A port thread sleeps in `poll()`
  on its packet socket and its eventfd, with no timeout. Woken, or on a receive
  error, it checks that the interface still exists with the ifindex the socket
  is bound to; if not, it closes the socket and opens one on the new interface
  as soon as a link event shows it. An interface missing at start-up is handled
  the same way. An interface deleted while down gives the socket no error, so
  the link event is the only notice. An idle bridge does not wake up at all.
- The socket of a port is shared through `Mutex<Option<Arc<OwnedFd>>>`: a sender
  holds its own reference, so a socket being replaced is never closed under it.
- One bridge per id: at start-up netio-bridge takes an `fcntl` write lock on
  `/tmp/netio<uid>/<appl_id>.lck`, as IOL does for its own socket, and writes
  its PID into it. A second instance (say, one started by hand for debugging)
  exits with an error instead of unlinking the socket and taking it over, which
  would leave IOL sending to a dead socket once it quit.
- The process exits only on a start-up error, such as an invalid NETMAP or
  Containerlab variable, a held lock or a NETIO socket that cannot be bound.
  Release builds write a panic to the log and abort, so a restart loop around
  the process (see below) recovers every port instead of leaving one thread
  dead.

## Usage

```text
netio-bridge <appl_id> [--prepare] [-q] [-l <log file>]

  <appl_id>      NETIO id of the bridge (Containerlab uses 513)
  --prepare      wait for the links, write /iol/NETMAP with every port of
                 every slot and print the slot count (for iol.bin -e)
  -q             quiet: leave out start-up messages
  -l <log file>  default /tmp/netio-bridge.log
  -h, --help     show this help
  -V, --version  show the version
```

A running bridge writes nothing to stdout or stderr: in an IOL container they
are the router console. The log holds start-up messages (`INFO`, left out with
`-q`), port state changes (`NOTICE`) and errors (`ERROR`, at most one per kind
and port every ten seconds, with a count of the ones held back). Send `SIGUSR1`
to write the traffic and drop counters to the log.

### Port layout

IOL reads `NETMAP` once, at boot, and uses only the ports listed there, while
Containerlab lists only the ports that have links (on a config-drift recreate,
only the management port). So `--prepare`, run before IOL starts:

- waits 5 s for Containerlab to add the node's links;
- takes `CLAB_IOL_NUM_SLOTS` slots when it is set, else enough slots for the
  highest of our ports in Containerlab's `/iol/NETMAP` (written before the
  start, so it lists links that are not back yet after a crash) and for the
  highest-numbered `eth` interface present;
- rewrites `/iol/NETMAP` with every port of every slot, each wired to the same
  port of IOL (`IOL_PID`), and prints the slot count.

The bridge then reads `/iol/NETMAP` back and serves `<slot>/<port>` on
`eth<4 x slot + port>`, except that the management port swaps interfaces with
`Ethernet0/0`, so `eth0` is always management. The management port is the one
`CLAB_IOL_MGMT_INTF` names (Containerlab 0.83 and later), else the one
Containerlab's `/iol/iouyap.ini` gives `eth0`, else `Ethernet0/0`. A link added
later to a running node (`containerlab tools veth create`, or a deploy with
`link-apply-mode: live`) then works too, and a restarted bridge serves exactly
the ports IOL booted with.

`--prepare` stops with an error, and the entrypoint should not start IOL, when
`IOL_PID` is missing or invalid, `CLAB_IOL_NUM_SLOTS` or `CLAB_IOL_MGMT_INTF` is
invalid, or the management port lies beyond the slots. The bridge stops at
start-up when `/iol/NETMAP` is missing, lists none of our ports, maps one of
them twice, or wires one of them to anything but a single IOL port.

### In a Containerlab IOL image

Build it from source in a builder stage. In the entrypoint, take the addresses
off `eth0` (IOL's management port uses them), run `--prepare`, start the bridge
in a restart loop, then IOL:

```dockerfile
FROM ubuntu:resolute AS netio-bridge-build
RUN apt-get update && apt-get install -y --no-install-recommends \
        cargo rustc git ca-certificates
ARG NETIO_BRIDGE_REPO=https://github.com/trustywolf/netio-bridge.git
ARG NETIO_BRIDGE_COMMIT=<full commit SHA>
WORKDIR /src
RUN git init -q . && git fetch -q --depth 1 "$NETIO_BRIDGE_REPO" "$NETIO_BRIDGE_COMMIT" \
    && git checkout -q FETCH_HEAD && cargo build --release --locked

FROM ubuntu:resolute
COPY --from=netio-bridge-build /src/target/release/netio-bridge /usr/bin/netio-bridge
```

```bash
ip addr flush dev eth0
ip -6 addr flush dev eth0
num_slots=$(/usr/bin/netio-bridge 513 --prepare) || exit 1
( while :; do /usr/bin/netio-bridge 513 -q; sleep 1; done ) &
exec /iol/iol.bin $IOL_PID -e $num_slots -s 0 -c config.txt -n 1024
```

The container must be allowed to open packet sockets and set promiscuous mode
(Containerlab runs IOL nodes privileged).

## Limitations

- Linux only (`AF_PACKET`).
- Only Containerlab's layout: one `eth` interface per port, IOL wired to
  `<appl_id>` port for port. No UDP tunnels, TAP devices, pcap capture, port
  mirroring or multi-member segments.
- Frames longer than 9220 bytes are dropped. Such frames come from the Linux
  stack with GSO/TSO, for example when testing with a host process on a veth
  wired to IOL; turn segmentation offload off on that interface
  (`ethtool -K <if> tso off gso off`).
- TX checksum offload on a Linux veth wired to IOL leaves packets with
  unfinished checksums, which IOL rejects. netio-bridge does not fix them up;
  turn it off on the Linux side (`ethtool -K <if> tx off`). Containerlab does
  this for the management veth.

## Building and testing

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

The only dependency is the `libc` crate, pinned in `Cargo.lock`.

## License

MIT, see [LICENSE](LICENSE).
