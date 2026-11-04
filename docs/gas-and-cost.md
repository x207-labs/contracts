# Gas and cost of interface 0.0.13, for the judges (CT-102)

Status: measured on NEAR testnet on 2026-11-04 and in near-sandbox. Every number names its source: a **transaction hash** on testnet (look it up on any testnet explorer; all are listed with their gas in [`docs/testnet-0.0.13.json`](../../docs/testnet-0.0.13.json)) or the **name of a test**. A number marked *estimate* is computed from measured ones, and says from which. Nothing here was measured on mainnet. The gas price of testnet was 1e8 yoctoNEAR per gas when these were measured (0.0001 NEAR per Tgas); mainnet may differ, so the NEAR columns are testnet prices.

The circle is `x207-v013.testnet` (code of the reproducible build `b9327f2b…7054`, owner the Sputnik DAO `x207v8.sputnikv2.testnet`); the epoch is epoch 2: 15 members, a complete graph of **210 edges** of 1 credit, clearing fees 10 and 5 bps.

## 1. What each call burns (testnet)
`Gas` is the gas burnt by the whole transaction, callbacks included. `NEAR` is what is burnt for it (`tokens_burnt`, which the caller pays).

| Call | Gas, Tgas | NEAR | Source (transaction hash) |
| --- | ---: | ---: | --- |
| deploy of the 657 KB wasm | 49.5 | 0.00493 | `EhnsJArcdru4WaLwTRF52SbfR9B5EtQ5orQHLBRm3tHL` |
| `new` (init) | 2.7 | 0.00025 | `2bk6ELv34kauFxVVU3AZyk8kWJBfa8AuWiT5s6SC71kM` |
| create a member account (with its key) | 8.6 | 0.00711 | `5h6UBRqDWZMh5VeXAMUM66QgLNJLdh83z7DG3rZ38UGM` |
| mint mock USDT (testnet token only) | 1.9 | 0.00017 | `C1DKp2H7bKcMLw9pXjRG4zougrKHLmzCHYCg9kvUzPtZ` |
| `join` | 3.0 | 0.00028 | `AcRRpf2rhHrsvnV1nf5mguUzkjykeG3V1gSNrnQUopBD` |
| stake (`ft_transfer_call` to the circle) | 7.2 | 0.00065 | `GCqshmiwPtNk881EspuHMKZBPYym4TgmA9ZQ96Tu1cdD` |
| `register_cumulative` (one voucher) | 4.8 | 0.00046 | `6bVntu1Msp3aZWHfnqMQr68UzhtPUa1FjgX853yxVooQ`; the mean of 10 more is the same (4.80 Tgas, 0.00046 NEAR; first of them `HUghFyJPZjGFXu2ywmjkUvBAf1nJaq52S7HvS9bd4SnD`) |
| `close_epoch` | 3.0 | 0.00028 | `329TWFnAzGwvKjR2fdRdqTKqVzLLRki7t5wWzR7KL4ox` |
| `submit_clearing`, 210 edges | 28.0 | 0.00278 | `7jL5ZiJDxUGDS1btrhKoa9SoRZBpeadrv3jkbz3xM7J5` |
| `commit_epoch`, 210 edges | 69.4 | 0.00692 | `8xcQqNZXhPNHgAukL4B42n8CMTXmEqx9BRmawH1XL1pb` (log 2,487 bytes of the 16,384 allowed) |
| `finalize_epoch` | 9.3 | 0.00091 | `DTXTVJaK55Pfubgh7215o3LtUAJPwb9TQu4NHSaJTtqi` |
| `prune_epoch` too early | refused with `ERR_PRUNE_TOO_EARLY` | | `tests/testnet-0.0.13.live.test.ts`, result `pruneTooEarly` in `docs/testnet-0.0.13.json` |

The largest sizes that the contract accepts were measured in near-sandbox, with account names of 64 characters: `commit_epoch` of 300 edges on 30 members 103.8 Tgas and 5,333 bytes of log (`ct099_commit_log_size::a_proposal_of_300_edges_can_be_committed`); 256 edges on 64 members 113.7 Tgas and 10,637 bytes (`a_proposal_on_64_members_can_be_committed`); `finalize_epoch` with 63 defaults 14,532 bytes of log (`finalize_with_63_defaults_goes_through`); a bundle of 64 legs 79.1 Tgas (`ct086_gas_limits`, `commit_bundle_gas_for_a_cycle_of_n_members`). No method of the interface is above 110 Tgas except `apply_upgrade` (110.1 Tgas, `ct098_gas_report::gas_of_bundles_deals_freeze_and_admin`); the limit of a transaction is 300 Tgas. All the other methods are in [`gas.md`](gas.md).

## 2. What one epoch costs
For the epoch of the table (210 edges, 15 members):

