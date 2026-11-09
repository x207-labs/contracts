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
# CT-111: optional time limit (X207_REPRO_TIMEOUT seconds, default none). macOS has no `timeout`: use `gtimeout` (brew install coreutils), else run without a limit.
limit=()
if [ -n "${X207_REPRO_TIMEOUT:-}" ]; then
  if command -v timeout >/dev/null 2>&1; then limit=(timeout "$X207_REPRO_TIMEOUT")
  elif command -v gtimeout >/dev/null 2>&1; then limit=(gtimeout "$X207_REPRO_TIMEOUT")
  else echo "warning: neither timeout nor gtimeout found (brew install coreutils); running without a time limit" >&2; fi
fi
${limit[@]+"${limit[@]}"} docker run --rm --platform linux/amd64 -v "$src:/home/near/code" -w /home/near/code "${image%%:*}@${digest}" \
  bash -c 'cargo near build non-reproducible-wasm --locked && if [ -f locker/Cargo.toml ]; then cargo near build non-reproducible-wasm --locked --manifest-path locker/Cargo.toml; fi && chmod -R a+rX target/near'
cp "$src/target/near/x207_circle.wasm" "$out/x207_circle.wasm"
cp "$src/target/near/x207_circle_abi.json" "$out/x207_circle_abi.json" 2>/dev/null || true
shasum -a 256 "$out/x207_circle.wasm"
# CT-125 (0.0.18): the locker contract has its own wasm and its own sha256
if [ -f "$src/target/near/x207_locker/x207_locker.wasm" ]; then
  cp "$src/target/near/x207_locker/x207_locker.wasm" "$out/x207_locker.wasm"
  shasum -a 256 "$out/x207_locker.wasm"
fi
