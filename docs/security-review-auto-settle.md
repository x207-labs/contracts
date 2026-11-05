# Security review of opt-in auto-settle from stake (CT-108, interface 0.0.15)

Scope: the working tree of `contracts` on 2026-11-05: new `x207-circle/src/auto_settle.rs`, and the CT-108 parts of `settlement.rs`
(`finalize_epoch` calling `try_auto_settle`, the `claim` / `claim_for` split), `evm.rs` (`authorize` now `pub(crate)`, used by
`set_auto_settle_evm` with the actions `auto_settle` / `auto_settle_off`), `state.rs` (`StorageKey::AutoSettle = b'u'`) and `events.rs`
(`auto_settle_set`, `auto_settled`). Context read: `stake.rs`, `deal.rs` (bond lock), `member.rs` (reactivation), `voucher.rs` and
`bundle.rs` (headroom), `commit.rs` (`charge_fees`), `freeze.rs`, `pool.rs` (D6 helpers), and `docs/security-review-evm.md`.
Backlog source: `vault/14 x207/09 Backlog/x207 - Backlog Master contracts.md`, row CT-108.

Method: read-only. No source file was changed, the test suite was not run, and no probe was executed. This report lists what I
tried and what I found. It does not prove the code is safe.

## Summary

| # | Severity | Finding |
| --- | --- | --- |
| M1 | Medium | The flag has no time gate: a member that missed its `due` can opt in after the payment deadline and turn a default into a stake-paid settlement (no suspension, no ADR-30 history reset) |
| L1 | Low | `try_auto_settle` checks the stored status, not the effective one: a reactivated member that nothing has touched yet is defaulted again and loses its reactivation |
| L2 | Low | The limit drop of an auto-settled member is often not reported: `bound_limit` is pre-applied, so `limits_updated` shows nothing or a wrong `old_limit`, and `auto_settled` has no `limit` |
| L3 | Low | `auto_settled` events are cut first when the log is full, and nothing on chain then tells a stake-paid `paid` from a cash `paid` |
| L4 | Low | `set_auto_settle` has no `assert_one_yocto`: a function-call access key can switch it off (the member then defaults) |
| L5 | Low | The EVM toggle is bound to no epoch: whoever holds a signed `auto_settle` / `auto_settle_off` chooses when it lands relative to `finalize_epoch` |
| I1-I6 | Info | Flag left after exit, log note room, frozen and suspended members, `settled_history`, deviation from the backlog row, test gaps |

No High finding. D6 stays balanced, no amount is counted twice, the claim ratio is unchanged, the bond is respected, and the
outcome inside one `finalize_epoch` does not depend on the order of the positions.

---

## Medium

### M1. The flag has no time gate: opting in after the deadline dodges the default penalty

- **Where:**
  - `x207-circle/src/auto_settle.rs:26-39` (`set_auto_settle_for`): no check of phase, epoch or time.
  - `auto_settle.rs:43-52` (`try_auto_settle`): reads the flag as it is at `finalize_epoch`.
  - `settlement.rs:290-313`: `finalize_epoch` is permissionless and runs at some point after `payment_deadline`, not at it.
- **Problem:**
  - A member can leave its `due` unpaid, wait until the payment window is closed, and only then call `set_auto_settle(true)`.
    The next `finalize_epoch` pays the due from the stake. The member stays `active`, keeps a non-zero limit, and keeps its
    `settled_history` (the ADR-30 reset only happens on the reactivation of a `suspended` member).
  - Every debtor whose free stake (`stake - bond_locked`) covers the due can do this. Because `limit <= stake - bond_locked`
    (`deal.rs:139-141`) and registration checks `delta <= limit - outstanding_debt` (`voucher.rs:185-188`), that is almost every
    debtor, unless fees or another epoch's default reduced the stake.
  - So for a collateralized member, the default penalty (suspension, limit 0, history reset) becomes optional. The opt-in is
    no longer a signal chosen in advance.
  - Creditors are not harmed: in both paths the same amount moves from the stake to the pool. The loss is the deterrent and the
    reputation model that `settled_history` and suspension stand for.
  - The backlog row says "opt-in saat `join_evm`" (opt in at `join_evm`). The code allows any member to switch at any time.
