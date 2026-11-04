# The real Intents contract in near-sandbox

Moved from the README of the contracts repository.

## The real Intents contract in near-sandbox (CT-103)

The residual tests of CT-036, BE-034 and CT-092 use `probes/mock_intents`. `tests/ct103_real_intents.rs` runs the circle against the **wasm of `intents.near` taken from mainnet** and pays and claims one residual through it: a deposit of USDT to Intents (`ft_transfer_call`), `mt_transfer_call` of Intents to the circle (`mt_on_transfer`, `settle`), `finalize_epoch`, `withdraw_intents_balance` (the real `ft_withdraw`), and `claim`.

```bash
scripts/fetch-intents-wasm.sh       # RPC view_code of intents.near, read only; checks the sha256 and writes target/intents-near/intents.wasm
cargo test --test ct103_real_intents
```

The wasm is not committed. It is 1,297,551 bytes with sha256 `a1afab98916d9f213746544dcf8e4e6bdb22c0be6bc5dfbd8ea8814c9922b547` (`code_hash` `BtA1BEFNS619KkvXFsgcpQ21Tb5yMm32aaUkn7xNothk`, seen at mainnet block 222990644). On a laptop, without the file, the test prints a note and passes; the CI of this repository runs `scripts/fetch-intents-wasm.sh` first and sets `X207_REQUIRE_INTENTS_WASM=1`, which turns a missing file into a failure, so a green CI means the circle ran against the real contract. When Intents is upgraded the script stops on the new hash: read the change, then update the hash in the script and in the test. The contract starts with `new({config: {wnear_id, fees, roles}})`, and the test gives it an account for the fee collector and the super admin.

## Environment variables

The contract and its tests read no secrets. Two optional switches are read by tests:

| Variable | Used by | Meaning |
| --- | --- | --- |
| `UPDATE_ABI` | `tests/ct068_abi_freeze.rs` | When set, rewrites `abi.json` instead of comparing with it |
| `X207_TEST_VECTORS` | `tests/ct023_voucher_vectors.rs` | Folder of voucher test vectors. Default `../sdk/spec/test-vectors`, or `tests/vectors/` when the sdk repo is absent |
| `NEAR_USD` | `tests/ct043_gas.rs` | NEAR price in USD for the cost per voucher. Default 3.0 (an assumption, not a quote) |

Secrets belong in `.env`, which is not committed.
