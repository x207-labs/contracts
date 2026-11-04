# Gas and storage of the public methods of interface 0.0.12 (CT-098)

Status: the tables are measured on 2026-11-03 in near-sandbox with the wasm of interface 0.0.12 (`46c2e62`). They show one method that did not work at the size that the contract accepts, `commit_epoch` with many edges; **the finding is fixed in 0.0.13** (section "Finding", with the numbers of 0.0.13).

## How to repeat it
```bash
cd contracts && cargo near build non-reproducible-wasm
# the methods of the interface, by group (one group at a time: each starts its own sandbox)
cargo test --test ct098_gas_report gas_of_the_life -- --nocapture --test-threads=1 | grep 'GAS|'
cargo test --test ct098_gas_report gas_of_bundles -- --nocapture --test-threads=1 | grep 'GAS|'
cargo test --test ct098_gas_report gas_of_the_escrow -- --nocapture --test-threads=1 | grep 'GAS|'
X207_GAS_OFFSETS=10 cargo test --test ct098_gas_report gas_of_a_clearing -- --nocapture --test-threads=1 | grep 'GAS|'   # 30 x offsets edges; 3, 5, 7, 10 were run
# the long inputs (bundles up to 64 legs, lists, defaults of 64 members) from the older tests
cargo test --test ct043_gas -- --nocapture --test-threads=1 | grep '^GAS'
cargo test --test ct086_gas_limits -- --nocapture --test-threads=1 | grep '^GAS'
```
A group takes one to six minutes. Run them one at a time.

## How to read the tables
- **Gas** is `total_gas_burnt` of the transaction, in Tgas (10^12 gas). It includes the receipts that follow the call: the callbacks of the contract (`on_claim_resolved`, `on_escrow_resolved`, `on_withdraw_resolved`, `on_solver_fee_resolved`) and the call of the token contract, which is why a method that pays out or takes a deposit is 7 to 12 Tgas and one that only writes state is 2 to 5 Tgas. The prepaid gas of the caller is not the gas used.
- **Storage added** is the change of `storage_usage` of the contract account in bytes, after minus before. A negative number is storage that the call freed. 1 byte of storage locks 10^19 yoctoNEAR, that is 0.00001 NEAR (1 KB is 0.01 NEAR); the join deposit that covers one member is measured in `ct062_storage_per_member` (0.35 NEAR).
- A **REFUSED** row is a call that failed in the sandbox; it is never a number.
- **The limit of one transaction.** The older limit of NEAR is 300 Tgas, and the backlog keeps 300 Tgas as the bar for "safe"; a probe on testnet on 2026-11-04 (transactions of 301, 400 and 1000 Tgas of prepaid gas were accepted by `x207-v013.testnet`) shows that testnet now takes up to 1000 Tgas. No method below went above 300 Tgas except where marked. The largest numbers are `apply_upgrade` (110 Tgas, it deploys 657 KB of code) and `commit_bundle` with 64 legs (79 Tgas).
- Views are free for the caller. The long lists are in the tables of `ct086_gas_limits`, repeated below.
- `register_voucher` (demo mode) takes the same path as `register_cumulative`; `migrate`, `new`, `on_*` callbacks are part of the transaction of their method; `withdraw_intents_balance`, `claim` to Intents and `mt_on_transfer` (settle) are measured in `be034_intents_withdraw` and `ct058` with the mock Intents contract and are not repeated here.

### Life of an epoch (3 members, fees on)

| method (case) | gas, Tgas | storage added, bytes | note |
| --- | ---: | ---: | --- |
| join | 2.7 | +323 | JOIN_STORAGE_DEPOSIT 0.35 NEAR covers the worst case of one member |
| ft_on_transfer (stake, through ft_transfer_call) | 7.0 | +0 | includes the transfer on the token |
| post_intent | 2.9 | +168 |  |
| remove_intent | 2.7 | -102 | frees the entry |
| register_cumulative (first voucher of a pair and epoch) | 4.5 | +640 | pair, totals and edge list of the epoch |
| register_cumulative (a new pair) | 4.5 | +480 | pair, totals and edge list of the epoch |
| register_cumulative (a later voucher of a known pair) | 4.4 | +0 | no new entry |
| request_exit | 2.5 | +8 |  |
| close_epoch | 2.7 | +303 |  |
| submit_clearing (3 edges) | 2.9 | +309 | stores the proposal |
| commit_epoch (3 edges, fees on) | 6.3 | +360 | positions of the epoch; fees move stake to the reserve and the solver |
| ft_on_transfer (settle, residual payment) | 7.4 | +0 | includes the transfer on the token |
| finalize_epoch | 4.1 | +8 |  |
| claim (to a NEAR account, with on_claim_resolved) | 7.6 | +0 | includes the transfer on the token and the callback |
| claim_solver_fee (with on_solver_fee_resolved) | 7.2 | +0 | includes the transfer on the token and the callback |
| prune_epoch (max_items 1000) | 4.5 | -1200 | frees the entries of the epoch |
| withdraw_stake (with on_withdraw_resolved) | 7.8 | +0 | includes the transfer on the token and the callback |