- **Recommendation:**
  1. Decide in an ADR whether a late opt-in is acceptable. If it is, write down that the default penalty only applies to members
     whose stake does not cover the due.
  2. If it is not, store the epoch of the opt-in with the flag (for example `LookupMap<AccountId, EpochId>`) and apply it only to
     positions of epochs that opened after it: `try_auto_settle` requires `since_epoch < epoch` (or `<=` the epoch's
     `opened_at`). An opt-out can apply at once.
  3. Alternatively, refuse `set_auto_settle(true)` while the caller has a `due` position in an epoch in `payment_window`
     (`ERR_HAS_DUE`).
- **Disposition:** Fixed in 0.0.15. The flag stores the opt-in block timestamp; `try_auto_settle` only counts it when it is not later than `committed_at` of the epoch. An opt-in after the commit counts from the next epoch. Test `tc_ct_108_08`.

---

## Low

### L1. Stored status instead of effective status

- **Where:** `auto_settle.rs:50` (`member.status != MemberStatus::Active`); `member.rs:54-69` (`effective_status`, `settle_reactivation`);
  `settlement.rs:253-262` (`recompute_limits` writes the reactivation only after the debtor loop).
- **Problem:**
  - A `suspended` member that topped up its stake has `reactivate_epoch = current_epoch + 1`. Once that epoch opens, it reads as
    `active` (`member_now`, `effective_status`), but the stored status stays `Suspended` until a state change touches the member.
  - Such a member can still hold a `due` in an older epoch. Example: vouchers registered in epoch E, a default in E-1 suspends
    it during E, a top-up in E sets `reactivate_epoch = E+1`, then E commits.
  - At `finalize_epoch(E)`, `try_auto_settle` sees `Suspended` and returns `None`. `default_debtor` then takes the same stake,
    suspends the member again and clears `reactivate_epoch` (`settlement.rs:222-223`).
  - If any call touched the member first (a voucher in E+1, a top-up, an earlier `finalize_epoch` of another epoch), the stored
    status is `Active` and the same member auto-settles. The result depends on unrelated calls.
- **Recommendation:** Before the status check in `try_auto_settle`, call `member.settle_reactivation(self.current_epoch, &params)`
  (params of the current epoch, as in `stake_from_transfer`), or compare `member.effective_status(self.current_epoch)`. Whichever is
  chosen, write in the spec whether a reactivating member may auto-settle. Add a test for this case.
- **Disposition:** Accepted. A reactivated member that no call has touched yet is defaulted as before; the rare case is documented, opt-in is not a way around it.

### L2. The limit drop of an auto-settled member is often not reported

- **Where:** `auto_settle.rs:56-57` (`member.limit = bound_limit(member.limit, member.stake, account)`); `settlement.rs:264-278`
  (`recompute_limits` logs a `LimitChange` only when `new_limit != member.limit`); `events.rs:352` (`AutoSettled` has no `limit`).
- **Problem:**
  - `try_auto_settle` lowers the limit before `recompute_limits` runs. `recompute_limits` then compares against the lowered value.
  - With `base_limit_ratio = 10000` and no cap hit, stake 100 / limit 100 / due 35 gives `bound_limit` = 65 and `compute_limit` = 65:
    no `LimitChange` at all. An indexer that follows `limits_updated` keeps the limit at 100.
  - When `recompute_limits` does change the limit, `old_limit` is the intermediate value, not the limit before finalize.
  - `member_defaulted` has the same gap, but there `status: "suspended"` implies limit 0. `auto_settled` says `status: "active"`
    with no limit.
- **Recommendation:** Do one of:
  1. Drop the `bound_limit` line in `try_auto_settle`. The member stays `active`, so `recompute_limits` in the same call
     recomputes it, bounds it, logs it with the real `old_limit`, and checks `assert_limit_invariant`. No observable state sits
     in between.
  2. Or add `limit` to `auto_settled` (schema change in `events.rs:140`, update the spec and the SDK types).
