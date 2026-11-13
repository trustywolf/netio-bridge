#!/bin/sh
# Builds target/netio-bridge_<version>_<arch>.deb: the release binary, the
# README and the licence. The version comes from Cargo.toml, the architecture
# from dpkg, and Depends from the libraries the binary links (dpkg-shlibdeps)
# on the Ubuntu release it is built on. Needs cargo and dpkg-dev.
set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
arch=$(dpkg --print-architecture)
name=netio-bridge_${version}_${arch}
root=target/deb/$name

cargo build --release --locked

rm -rf target/deb
install -Dm755 target/release/netio-bridge "$root/usr/bin/netio-bridge"
install -Dm644 README.md "$root/usr/share/doc/netio-bridge/README.md"
install -Dm644 LICENSE "$root/usr/share/doc/netio-bridge/copyright"

# dpkg-shlibdeps wants a debian/control beside it; -O prints the result
mkdir -p target/deb/debian
printf 'Source: netio-bridge\n\nPackage: netio-bridge\nArchitecture: any\n' \
    > target/deb/debian/control
depends=$(cd target/deb && dpkg-shlibdeps -O "$name/usr/bin/netio-bridge" \
    | sed -n 's/^shlibs:Depends=//p')

mkdir "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: netio-bridge
Version: $version
Architecture: $arch
Maintainer: Xinyu Wei <trustywolf@wolflab.net>
Installed-Size: $(du -ks "$root/usr" | cut -f1)
Depends: $depends
Section: net
Priority: optional
Homepage: https://github.com/trustywolf/netio-bridge
Description: Bridge Cisco IOL NETIO ports to Linux interfaces
 netio-bridge connects the Ethernet ports of a Cisco IOL (IOS on Linux)
 instance to the eth interfaces of a Containerlab IOL node, in place of
 iouyap.
CONTROL

# Only the package's path goes to stdout
dpkg-deb --root-owner-group --build "$root" target >&2
echo "target/$name.deb"
