#!/usr/bin/env bash
# CT-103: downloads the wasm of the real Intents contract (`intents.near`) from mainnet with the RPC view `view_code` (read only, no transaction, no key) and checks its sha256, so that
# tests/ct103_real_intents.rs can run the circle against the real contract in near-sandbox. Usage: scripts/fetch-intents-wasm.sh [rpc-url]   (default https://rpc.mainnet.fastnear.com)
# The file goes to target/intents-near/intents.wasm (not committed). The sha256 below is the code of intents.near seen at block 222990644 (code_hash BtA1BEFNS619KkvXFsgcpQ21Tb5yMm32aaUkn7xNothk);
# when Intents is upgraded the hash changes and this script stops: look at the new code, then change the hash here and in the test.
set -euo pipefail
cd "$(dirname "$0")/.."
expected="a1afab98916d9f213746544dcf8e4e6bdb22c0be6bc5dfbd8ea8814c9922b547"
rpc="${1:-https://rpc.mainnet.fastnear.com}"
out="target/intents-near"
mkdir -p "$out"
curl -fsS -X POST "$rpc" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"query","params":{"request_type":"view_code","finality":"final","account_id":"intents.near"}}' \
  | python3 -I -c 'import base64,json,sys; r=json.load(sys.stdin)["result"]; sys.stdout.buffer.write(base64.b64decode(r["code_base64"]))' > "$out/intents.wasm"
actual=$(shasum -a 256 "$out/intents.wasm" | cut -d' ' -f1)
echo "intents.near wasm: $(wc -c < "$out/intents.wasm" | tr -d ' ') bytes, sha256 $actual"
if [ "$actual" != "$expected" ]; then
  echo "sha256 differs from the expected $expected: intents.near was upgraded; the file was kept at $out/intents.wasm but the test will refuse it" >&2
  exit 1
fi
