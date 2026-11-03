# Rules of conduct for 0.0.12 (ADR-43): bonds with an arbiter, `freeze_member`, official contracts per asset type

Status: **a design, no code, no ABI change yet.** It proposes the names, fields, errors, storage and effects of CT-094, CT-095 and CT-096, for the spec (section 22) before anything is built. Every number marked *proposal* is a choice that the coordinator or the owner can change. Where a choice has a real alternative, it is in "Open questions".

The rule of ADR-20 holds through all three: **nobody, not the DAO, not the owner, not an arbiter, can move money out of the pool or out of a stake to an account of their choosing.** The only movement of money that this document adds is a bond that goes from the stake of the party that a named arbiter found at fault to the stake of the other party of the same deal, inside the amount that both parties locked when they signed.

## 0. What is reused from the earlier notes
- `docs/payment-hold.md` (CT-093, H2) proposed a *hold on a leg*: the payment waits until the receiver releases it. ADR-43 does not hold the payment; it asks both sides for a **bond**. From H2 this design keeps the arbiter named in the signed terms, the three outcomes of a ruling, the rule that a silent arbiter cannot take money, and the argument for ADR-20. The hold on a leg itself is **not** built in 0.0.12 (it can still follow).
- `BE-047` (the delivery flow in the backend) stays the way the parties say "delivered" and "confirmed" off-chain. The two on-chain calls below, `confirm_deal` and `dispute_deal`, are the same words made binding on the bond.
- `UM-071` (reputation): the outcome of every deal is an event, so `BE-053` can count it.

## 1. CT-094: bond of both sides, with an arbiter

### 1.1 What the bond is, and why
**A part of the stake, locked.** Not headroom, not a new deposit.
- Headroom is a capacity to owe, not money: a bond made of headroom cannot be given to a victim.
- A new deposit would need a new token path (`ft_on_transfer` action, refunds, a new place in the pool accounting) and a new storage per deal. The stake is already real USDT in the contract (D1), already the collateral of the credit limit (ADR-18), and already what `finalize_epoch` cuts for a default. Locking a part of it adds one number per member and one record per deal.
- **Rule that keeps the books true:** at all times `limit(m) <= stake(m) - bond_locked(m)`. A bond can be locked only from the slack between the limit and the stake. Because `outstanding_debt <= limit`, a default can always be paid from `stake - bond_locked`, so a bond is never eaten by a default, and a ruling can move the bond without touching the invariant `limit <= stake` of the cheater.
- Consequence for members: the size of deals that a member can bond is `stake - limit`. A member that wants bigger deals stakes more. Limit growth at `finalize_epoch` is capped by `stake - bond_locked` instead of `stake`.

### 1.2 When a bond is required
A bundle is a **deal with a bond** when it has **exactly two participants** (a swap) and its value is at or above `bond_threshold`.
- *Value of the deal* `V` = the larger of the two outgoing totals (the sum of `max_amount` of the legs of each participant). *Bond* of each side = `V * bond_bps / 10_000`, the same amount for both sides.
- Below the threshold nothing changes: a bundle without the new fields is accepted and hashes as before.
- At or above the threshold the signed terms must carry the three new fields below; without them `commit_bundle` fails with `ERR_BOND_REQUIRED`. *Rings of three or more are not bonded in 0.0.12* (see open question 1).

### 1.3 Fields in the signed terms
`BundleInput` (and the canonical JSON of the terms that every participant signs, `terms_hash`) gets three optional fields. When absent they are left out of the canonical JSON, so the hash of every bundle that does not use them does not change.

| Field | Meaning |
| --- | --- |
| `arbiter` | account id chosen by both parties. Not a participant, not `owner_id`, not the circle account. May be outside the circle. |
| `agreement_hash` | optional, 64 lowercase hex: sha256 of the document of the agreement (the parties keep the document off-chain). Signed with the terms and stored; the contract never reads it. |
| `confirm_by` | nanoseconds: the end of the window in which a party can confirm or dispute. Between `commit + 1 hour` and `commit + 14 days` (*proposal*). |

A signature on the terms is the consent to the arbiter, to the bond and to the window. This is the evidence that the parties accepted, not a legal contract.

