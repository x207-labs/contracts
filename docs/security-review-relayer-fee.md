# Security review: relayer fee and deadline limit (CT-117, interface 0.0.16)

Scope: `claim_evm` and `withdraw_stake_evm` take a fee for the relayer (`predecessor_account_id`); every `*_evm` action refuses a deadline more than 24 h ahead. Design in `adr-relayer-fee.md`. This is a self review by the contracts lane. No external audit.

## Threat model

- Attacker A: any NEAR account that sees a signed Action (relayers, mempool watchers). May send it first, with its own account as `predecessor`.
- Attacker B: a relayer that is not registered on the USDT token (or leaves it after the member signs).
- Attacker C: a member who wants the fee path to cancel or reduce a payout (griefing the circle).
- Attacker D: a relayer who wants more than the member agreed to.
- Out of scope: a compromised DAO (can set `relayer_fee` to at most `RELAYER_FEE_MAX`), a compromised USDT token.

## Findings

| # | Topic | Result | Evidence |
| --- | --- | --- | --- |
| 1 | Fee above what the member agreed to (D) | Not possible. `fee = min(relayer_fee, maxFee, base / 2)`; `maxFee` is part of the signed digest (9th field), so changing it breaks the signature. `maxFee = 0` means no fee. | unit `ct117_relayer_fee_math`, `evm::tests` (extra `assert_ne` for a different `maxFee`), sandbox `max_fee_zero_means_no_fee`, fixture `evm-eip712.json` checked against viem |
| 2 | Fee above the DAO limit | Not possible. `relayer_fee` is validated at `new` and `set_params` (`ERR_PARAM_OUT_OF_RANGE: relayer_fee`, hard cap `RELAYER_FEE_MAX` = 1 credit). | unit `params::tests`, sandbox `relayer_fee_above_the_cap_is_refused` |
| 3 | Unregistered relayer blocks the payout (B, C) | Closed. Payout and fee are two separate promises, joined by `Promise::and` into one callback. A failed fee leg does not cancel the payout. The unpaid fee returns to the member: claim -> the position is `claimable` again for that amount; withdraw -> sent on to the member's receiver, and if that also fails -> reserve. Nothing is lost, nothing is stuck in `in_flight`. | unit `ct117_relayer_unregistered_fee_fails_payout_stays_paid`, `ct117_withdraw_relayer_unregistered_fee_goes_to_member`, `ct117_withdraw_refund_fails_fee_goes_to_reserve`; sandbox `unregistered_relayer_does_not_block_the_payout`, `withdraw_with_unregistered_relayer_sends_the_fee_to_the_receiver` |
| 4 | D6 accounting (`accounted == USDT balance`) | Holds in all four outcomes (payout ok/fail x fee ok/fail). One callback books both legs, so there is no race between two callbacks. `assert_d6` runs after each case. | unit `ct117_both_ok`, `ct117_payout_fails_fee_ok`, `ct117_both_fail_everything_returns`, `ct117_withdraw_*`; sandbox `assert_d6` after each flow |
| 5 | Double settlement by a second callback | A callback acts only on a position in `claimed` state (claim) or a member with `withdraw_pending` (withdraw); otherwise it returns without moving money. | unit `ct117_second_callback_is_a_noop` |
| 6 | Intents destination with partial use | Fee is taken from the amount before the `ft_transfer_call`; the part Intents refunds returns to the pool together with an unpaid fee; `used` is capped at the sent amount. | code `on_claim_resolved_fee`; existing Intents tests of CT-035/CT-058 still pass. Sandbox test with a partial Intents refund and a fee: not written (open) |
| 7 | Front-running the fee (A) | Possible and accepted: the fee goes to whoever sends the transaction first. The member loses at most the signed `maxFee`, the same as with the intended relayer. The nonce makes the action single use. Documented in `docs/evm-agents.md`. | ADR D5 note |
| 8 | Dust and rounding | `fee <= base / 2`, integer division; for `base < 2` the fee is 0. A remainder that stays `claimable` after a failed fee can be claimed again; each claim needs a new signature and takes at most half of the remainder, bounded by the new `maxFee`. | unit `ct117_relayer_fee_math` |
| 9 | Deadline limit (R1a) | `ERR_EVM_EXPIRED` first, then `ERR_EVM_DEADLINE_TOO_FAR` (> now + 24 h), then nonce and signature. A signature meant to live for years can no longer be stored for later replay with a changed fee context. | unit `ct117_deadline_too_far`, sandbox `deadline_more_than_24_hours_ahead_is_refused` |
| 10 | Storage and upgrade | `relayer_fee` is stored in raw keys outside the Borsh of `Params` (`#[borsh(skip)]`), like `prune_delay_epochs`. No state migration; a circle upgraded from 0.0.15 reads 0 until the DAO sets it. `new` persists the initial value (bug found by the sandbox test and fixed in 5f5cb4d). | `params::tests`, sandbox tests that deploy with `relayer_fee` |
| 11 | EIP-712 domain | Version stays `"1"`: the typed-data name `Action` has a new field list, so a 0.0.15 signature cannot verify on 0.0.16, and the circle id is in the salt. Reason in ADR D2. | `evm-eip712.json` (9 fields) and `evm-eip712-v1.json` (8 fields, kept for 0.0.14/0.0.15 clients) |
| 12 | Gas | `claim_evm` with fee 11.1 Tgas, `withdraw_stake_evm` with fee 12.1 Tgas (sandbox, burnt). The claim callback with fee gets a fixed 20 Tgas (`CLAIM_FEE_CALLBACK_GAS`). | sandbox output of `ct117_relayer_fee` |

## Known limits (not fixed in 0.0.16)

- `join_evm`, `request_exit_evm`, `auto_settle` actions pay no fee: the relayer carries their gas. A fee from free stake is postponed to 0.0.17.
- The fee is a flat amount in token units, not a percentage; the DAO must retune it if the token price moves.
- Item 6 has no sandbox test with a partial Intents refund.
- `set_params` of `relayer_fee` is tested in the sandbox with the owner account (cap, pending, in force from the next epoch: `owner_set_params_changes_relayer_fee_from_the_next_epoch`), not through a real Sputnik proposal.

## Verdict

No open finding that moves funds against the member's signature or breaks D6. Items marked open above are test gaps, not known bugs.
