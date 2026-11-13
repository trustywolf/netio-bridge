#!/bin/sh
# usage: packaging/deb.sh [amd64|amd64v3]
#
# Builds target/netio-bridge_<version>_<amd64|amd64v3>.deb: the release binary,
# the README and the licence. The version comes from Cargo.toml, and Depends
# from the libraries the binary links (dpkg-shlibdeps) on the Ubuntu release it
# is built on. Needs cargo and dpkg-dev, on an amd64 machine.
#
# amd64v3 is Ubuntu's variant of amd64 for x86-64-v3 processors: the package
# keeps Architecture: amd64 and adds Architecture-Variant: amd64v3, and the
# binary is built with target-cpu=x86-64-v3, the counterpart of the
# -march=x86-64-v3 that Ubuntu's dpkg-buildflags sets for the variant.
set -eu
cd "$(dirname "$0")/.."

variant=${1:-amd64}
case $variant in
amd64)
    target=target
    ;;
amd64v3)
    # Its own directory: target/release stays the plain build
    target=target/amd64v3
    export RUSTFLAGS="-C target-cpu=x86-64-v3"
    ;;
*)
    echo "usage: $0 [amd64|amd64v3]" >&2
    exit 2
    ;;
esac
arch=$(dpkg --print-architecture)
if [ "$arch" != amd64 ]; then
    echo "$0: this machine is $arch, not amd64" >&2
    exit 1
fi

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
name=netio-bridge_${version}_${variant}
work=target/deb/$variant
root=$work/$name

cargo build --release --locked --target-dir "$target"

rm -rf "$work"
install -Dm755 "$target/release/netio-bridge" "$root/usr/bin/netio-bridge"
install -Dm644 README.md "$root/usr/share/doc/netio-bridge/README.md"
install -Dm644 LICENSE "$root/usr/share/doc/netio-bridge/copyright"

# dpkg-shlibdeps wants a debian/control beside it; -O prints the result
mkdir -p "$work/debian"
printf 'Source: netio-bridge\n\nPackage: netio-bridge\nArchitecture: any\n' \
    > "$work/debian/control"
depends=$(cd "$work" && dpkg-shlibdeps -O "$name/usr/bin/netio-bridge" \
    | sed -n 's/^shlibs:Depends=//p')

mkdir "$root/DEBIAN"
{
    echo "Package: netio-bridge"
    echo "Version: $version"
    echo "Architecture: $arch"
    if [ "$variant" != "$arch" ]; then
        echo "Architecture-Variant: $variant"
    fi
    cat <<CONTROL
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
} > "$root/DEBIAN/control"

# Only the package's path goes to stdout
dpkg-deb --root-owner-group --build "$root" "target/$name.deb" >&2
echo "target/$name.deb"