### Bundles, deals with a bond, freeze, registry, parameters and upgrade

| method (case) | gas, Tgas | storage added, bytes | note |
| --- | ---: | ---: | --- |
| commit_bundle (2 participants, 2 legs) | 5.5 | +821 | reserves headroom; the cases with 64 legs are in ct086 and ct043 |
| register_in_bundle | 5.1 | +630 | same path as register_cumulative |
| commit_bundle (2 participants, 2 legs, with arbiter: opens a deal and locks 2 bonds) | 7.0 | +916 | a Deal record, 2 bond_locked events |
| confirm_deal (first party) | 2.8 | -86 | releases the bond of the other party |
| confirm_deal (second party, closes the deal) | 3.1 | -444 | deletes the Deal record |
| dispute_deal | 2.6 | +69 |  |
| rule_deal (accused_guilty) | 4.1 | -211 | moves stake between members and may lower limits (limits_updated) |
| rule_deal (complaint_false) | 4.2 | -211 | moves stake between members and may lower limits (limits_updated) |
| rule_deal (no_fault) | 4.1 | -211 | moves stake between members and may lower limits (limits_updated) |
| settle_deal (window over, nobody complained) | 3.3 | -468 | releases the bonds |
| propose_freeze | 2.6 | +151 | a FreezeRecord of about 200 bytes |
| cancel_freeze | 2.4 | -151 |  |
| apply_freeze (after the 24 hour timelock) | 2.4 | +0 |  |
| unfreeze_member | 2.4 | -83 | deletes the FreezeRecord |
| set_asset_class | 2.6 | +227 | one entry |
| set_asset_contracts (16 contracts, the most) | 2.8 | +383 |  |
| set_params (pending for the next epoch) | 2.9 | +145 | also writes the bond rules |
| propose_upgrade | 2.4 | +92 |  |
| apply_upgrade (deploys the code and runs migrate) | 110.1 | -92 | migrate is part of this transaction |

### Escrow of a class A asset, three forms

| method (case) | gas, Tgas | storage added, bytes | note |
| --- | ---: | ---: | --- |
| offer_grant (class A, NFT scope) | 3.5 | +411 |  |
| nft_on_transfer (deposit through nft_transfer_call) | 6.9 | +105 | an Escrow record; includes the transfer on the NFT contract |
| complete_escrow (with on_escrow_resolved) | 10.9 | +733 | registers the voucher and delivers the NFT |
| cancel_escrow (after expiry, with on_escrow_resolved) | 8.2 | -8 | gives the NFT back |
| offer_grant (class A, NEP-141 scope) | 3.4 | +408 |  |
| ft_on_transfer (escrow action, through ft_transfer_call) | 7.3 | +115 | an FtEscrow record; includes the transfer on the token |
| complete_escrow (with on_escrow_resolved) | 11.3 | +733 | registers the voucher and sends the tokens |
| cancel_escrow (after expiry, with on_escrow_resolved) | 8.6 | -8 | gives the tokens back |
| offer_grant (class A, NEP-245 scope) | 3.5 | +442 |  |
| mt_on_transfer (escrow action, through mt_transfer_call) | 7.4 | +140 | an MtEscrow record; includes the transfer on the multi-token |
| complete_escrow (with on_escrow_resolved) | 11.4 | +733 | registers the voucher and sends the tokens |
| cancel_escrow (after expiry, with on_escrow_resolved) | 8.8 | -8 | gives the tokens back |

### Clearing of many edges (30 members, each owes the next N members one credit, fees on)

