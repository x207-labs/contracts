# ADR: contract 0.0.17 (CT-118)

Status: proposed, not implemented. No code, no deploy. Builds on `adr-relayer-fee.md` (0.0.16) and `security-review-relayer-fee.md`.

Two changes are proposed: (1) `get_epoch.params` shows the rules that were in force; (2) the relayer fee also covers `request_exit_evm` and `set_auto_settle_evm`, paid from the member's free stake. `join_evm` stays sponsor-paid (reason in part 2).

## 1. `get_epoch.params` carries the in-force `relayer_fee` and `bond_*`

### Problem

`Epoch.params` has `#[borsh(skip)] #[serde(default)]` fields (`relayer_fee`, the bond rules, `prune_delay`). The live values sit in raw storage keys (`x207_relayer_fee`, `x207_relayer_fee_pending`, and the bond/prune keys) and `get_params` overlays them with `with_rules`. `get_epoch` returns `e.to_view(...)` from the stored record (`clearing.rs`, `get_epoch`), so a skipped field reads as its default: `"0"`. After a `set_params` or an upgrade a client that reads only `get_epoch` sees `relayer_fee = "0"` while 250000 is in force. 0.0.16 documents this (`evm-agents.md`: "read the live value from `get_params`"); it is a display bug, not a money bug, because every payout path reads `current_relayer_fee()`.

### Options

| | What | State migration | Old epochs |
|---|---|---|---|
| A | `get_epoch` overlays `with_rules` (live values) on every view | none | wrong: shows today's values for a past epoch |
| B | drop `#[borsh(skip)]`, store the fields in `Epoch` | **required**: the borsh layout of every stored `Epoch` changes; an old record fails to deserialize | would need a versioned read or a rewrite of all epochs |
| C | snapshot in a side key per epoch, written when the epoch opens (`x207_epoch_rules_<id>`); `get_epoch` reads it | none | no snapshot: current epoch falls back to the live overlay, older epochs return the skipped defaults and the view says so |

Recommendation: **C**. It keeps the `Epoch` layout (the reason the fields were skipped in 0.0.16 was to avoid a migration), and it is correct for past epochs, which A is not.

### Design (C)

- Written in two places that already write the rules: `with_config` (epoch 1, from `new()`) and `close_epoch` when it opens the next epoch (after `apply_pending_rules`, so the snapshot holds what is in force for that epoch). Value: borsh of `{ relayer_fee, bond_*, prune_delay }`, about 100 bytes.
- `get_epoch(id)`: if the snapshot exists, overlay it on the view. If it does not exist and `id` is the current epoch, overlay the live rules. Otherwise return the stored defaults and add no claim of accuracy: the field list is documented as "in force at epoch open; epochs opened before 0.0.17 show the defaults".
- `prune_epoch` removes the snapshot with the epoch (otherwise storage leaks; add a test).
- Storage: about 100 bytes per epoch plus the key, paid from the contract's own storage balance like the epoch record. Pruning bounds it.
- `get_params` is unchanged.

### Impact

- **wasm hash**: changes (any source change does; `repro-build.sh` gives a new sha256, new `KNOWN_SHA256` entry, new prebuilt, new version string).
- **State migration**: none for C. Existing `x207-v016` style circles upgraded to 0.0.17 keep working: the first epoch opened after the upgrade gets a snapshot; the current epoch is covered by the live fallback.
- **ABI**: no new method, no changed signature. The JSON of `get_epoch` gains no field (the fields exist in `params` already), so SDK/frontend need no change. `abi.json` changes only through the version string; `ct068_abi_freeze` stays at its counts.
- **Tests**: unit tests for snapshot write at `new()` and at `close_epoch` with a pending `set_params` (the epoch opened after the change shows the new fee, the closed one the old fee); `prune_epoch` removes the key; sandbox test: `set_params` then two epochs, `get_epoch` of each shows its own value; upgrade test: a 0.0.16 state without snapshots reads the live fallback for the current epoch.

## 2. Relayer fee for `join_evm`, `request_exit_evm`, `set_auto_settle_evm`

### Today (0.0.16)

The signed `Action` already has `maxFee`; for these three it is `0` and the relayer pays the gas. Only `claim_evm` and `withdraw_stake_evm` pay a fee, by `fee = min(relayer_fee, maxFee, base/2)`.

### Per method

| Method | Fee in 0.0.17? | Why |
|---|---|---|
| `join_evm` | **no** (`maxFee` must stay 0) | The member has no stake yet: stake arrives later by `ft_transfer_call`. There is no free stake to charge. The sponsor already attaches at least 0.35 NEAR and gets the excess and the later refund (`refund_sponsor`), so the sponsor is already compensated in NEAR. A signed `maxFee > 0` is refused so the signature cannot promise a fee that cannot be paid. |
| `request_exit_evm` | yes | The member has stake while it is a member. |
| `set_auto_settle_evm` (`auto_settle`, `auto_settle_off`) | yes | Same. |

The `Action` type does not change (the nine-field type is already used for all actions on 0.0.16), so signatures made for 0.0.16 with `maxFee = 0` stay valid and charge nothing. No domain version bump.

### Funding source: free stake

