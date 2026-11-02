# CT-058: N-party residual settlement through `execute_intents` (feasibility note)

Status: study only. No change to the contract or `abi.json`. Sources read on 2026-11-02 from `near/intents`
at commit `d340060afe1debb8c8f2f2fc6b6c999d48c886b0` (2026-10-21), files under `contracts/defuse/`.

## The idea (ADR-21 stretch goal)
Today a debtor pays its `due` residual with `ft_transfer_call` of USDT to the circle (`ft_on_transfer`, msg `settle`), one
transaction per debtor. The stretch goal: every net debtor signs one Intents `transfer` of USDT per epoch, and the solver
wraps all of them in one `execute_intents(signed)`. The circle learns about each payment from `mt_on_transfer`.

## What the source shows (verified)
| Claim | Result | Where |
| --- | --- | --- |
| `execute_intents` is permissionless and a single error aborts the whole call | Verified. Each payload is checked in order; `execute_signed_intents(..).unwrap_or_else(\|e\| e.panic())`, so any error panics the call and no Intents balance moves | `src/contract/intents/mod.rs`, `core/src/engine/mod.rs` |
| Signature, `verifying_contract`, deadline, public key of the signer, nonce are checked per payload | Verified (`InvalidSignature`, `WrongVerifyingContract`, `DeadlineExpired`, `PublicKeyNotExist`, nonce salt and expiry) | `core/src/engine/mod.rs` |
| `Transfer { receiver_id, tokens, memo, msg, min_gas }` moves balance inside Intents with no fee | Verified for the code path: only `internal_sub_balance` and `internal_add_balance`, no fee term. The docs page on fees was not read, so "docs say fee applies to every transfer" stays unchecked | `core/src/intents/tokens.rs` |
| A `Transfer` with `msg` calls `mt_on_transfer(sender_id, previous_owner_ids, token_ids, amounts, msg)` on the receiver | Verified. Gas floor 5 Tgas, default 30 Tgas (`MT_ON_TRANSFER_GAS_MIN`, `_DEFAULT`). `previous_owner_ids` is the signer repeated per token; `token_ids` look like `nep141:<token account>` | `core/src/intents/tokens.rs`, `src/contract/tokens/nep245/core.rs` |
| The notification is detached from the transaction | Verified: `notify_and_resolve_transfer(..).detach()`. The call runs as its own receipt after `execute_intents` returned; `mt_resolve_transfer` refunds what the receiver returns | `src/contract/intents/state.rs`, `nep245/core.rs`, `nep245/resolver.rs` |
| The circle can move or withdraw its own Intents balance without a signature | Verified in the code: `mt_transfer`, `mt_batch_transfer` and `ft_withdraw` are public, `#[payable]` with `assert_one_yocto`, and act on `ensure_auth_predecessor_id()`. Behaviour of that helper for a contract caller was not tested | `nep245/core.rs`, `nep141/withdraw.rs` |
| Intents balances are `u128`, no negative balance, no allowlist | Taken from the research report, not re-read here | report |
| `intents.testnet` has no code | Taken from the research report and spikes S2.2/S3.1, not re-checked today | report |

## The important finding: atomic on Intents, not atomic on the circle
`execute_intents` is all-or-nothing for the **Intents balances**: either all N debtors' USDT moves to the circle's Intents
account or none. The circle's bookkeeping is not part of that. Each `mt_on_transfer` is a separate receipt that runs after the
call. If the circle panics or returns the amount for one debtor, only that debtor's transfer is refunded by
`mt_resolve_transfer`; the other N-1 stay settled. So the demo claim "atomic N-party settlement" is true for the money
movement and false for the `paid` status in D5. The state after a partial failure is the same as today's independent payments
(some debtors `paid`, some `due`), so it does not break an invariant, but it must not be described as all-or-nothing on the circle.
Mitigation: the solver runs `simulate_intents` and reads the circle positions first; the circle's `mt_on_transfer` must be
cheap and unable to fail for a valid `settle` msg.

## What the circle would need (not done)
1. New method `mt_on_transfer(sender_id, previous_owner_ids, token_ids, amounts, msg) -> PromiseOrValue<Vec<U128>>`.
   Only `predecessor == intents_id` and `token_ids == ["nep141:" + usdt_token_id]`; msg is the existing
   `{"action":"settle","epoch":..,"member_id":..}`; it returns the unused amount as the refund. This changes the ABI
   (interface bump) and adds an error path per check, as `ft_on_transfer` has.
2. Pool accounting (D6): paid USDT would sit as an Intents balance of the circle, not as a NEP-141 balance on `usdt_token_id`. The
   pool needs a second counter or a withdraw step. Claims with `destination: intents` could use `mt_transfer` (no token hop),
   claims to `near` need `ft_withdraw` first, which is asynchronous and needs its own callback and in-flight state like `claim`.
3. Event `residual_paid` gets a field for the route, and the backend handler (BE-034 reconciliation) reads it.
4. Gas: each notification takes at least 5 Tgas and defaults to 30 Tgas, plus resolve gas per token. How many debtors fit
   in one 300 Tgas `execute_intents` is **not measured**; it needs a run against the real Verifier.

## Risks
- Cannot be tested on testnet (`intents.testnet` has no code) and mainnet is out of bounds for this build. The only local
  option is to run a Verifier wasm in near-sandbox, which was not tried and whose source of the wasm is not vetted.
- The Verifier has admin roles (pause, `force_lock_account`, `mt_force_transfer`, per the report); money parked on Intents
  is under that role, unlike USDT held by the circle itself.
- The research report cites a deposit bug of about 3.8 million USD on 2026-10-01; not verified here.

## Verdict
Feasible as a demo for the money movement. Sizing: one new contract method, a pool counter, a claim path from the Intents
balance, an event field and a backend handler; about 3 to 4 days of work with tests, plus a mainnet-only validation that no
agent session may run. Because the circle-level status is not atomic and the Intents leg cannot be tested before mainnet, the
recommendation is to keep it a described stretch goal and not build it before the P0 items are green.
