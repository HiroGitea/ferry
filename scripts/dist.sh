#!/usr/bin/env bash
# Package already-built release binaries into dist/ferry-<version>-<target>.tar.gz.
#   scripts/dist.sh <target> [version]
# Expects target/<target>/release/{ferryd,ferry} (or target/release/... when <target> is "host").
set -euo pipefail
cd "$(dirname "$0")/.."

target=${1:?usage: scripts/dist.sh <target> [version]}
version=${2:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)}
bindir=target/$target/release
[ "$target" = host ] && { bindir=target/release; target=$(rustc -vV | sed -n 's/host: //p'); }

name=ferry-$version-$target
stage=dist/$name
rm -rf "$stage" && mkdir -p "$stage/packaging"
install -m755 "$bindir/ferryd" "$bindir/ferry" "$stage/"
cp README.md README.zh.md LICENSE "$stage/"
cp packaging/ferry.toml packaging/ferryd.service packaging/ferry-agent.toml packaging/ferry-agent.service \
   packaging/ferry.sysusers packaging/ferry.tmpfiles "$stage/packaging/"
mkdir -p "$stage/images" && cp images/Containerfile "$stage/images/"

tar -C dist --owner=0 --group=0 --numeric-owner -czf "dist/$name.tar.gz" "$name"
rm -rf "$stage"
(cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "dist/$name.tar.gz"