| method (case) | gas, Tgas | storage added, bytes | note |
| --- | ---: | ---: | --- |
| submit_clearing (90 edges, 30 members) | 11.7 | +4637 | stores the proposal |
| commit_epoch (90 edges, 30 members, fees on) | 46.3 | -4380 | 30 positions; the edges are dropped |
| submit_clearing (150 edges, 30 members) | 17.8 | +7637 | stores the proposal |
| commit_epoch (150 edges, 30 members, fees on) | 61.8 | -7380 | 30 positions; the edges are dropped |
| submit_clearing (210 edges, 30 members) | 23.9 | +10637 | stores the proposal |
| commit_epoch (210 edges, 30 members, fees on) | REFUSED after 41.5 | +0 | the logs of the receipt are 16,825 bytes, the limit is 16,384 |
| submit_clearing (300 edges, 30 members) | 33.2 | +15137 | stores the proposal |
| commit_epoch (300 edges, 30 members, fees on) | REFUSED after 51.4 | +0 | the logs of the receipt are 19,785 bytes, the limit is 16,384 |


### The long inputs, from `ct043_gas` and `ct086_gas_limits` (run on the same build)

| method (case) | gas, Tgas |
| --- | ---: |
| commit_bundle, a cycle of 4 / 8 / 16 / 32 / 64 participants and legs | 7.9 / 12.5 / 21.8 / 40.8 / 79.1 |
| commit_bundle, a dense bundle of 20 members (380 legs) or 24 (552) | refused `ERR_BAD_BUNDLE`: the limit is 64 legs |
| grants_of, a list of 80 grants, page of 100 / page of 10 | 12.7 / 3.5 |
| list_asset_classes, a registry of 200 entries, limit 1000 / limit 20 | 23.1 / 4.5 |
| commit_epoch, 3 / 12 / 30 edges (6 participants) | 5.4 / 8.1 / 12.5 |
| finalize_epoch, 64 members, 63 defaults | 64.4 |
| claim to Intents, with its callback | 11.5 |
| register_cumulative, a continuing pair | 4.4 |

## Finding: `commit_epoch` failed above about 180 edges (found by this report, fixed in 0.0.13)
NEAR limits the **total** length of the logs of one receipt to 16,384 bytes (`HostError::TotalLogLengthExceeded`). In 0.0.12 `commit_epoch` logged every edge (`clearing_applied`, chunks of 50) and every position (`epoch_committed`); the chunks are small, their sum is not: with account names of 50 characters 210 edges gave 17,615 bytes and 300 edges 19,785 (the tables above, measured on 0.0.12, show `REFUSED`); with names of 64 characters the limit was reached at about 100 edges. `submit_clearing` accepts 300 edges, so an accepted proposal could be one that `commit_epoch` could never apply.

**Fixed in 0.0.13 (CT-099).** `clearing_applied` is a summary (`edge_count`, `edges_hash`) and `epoch_committed` carries all positions in one log with `position_count` and `positions_hash`. Measured in the sandbox on the 0.0.13 wasm, with account names of 64 characters:

| call | gas, Tgas | total log, bytes | budget (90 percent of 16,384 = 14,745) |
| --- | ---: | ---: | --- |
| `commit_epoch`, 300 edges, 30 members, fees on | 103.8 | 5,333 | passes |
| `commit_epoch`, 256 edges, 64 members (the most members), fees on | 113.7 | 10,637 | passes |
| `commit_bundle`, 64 legs (the most), 64 members | | 11,071 | passes |
| `finalize_epoch`, 63 defaults, 64 members | | 14,532 | passes (the `member_defaulted` events are cut to fit 90 percent; before the change it logged 16,252, under the limit but with no margin) |

The gas of `commit_epoch` grows with the edges (about 0.3 Tgas per edge, 104 Tgas at 300), far below 300 Tgas. The tests are `tests/ct099_commit_log_size.rs` (they fail above the budget) and `tests/ct099_hash_vectors.rs`; repeat one at a time with `cargo test --test ct099_commit_log_size <name> -- --nocapture --test-threads=1`.

## What is above 100 Tgas
`apply_upgrade` 110 Tgas (deploys the code and runs `migrate`; 300 Tgas is the conservative bar, testnet takes up to 1000). Nothing is above 300 Tgas.
