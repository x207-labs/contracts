# Holding a payment until the receiver confirms (CT-093, a design, no code)

Status: design note and an ABI proposal. Nothing here is built. It answers: can the contract keep a payment back until the receiver of an off-chain asset (BE-047) confirms, without breaking ADR-20 (nobody, not even the DAO, can move money out of the pool)?

## What exists today
- A bundle reserves the headroom of every payer for its legs (`commit_bundle`, `BundleReserves`). A reservation is not debt: it only stops the payer from spending the same headroom twice.
- Debt appears when a voucher is registered against a leg (`register_in_bundle`), by anybody (the facilitator). From then on the payer owes up to `max_amount`; clearing and residual follow.
- BE-047 (backend) keeps a bundle `proposed` until the receiver confirms, so the bundle is committed only after the handover. That costs no contract change.

## What the backend gate cannot do
- It guards nothing on-chain. Anybody who holds the signed terms can call `commit_bundle` and register the vouchers; the gate works only because honest software waits.
- Between the confirmation and the commit the seller has no assurance that the buyer still has the headroom.
- For a plain payment (one voucher) there is no bundle to wait on.

## Design H1: confirmation before commit (what BE-047 does)
No contract change. Weakness above. Good enough for a demo among known agents.

## Design H2: a leg with a hold (contract change, proposal)
A leg can carry a hold that only its payer (the receiver of the asset) can lift.

- `BundleLeg` gets `hold: Option<LegHold>` with `LegHold { confirm_by: U64 (ns), arbiter: Option<AccountId> }`. It is part of the canonical terms, so it is signed by everybody and changes `terms_hash`. This is a breaking change of the barter format (`sdk/spec/barter-format.md`).
- `commit_bundle` reserves the headroom as today. A leg with a hold takes no voucher: `register_in_bundle` refuses it with `ERR_LEG_HELD` until the hold is lifted.
- `release_leg(bundle_id, leg_index)`: caller is the payer of the leg. It marks the leg released; vouchers register as for any leg. Event `leg_released`.
- `drop_leg(bundle_id, leg_index)`: callable by the payee at any time (the seller gives up) and by anybody after `confirm_by` if nobody released. The reservation of the leg is freed; no debt ever existed. Event `leg_dropped`.
- With an `arbiter`: `rule_leg(bundle_id, leg_index, release: bool)` callable by the arbiter only after the payer raised `dispute_leg` and before `confirm_by + window`. `release: true` has the same effect as `release_leg`; `false` as `drop_leg`.

### Why this respects ADR-20
No call moves a token. The most an arbiter or anybody else can do is decide whether a debt between the two parties of the leg may come into existence, within the `max_amount` that both signed and that the payer's own headroom already reserved. The pool, the stakes and the DAO's powers are untouched. A hostile arbiter can make a seller go unpaid (or force a payment of at most `max_amount` by the buyer, which the buyer agreed to when signing the terms and naming that arbiter). The arbiter is chosen by the parties in the signed terms, not by the DAO.

### Interaction with the rest
- The reservation of a held leg lasts until the cutoff at the latest: `confirm_by` must be before `cutoff_at`; a hold not lifted by the cutoff lapses like an unregistered leg (`prune_epoch` already frees reservations of finalized epochs).
- 64 legs per bundle stay the cap. Each hold adds two storage entries per leg (about 150 bytes); the 0.32 NEAR join deposit (ADR-41) must be re-checked against the worst case.
- A buyer that never confirms costs the seller the handover (the asset is already given): that is the dishonest-buyer case, not solved by the hold. The hold protects the buyer against a seller who takes the voucher and delivers nothing, and gives the seller the headroom reservation as assurance. The honest answer for the rest is reputation (UM-071 options 2 and 3) or an arbiter.
- Wash trading: a hold lets a pair park headroom for a whole epoch. The reservation is already capped by the payer's headroom, so the cost is lost use of the headroom, not stake.
- Escrow class A is unchanged.

### ABI proposal (not applied)
| Item | Change |
| --- | --- |
| `BundleLeg` / `BundleLegView` | new `hold` / `released: bool`, `dropped: bool` |
| methods | `release_leg`, `drop_leg`, `dispute_leg`, `rule_leg` |
| events | `leg_released`, `leg_dropped`, `leg_disputed` |
| errors | `ERR_LEG_HELD`, `ERR_LEG_NOT_HELD`, `ERR_HOLD_AFTER_CUTOFF`, `ERR_NOT_LEG_PAYER`, `ERR_NOT_ARBITER` |
| terms | `hold` in the canonical JSON of the legs; new `terms_hash` vectors in the SDK |
| storage | per-leg flags in the bundle record; no new top-level key |

## Recommendation
Keep H1 for the submission. Build H2 only for a later version: it changes the signed terms and the bundle record, so it needs the audit that the other ABI changes got, and it adds an attack surface (griefing by holds) that the demo does not need. Until then the contract has no remedy for a seller who delivers and is not paid by a buyer who stays silent, and this note and `backend/docs/delivery.md` say so.

## Not checked
- Gas of `register_in_bundle` with the extra read, and the storage per leg, were not measured.
- The effect of a hold on the solver's search for cycles (a held leg should probably be left out of clearing) was not worked out.
