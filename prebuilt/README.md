# Prebuilt test contract (UM-107)

`mock_ft.wasm` is the mock USDT (a NEP-141 token with a `mint`) that the near-sandbox demos and tests deploy. It is built from `probes/mock_ft` with `cargo near build non-reproducible-wasm --manifest-path probes/mock_ft/Cargo.toml` and committed here **only** so that the quick demo (`npm run demo:quick` in the root of the repository) needs no Rust toolchain. It is a test fixture: it has no access control and must never be deployed on a real network.

- sha256: `effd353047fda87f3ce0b97c78a3c3f62cebb5b1d890e9374b9d5b0ff985489d` (143,282 bytes). `scripts/demo-quick.mjs` checks it before the demo starts; the number lives in `KNOWN_SHA256` of `scripts/deploy-mainnet.mjs`, the table that also pins the wasm of the release.
- It is not a reproducible build (the bytes depend on the local toolchain). When `probes/mock_ft` changes, build it again, copy the file here and update `KNOWN_SHA256.mock_ft` in `scripts/deploy-mainnet.mjs` and the hash in this file.
- The wasm of the circle is not here: the quick demo downloads the release wasm and checks it against the sha256 of the release.
