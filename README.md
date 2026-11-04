# x207 contracts

The `x207-circle` smart contract (Rust, NEAR) and the Sputnik DAO scripts of [x207](https://github.com/x207-labs/x207): the mutual credit settlement layer for x402 agent payments.

## Its role in x207

The contract is the one place where the credit circle is true. It holds the members and their stakes, records the signed vouchers per pair, accepts a clearing proposal from any solver after checking it on chain, applies the best one, covers a default from the debtor's stake, and lets creditors claim USDT. Everything else in x207 (the [SDK](https://github.com/x207-labs/sdk), the [indexer and read API](https://github.com/x207-labs/backend), the [solver](https://github.com/x207-labs/solver), the [dashboard](https://github.com/x207-labs/frontend)) reads or calls it.

- **Running now:** release [v0.0.13](https://github.com/x207-labs/contracts/releases/tag/v0.0.13), interface 0.0.13 (`INTERFACE_VERSION` in `x207-circle/src/lib.rs`, frozen in `abi.json`), as the circle `x207-v013.testnet` on NEAR testnet. No mainnet deploy has been made.
- **Reproducible build:** the wasm is built in a pinned container, so two builds of one commit give the same bytes. v0.0.13 (commit `8799ebe`, built twice, byte-identical): sha256 `b9327f2b5d64b1180ba76dd619c6091ae57b48964f520a784e366c131e787054`, on-chain `code_hash` `DTw5rUUdJSbWjD361MmBxKW4nm4ETnNkex63mfUCjsjh`.

| Path | Contents |
| --- | --- |
| `x207-circle/src/` | Contract source: `member.rs`, `voucher.rs`, `clearing.rs`, `commit.rs`, `settlement.rs`, `bundle.rs`, `risk.rs`, `params.rs`, `events.rs`, and others |
| `tests/` | near-sandbox and unit tests |
| `dao/` | Scripts that print Sputnik DAO commands ([`dao/README.md`](dao/README.md)) |
| `abi.json` | Frozen contract interface |
| `scripts/` | `repro-build.sh`, `verify-release.sh`, `fetch-intents-wasm.sh` |

## Build and test

Needs Rust 1.99.1 with the `wasm32-unknown-unknown` target (pinned in `rust-toolchain.toml`) and `cargo-near` 0.22.0.

```bash
cargo near build non-reproducible-wasm
cargo test
```

Sandbox tests need the wasm from the build step. A sandbox test that walks a whole epoch takes about one minute, because it fast-forwards block time. The solver tests in `tests/sv006_solver_cli.rs` run `../solver/dist/cli.js` and skip themselves when the solver repository is not next to this one.

Check a release, and optionally a deployed account (needs Docker for the reproducible build):

```bash
scripts/repro-build.sh <git-ref> <outdir>               # prints the sha256 of x207_circle.wasm
scripts/verify-release.sh v0.0.13 [account] [rpc-url]
```

What the check proves: [docs/verify-release.md](docs/verify-release.md).

## What it costs

A full epoch of 210 edges on 15 members burnt **0.1075 NEAR** on testnet (about 0.0005 NEAR per edge), of which 0.0109 NEAR for the four steps of the keeper and the solver; `commit_epoch` of 210 edges burnt 69.4 Tgas. A later voucher burns 3.8 Tgas in near-sandbox. A transaction reserves 0.001 NEAR per Tgas of prepaid gas, so a solver needs at least 0.07 NEAR and a keeper at least 0.16 NEAR.

- Every number with its transaction hash or test name: [docs/gas-and-cost.md](docs/gas-and-cost.md); the gas of every method: [docs/gas.md](docs/gas.md).
- The measurement tables (`tests/ct043_gas.rs`) and how to repeat them: [docs/gas-measurements.md](docs/gas-measurements.md).
- The rules of the interface 0.0.13 (logs under the NEAR limit of 16,384 bytes, proposals read in pages with `get_proposal`): [docs/rules-0.0.13.md](docs/rules-0.0.13.md).

## Evidence

- Reproducible build and release check, above.
- `tests/ct103_real_intents.rs` runs the circle against the wasm of `intents.near` taken from mainnet in near-sandbox and pays and claims one residual through it: [docs/real-intents-test.md](docs/real-intents-test.md). It has not run on testnet or mainnet.
- Security reviews and the finding behind each fix: [docs/security-review.md](docs/security-review.md). The threat model of the whole project is in the [umbrella docs](https://github.com/x207-labs/x207/blob/main/docs/security-and-limits.md#threat-model).
- The tests read no secrets. Optional switches: `UPDATE_ABI` (rewrites `abi.json`), `X207_TEST_VECTORS` (voucher test vectors folder), `NEAR_USD` (price for the cost per voucher, default 3.0, an assumption).

## Known risk

An NFT in the escrow of a class A grant leaves only through `complete_escrow` or `cancel_escrow`. If the callback `on_escrow_resolved` never runs the escrow stays `completing`, and if the delivery cannot be taken back it is `stuck`: the NFT stays in the circle. There is no rescue method, because ADR-20 forbids the owner to move assets. Offer only NFTs you accept to lose this way ([docs/security-review.md](docs/security-review.md), F10).

## Back to x207

Overview, live demo links and the claims map: [x207-labs/x207](https://github.com/x207-labs/x207).

## License

Apache-2.0
