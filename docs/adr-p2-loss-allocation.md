# ADR (P2, roadmap): loss allocation when a debtor defaults (UM-161)

Status: proposed, **not implemented**. Nothing in v0.0.14 changes. Owner approved the write-up on 2026-11-05; the code waits for after the hackathon submission. This is the "P2 haircut (CT-057)" that `finalize_epoch` still skips.

## Context

x207 clears an epoch in three steps: members register vouchers (obligations per pair), a solver proposes cycle clearing (`ClearingEdge { debtor, creditor, cleared }`), and `commit_epoch` applies it. What is left is a **net position** per member: net debtors pay in during the payment window, net creditors claim from the epoch pool.

When a debtor does not pay, `finalize_epoch` runs `default_debtor` (`settlement.rs`): stake first, then the reserve, and the rest is `shortfall`. `claim` then pays every creditor the same fraction, frozen at finalize: `amount * pool_final / claimable_final` (K-10, so the order of claims does not matter).

Two things follow, and the second is the problem:

1. Cleared obligations (the cycles) are already gone. They never become a claim again. This is good and it stays.
2. The shortfall is shared by **all** creditors of the epoch, also by creditors who never extended credit to the defaulter. A creditor whose claim comes from a different debtor loses part of it.

How often: while `limit <= stake` (`bound_limit`), a defaulter's stake covers its due and `shortfall` is 0, so this is a tail case. It becomes real if limits are raised above stake (P2 `limit_cap`/reserve policies) or the stake is partly locked as a bond (`bond_locked_of`).

## Decision (proposed)

Cycle clearing keeps protecting what it removed, and the haircut goes **only to the direct creditors of the debtor that failed**, in proportion to what that debtor owed each of them after clearing.

Definitions for a defaulted debtor `d` in epoch `e`:

- `r(d,c)` = `obligation - cleared` of the pair `(d,c)`: what `d` still owed `c` after clearing.
- `R(d)` = sum of `r(d,c)` over `c` (= `owed_out - cleared_out` of `d`).
- `loss(d)` = `shortfall` of `d` (due minus stake minus reserve).
- Haircut of creditor `c` from `d`: `h(d,c) = loss(d) * r(d,c) / R(d)` (round down; the rounding dust stays in the pool and goes to the reserve).
- Claim of `c`: `net_claim(c) - sum over d of h(d,c)`, never below 0.

**Cap rule.** `c` may be a net creditor of less than `h(d,c)` (it owes others too). Then `c` bears at most its `net_claim(c)`; the remainder `h - net_claim(c)` is added to a `spill` amount that is shared pro rata, as today, by the creditors that have not been cut to 0. The cap keeps a claim from turning into a debt, so no second default can start inside the claim step.

Alternatives, rejected:

- Keep the global pro-rata (today). Simple and order-free, but unrelated creditors pay for someone else's default.
- Reinstate the cleared cycle obligations of `d` (claw back). It undoes work the solver and the members already relied on, and the members of the cycle may have left or been paid. It breaks the rule that `commit_epoch` is final.

## Example (small numbers, credit units)

Obligations of the epoch: cycle `A->B 10`, `B->C 10`, `C->A 10`; and `D->E 6`, `D->F 4`, `E->B 2`.

Clearing: the solver clears the cycle (3 edges x 10), so A, B and C owe each other nothing. Residual: `D->E 6`, `D->F 4`, `E->B 2`.

Net positions: D = -10 (debtor, due 10); E = +6 - 2 = +4; F = +4; B = +2. Creditors' claims total 10.

D defaults. Stake 6, reserve 0, so `loss(D) = 4` and the pool is 6.

| creditor | claim before haircut | today (global 0.6) | proposed |
|---|---|---|---|
| E (direct, `r`=6) | 4 | 2.4 | 4 - 4*6/10 = **1.6** |
| F (direct, `r`=4) | 4 | 2.4 | 4 - 4*4/10 = **2.4** |
| B (only owed by E, indirect) | 2 | 1.2 | **2** |
| total paid | 10 | 6.0 | 6.0 |

A, B and C keep the cancelled cycle in both versions. B, who never lent to D, is whole in the proposal.

Cap case: if E had also owed B 6 instead of 2, E's net claim would be 0 and its share of the haircut (2.4) could not be taken from E. The 2.4 is spilled and shared pro rata by F and B (the creditors not already at 0).

## Contract impact

State (no migration of old data; new fields have defaults):

- `Position` (D5): add `shortfall: u128` for a defaulted debtor (derivable as `amount - recovered`, so can be skipped) and for a creditor `haircut_paid: u128`, written at claim time or at finalize.
- `ParticipantTotals`: add `cleared_out: u128` (set when `commit_epoch` applies the clearing) so `R(d) = owed_out - cleared_out` needs no scan. This is a Borsh layout change of a map value: it needs a version bump and a migration path (`migrate`), or a sidecar map under a new `StorageKey`, which needs neither. The sidecar is the safer choice (same pattern as `evm_signers`).
- `Epoch`: add `spill_final: u128` next to `pool_final` and `claimable_final`.

Functions:

- `finalize_epoch`: after `default_debtor` for each defaulter, record `loss(d)` and `R(d)` (a vector of at most 64 entries, one per member). Compute `spill` only if some creditor would be cut below 0, which needs the creditor's `net_claim` and its `r(d,c)`; the loop is over defaulters x their direct creditors.
- `claim` / `claim_for`: compute `sum h(d,c)` for the claimant over the defaulters of the epoch: one read of `pairs[(e,d,c)]` per defaulter (at most 63), then pay `amount - haircut + share of spill`. The frozen totals keep the result independent of the order of claims, as K-10 does now.
- New view `claim_preview(epoch, account_id)` (optional) returns the amount and the haircut per defaulter, so agents can see the loss before claiming.
- `pairs` of an epoch must not be pruned before the last claim: `prune_epoch` already waits `prune_delay_epochs`; the rule becomes "also no `claimable` position left".
- Events: `member_defaulted` keeps its fields; `residual_claimed` gains `haircut` (an interface bump, so the ABI freeze test and the indexer schema change).

Gas (estimate, to be measured in CT-043 style):

- `finalize_epoch` today is bounded by 64 positions and measured under the 90 percent log rule. The extra work is a handful of u128 writes per defaulter; the loop over direct creditors would add up to `edges of defaulters` storage reads. At about 0.05-0.1 Tgas per storage read and 300 edges that is 15-30 Tgas, well inside 300 Tgas but visible. Avoiding the scan (using `cleared_out`) keeps finalize flat.
- `claim`: up to 63 reads of `pairs` (about 3-6 Tgas) on top of the current claim, which is below its `CLAIM_MIN_GAS`; the minimum must be raised by that amount.

## Risks and open points

- The cap and spill rule makes the result depend on a second pass (who is cut to 0). It must stay order-free: compute it once in `finalize_epoch` and freeze it, not at claim time.
- Rounding: round haircuts down for creditors' shares; dust to the reserve; property test: pool = sum of payouts + dust.
- Incentives: direct creditors now carry the risk of who they trade with, which is what limits and stakes are for. It argues for showing `r(d,c)` exposure per counterparty in the dashboard.
- Not a replacement for a bond or arbiter (`deal.rs`): those protect a single deal; this decides who bears an unbacked netting loss.
- Needs a property test next to `ct030_clearing_property.rs` (conservation, no claim above its net, no negative claim) and a sandbox scenario with a default and a partial stake.
