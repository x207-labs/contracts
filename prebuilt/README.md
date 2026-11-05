# Prebuilt test contract (UM-107)

`mock_ft.wasm` is the mock USDT (a NEP-141 token with a `mint`) that the near-sandbox demos and tests deploy. It is built from `probes/mock_ft` with `RUSTFLAGS="-C link-arg=-s --remap-path-prefix=$HOME=/build --remap-path-prefix=$PWD=/src" cargo build --release --target wasm32-unknown-unknown --manifest-path probes/mock_ft/Cargo.toml` (no `cargo near build`: it overrides `RUSTFLAGS`, so the paths of the build machine stay in the file; this build has no `wasm-opt` pass and is larger) and committed here **only** so that the quick demo (`npm run demo:quick` in the root of the repository) needs no Rust toolchain. It is a test fixture: it has no access control and must never be deployed on a real network.

- sha256: `65d8a18be9152a4f9a62799f963c91f0542b73a68982c68c2625837252b2c897` (174,475 bytes). `scripts/demo-quick.mjs` checks it before the demo starts; the number lives in `KNOWN_SHA256` of `scripts/deploy-mainnet.mjs`, the table that also pins the wasm of the release.
- It is not a reproducible build (the bytes depend on the local toolchain). When `probes/mock_ft` changes, build it again, copy the file here and update `KNOWN_SHA256.mock_ft` in `scripts/deploy-mainnet.mjs` and the hash in this file.
- The wasm of the circle is not here: the quick demo downloads the release wasm and checks it against the sha256 of the release.

# Circle 0.0.14 (CT-106)

`x207_circle-0.0.14.wasm` is the `cargo near build non-reproducible-wasm` build of the circle at interface 0.0.14 (EVM agents, CT-104/CT-105), committed so that the sandbox tests of `sdk` can run without a Rust toolchain: `X207_CIRCLE_WASM=contracts/prebuilt/x207_circle-0.0.14.wasm`.

- sha256: `7d731dfb15db44aa3a1ca95c28616a6e2faecbf11b87de538940991da33d39f9` (709,266 bytes, base58 `9ShmoHDuE6QT3AAj6jYdW2Wo6pr53h5JFfs8ea7pYg6L`). It is the **reproducible** build of commit `69b129f` (`scripts/repro-build.sh`) and the code of `x207-v014.testnet` since 2026-11-05 (redeploy tx `7TiLoPdbBcyksM3XSQ6MKRjBnqTC4V1g1h2DQDfi9zEB`, state unchanged). The SD-090 run happened on the earlier host build, sha256 `743109cbf74c266c75b5416f788ed10aad0233ad74d0211bf3daa9c39c8c2a87` (base58 `8pZfS6mGmJWJxMtPgVVVVLET6uMyobsVunmS8wTNkDGi`).
- CT-111: the old host build differed from the reproducible one because it held 27 absolute paths of the build host (`/Users/<name>/.cargo/...`) against `/home/near/.cargo/...` in the container; same `Cargo.lock`, same toolchain 1.99.1.

# Circle 0.0.15 (CT-108)

`x207_circle-0.0.15.wasm` is the `cargo near build non-reproducible-wasm` build at interface 0.0.15 (opt-in auto-settle from stake).

- sha256: `3632861bddfb9b2993cb337346dfa3a4e6f3f11de0d0e006c8573af90b0d28fe` (base58 `4eZhZF16ySGGv9ux3gzbxnH6znVxcHNQYivVFdfpuHz5`). It is the code of `x207-v015.testnet` (deploy tx `Ex6CjaGH1XgJfRD19KGpFftEapBNqszszcFPnf3roYy9`).
- CT-111: the reproducible build of the same commit (`scripts/repro-build.sh HEAD`) gives sha256 `f832e53f01bf6b0739efb54bf0977d70544dd9bcece2234abd3e18525b4900de` (721,250 bytes), which differs from the line above for the same reason as 0.0.14 (build-host paths in the prebuilt file, `/Users/<name>/.cargo` against `/home/near/.cargo`). It becomes the release hash; the prebuilt file is replaced after the redeploy of `x207-v015.testnet`.
