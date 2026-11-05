# Verify that a deployed circle runs the code of a release (CT-087)

The wasm of a release is built in a pinned container (`[package.metadata.near.reproducible_build]` in `Cargo.toml`: the image `sourcescan/cargo-near:0.22.0-rust-1.99.1` by digest, the command `cargo near build non-reproducible-wasm --locked`, `Cargo.lock` committed). Two builds of the same commit give the same bytes; the release workflow builds twice and publishes nothing if the two sha256 differ. So the `code_hash` of an account on chain can be traced to a tag. Versions up to v0.0.7 were built on a developer machine and are **not** reproducible (their wasm differs from a build in the container).

## What this proves and what it does not
`cargo near build reproducible-wasm` (the standard tool, also used by SourceScan) clones the repository from its remote and builds the files of a tag. `scripts/repro-build.sh` runs the **same image and the same command** that `Cargo.toml` names, on the files of a git ref, without the clone. It proves that this pipeline is deterministic (two builds, same bytes; the workflow `reproducible.yml` checks it on a GitHub runner). It does **not** by itself prove that the sha256 equals what SourceScan or another third-party verifier computes from the same tag, because their build path differs. With the repository public, run `cargo near build reproducible-wasm` on the tag and compare with the sha256 of the release.

## In one command
`scripts/verify-release.sh v0.0.9 [account] [rpc-url]` does steps 1 and 2 below (download into a new folder, `shasum -c`, sha256 in hex and base58), compares with the `code_hash` of `account` when one is given, and with `REBUILD=1` also rebuilds the tag in the container (step 3) and compares. It exits with 1 when something does not match. The negative path was tried: the testnet circle `x207-circle-v10.testnet`, which runs a local build, does **not** match the wasm of v0.0.8 (`NO MATCH`).

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

## Result for v0.0.9 (run on 2026-11-03, `REBUILD=1 scripts/verify-release.sh v0.0.9`)
- Assets of the release: `x207-circle-v0.0.9.wasm`, `abi-v0.0.9.json`, `SHA256SUMS`. `shasum -a 256 -c SHA256SUMS`: both files `OK`.
- sha256 of the wasm (hex): `fa8309059a847263e2b7e59c08f1aff2370e706cff3ff58544412218c187d72b`; base58: `Hrtp8q8p8NfTWXqgF8q7446DTLbhfVJewq3npiXzQauQ`. These are also the two hashes of the builds of the release workflow on the runner.
- Rebuild of the tag (`v0.0.9`, commit `b9236b1`) with `scripts/repro-build.sh` in the pinned image `sourcescan/cargo-near@sha256:c9655eaf…b5bb` on this Mac (Docker, amd64 under emulation): size 585501 bytes, sha256 `fa8309059a847263e2b7e59c08f1aff2370e706cff3ff58544412218c187d72b`. The script printed `REPRODUCED: the build of the tag has the same sha256 as the release`.
- So three builds of the same source (two on the runner, one here) gave the same bytes, on two different machines.
- Not done: no account was given, so the `code_hash` over RPC was not compared (no circle runs this release yet).

## A real example on chain: x207-circle-v11.testnet runs the wasm of v0.0.9 (UM-074, 2026-11-03)
This is the check of steps 1 and 2 on a circle that was deployed from the downloaded release file and not from a local build. The whole record is in `docs/testnet-v11.json` of the umbrella repository (names and hashes only).
- **The file that was deployed:** `x207-circle-v0.0.9.wasm` from `gh release download v0.0.9 --repo x207-labs/contracts`, sha256 `fa8309059a847263e2b7e59c08f1aff2370e706cff3ff58544412218c187d72b` (the same as in `SHA256SUMS`). The test (`tests/testnet-rehearsal-v11.live.test.ts`) refuses to start without `X207_RELEASE_WASM`.
- **The deploy transaction:** `BNq8jJDj2Nkvpho9zDXTfFGs8P7wfHRJXuu4Rhi1X4AD`. Its `DeployContract` action carries the code hash `+oMJBZqEcmPit+WcCPGv8jcOcGz/P/WFREEiGMGH1ys=` (base64), which is the same 32 bytes as the sha256 above. The init transaction is `2VjY2TAWHoWUJ4RcuNJYdmPUyUJobZK1H8wrrA418pc9`. (Both hashes were read back from the chain: the first run of the test stopped on a bug of the test right after sending them.)
- **The code hash of the account over RPC** (`view_account`, `finality: final`): `Hrtp8q8p8NfTWXqgF8q7446DTLbhfVJewq3npiXzQauQ`, which is the base58 of the sha256 of the release wasm. The test checks this equality itself before it goes on, and the view `interface_version` answers `0.0.9`.
- **What ran on that code:** one epoch with five members (join, stake), two bundles (a ring and a quick barter), clearing, residual payment, finalize, claims, `prune_epoch` and the claim after it, in one uninterrupted run of 2001 seconds. Two earlier runs stopped before the epoch, both on faults of the test (a name that shadowed a function; members funded with 0.25 NEAR while the join deposit is 0.32 NEAR), not of the contract.
- **Not covered:** a circle on mainnet, real Intents (`intents.testnet` has no code), and the token and multi-token escrows (they are in 0.0.11, not in this release).

