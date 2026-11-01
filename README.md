# x207 contracts

The `x207-circle` contract (module M1) and the Sputnik DAO scripts (module M9) of x207. Features F1–F7. Part of [x207](https://github.com/x207-labs/x207).

| Path | Contents | Module | Features |
| --- | --- | --- | --- |
| `x207-circle/src/` | Contract source: `member.rs`, `voucher.rs`, `clearing.rs`, `commit.rs`, `settlement.rs`, `bundle.rs`, `risk.rs`, `params.rs`, `events.rs`, and others | M1 | F1–F7 |
| `tests/` | near-sandbox and unit tests | M1 | F1–F7 |
| `dao/` | Scripts that print Sputnik DAO commands. See `dao/README.md`. | M9 | F7 |
| `abi.json` | Frozen contract interface | M1 | F1–F7 |

## Build and test

Needs Rust 1.99.1 with the `wasm32-unknown-unknown` target (pinned in `rust-toolchain.toml`) and `cargo-near` 0.22.0.

```bash
cargo near build non-reproducible-wasm
```

```bash
cargo test
```

Sandbox tests need the wasm from the build step.

## Environment variables

The contract and its tests read no secrets. Two optional switches are read by tests:

| Variable | Used by | Meaning |
| --- | --- | --- |
| `UPDATE_ABI` | `tests/ct068_abi_freeze.rs` | When set, rewrites `abi.json` instead of comparing with it |
| `X207_TEST_VECTORS` | `tests/ct023_voucher_vectors.rs` | Folder of voucher test vectors. Default `../sdk/spec/test-vectors` |

Secrets belong in `.env`, which is not committed.

## License

Apache-2.0
