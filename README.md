# netio-bridge

netio-bridge connects the Ethernet ports of a Cisco IOL (IOS on Linux) instance
to Linux network interfaces. It is a small replacement for `iouyap` in
[Containerlab](https://containerlab.dev) IOL images: it reads the same
`iouyap.ini` and `NETMAP` files Containerlab writes, and supports only what
Containerlab uses, one Linux interface per IOL port.

It is an independent implementation written in Rust. It accepts the iouyap
configuration format but contains no iouyap code.

## Why

`iouyap` 0.97.5, as shipped for Containerlab, has three failure modes that take
IOL ports down for good:

| Event | iouyap 0.97.5 | netio-bridge |
| --- | --- | --- |
| A peer's veth disappears (peer container stopped or removed) | The thread that serves every port exits on the first failed write; the whole node, management port included, goes dark ([containerlab#3422](https://github.com/srl-labs/containerlab/issues/3422)) | Only that frame is lost; other ports are untouched |
| The container's own interface goes down and up | The port's reader exits on `ENETDOWN` and never reopens | The port resumes when the interface is up again |
| The interface is deleted and recreated (new ifindex) | The port stays dead | The port reopens on the new interface within about a second |
| An 802.1Q-tagged frame longer than 4092 bytes arrives | Stack buffer overflow; the process is killed | Frames up to 9220 bytes are forwarded; longer ones are counted and dropped |

## How it works

- One thread reads NETIO datagrams from IOL on `/tmp/netio<uid>/<appl_id>` and
  sends each frame to its port's packet socket with `MSG_DONTWAIT`. A send error
  costs that frame only; this thread never changes port state.
- Each port has its own thread with an `AF_PACKET` socket bound to the
  interface's ifindex (promiscuous, `PACKET_AUXDATA`, `PACKET_IGNORE_OUTGOING`,
  1 s receive timeout). Frames are sent to IOL with an 8-byte NETIO header.
  When IOL's receive queue is full, the port's thread waits up to 100 ms for
  room before it drops the frame. The queue holds `net.unix.max_dgram_qlen`
  datagrams, only 10 in a container's fresh network namespace, so a burst of
  broadcasts (an ARP sweep of the management network) fills it while IOL is
  still reading. Raising the sysctl in the container (Containerlab: `sysctls:`
  `net.unix.max_dgram_qlen: 512`) also helps the other direction.
- The kernel strips the outer VLAN tag before a packet socket sees a frame.
  netio-bridge puts it back from `PACKET_AUXDATA`: 802.1Q or 802.1ad TPID as
  reported, priority-tagged frames (VID 0) included.
- On a receive timeout or error, the port thread checks that the interface still
  exists with the ifindex the socket is bound to. If not, it closes the socket
  and retries every second until the interface is back. An interface missing at
  start-up is handled the same way.
- The socket of a port is shared through `Mutex<Option<Arc<OwnedFd>>>`: a sender
  holds its own reference, so a socket being replaced is never closed under it.
- One bridge per id: at start-up netio-bridge takes an `fcntl` write lock on
  `/tmp/netio<uid>/<appl_id>.lck`, as IOL does for its own socket, and writes
  its PID into it. A second instance (say, one started by hand for debugging)
  exits with an error instead of unlinking the socket and taking it over, which
  would leave IOL sending to a dead socket once it quit.
- The process exits only on a start-up error (invalid ini or NETMAP, the lock
  is held, NETIO socket cannot be bound). Release builds abort on a panic, so a
  restart loop around the process (see below) recovers every port instead of
  leaving one thread dead.

## Usage

```text
netio-bridge <appl_id> [-q] [-c <ini file>] [-l <log file>]

  <appl_id>      NETIO id of the bridge (Containerlab uses 513)
  -q             quiet: leave out start-up messages
  -c <ini file>  default /iol/iouyap.ini
  -l <log file>  default /tmp/netio-bridge.log
```

Nothing is written to stdout or stderr at run time: in an IOL container they are
the router console. The log holds start-up messages (`INFO`, left out with
`-q`), port state changes (`NOTICE`) and errors (`ERROR`, at most one per kind
and port every ten seconds, with a count of the ones held back). Send `SIGUSR1`
to write the traffic and drop counters to the log.

### Configuration

The subset Containerlab generates:

```ini
[default]
base_port = 49000
netmap = /iol/NETMAP
[513:0/0]
eth_dev = eth0
[513:0/1]
eth_dev = eth1
```

```text
1:0/0 513:0/0
1:0/1 513:0/1
```

- Sections `[<appl_id>:<slot>/<port>]` need `eth_dev`; other keys and sections
  for other ids are ignored. A relative `netmap` path is taken relative to the
  ini file; without one, `NETMAP` next to the ini file is used.
- NETMAP: `#` starts a comment, an `@host` suffix is ignored, lines that do not
  involve `<appl_id>` are ignored. Slot and port must be below 16.
- Each of our ports must appear in exactly one NETMAP line with exactly one IOL
  endpoint. Segments (more than two members), duplicate mappings, one interface
  on two ports, or a port with no NETMAP line stop the bridge at start-up.

### In a Containerlab IOL image

Build it from source in a builder stage and start it from the entrypoint in a
restart loop, before IOL:

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
( while :; do /usr/bin/netio-bridge 513 -q; sleep 1; done ) &
exec /iol/iol.bin $IOL_PID -e $num_slots -s 0 -c config.txt -n 1024
```

The container must be allowed to open packet sockets and set promiscuous mode
(Containerlab runs IOL nodes privileged).

## Limitations

- Linux only (`AF_PACKET`).
- Only `eth_dev` ports. No UDP tunnels, TAP devices, pcap capture, port
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
