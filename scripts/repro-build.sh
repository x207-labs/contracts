#!/usr/bin/env bash
# CT-087: builds the wasm of x207-circle from a git ref inside the pinned cargo-near container, the same image and command that
# [package.metadata.near.reproducible_build] in Cargo.toml names. Only committed files are used (git archive), so two runs of the same ref
# give the same bytes. Needs Docker. Usage: scripts/repro-build.sh [ref] [outdir]   (default: HEAD, ./repro-out). Nothing is pushed or published.
set -euo pipefail
cd "$(dirname "$0")/.."
ref="${1:-HEAD}"
out="${2:-repro-out}"
image=$(sed -n 's/^image = "\(.*\)"/\1/p' Cargo.toml | head -1)
digest=$(sed -n 's/^image_digest = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$image" ] && [ -n "$digest" ] || { echo "no [package.metadata.near.reproducible_build] image in Cargo.toml" >&2; exit 1; }
src=$(mktemp -d)
# the container builds as the user `near` (uid 1000), the runner of GitHub as another uid: the tree must be readable and writable for both, and
# what the container leaves in it may not be removable by the caller, which is not an error of the build
trap 'rm -rf "$src" 2>/dev/null || true' EXIT
git archive "$ref" | tar -x -C "$src"
chmod -R a+rwX "$src"
mkdir -p "$out"
docker run --rm --platform linux/amd64 -v "$src:/home/near/code" -w /home/near/code "${image%%:*}@${digest}" \
  bash -c 'cargo near build non-reproducible-wasm --locked && chmod -R a+rX target/near'
cp "$src/target/near/x207_circle.wasm" "$out/x207_circle.wasm"
cp "$src/target/near/x207_circle_abi.json" "$out/x207_circle_abi.json" 2>/dev/null || true
shasum -a 256 "$out/x207_circle.wasm"