Run on 2026-11-03 after the epoch, the same check for this circle answers `MATCH: the account runs the wasm of v0.0.9`. In one command: `scripts/verify-release.sh v0.0.9 x207-circle-v11.testnet https://rpc.testnet.fastnear.com`.

## Testnet circles v014 and v015 run a prebuilt local build, not a release (CT-110, 2026-11-05)
`x207-v014.testnet` (interface 0.0.14) and `x207-v015.testnet` (interface 0.0.15) were deployed from `cargo near build non-reproducible-wasm` on one machine, and the exact files are committed in `prebuilt/`. They are not the reproducible build of a tag, so `scripts/verify-release.sh` (which downloads a release) does not apply to them. The chain of proof is shorter and has no container: the committed file equals the code on chain.

| Account | File | sha256 (hex) | base58 (= `code_hash`) |
|---|---|---|---|
| `x207-v014.testnet` | `prebuilt/x207_circle-0.0.14.wasm` | `7d731dfb15db44aa3a1ca95c28616a6e2faecbf11b87de538940991da33d39f9` | `9ShmoHDuE6QT3AAj6jYdW2Wo6pr53h5JFfs8ea7pYg6L` (redeployed 2026-11-05, tx `7TiLoPdbBcyksM3XSQ6MKRjBnqTC4V1g1h2DQDfi9zEB`; the code before was `743109cb…` / `8pZfS6mG…`, the code of the SD-090 transactions) |
| `x207-v015.testnet` | `prebuilt/x207_circle-0.0.15.wasm` | `f832e53f01bf6b0739efb54bf0977d70544dd9bcece2234abd3e18525b4900de` | `Hhs82yW5iJD2QH4juP6Ud5u7ApHGhcw8Zo5xCK1hxmoj` (redeployed 2026-11-05, tx `BYHH9YUPWuYtk453LELRFaFut7J4HKNiXyo9n4RydHAW`; the SD-094 transactions ran on the code before: `3632861bddfb9b2993cb337346dfa3a4e6f3f11de0d0e006c8573af90b0d28fe` / `4eZhZF16ySGGv9ux3gzbxnH6znVxcHNQYivVFdfpuHz5`) |

Checked on 2026-11-05: the sha256 of the code returned by the RPC `view_code` of each account (708,964 and 720,952 bytes) is equal to the sha256 of the committed file, and the `hash` field of `view_code` equals the base58 above. To repeat it:

```bash
ACCOUNT=x207-v015.testnet; FILE=prebuilt/x207_circle-0.0.15.wasm
curl -s -X POST https://rpc.testnet.fastnear.com -H 'content-type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"query\",\"params\":{\"request_type\":\"view_code\",\"finality\":\"final\",\"account_id\":\"$ACCOUNT\"}}" \
  | python3 -c 'import json,sys,base64,hashlib;r=json.load(sys.stdin)["result"];print(hashlib.sha256(base64.b64decode(r["code_base64"])).hexdigest(), r["hash"])'
shasum -a 256 "$FILE"    # the two hex values must be equal
```

What this does **not** prove: that the file was built from a given commit. A local build is not byte-reproducible on another machine (a different toolchain path or container gives another hash). A reproducible build of the same source (`scripts/repro-build.sh <ref> <outdir>`, needs Docker) has its own hash and will differ from the account above; a mainnet deployment must use the wasm of a release, as written above. Source of the 0.0.14 file: commits `87f29fb` to `f03879e`; of the 0.0.15 file: commit `a933555` (`CT-108`).
