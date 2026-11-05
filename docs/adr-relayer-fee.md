# ADR: relayer fee for `*_evm` actions, and the deadline limit (CT-117, interface 0.0.16)

Status: **proposed, not implemented** (2026-11-05). Nothing here is deployed. Read with [evm-agents.md](evm-agents.md), [security-review-evm.md](security-review-evm.md) and the CT-113 review (`tests/ct113_evm_replay.rs`).

## Problem

An EVM agent holds only a `0x` key and no NEAR. Someone else sends the NEAR transaction and pays its gas: today the team's relayer or the sponsor. Nothing in 0.0.15 pays them back, so a third party has no reason to relay. The owner decided (U-73) that the relayer is paid in the circle's token. The same release closes R1a of CT-113: an `Action` deadline has no upper bound.

## Decisions

### D1. What is charged, and who gets it

A fee in the circle token (the USDT of the circle), paid by `ft_transfer` to `env::predecessor_account_id()` of the `*_evm` call (the relayer). The fee is charged on the two actions that move tokens out:

| Action | Fee taken from | Receiver gets |
| --- | --- | --- |
| `claim_evm` | the payout of the claim | `amount - fee` (to `receiver`, or to Intents for `member`) |
| `withdraw_stake_evm` | the stake that is withdrawn | `stake - fee` |
| `request_exit_evm`, `set_auto_settle_evm`, `join_evm` | **no fee in 0.0.16** | |

Why not take the fee from the free stake for the actions without payout: it lowers `stake`, so `limit <= stake` has to be re-clamped, `total_stake` moves to a third place and the invariants (I-04, `accounted == balance`) need a new transition and a new callback. That is a bigger state change than the 0.0.16 deadline allows, and the gas of these three actions is small (about 0.0003 NEAR). If the owner wants it, it is the next step (0.0.17), not a reason to delay this one. **Decision to confirm with the coordinator.**

The two payout actions are the ones where a relayer does real work (a promise and a callback) and where the member has a balance to pay from.

### D2. The signer approves a ceiling: `maxFee` in the `Action`

`Action` gets one more field, `uint256 maxFee` (token units, base 10 string in JSON like the other amounts), signed by the member:

```
Action(string circle,string member,string action,uint64 epoch,string receiver,string termsHash,uint64 nonce,uint64 deadline,uint256 maxFee)
```

The fee actually taken is `fee = min(relayer_fee, maxFee)` (D3). A member who signs `maxFee = 0` pays nothing; a relayer may refuse to send that action. A DAO change of `relayer_fee` after signing cannot cost the member more than what it signed. All five `*_evm` methods carry `max_fee: U128` (for `join_evm` and the two without payout it is signed and must equal `0`; it is part of the one `Action` type, not a second type).

**Domain `version` stays `"1"`.** The type string of `Action` is inside the type hash, so a 0.0.15 signature (no `maxFee`) cannot verify under 0.0.16, and the other way round. The salt already contains the contract account id, so `x207-v016.testnet` shares no signature with `x207-v015.testnet`. Raising the version to `"2"` would also change the `Voucher` and `Bundle` digests, which do not change: every EVM voucher fixture, the viem vectors (`tests/fixtures/evm-eip712.json`) and all signed bundles would need re-signing for no gain in separation. What a `"2"` would add is a wallet-visible label, and the type name list already shows `maxFee`. If the owner prefers a visible version, the cost is the re-signing of voucher and bundle vectors in contracts and sdk.

### D3. DAO parameter `relayer_fee` (default 0) and its upper bound

- `relayer_fee: U128` in `Params` (JSON), a **flat** amount in token units, not basis points: the relayer's cost is gas, which does not grow with the amount.
- Stored like `prune_delay_epochs` (CT-099): its own raw key `x207_relayer_fee` and `x207_relayer_fee_pending`, `#[borsh(skip)]` in `Params`, laid over by `get_params`, `get_pending_params` and `set_params`; it applies from the next epoch like every other parameter. **No state migration**: the stored `Epoch` and `Params` records do not change, a circle without the key reads the default `0`.
- Hard upper bound in code: `RELAYER_FEE_MAX = 1_000_000` (1.00 of a 6-decimals token). `set_params` panics with `ERR_BAD_PARAM: relayer_fee` above it. The DAO cannot set an abusive fee, whatever the vote. The constant is for USDT with 6 decimals, the only token the circle accepts (`usdt_token_id`); a circle of another token needs another constant.
- Default `0` means 0.0.16 behaves like 0.0.15 for every member: a fee appears only after a DAO vote.

