#!/usr/bin/env bash
# CT-044: package abi.json and the wasm of x207-circle for one semver tag.
# Usage: scripts/package-release.sh v0.0.4   (output in dist/, nothing is pushed or published)
set -euo pipefail
cd "$(dirname "$0")/.."
tag="${1:?usage: package-release.sh v0.x.y}"
[[ "$tag" =~ ^v0\.[0-9]+\.[0-9]+$ ]] || { echo "tag must look like v0.x.y, got $tag" >&2; exit 1; }
version="${tag#v}"
crate=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
iface=$(sed -n 's/.*INTERFACE_VERSION: &str = "\(.*\)";/\1/p' x207-circle/src/lib.rs | head -1)
[ "$crate" = "$version" ] || { echo "Cargo.toml version $crate differs from tag $tag" >&2; exit 1; }
[ "$iface" = "$version" ] || { echo "INTERFACE_VERSION $iface differs from tag $tag" >&2; exit 1; }
cargo near build non-reproducible-wasm
rm -rf dist && mkdir dist
cp target/near/x207_circle.wasm "dist/x207-circle-$tag.wasm"
cp abi.json "dist/abi-$tag.json"
(cd dist && shasum -a 256 "x207-circle-$tag.wasm" "abi-$tag.json" > SHA256SUMS)
cat dist/SHA256SUMS