| Item | NEAR | How it is computed |
| --- | ---: | --- |
| 210 vouchers registered (`register_cumulative`) | 0.0966 | 210 × 0.00046 (*estimate* from the measured 0.00046 per voucher; the 210 transactions of the run took 158 seconds in parallel, `edgesRegistered` in `docs/testnet-0.0.13.json`) |
| `close_epoch` + `submit_clearing` + `commit_epoch` + `finalize_epoch` | 0.0109 | 0.00028 + 0.00278 + 0.00692 + 0.00091 (sum of the measured four) |
| **Burnt in total for the epoch** | **0.1075** | about 0.0005 NEAR per edge (*estimate*) |

The vouchers are paid by whoever relays them (the facilitator or the payer); the four steps of the epoch by the keeper and the solver. The clearing fees of the protocol (10 and 5 bps of the cleared value) are not NEAR: they are credit taken from the stake of the debtors and are described in `tests/vectors/fee_charges.json` (the 21,000 units per member of this epoch are the case `testnet x207-v013 epoch 2`).

**Storage is locked, not burnt.** A byte of contract storage locks 0.00001 NEAR on the contract account. Measured in near-sandbox (`ct098_gas_report`): a new pair of a voucher adds 313 to 640 bytes, `submit_clearing` of 300 edges adds 15,137 bytes (about 0.15 NEAR), `commit_epoch` gives the edges of the proposal back (150 edges: 7,380 bytes), and `prune_epoch` gives the pairs back after the prune delay of 2 epochs (3 pairs: 1,200 bytes). For the 210 edges of the table the pairs hold about 100 KB, about 1 NEAR locked until the prune (*estimate*: 210 × the 480 bytes of a new pair in `gas_of_the_life_of_an_epoch`). The join deposit of 0.35 NEAR per member covers the worst case of one member, 34,236 bytes (`ct062_storage_per_member`).

## 3. Prepaid gas and the balance an account needs
A transaction **reserves** its prepaid gas from the balance of the signer and gives back what it does not burn. The reserve is not what the call costs: on testnet it is **0.001 NEAR per Tgas of prepaid gas plus about 0.0009 NEAR** (ten times the burn price). Measured with a signer of 0.236 NEAR: 250, 245 and 240 Tgas were refused with costs of 0.2509, 0.2459 and 0.2409 NEAR, 100 Tgas was refused with 0.1009, 235 Tgas was accepted, and with 0.0647 NEAR 60 Tgas was accepted (`solver/test/prepaid-gas.test.ts`, which repeats the six numbers; the first refusal is run 1 of `docs/testnet-0.0.13.json`, a solver with 0.24 NEAR and 250 Tgas, so the proposal window of epoch 1 passed and that epoch has no proposal).

| Caller | Call | Prepaid gas to attach | Burnt (measured) | Reserve = balance needed to send |
| --- | --- | ---: | ---: | ---: |
| keeper | `close_epoch` | 20 Tgas | 3.0 Tgas | 0.021 NEAR |
| solver | `submit_clearing` | 60 Tgas (default of `SUBMIT_GAS_TGAS`, `solver` commit `6016c73`) | 28.0 for 210 edges | 0.061 NEAR |
| keeper | `commit_epoch` | 150 Tgas | 69.4 for 210 edges; 103.8 for 300 | 0.151 NEAR |
| keeper | `finalize_epoch` | 100 Tgas | 9.3; 64.4 for 63 defaults (`ct043_gas`) | 0.101 NEAR |
| keeper | `prune_epoch` | 200 Tgas | 70.2 for 196 items (review of the contract) | 0.201 NEAR |
| relayer | `register_cumulative` | 30 Tgas (what the run used) | 4.8 | 0.031 NEAR |

*Estimates* from the rule above: **a solver account needs at least 0.07 NEAR** (0.061 reserve plus the 0.003 burnt), **a keeper that closes, commits and finalizes needs at least 0.16 NEAR** (the largest reserve is `commit_epoch`, 0.151; the calls follow one another, so the reserves are not held together), 0.21 NEAR if it also prunes. Attaching 250 to 300 Tgas to every call, as the first solver and keeper did, needs 0.25 to 0.30 NEAR for calls that burn a tenth of it. A balance well above these (a few NEAR) is the normal setting for a keeper that runs for days: the burn is about 0.011 NEAR per epoch for the four steps.

## 4. How to repeat it
```bash
# testnet (one epoch of 210 edges, real time about 30 minutes): needs the accounts of x207-v013 and the keys in .secrets/
X207_TESTNET=1 X207_WASM=<wasm of scripts/repro-build.sh> X207_CT100_OUT=docs/testnet-0.0.13.json npx vitest run tests/testnet-0.0.13.live.test.ts
# sandbox: the gas of every method
cd contracts && cargo test --test ct098_gas_report -- --nocapture --test-threads=1 | grep 'GAS|'
cd contracts && cargo test --test ct099_commit_log_size -- --nocapture --test-threads=1
# the reserve of the node
cd solver && npx vitest run test/prepaid-gas.test.ts
```
