# Prebuilt test contract (UM-107)

`mock_ft.wasm` is the mock USDT (a NEP-141 token with a `mint`) that the near-sandbox demos and tests deploy. It is built from `probes/mock_ft` with `RUSTFLAGS="-C link-arg=-s --remap-path-prefix=$HOME=/build --remap-path-prefix=$PWD=/src" cargo build --release --target wasm32-unknown-unknown --manifest-path probes/mock_ft/Cargo.toml` (no `cargo near build`: it overrides `RUSTFLAGS`, so the paths of the build machine stay in the file; this build has no `wasm-opt` pass and is larger) and committed here **only** so that the quick demo (`npm run demo:quick` in the root of the repository) needs no Rust toolchain. It is a test fixture: it has no access control and must never be deployed on a real network.

- sha256: `65d8a18be9152a4f9a62799f963c91f0542b73a68982c68c2625837252b2c897` (174,475 bytes). `scripts/demo-quick.mjs` checks it before the demo starts; the number lives in `KNOWN_SHA256` of `scripts/deploy-mainnet.mjs`, the table that also pins the wasm of the release.
- It is not a reproducible build (the bytes depend on the local toolchain). When `probes/mock_ft` changes, build it again, copy the file here and update `KNOWN_SHA256.mock_ft` in `scripts/deploy-mainnet.mjs` and the hash in this file.
- The wasm of the circle is not here: the quick demo downloads the release wasm and checks it against the sha256 of the release.

# Circle 0.0.14 (CT-106)

`x207_circle-0.0.14.wasm` is the `cargo near build non-reproducible-wasm` build of the circle at interface 0.0.14 (EVM agents, CT-104/CT-105), committed so that the sandbox tests of `sdk` can run without a Rust toolchain: `X207_CIRCLE_WASM=contracts/prebuilt/x207_circle-0.0.14.wasm`.

- sha256: `743109cbf74c266c75b5416f788ed10aad0233ad74d0211bf3daa9c39c8c2a87` (base58 `8pZfS6mGmJWJxMtPgVVVVLET6uMyobsVunmS8wTNkDGi`). It is the code of `x207-v014.testnet`.
- Not a reproducible build; the release wasm (reproducible) comes with the tag.

# Circle 0.0.15 (CT-108)

`x207_circle-0.0.15.wasm` is the `cargo near build non-reproducible-wasm` build at interface 0.0.15 (opt-in auto-settle from stake).

- sha256: `3632861bddfb9b2993cb337346dfa3a4e6f3f11de0d0e006c8573af90b0d28fe` (base58 `4eZhZF16ySGGv9ux3gzbxnH6znVxcHNQYivVFdfpuHz5`). It is the code of `x207-v015.testnet` (deploy tx `Ex6CjaGH1XgJfRD19KGpFftEapBNqszszcFPnf3roYy9`).