### 1.4 Life of a deal
```
commit_bundle (value >= threshold, 2 participants)  -> deal OPEN, both bonds LOCKED  (event bond_locked x2)
confirm_deal(P)   : P says "Q delivered"            -> bond of Q RELEASED             (event deal_confirmed, bond_released)
dispute_deal(P)   : P says "Q did not"  (once)      -> deal DISPUTED                  (event deal_disputed)
rule_deal(arbiter): verdict                         -> bonds settled                  (event deal_ruled, bond_slashed / bond_released)
settle_deal(any)  : window over / arbiter silent    -> remaining bonds RELEASED       (event bond_released, reason)
```
- `confirm_deal(bundle_id)`: caller is a party. It releases the bond of **the other party** (the bond protects the receiver). If both bonds are released the deal is `closed` and its record is deleted.
- `dispute_deal(bundle_id, evidence_hash)`: caller is a party, before `confirm_by`, only while the bond of the other party is still locked (a party that confirmed cannot dispute afterwards). One dispute per deal. `evidence_hash` is 64 hex, stored, never read.
- `rule_deal(bundle_id, verdict)`: caller is the arbiter, only when the deal is disputed and before `disputed_at + 7 days` (*proposal*). `verdict`:
  - `accused_guilty`: the bond of the accused goes to the **stake** of the complainant; the bond of the complainant is released.
  - `complaint_false`: **half** of the bond of the complainant goes to the stake of the accused (the coordinator's decision, 2026-11-03); the rest of it is released to the complainant; the bond of the accused is released.
  - `no_fault`: both bonds are released.
- `settle_deal(bundle_id)`: anybody. If the deal is open and `now >= confirm_by`: the remaining bonds are released (nobody complained in time: deemed done). If the deal is disputed and `now >= disputed_at + 7 days`: both bonds are released (**the arbiter was silent: nobody is punished without a ruling**). Otherwise `ERR_DEAL_NOT_DUE`.
- A bond that is slashed moves between two stakes of the same circle: `total_stake` and the pool accounting do not change; the loser's limit is lowered to the new stake if needed (`limits_updated`), the winner's limit is unchanged.

### 1.5 Methods, views, events, errors
| Kind | Name | Notes |
| --- | --- | --- |
| change (existing) | `commit_bundle(bundle, signatures)` | `bundle` gets `arbiter`, `agreement_hash`, `confirm_by` (all `Option`, `#[serde(default)]`). Locks the bonds. Extra checks: `ERR_BOND_REQUIRED`, `ERR_ARBITER_INVALID`, `ERR_DEAL_WINDOW`, `ERR_BOND_INSUFFICIENT_STAKE`, `ERR_DEAL_LIMIT`. |
| change (new) | `confirm_deal(bundle_id: u64)` | party |
| change (new) | `dispute_deal(bundle_id: u64, evidence_hash: String)` | party |
| change (new) | `rule_deal(bundle_id: u64, verdict: Verdict)` | `Verdict` = `accused_guilty`, `complaint_false`, `no_fault`; arbiter only |
| change (new) | `settle_deal(bundle_id: u64)` | permissionless |
| view | `get_deal(bundle_id) -> DealView | null` | `{bundle_id, parties[2], arbiter, agreement_hash, confirm_by, bond_each, locked[2] (bool), state (open, disputed), complainant, accused, disputed_at, evidence_hash}` |
| view | `bond_locked(account_id) -> U128` | sum of the bonds still locked |
| event | `bond_locked` | `{bundle_id, account_id, amount}` |
| event | `deal_confirmed` | `{bundle_id, account_id, of}` (`of` = the party whose bond is released) |
| event | `deal_disputed` | `{bundle_id, complainant, accused, evidence_hash, disputed_at}` |
| event | `deal_ruled` | `{bundle_id, arbiter, verdict}` |
| event | `bond_slashed` | `{bundle_id, from, to, amount, from_stake, to_stake}` |
| event | `bond_released` | `{bundle_id, account_id, amount, reason}`; `reason` = `confirmed`, `elapsed`, `arbiter_silent`, `ruling` |
| error | `ERR_BOND_REQUIRED`, `ERR_BOND_INSUFFICIENT_STAKE`, `ERR_ARBITER_INVALID`, `ERR_DEAL_WINDOW`, `ERR_DEAL_LIMIT`, `ERR_DEAL_UNKNOWN`, `ERR_DEAL_NOT_PARTY`, `ERR_DEAL_CLOSED`, `ERR_DEAL_NOT_DISPUTED`, `ERR_DEAL_ALREADY_DISPUTED`, `ERR_DEAL_WINDOW_OVER`, `ERR_NOT_ARBITER`, `ERR_RULING_WINDOW`, `ERR_DEAL_NOT_DUE`, `ERR_BOND_LOCKED` |

`ERR_BOND_LOCKED` is the new refusal of `request_exit` and `withdraw_stake` while `bond_locked > 0` (open deals end by themselves at the latest `confirm_by + 7 days`, so a member is held at most 21 days).

### 1.6 Parameters (D7)
`Params` gets `bond_threshold: U128` (*proposal*: default `100_000_000` = 100 credit; range 10 credit to 1,000,000 credit) and `bond_bps: u16` (default `2000`; range 500 to 5000). Both `#[serde(default)]` so that a `set_params` call from an older client, and a `params_updated` event, keep working with the defaults. `validate_params` and the `changed` list of `params_updated` get the two names. Params are stored in Borsh: **a `migrate` that fills the two defaults is needed** (test in the style of `ct083_upgrade_from_0_0_7`, from 0.0.11).

### 1.7 Storage
- `StorageKey::Deals = b'd'`: `bundle_id -> Deal`. A closed deal is deleted. A deal is **not** removed by `prune_epoch`; it is a separate map from the bundle, so pruning a bundle never frees a bond.
- Raw key `x207_bond_locked:<account>` -> `u128`: the sum of locked bonds (absent = 0). Kept apart from `Member`, so that the stored `Member` of 0.0.11 reads unchanged.
- `MAX_OPEN_DEALS_PER_MEMBER = 8` (*proposal*), counted in a raw key `x207_deal_count:<account>`; `ERR_DEAL_LIMIT` at commit.

### 1.8 Attacks and failure modes considered
| Case | What happens |
| --- | --- |
| The arbiter and one party are the same person | The other party signed the arbiter, so it accepted that. The loss is capped at the bond (`bond_bps` of `V`). No money outside the two bonds moves. |
| The arbiter is the DAO or the owner | Refused (`ERR_ARBITER_INVALID`): ADR-20. The DAO can name no one for a deal; the parties name the arbiter. |
| The arbiter is silent | `settle_deal` after 7 days releases both bonds; nobody is punished. A cheater who picks a silent arbiter loses nothing, so the other party must judge the arbiter before it signs (reputation of the arbiter, BE-053). |
| A party disputes to freeze the other's bond | The dispute locks the accused's bond for at most 7 more days, and puts the complainant's own bond at risk (`complaint_false`). |
| A party confirms, then disputes | Refused: a party that confirmed released the other bond; there is nothing left to dispute. |
| A deal after one party is frozen (CT-095) | Frozen members cannot enter new deals; their open deals continue and they can still confirm, dispute and be ruled on. |
| A default (`finalize_epoch`) of a bonded member | The debt is at most the limit, the limit is at most `stake - bond_locked`: the default is paid without touching a bond. |
| The victim exits before the ruling | Cannot: its own bond is locked in the same deal (`ERR_BOND_LOCKED`). |
| A bundle signed long ago is committed later | Terms carry the epoch, and `confirm_by` is relative to the commit: a stale bundle is refused by the epoch check as today. |
| The same person on both sides | Locks its own stake twice and gets it back: no gain. |

## 2. CT-095: `freeze_member`

A frozen member cannot start anything new. It can still pay what it owes, leave, and take its stake. **No money moves.**

### 2.1 Methods, views, events, errors
| Kind | Name | Notes |
| --- | --- | --- |
| change | `propose_freeze(account_id, reason_hash: Option<String>)` | owner (the DAO, by a proposal). Fixes `eta = now + FREEZE_TIMELOCK_NS`. A second proposal for the same account is `ERR_FREEZE_PENDING`. |
| change | `apply_freeze(account_id)` | anybody, once `now >= eta`. |
| change | `cancel_freeze(account_id)` | owner, while pending. |
| change | `unfreeze_member(account_id)` | owner, at once (lifting a restriction needs no waiting). |
| view | `get_freeze(account_id) -> FreezeView | null` | `{state (pending, frozen), eta, reason_hash}` |
| view | `is_frozen(account_id) -> bool` | |
| event | `freeze_proposed` | `{account_id, eta, reason_hash}` |
| event | `member_frozen` | `{account_id}` |
| event | `freeze_cancelled` | `{account_id}` |
| event | `member_unfrozen` | `{account_id}` |
| error | `ERR_NOT_OWNER` (existing), `ERR_NOT_MEMBER` (existing), `ERR_FREEZE_PENDING`, `ERR_FREEZE_NOT_PENDING`, `ERR_FREEZE_TIMELOCK`, `ERR_ALREADY_FROZEN`, `ERR_NOT_FROZEN`, `ERR_FROZEN` |

`FREEZE_TIMELOCK_NS = 24 hours` (*proposal*, the same as `UPGRADE_TIMELOCK_NS`). The wait is the warning: the member sees `freeze_proposed` and can settle its debts, finish its deals, or leave before the freeze applies.

### 2.2 What a frozen member cannot and can do
Cannot (`ERR_FROZEN`, when the frozen account is the caller, the payer, the payee, the grantor, the grantee or a participant): `register_cumulative`, `register_voucher`, `register_in_bundle` (these three only for a voucher of an epoch **after** the epoch in which the freeze took effect, so a seller that already delivered can register until the cutoff, also in a bundle that was committed before), `commit_bundle` (any participant frozen), `post_intent`, `offer_grant`, `accept_grant`, `complete_escrow`, and a new deal.

Can: pay a residual (`ft_on_transfer` and `mt_on_transfer` `settle`), `claim`, add stake, `request_exit`, `withdraw_stake`, `cancel_escrow`, `revoke_grant`, `remove_intent`, `confirm_deal`, `dispute_deal`, `settle_deal`. Open obligations stay in clearing as they are. A frozen member keeps its stake and its right to claim; the DAO cannot reach either (ADR-20).

### 2.3 Storage
A raw key `x207_freeze:<account>` -> `{state, eta, reason_hash}` (absent = not frozen). `Member` is unchanged.

### 2.4 Review notes (BE-064 part 1, 2026-11-03)
- **C2, as designed.** A freeze applied while epoch E is clearing is recorded with `from_epoch` = the epoch that is open then (E+1). A voucher of E+1 therefore still registers; only vouchers of epochs after `from_epoch` are refused. This is the rule "vouchers of the epoch in which the freeze took effect stay registrable". `ct095_freeze` locks it.
- **C3, as designed.** A freeze record is **not** deleted when the member exits, so that leaving and joining again does not undo a freeze. The owner can still call `unfreeze_member`.

## 3. CT-096: official contracts per asset type

### 3.1 What it is
For an `asset_type` of class A, the DAO can set a list of the token contracts that may be escrowed for it. The list is the answer to a fake token with the right name.
- **Empty list (the default) means the old behaviour**: any contract named in the scope is taken.
- With a list, the contract named in the scope of `offer_grant` must be in it (`ERR_CONTRACT_NOT_OFFICIAL`, at the offer, so that the owner finds out before it puts anything in escrow), and a deposit from a contract outside the list is refused again at the receiver (`nft_on_transfer`, `ft_on_transfer`, `mt_on_transfer`; the multi-token receiver hands everything back by its return value, as in CT-092).
- `intents:multi-token` and other multi-token types are checked on `mt_contract`; the token id inside is still the one of the scope.

### 3.2 Methods, views, events, errors
| Kind | Name | Notes |
| --- | --- | --- |
| change | `set_asset_contracts(asset_type, contracts: Vec<AccountId>)` | owner (a DAO proposal). At most 16 accounts. The asset type must be registered with class `A`. An empty vector clears the list. |
| view | `get_asset_contracts(asset_type) -> Vec<AccountId>` | empty = no list |
| event | `asset_contracts_set` | `{asset_type, contracts}` |
| error | `ERR_ASSET_CLASS_NOT_A`, `ERR_TOO_MANY_CONTRACTS`, `ERR_CONTRACT_NOT_OFFICIAL`, `ERR_UNKNOWN_ASSET_TYPE` (existing code reused) |

### 3.3 Storage
`StorageKey::AssetContracts = b'w'`: `asset_type -> Vec<AccountId>`. A separate map, so that `AssetClassEntry` (stored in D10) keeps its layout; no migration for it.

### 3.4 Limits
The list is the circle's opinion of which contract is the "real" one; the contract cannot tell. A list changed by the DAO does not affect an escrow that is already held. A grant offered before the list was set is not re-checked at the offer, but its deposit is checked.

### 3.5 Review notes (BE-064 part 1, 2026-11-03)
- **C1, fixed.** `complete_escrow` checks `is_official` for all three forms (NFT, NEP-141, NEP-245), so a token put in escrow before the owner set the list cannot be bought after it (`ERR_CONTRACT_NOT_OFFICIAL`). `cancel_escrow` is not checked: the grantor takes the token back. `ct096_asset_contracts` has the test.
- **C4.** The list is read at offer, deposit and completion; a held escrow is not touched by a change of the list, but it cannot be completed once its contract is off the list.
- **C5.** Clearing the list (an empty vector) switches the protection off at once, without a timelock; the owner (the DAO) decides this by a proposal.

## 4. Effects on the rest

### 4.1 `JOIN_STORAGE_DEPOSIT` (ADR-41, 0.32 NEAR today)
The deposit covers the worst case of what one member makes the contract store (31,282 bytes measured by `ct062_storage_per_member`: 64 intents, 16 grants, the member row). 0.0.12 adds, for one member:
- up to 8 open deals (a deal record is about 350 to 400 bytes with two account ids, the arbiter, two hashes, the times and the flags; counted in full for each of its two members, which is cautious): about 3,200 bytes;
- the locked-bond number and the deal counter (raw keys, about 140 bytes together) and one freeze record (about 180 bytes).

**Measured** by the extended `ct062_storage_per_member` (`what_bonds_and_a_freeze_add_to_the_worst_case_of_one_member`, names of 64 characters): an open deal adds **344 bytes** (8 deals: 2,752), a freeze record **202 bytes**, so 0.0.12 adds **2,954 bytes, 0.0295 NEAR** for one member, and the worst case becomes 31,282 + 2,954 = **34,236 bytes = 0.3424 NEAR**. The estimate above (0.36 NEAR) was too high; a deposit of **0.35 NEAR** covers the measurement. The coordinator approved **0.35 NEAR** (ADR-41); `JOIN_STORAGE_DEPOSIT` is 350 mNEAR in the CT-094 commit. Existing members joined with 0.32 and are not asked for more; the extra storage of an old member is paid by the circle account, which has to keep a balance.

### 4.2 Interface and tests
- ABI: 7 changed or new methods for bonds, 4 for freeze, 1 for contracts, 5 views, 11 events, about 25 error codes. `ct068_abi_freeze` and `abi.json` are regenerated; the interface is 0.0.12.
- Tests planned: unit tests for each transition and each error; near-sandbox for the four paths of CT-094 (two confirmations, the buyer wins, the seller wins, a silent arbiter), the freeze (a frozen member is refused for new things and leaves with its whole stake), the contract list (a fake token is refused, an empty list keeps the old behaviour), the upgrade from 0.0.11 with a `Params` migration, an extended `ct062`, and a stateful property test that `limit <= stake - bond_locked` and `total_stake` constant hold through any sequence.
- Backend (BE-053 and later): the events above enter the read model; listings and offers of a frozen member are refused (the indexer sees `member_frozen`).

## 5. Decisions (coordinator, 2026-11-03) and what remains open
Decided: (1) rings of three or more have **no bond in 0.0.12**: this is a limit, written here and in the CHANGELOG; (2) `complaint_false` costs **half** of the complainant's bond; (3) a silent arbiter releases all bonds and `bond_released {reason}` says `arbiter_silent`; (4) open deals of a frozen member continue; (5) deposit measured (section 4.1); (6) a frozen arbiter can still rule. The text below is the original question list, kept for the record.

### Original questions
1. **Rings of three or more.** Bonds are required only for two-party swaps in this design. A ring above the threshold goes without a bond. The alternatives are to refuse it (it stops the clearing of rings, the core of the product) or to bond every payer against one arbiter (a dispute is then between pairs, and the rulings multiply). *Recommended: two-party only in 0.0.12, and write the gap.*
2. **Who loses a dismissed dispute.** `complaint_false` makes the complainant pay its bond to the accused. This discourages false complaints but also makes an honest mistake costly; `no_fault` is the arbiter's way out. Do you want `complaint_false` to cost the whole bond or only half?
3. **A silent arbiter.** Releasing both bonds is safe but gives a cheater an easy road if the victim accepted a weak arbiter. Alternative: the arbiter's own stake or a fee (not in this design, the arbiter may be outside the circle).
4. **Frozen member and open deals.** Open deals of a frozen member run to their end. The alternative, freezing them, would let the DAO hold a bond hostage, which ADR-20 does not allow.
5. **Deposit.** 0.36 NEAR is an estimate. Do you accept raising the join deposit a second time in two releases?
6. **`freeze_member` and the arbiter.** A frozen arbiter (if it is a member) can still rule: otherwise the DAO could silence an arbiter and force the "silent" outcome.
