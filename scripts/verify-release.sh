#!/usr/bin/env bash
# CT-087: checks a release of x207-circle (docs/verify-release.md). Usage: scripts/verify-release.sh v0.0.9 [account] [rpc-url]
#   1. downloads the wasm, abi and SHA256SUMS of the release into a new folder and runs `shasum -a 256 -c SHA256SUMS`;
#   2. prints the sha256 of the wasm as hex and as base58 (the form of `code_hash` on chain);
#   3. with an `account`: reads its `code_hash` over RPC and compares (default RPC: testnet if the account ends in .testnet, else mainnet);
#   4. with REBUILD=1 (needs Docker and the tag in this clone): builds the tag with scripts/repro-build.sh and compares the two sha256.
# Nothing is pushed or published.
set -euo pipefail
cd "$(dirname "$0")/.."
tag="${1:?usage: scripts/verify-release.sh v0.x.y [account] [rpc-url]}"
account="${2:-}"
repo="${X207_RELEASE_REPO:-x207-labs/contracts}"
dir=$(mktemp -d)
echo "folder: $dir"
(cd "$dir" && gh release download "$tag" --repo "$repo" >/dev/null && shasum -a 256 -c SHA256SUMS)
wasm="$dir/x207-circle-$tag.wasm"
hex=$(shasum -a 256 "$wasm" | cut -d' ' -f1)
b58=$(python3 -I - "$hex" <<'PY'
import sys
alpha = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
n = int(sys.argv[1], 16)
s = ""
while n:
    n, r = divmod(n, 58)
    s = alpha[r] + s
print(s)
PY
)
echo "wasm sha256 (hex):    $hex"
echo "wasm sha256 (base58): $b58"
status=0
if [ -n "$account" ]; then
  rpc="${3:-}"
  if [ -z "$rpc" ]; then
    case "$account" in *.testnet) rpc="https://rpc.testnet.fastnear.com" ;; *) rpc="https://rpc.mainnet.fastnear.com" ;; esac
  fi
  onchain=$(curl -s "$rpc" -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"query\",\"params\":{\"request_type\":\"view_account\",\"finality\":\"final\",\"account_id\":\"$account\"}}" \
    | python3 -I -c "import sys,json; print(json.load(sys.stdin)['result']['code_hash'])")
  echo "code_hash of $account: $onchain"
  if [ "$onchain" = "$b58" ]; then echo "MATCH: the account runs the wasm of $tag"; else echo "NO MATCH: the account does not run the wasm of $tag"; status=1; fi
fi
if [ "${REBUILD:-}" = "1" ]; then
  rebuilt=$(scripts/repro-build.sh "$tag" "$dir/rebuild" | tail -1 | cut -d' ' -f1)
  echo "rebuild of $tag: $rebuilt"
  if [ "$rebuilt" = "$hex" ]; then echo "REPRODUCED: the build of the tag has the same sha256 as the release"; else echo "NOT REPRODUCED: the build of the tag differs from the release"; status=1; fi
fi
exit $status
