# Gas and storage measurements

Moved from the README of the contracts repository. The summary is in [gas-and-cost.md](gas-and-cost.md).

## Gas and storage (CT-043)

Measured in near-sandbox by `tests/ct043_gas.rs`, on the real contract with real members, wasm built by
`cargo near build non-reproducible-wasm`. `Tgas` is `total_gas_burnt` of the whole call including receipts.
The limit of one transaction is 1000 Tgas; the test fails any method above 500 Tgas.

| Method | Case | Tgas |
| --- | --- | --- |
| `register_cumulative` | first voucher of a pair | 4.0 |
| `register_cumulative` | later voucher of the same pair | 3.8 |
| `submit_clearing` | 3 / 12 / 30 edges | 2.5 / 3.4 / 5.2 |
| `commit_epoch` | 3 edges, 3 participants | 5.0 |
| `commit_epoch` | 12 edges, 6 participants | 7.8 |
| `commit_epoch` | 30 edges, 6 participants | 12.3 |
| `commit_epoch` | 63 edges, 64 participants (`MAX_MEMBERS`) | 46.3 |
| `finalize_epoch` | 64 members, 63 defaults (worst case) | 60.9 |
| `claim` to a NEAR account, with callback, prepaid exactly `CLAIM_MIN_GAS` (100 Tgas) | | 6.9 |
| `claim` to Intents, with callback, prepaid 100 Tgas | | 10.7 |

`MAX_EDGES_PER_TX` stays 300 (CT-004); the solver reads it from `MAX_EDGES_PER_TX`. `CLAIM_MIN_GAS` stays 100 Tgas: a claim to
Intents with 50 Tgas for `ft_transfer_call` and 15 Tgas for the callback completes with that prepaid gas.

Cost of a voucher: a later voucher burns 3.77 Tgas, which is 0.000377 NEAR at the sandbox gas price (1e8 yocto per gas),
about 0.0011 USD at 3 USD per NEAR, or 5.7% of a payment of 0.02 USD.

Storage: a member entry takes 311 bytes (3.1 mNEAR at 1e19 yocto per byte, inside the join deposit of 10 mNEAR).
The first voucher of a new pair takes 620 bytes (pair entry, two participant entries, epoch counters).

Repeat the measurement (about 5 minutes; runs of the same code differ by at most 0.1 Tgas, for example 46.2 and 46.3 for
`commit_epoch` with 63 edges):

```bash
cargo near build non-reproducible-wasm
```

```bash
cargo test --locked --test ct043_gas -- --nocapture --test-threads=1 | grep GAS
```

### Log limit

near allows 16 KiB of log per call. `finalize_epoch` writes one `member_defaulted` per debtor; with long account ids it
stops fitting at about 60 debtors. So the contract keeps `limits_updated` and `epoch_finalized` and drops the
`member_defaulted` events that no longer fit, adding one plain line that says how many were left out. The positions stay
readable with `list_positions`, and `epoch_finalized.defaulted_count` is always right. `commit_epoch` has the same limit
for `clearing_applied` (see CT-004); both need a decision in the design notes.

## Gas and cost of the interface 0.0.13 (CT-102)
A full epoch of 210 edges on 15 members burnt **0.1075 NEAR** on testnet (about 0.0005 NEAR per edge), of which 0.0109 NEAR for the four steps of the keeper and the solver; `commit_epoch` of 210 edges burnt 69.4 Tgas. A transaction reserves 0.001 NEAR per Tgas of prepaid gas, so a solver needs at least 0.07 NEAR and a keeper at least 0.16 NEAR (prepaid gas: `submit_clearing` 60, `commit_epoch` 150, `finalize_epoch` 100 Tgas). Every number with its transaction hash or test name is in [docs/gas-and-cost.md](gas-and-cost.md); the gas of every method in near-sandbox is in [docs/gas.md](gas.md).
