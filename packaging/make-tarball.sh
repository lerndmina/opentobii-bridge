#!/bin/sh
# Build the release tarball from an existing release build.
#   packaging/make-tarball.sh VERSION [OUT_DIR]
set -eu

version=${1:?usage: make-tarball.sh VERSION [OUT_DIR]}
root=$(cd "$(dirname "$0")/.." && pwd)
out=${2:-$root/target/dist}
name="opentobii-$version-x86_64-linux"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/$name" "$out"
install -m 755 "$root/target/release/opentobii" "$stage/$name/opentobii"
install -m 755 "$root/packaging/install.sh" "$root/packaging/uninstall.sh" "$stage/$name/"
install -m 644 "$root/packaging/opentobii.service" "$root/packaging/opentobii.desktop" \
    "$root/packaging/opentobii.svg" "$root/LICENSE" "$root/README.md" "$stage/$name/"
tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
echo "$out/$name.tar.gz"