`free = stake - locked_bond - open_debt`, where `open_debt` is the member's `outstanding_debt` in the open epoch (positions in status `due`) and `locked_bond` is the existing bond rule. Taking the `due` into account matters for auto-settle: `finalize_epoch` takes the `due` from free stake, so a fee that eats into it would turn an auto-settle into a default. The fee never touches the bond or the `due`.

`fee = min(relayer_fee, maxFee, free / 2)`. The `free / 2` ceiling mirrors the `base / 2` rule of 0.0.16 (written explicitly in the docs). If `free` is zero the fee is zero and the action still succeeds: **an exit or an auto-settle switch is never blocked for lack of fee money**.

### Flow

1. Verify the EIP-712 signature, deadline (24 h rule), nonce, as today.
2. Apply the action itself synchronously (set `exit_requested_epoch`, set or clear the auto-settle flag). The action does not depend on the fee outcome.
3. Compute `fee`. If `fee > 0`: debit `Member.stake` by `fee`, `total_stake -= fee`, `in_flight += fee`, then `ft_transfer(predecessor, fee)` with a callback `on_action_fee_resolved(member, fee, action, relayer)`. Re-run the limit guard: stake shrank, so `limit` follows through `bound_limit` (as `auto_settled` does). `bound_limit` may only lower the limit, never raise it.
4. Callback, success: `in_flight -= fee`, event `relayer_fee_paid { account_id, relayer, action, epoch, amount }` with `action` = `request_exit`, `auto_settle` or `auto_settle_off`.
5. Callback, failure (relayer not registered on the token, token paused): the fee returns to the member: `in_flight -= fee`, `total_stake += fee`, `Member.stake += fee`, `bound_limit` again; event `relayer_fee_failed`. The action stays done. It cannot go to the reserve: the member did not owe it.

Reentrancy: the debit before the call and the credit in the callback follow the pattern of `withdraw_stake_for`; `withdraw_pending` is not needed because the amount in flight is already removed from `stake`. A member that calls `withdraw_stake` while a fee is in flight sees the reduced stake; the failure credit restores it (same as the claim path).

### Limits and abuse

- The member signs the ceiling (`maxFee`); the contract never charges more than the signed value, `relayer_fee` or `free/2`.
- The nonce is used once. A relayer cannot repeat the charge. A relayer can front-run another relayer to take the fee (accepted in 0.0.16, documented; unchanged).
- The fee is flat and capped by `RELAYER_FEE_MAX` (1 credit), so a member cannot be drained by a toggle loop: each toggle needs a fresh signature and costs the member at most `relayer_fee`. An SDK or agent that toggles auto-settle in a loop pays per toggle; the docs say so.
- `request_exit_evm` can be signed only while the member has not exited (`ERR_MEMBER_EXITED`), so it charges once per exit.

### D6 invariants

`accounted = total_stake + reserve + epoch_pools + solver_fees + in_flight == USDT balance` must hold after every step:

- debit step: `total_stake -fee`, `in_flight +fee`: sum unchanged, balance unchanged.
- success: `in_flight -fee`, balance `-fee` (the transfer): sum and balance both drop by `fee`.
- failure: `in_flight -fee`, `total_stake +fee`: sum back to before, balance unchanged.
- `Member.stake` always equals the sum in `total_stake` (per-member sum check in `assert_d6`).
- Both callbacks run in one callback each, so there is no state where `in_flight` stays set. A callback that is never delivered is the generic NEAR risk already accepted for claim and withdraw.

### Impact

- **wasm hash**: changes. New version, build, prebuilt, `KNOWN_SHA256`.
- **State migration**: none. No new stored field on `Member` or `Epoch`. `in_flight` already exists.
- **ABI**: one new `#[private]` change method `on_action_fee_resolved` (ABI freeze `CHANGE_METHODS` +1, `ct008_init` allowlist like `on_withdraw_resolved_fee`). `request_exit_evm` and `set_auto_settle_evm` take `max_fee` as a new parameter, so **their JSON signatures change** (SIGNATURES entries). SDK must pass `max_fee` for both and sign `maxFee` for them; coordinate with the SDK owner before tagging, as for SD-100.
- **Gas**: the two methods grow by one promise and one callback; expect about 5-6 Tgas more. Measure in the sandbox; claim and withdraw with fee measured 11.1 and 12.1 Tgas in 0.0.16.
- **Tests**: unit (fee 0 when free 0; `free/2` cap; bond and `due` never touched; failure credit; `bound_limit` after the debit), sandbox (exit with fee to a registered relayer, with an unregistered relayer, with `maxFee 0`; auto-settle toggle with fee; a member whose free stake equals the `due` keeps the auto-settle after a fee: fee is 0), `join_evm` with `maxFee > 0` refused, `assert_d6` after every case, ABI freeze, full suite.

### Open questions for review

1. Is a refused `join_evm` with `maxFee > 0` right, or should it be accepted and ignored? Refusing is safer for signers.
2. Should the failure credit go back to the member stake (proposed) or to the member's receiver as in `withdraw_stake`? Stake is simpler here: no receiver is signed for these actions.
3. Ship 1 and 2 together as 0.0.17, or only 1 first? They share no code. Part 2 changes two method signatures and so needs the SDK; part 1 does not.

## Out of scope

Fee for `register_cumulative`/voucher actions, a percentage fee, a per-relayer allowlist, and a bump of the EIP-712 domain version.