### D4. When the stake or the payout is too small

`fee = min(relayer_fee, maxFee, base)` where `base` is the payout (`claim_evm`) or the stake (`withdraw_stake_evm`). The member signed `maxFee` knowing the amounts it may be applied to; a payout smaller than the fee pays the fee out of the whole payout and nothing reaches the receiver. A relayer that would earn less than its gas simply does not relay. The call does **not** revert for a small payout: a member who cannot afford any fee must still be able to claim with `maxFee = 0`. A payout or stake of `0` pays no fee and keeps today's behavior (the existing errors, for example `ERR_NOT_CLAIMABLE`, come first).

### D5. How it is paid, and why the accounting does not change

The fee transfer and the payout transfer are two `ft_transfer` function-call actions **on one promise** to the token contract (`Promise::new(usdt).function_call(fee ...).function_call(payout ...)`). Actions of one receipt are atomic: if the second fails, the first is rolled back. So both leave, or neither does. The full amount (`payout + fee`) goes from the pool to `in_flight` as today (`d6_pool_to_in_flight`), and the callback releases it the same way, so I-04 and `accounted == balance` do not change.

- The payout action is the **last** one of the batch, so `promise_result` of the callback is the result of the payout (for `Intents`, the `ft_transfer_call` return value, the amount the receiver used). Callbacks get one more argument, `fee: U128`, and compare `used` with `sent - fee`; on success they release `used + fee` from `in_flight`; on failure everything goes back to the pool (as today); on a partial refund of an Intents call only `sent - fee - used` goes back, the fee stays paid.
- Gas: one more `ft_transfer` (15 Tgas, `FT_TRANSFER_GAS`) when `fee > 0`; `CLAIM_MIN_GAS` (100 Tgas) is checked against the need of the call with a fee, and the 0.0.15 gas report (`ct098_gas_report.rs`, `ct108` gas test) gets a row for the fee case. A call with `fee == 0` builds the same promise as today (no extra action), so its gas does not change.
- The relayer is the `predecessor`. A relayer that is a contract works; a relayer whose account has no storage in the token fails the fee transfer, and with it the whole batch (the member's claim stays `Claimable`; the relayer sees the failure and may retry from another account).
- New event `relayer_fee_paid {member, relayer, action, epoch, amount}` on every call where `fee > 0`, so the indexer and the dashboard can show it. Nothing is logged when `fee == 0`.

### D6. R1a: `ERR_EVM_DEADLINE_TOO_FAR`

`authorize` and `join_evm` get one more check, after `ERR_EVM_EXPIRED`: `deadline <= now_ms + MAX_EVM_DEADLINE_MS`, else `ERR_EVM_DEADLINE_TOO_FAR: deadline {deadline} now {now_ms}`. `MAX_EVM_DEADLINE_MS = 86_400_000` (24 hours), a constant, not a DAO parameter. Reason for 24 hours: the SDK signs 10 minutes by default, but a relayer with a queue (or an offline signer) needs hours, not a month; 24 hours bounds what a held signature (for example `auto_settle_off` or a `claim` to a receiver) can do after the member forgot it. A parameter would add a state key and a vote for no gain. The check runs before the nonce check, so it does not reveal the nonce.

### D7. Interface and compatibility

| Item | 0.0.15 | 0.0.16 |
| --- | --- | --- |
| `INTERFACE_VERSION` | `0.0.15` | `0.0.16` |
| `*_evm` methods | `(…, nonce, deadline, signature)` | `(…, nonce, deadline, max_fee, signature)` |
| `Action` type | 8 fields | 9 fields (`maxFee`) |
| `get_params` | | `relayer_fee` (default `"0"`) |
| New errors | | `ERR_EVM_DEADLINE_TOO_FAR`, `ERR_BAD_PARAM: relayer_fee` |
| Events | | `relayer_fee_paid` |

- **State:** unchanged layout (`EvmSigner`, `evm_signers`, members, positions). The two new raw keys are optional. An in-place upgrade of `x207-v015.testnet` would work without `migrate` changes, but the plan is a **new circle `x207-v016.testnet`** (the argument of the coordinator): v015 keeps running with the old types, and a signature for one never verifies on the other (D2).
- **The methods change signature**, so a 0.0.15 client cannot call 0.0.16 and the reverse. The SDK handles both: `signAction(input, options)` signs the 9-field type **only when `maxFee` is given** (`maxFee: 0n` is enough); without it the 8-field type of 0.0.14 and 0.0.15 is used, so the SDK stays compatible with the circles that run. The new `signAction` default deadline of CT-114 (now + 10 minutes) stays; the new `maxTtlMs` option can use the 24-hour contract limit as its default ceiling. `verifyEvmAction` and `actionTypedData` take the same option. The contract fixture `tests/fixtures/evm-eip712.json` gets a second set of action vectors with `maxFee`, generated by `scripts/gen-evm-vectors.mjs` from viem; the 8-field vectors stay for 0.0.15.
- **Callers to change:** `sdk` (`evm.ts`, the contract client builders of the five methods), the facilitator and the backend relayer routes that call `*_evm` (they pass `max_fee`), the EVM agent example (`examples/evm-agent`, Worker E), `docs/evm-agents.md`.

## Risks and what the design does about them

| Risk | Handling |
| --- | --- |
| The DAO raises `relayer_fee` after a member signed | The fee is `min(relayer_fee, maxFee)`; the member's signed ceiling holds. |
| A malicious DAO sets a huge fee | `RELAYER_FEE_MAX` in code; `set_params` refuses above it. |
| A relayer front-runs another relayer's pending transaction and takes the fee | Accepted: relaying is permissionless and the first transaction that lands is paid. The nonce makes the second one fail with `ERR_EVM_NONCE`. |
| A member signs `maxFee` high by mistake | The SDK default is `0n`; the signer API shows the ceiling; the contract takes no more than `relayer_fee`, which is at most 1.00. |
| Replay of a signed action to take the fee again | `maxFee` is inside the digest and the nonce is used once. A signature without `maxFee` (0.0.15) does not verify (D2). |
| The fee transfer fails and leaves the payout paid | One promise, atomic actions (D5). Covered by a test that makes the fee receiver unregistered at the token. |
| Pool or stake accounting drifts | `in_flight` carries `payout + fee` as before; a property test checks I-04 and `accounted == balance` over claims and withdrawals with `relayer_fee > 0`. |
| A held signature is used long after (R1) | D6, 24 hours. |
| Name reuse after state reset (R2) and the salt label (R3) | Not changed here. R2 stays a rule of the runbook; the init parameter for R3 is a separate item (it changes `new`). |

## Plan after approval

1. Contract: `Params.relayer_fee` and its raw keys, `max_fee` on the five methods, the 9-field `ACTION_TYPE`, the deadline limit, the batched fee promise, the callbacks, the event, `INTERFACE_VERSION`, `abi.json`.
2. Tests: fixture vectors with `maxFee` (viem), unit tests of fee math (`min` rules, zero cases), sandbox tests (claim and withdrawal with a fee, an Intents partial refund, a failing fee transfer, the DAO change of `relayer_fee`, the cap), the deadline limit, regression: `relayer_fee = 0` gives the same balances as 0.0.15, gas report.
3. Security review (`docs/security-review-relayer-fee.md`), reproducible build and hash in `prebuilt/README.md`, deploy `x207-v016.testnet` from the reproducible wasm, a mixed epoch with a relayer that earns a fee.
4. SDK: option `maxFee` in `signAction` and friends, tests, docs.

Estimated cost of 1 to 3: one working day of the contract lane.
