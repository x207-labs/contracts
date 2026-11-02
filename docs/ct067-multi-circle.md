# CT-067: several circles and clearing between circles

Design note. No change to the contract. Facts about today's build are from the code (`contracts` HEAD, interface 0.0.7), `backend/src/db/migrations.ts`
and the sdk binding; the proposals are mine and untested.

## What already supports several circles
- One contract account is one circle: `new(owner_id, circle_id, usdt_token_id, intents_id, params)`, `get_circle` returns `circle_id`, `contract_id`
  and the network `x207:<circleId>`.
- The binding is per circle: the x402 network is `x207:<circleId>` and `extra.circleContract` names the contract; the facilitator's
  `defineCircleConfig` maps one network to one contract.
- D9 has a `circles` table (`circle_id` primary key, `contract_id` unique, `network = 'x207:' || circle_id`), every table is keyed by `circle_id`, and the
  read API is `/x207/circles/{circle}/...`. The indexer is started per circle (`circleId`, `contractId`).
- Each circle has its own stake pool, limits, epochs and defaults (ADR-18: `limit <= stake`).

## What is missing
- No member can be in two circles with one voucher key and one limit: a voucher names `circle`, so a payment is always inside one circle.
- No path for an obligation from circle A to circle B.
- The solver clears one circle per epoch; epochs of different circles are not aligned.

## Options for clearing between circles
1. **Bridge member (no contract change).** An agent joins both circles with a stake in each. A debtor in A pays the bridge in A, the bridge pays
   the creditor in B, using the sdk as two ordinary vouchers. Risk: the bridge carries the exposure between the two epochs; each circle sees only a
   normal payment. This works today and is the recommended first step.
2. **Circle as member.** Circle A's contract holds a member entry in circle B (stake in B, voucher key of its own), and a keeper signs vouchers for
   net inter-circle positions. Needs a contract that signs, which is not possible without a key held by an operator or a chain-signature.
3. **Netting layer above circles.** A third contract (or the same code in a "super circle" whose members are circle accounts) clears the net positions
   that circles report after `commit_epoch`. Needs the same unit (`credit`, 6 decimals) in both and a rule for who covers a default in B owed to A.

## Effect on the data model if option 3 is built
- Epoch alignment: use the same `epoch_length` and start, or net only on the epochs that overlap.
- D2 gets no new edge type; the super circle has its own pairs between circle accounts.
- `network` stays `x207:<circleId>` per circle; the super circle is one more id. D9 needs no schema change (it is keyed by `circle_id` already).
- Risk rule: the stake of a circle in the super circle bounds its net debt (ADR-18 again), and a default there is a default of the whole circle, so
  members of that circle would bear it through its reserve fund (D6); that needs a spec decision.

## Open questions
Who signs for a circle in the super circle; how the unit of two circles with different risk is priced (credit of A against credit of B at 1:1 is not
guaranteed by anything); whether a member's headroom in A should shrink when the member is a bridge exposed in B.

## Verdict
Several independent circles need nothing new. Inter-circle clearing is possible with a bridge member today; a netting layer is a new contract and a
spec change, and is roadmap.