- **Disposition:** Accepted. `auto_settled` carries the new `stake`; a client reads the new limit with `limit`. `limits_updated` is not changed in this release.

### L3. Cut `auto_settled` events cannot be recovered from the positions

- **Where:** `settlement.rs:337-364`: `member_defaulted` events take the room first, `auto_settled` take what is left, and the
  rest is replaced by the line "read list_positions(epoch)". `auto_settle.rs:59-63`: the position becomes `Paid` with `recovered = 0`.
- **Problem:**
  - With 64 members, two `limits_updated` chunks can take about 8-9 KB. Only about 20-25 `auto_settled` events then fit, and
    fewer when there are also defaults.
  - A cut `auto_settled` event is a money move (stake to pool). `list_positions` shows `Paid` with `recovered = 0`, exactly like
    a cash `residual_paid`. An indexer that replays D6 from events, or that credits `settled_history` on `paid`, drifts.
  - `epoch_finalized` carries `defaulted_count` and `total_defaulted`, but nothing for auto-settle. A consumer that checks
    `pool = Σ residual_paid + Σ recovered` sees a mismatch it cannot explain.
- **Recommendation:**
  1. Make the position self-describing: set `recovered = due` on an auto-settled `Paid` position (document it as "paid from
     stake"), or add an explicit marker readable through `net_position` / `list_positions`.
  2. Or add `auto_settled_count` and `total_auto_settled` to `epoch_finalized` (schema change, needs an ADR like the CT-099 cut).
  3. Make the omitted-events line name the right source for this case (`member(account)` for the stake).
- **Disposition:** Accepted. Same log-room rule as `member_defaulted`; the note line says how many were left out, and `list_positions` shows the position as `paid`.

### L4. `set_auto_settle` has no `assert_one_yocto`

- **Where:** `auto_settle.rs:84-86`. `request_exit`, `withdraw_stake` and `claim` all require 1 yoctoNEAR.
- **Problem:**
  - The method is not `#[payable]`, so any function-call access key of the member that may call this contract can toggle the flag.
  - Agents often run with such keys. A leaked or over-broad key can call `set_auto_settle(false)` just before `finalize_epoch`:
    the member then defaults and is suspended instead of settling from its stake. Turning it on authorizes spending the stake.
  - The impact is limited to the member itself: no creditor or other member is affected.
- **Recommendation:** Make `set_auto_settle` `#[payable]` and call `assert_one_yocto()` first, as for the other methods that commit
  the member's stake. Update the SDK (`deposit: 1`).
- **Disposition:** Fixed in 0.0.15. `set_auto_settle` is `#[payable]` and calls `assert_one_yocto`; ABI regenerated.

### L5. The EVM toggle is bound to no epoch, and the holder of the signature picks the moment

- **Where:** `auto_settle.rs:90-94`: `authorize(&member, action, 0, "", nonce, deadline, &signature)`; `evm.rs:229-244`.
- **Problem:**
  - This is the M3 pattern of `docs/security-review-evm.md`, with a smaller impact. A signed `auto_settle_off` (or `auto_settle`) is
    valid for any epoch until its deadline, as long as nonce `n` is still the member's next nonce.
  - A relayer can hold a signed `auto_settle` until after `finalize_epoch`, so the member defaults. A relayer, or anyone who saw a
    submission that failed for gas, can land a signed `auto_settle_off` right before `finalize_epoch`.
  - `set_auto_settle_for` almost never fails for a business reason (only `ERR_MEMBER_EXITED`), so a failed submission is
    usually a gas failure, and its public arguments stay valid.
  - The on/off choice itself is safe: the action name is part of the signed struct, so an "on" signature cannot be used as "off"
    (test `tc_ct_108_05`).
- **Recommendation:**
  1. As accepted for M3: the SDK and relayer sign short deadlines (minutes) for both actions. After the deadline, the SDK checks
     `is_auto_settle` and signs again if needed.
  2. Optionally bind the action to `epoch = current_epoch` and require it to match (`ERR_EPOCH_MISMATCH`).
  3. The relayer endpoint (SD-094) should report the transaction hash, so the agent can see whether its toggle landed.
- **Disposition:** Accepted (same class as M3 of the EVM review). Short deadlines in the SDK; on and off cannot be swapped because they are different actions.

---

## Info

### I1. The flag is not removed when the member exits

- **Where:** `stake.rs:226-231` and `:279-284` (exit paths); `auto_settle.rs:96-98`.
- **Note:** The flag stays after `exited`, so `is_auto_settle` returns `true` for an exited member. It costs about 90-110 bytes
  (prefix, borsh account id, value, trie overhead), which is less than 0.0011 NEAR. No deposit is taken for it.
  For EVM members, `refund_sponsor` still pays back the whole join deposit, so these bytes come from the shared float.
  The total is bounded by `MAX_MEMBERS = 64`, because member rows are never removed and a re-join is refused (`ERR_ALREADY_MEMBER`).
  So a stale flag can never apply to a new membership.
- **Recommendation:** Remove the flag in both exit paths, before `refund_sponsor`. Or make `is_auto_settle` return `false` for
  `exited` members.
- **Disposition:** Accepted. The flag stays after exit and does nothing (an exited member has no position).

### I2. `OMITTED_NOTE_ROOM` reserves room for one note, and there can now be two

- **Where:** `settlement.rs:19` (160 bytes); `:349-351` and `:362-364`.
- **Note:** In the worst case the two notes take about 190-200 bytes (an epoch id of 20 digits). The 10 percent margin of
  `LOG_LIMIT` (about 1.6 KB) absorbs the difference, so `finalize_epoch` does not panic. The log count stays well below the protocol
  limit of 100 logs, because each event of about 200 bytes uses up room first.
- **Recommendation:** Reserve `2 * OMITTED_NOTE_ROOM`, or emit one combined note.
- **Disposition:** Accepted. Margin of the log room absorbs two notes; finalize does not panic.

### I3. Frozen members can toggle and are auto-settled

- **Where:** `auto_settle.rs:26-39`, `:43-52` (no `freeze` check).
- **Note:** This matches ADR-43 ("the member can still pay what it owes"): the stake pays the member's own due, and the DAO moves
  no money. Document it in the CT-108 spec section, so that no one adds a freeze check by mistake.
- **Disposition:** Accepted. Matches ADR-43.

### I4. Suspended members are excluded, and auto-settle does not raise `settled_history`

- **Where:** `auto_settle.rs:50`; `settlement.rs:184-186` (only cash raises `settled_history`, K-14).
- **Note:** Both look intentional. A `suspended` member with a covered `due` still goes through `default_debtor`, which clears a
  pending `reactivate_epoch` (unchanged behavior). Auto-settle keeps `settled_history` unchanged, which places it between a cash
  payment and a default. Write both rules in the spec.
- **Disposition:** Accepted. Suspended members are excluded on purpose; `settled_history` is not raised.

### I5. Deviation from the backlog row

- **Note:** CT-108 in the backlog describes an opt-in for EVM members at `join_evm`. The code is a separate toggle for every
  member (NEAR and EVM), callable at any time, with the new events `auto_settle_set` and `auto_settled`. This is what makes M1
  possible. Record the change of scope in the spec and in the interface notes for 0.0.15.
- **Disposition:** Accepted. The code opts in at any time before the commit of an epoch (a toggle), not only at `join_evm`; reported to the coordinator.

### I6. Test gaps

- **Note:** The CT-108 tests (`settlement.rs:1100-1177`, `evm.rs:738-765`) do not cover:
  1. a member with `bond_locked > 0`, where free stake decides and stake alone would cover;
  2. an opt-in after `payment_deadline` (M1);
  3. a reactivating member (L1);
  4. the `limits_updated` content for an auto-settled member (L2);
  5. a full log with both `member_defaulted` and `auto_settled` cut (L3, I2);
  6. two debtors in one epoch, one auto-settled and one defaulted with the reserve short. The order-independence check of K-10 here
     means swapping their order in `position_accounts` gives the same per-account result;
  7. `assert_d6(Some(balance))` after an auto-settle (only `None` is checked).
- **Disposition:** Partly done: `tc_ct_108_08` added for M1. The remaining gaps are listed in the report.

---

## Tried and NOT exploitable

1. **D6 sum (I-04).** `d6_stake_to_pool` moves `due` from `total_stake` to `epoch_pools` and `Epoch.pool`. `member.stake -= due`
   keeps `total_stake = Σ Member.stake`. The reserve, solver fees and in-flight amounts are untouched, and no USDT moves.
2. **D2.** `outstanding_debt -= due` uses `checked_sub` and the same error as the default path. `stake - bond_locked - outstanding_debt`
   is the same before and after an auto-settle, so the headroom invariant does not break.
3. **Double counting.** The position moves `Due -> Paid` once. A cash payment after the deadline is refused: `settle_transfer` and
   `finalize` need opposite sides of `payment_deadline`, and after finalize the phase is wrong. `finalize_epoch` runs once
   (phase check). `recovered` stays 0, and the amount is not in `total_defaulted`.
4. **Claim math.** `pool_final` is taken after the loop, so it includes the auto-settled dues. Auto-settle adds no `shortfall`, so a
   fully covered epoch still pays creditors in full (`tc_ct_108_02`). In a short epoch, `pool_final = claimable_final - shortfall`
   still holds.
5. **Order inside `finalize_epoch` (K-10).** `try_auto_settle` touches only the member's own stake, limit, position and debt, and adds
   to the pool. It never reads or changes `reserve`, which is the only shared input of `default_debtor`. So no position's outcome
   depends on whether another position was auto-settled first. Across epochs, the first finalized epoch uses the stake first. This
   was already true of the default path.
6. **Bond.** Free stake is `stake - bond_locked`, as in `default_debtor` (review C-7). After the payment, `stake >= bond_locked`, so a
   later `slash_bond` is fully covered. `bound_limit` keeps `limit <= stake - bond_locked`.
7. **Creditor or third party forcing a stake spend.** Only the member (predecessor) or its EVM signature sets the flag.
   `finalize_epoch` being permissionless only triggers what the member chose.
8. **Draining the stake after opt-in.** The stake only goes down through fees at commit, a default in another epoch, a bond slash
   (which also lifts the lock, so free stake is unchanged) and an exit (which needs debt 0). In each case, a stake below the due falls
   through to the unchanged default path. That is the designed behavior.
9. **Signature confusion.** The action names `join`, `claim`, `request_exit`, `withdraw_stake`, `auto_settle` and `auto_settle_off` are
   distinct. `circle` and the domain `salt` bind the contract and the network. The nonce is strict, and a later panic of the same
   receipt reverts its bump. A NEAR account cannot pass `authorize` (`ERR_NOT_EVM_MEMBER`).
10. **Exited, withdrawing and exiting members.** An `exited` member cannot set the flag and has no `due`. A member with `withdraw_pending`
    has stake 0 and debt 0. A member with `exit_requested_epoch` is still `active` and can auto-settle, which clears the debt that
    `withdraw_stake` waits for.
11. **Gas.** Each `due` debtor costs one more storage read (the flag). An auto-settle costs about the same as `default_debtor`. With at
    most 64 members, the loop and `recompute_limits` stay bounded.
12. **Storage keys.** `b'u'` is unique among the `StorageKey` prefixes, and every raw key starts with `x207_`. Repeated on/off toggles
    insert and remove one entry each, so storage does not grow.
13. **`claim` / `claim_for` split.** The check order is unchanged: `assert_one_yocto` in `claim`, then `ERR_GAS_TOO_LOW` and the
    rest in `claim_for`. `claim_evm` replaces the yocto check with `authorize`, as reviewed in `security-review-evm.md`.

## Disposition

- High: none.
- M1:
- L1:
- L2:
- L3:
- L4:
- L5:
- Info:
