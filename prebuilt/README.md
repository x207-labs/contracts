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

- sha256: `f832e53f01bf6b0739efb54bf0977d70544dd9bcece2234abd3e18525b4900de` (721,250 bytes, base58 `Hhs82yW5iJD2QH4juP6Ud5u7ApHGhcw8Zo5xCK1hxmoj`). It is the **reproducible** build (`scripts/repro-build.sh`) and the code of `x207-v015.testnet` since 2026-11-05 (redeploy tx `BYHH9YUPWuYtk453LELRFaFut7J4HKNiXyo9n4RydHAW`, state unchanged). The SD-094 run happened on the earlier host build, sha256 `3632861bddfb9b2993cb337346dfa3a4e6f3f11de0d0e006c8573af90b0d28fe` (base58 `4eZhZF16ySGGv9ux3gzbxnH6znVxcHNQYivVFdfpuHz5`, deploy tx `Ex6CjaGH1XgJfRD19KGpFftEapBNqszszcFPnf3roYy9`).
- CT-111: the old host build differed because it held build-host absolute paths (`/Users/<name>/.cargo`) against `/home/near/.cargo` in the container.

# Circle 0.0.16 (CT-117)

`x207_circle-0.0.16.wasm` is the **reproducible** build (`scripts/repro-build.sh`) at interface 0.0.16 (relayer fee on `claim_evm` and `withdraw_stake_evm`, `maxFee` in the EIP-712 `Action`, 24 h deadline limit).

- sha256: `4295351f395d0cc31419d43adf10e5b60f3c2a0e546ae0e912c5da4fef5aebe7` (742,662 bytes, base58 `5UusLiFf2xXdgyFGnfrjuhqFNof7TpyBoMGDdsx8Qu8S`). Built twice from the committed ref, byte-identical. It is the code of `x207-v016.testnet` (deployed with `new`, owner `x207v8.sputnikv2.testnet`, `admission` open, `relayer_fee` 250000).
- Source: contracts commit `93f8d8e` (the ref given to `repro-build.sh`). The commit that adds this file changes only `prebuilt/`.

# Circle 0.0.17 (CT-118)

`x207_circle-0.0.17.wasm` is the **reproducible** build (`scripts/repro-build.sh`) at interface 0.0.17: `get_epoch.params` shows the rules in force when the epoch opened, relayer fee for `request_exit_evm` and `set_auto_settle_evm` from the free stake (new `max_fee` argument), `join_evm` refuses a signed fee (`ERR_EVM_FEE_NOT_ALLOWED`).

- sha256: `cbb8a9108f975ba8fca86dc6f6d470e3e377b91309fce56b2ec18a77ff4a4b97` (751,548 bytes). Built twice from the committed ref, byte-identical. **Not deployed anywhere**; a deploy needs a decision of the owner.
- Source: contracts commit `83cc5d7` (the ref given to `repro-build.sh`). The commit that adds this file changes only `prebuilt/`.

# Circle 0.0.18 (P1, UM-076) and the locker

`x207_circle-0.0.18.wasm` is the **reproducible** build (`scripts/repro-build.sh`) at interface 0.0.18: sale of a NEAR account through a locker contract (`near:account`, `near:member_account`), `cancel_expired_account_escrow`, `remove_pending` and `make_room` with an owner-set delay, `set_locker_code_hashes`. `x207_locker-0.0.18.wasm` is the locker that the circle accepts, built by the same script. This is the branch `p1-0.0.18`; the main release stays 0.0.17 until the owner agrees to a migration.

- `x207_circle-0.0.18.wasm` sha256: `4701ecce4168d18b0674b6b4af9f789219afc33be99d89c09f3a433c0bd0d198` (787,568 bytes, base58 `5nBeeNaeWbVH7Q1c3mfG1NPThENb9PA8BvVgjauf6Aib`). It is the code of `x207-v018.testnet` since the redeploy of CT-129 (tx `Bx2qeVrB9z8WTH2FXnHvxp5DztEpmuJRGPaQ3SwgzTXu`, state unchanged) and adds `set_max_escrow_ttl` / `get_max_escrow_ttl` and `ERR_ESCROW_TTL_TOO_LONG`. The first build of 0.0.18 (before CT-129) had sha256 `e64d4ec74c42897fc94ddd1dcf6f3b60822486b8ff981f9da5056028a9e1534a` (base58 `GW19E89YzCbXLxgC1UHPqDeDGD9sxUuMSLUgPK5nd2SD`); the testnet runs of the epoch (#11, #14) happened on that build.
- `x207_locker-0.0.18.wasm` sha256: `6d2ebdf437471a666bf9073637e012c04189cab08921e606ddbfec0bd9524e71` (131,595 bytes, base58 `8MCn7wQpw941cSfH9htjew9yfZedVz37viVEwWZGMZjW`). That base58 is the official locker code hash registered in the circle.
- Source: contracts commit `f551f51` for the circle (CT-129; the ref given to `repro-build.sh`), built twice from that ref, byte-identical; the locker is the same bytes as with `993128d`. The commit that adds these files changes only `prebuilt/`.
