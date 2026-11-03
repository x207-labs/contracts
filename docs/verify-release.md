# Verify that a deployed circle runs the code of a release (CT-087)

The wasm of a release is built in a pinned container (`[package.metadata.near.reproducible_build]` in `Cargo.toml`: the image `sourcescan/cargo-near:0.22.0-rust-1.99.1` by digest, the command `cargo near build non-reproducible-wasm --locked`, `Cargo.lock` committed). Two builds of the same commit give the same bytes; the release workflow builds twice and publishes nothing if the two sha256 differ. So the `code_hash` of an account on chain can be traced to a tag. Versions up to v0.0.7 were built on a developer machine and are **not** reproducible (their wasm differs from a build in the container).

## 1. Take the wasm of the release and check it against the published sums
```bash
mkdir release-check && cd release-check
gh release download v0.0.9 --repo x207-labs/contracts
shasum -a 256 -c SHA256SUMS
```

## 2. Compare with the account on chain
`code_hash` of an account is the sha256 of its wasm in base58. Convert the hex sha256 of the release wasm and compare:
```bash
python3 -I - <<'PY'
import hashlib
alpha = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
n = int.from_bytes(hashlib.sha256(open("x207-circle-v0.0.9.wasm", "rb").read()).digest(), "big")
s = ""
while n:
    n, r = divmod(n, 58)
    s = alpha[r] + s
print(s)
PY
curl -s https://rpc.mainnet.near.org -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"query","params":{"request_type":"view_account","finality":"final","account_id":"<circle account>"}}' \
  | python3 -c "import sys,json; print(json.load(sys.stdin)['result']['code_hash'])"
```
The two strings must be equal. Use `rpc.testnet.near.org` for a testnet circle. The view `interface_version` of the account must equal the version in the tag.

## 3. Build the same bytes yourself from the tag
Needs Docker (the image is amd64; Apple silicon runs it under emulation).
```bash
git clone https://github.com/x207-labs/contracts && cd contracts
git checkout v0.0.9
scripts/repro-build.sh v0.0.9 /tmp/x207-build
```
The last line is the sha256 of `/tmp/x207-build/x207_circle.wasm`. It must equal the line for the wasm in `SHA256SUMS` of the release.

## Rules for a deployment
- A rehearsal or mainnet deployment uses the wasm **downloaded from the release**, not a local `cargo near build`. A local build has another sha256 and cannot be traced to a tag.
- After `deploy_contract`, read the `code_hash` of the account and write it next to the transaction hash in the record of the run.
- `apply_upgrade` takes the raw wasm of a release; `propose_upgrade` is given its `sha256` (base58). Check step 2 against the proposed hash before voting.
