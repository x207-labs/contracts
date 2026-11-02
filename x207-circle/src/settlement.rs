//! F4 settlement: D5 positions, D6 accounting, `ft_on_transfer`, `finalize_epoch`, `claim`.

use near_sdk::json_types::{I128, U128, U64};
use near_sdk::serde_json::json;
use near_sdk::{assert_one_yocto, env, near, AccountId, Gas, NearToken, Promise, PromiseOrValue};

use crate::clearing::{ClearingProposal, EpochPhase};
use crate::events::{
    chunked_logs, EpochFinalized, LimitChange, LimitsUpdated, MemberDefaulted, ResidualClaimFailed, ResidualClaimed, ResidualPaid, X207Event,
};
use crate::member::MemberStatus;
use crate::risk::{assert_limit_invariant, compute_limit, decay_limit};
use crate::state::{X207Circle, X207CircleExt};
use crate::types::{ClaimDestination, EpochId};

/// Total log bytes one call may write (near protocol), and the room kept for the "left out" line.
const LOG_LIMIT: usize = 16 * 1024;
const OMITTED_NOTE_ROOM: usize = 160;

/// Gas constants of spec 4.6; the final values come from CT-043.
pub const CLAIM_MIN_GAS: Gas = Gas::from_tgas(100);
pub const CLAIM_CALLBACK_GAS: Gas = Gas::from_tgas(15);
pub const FT_TRANSFER_GAS: Gas = Gas::from_tgas(15);
pub const FT_TRANSFER_CALL_GAS: Gas = Gas::from_tgas(50);

/// Spec 3.5. Debtor: `due -> paid | defaulted`. Creditor: `claimable <-> claimed` (ADR-24).
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PositionStatus {
    Due,
    Paid,
    Defaulted,
    Claimable,
    Claimed,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PositionRole {
    Debtor,
    Creditor,
}

/// D5 record per `(epoch, account)` (spec 9.1, prefix `s`); the key carries epoch and account.
/// Net-zero participants have no record.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub net: i128,
    pub role: PositionRole,
    pub status: PositionStatus,
    pub amount: u128,
    pub recovered: u128,
    pub payment_deadline: u64,
    pub updated_at_block: u64,
}

impl PositionStatus {
    /// Forward moves of D5 (spec 3.5): `due -> paid | defaulted`, `claimable -> claimed`.
    pub fn can_move_to(self, to: PositionStatus) -> bool {
        use PositionStatus::*;
        matches!((self, to), (Due, Paid) | (Due, Defaulted) | (Claimable, Claimed))
    }

    /// The only way back: a failed claim transfer sets `claimed -> claimable` (ADR-24).
    pub fn can_revert_to(self, to: PositionStatus) -> bool {
        matches!((self, to), (PositionStatus::Claimed, PositionStatus::Claimable))
    }
}

impl Position {
    /// Moves along the forward transitions and records the block. Err is `ERR_BAD_TRANSITION`;
    /// the public methods map it to `ERR_NOT_DUE` or `ERR_NOT_CLAIMABLE`.
    pub fn transition(&mut self, to: PositionStatus, block: u64) -> Result<(), String> {
        if !self.status.can_move_to(to) {
            return Err(format!("ERR_BAD_TRANSITION: {:?} -> {:?}", self.status, to));
        }
        self.status = to;
        self.updated_at_block = block;
        Ok(())
    }

    /// Callback path only: `claimed -> claimable` after a failed claim transfer.
    pub fn revert_claim(&mut self, block: u64) -> Result<(), String> {
        if !self.status.can_revert_to(PositionStatus::Claimable) {
            return Err(format!("ERR_BAD_TRANSITION: {:?} -> {:?}", self.status, PositionStatus::Claimable));
        }
        self.status = PositionStatus::Claimable;
        self.updated_at_block = block;
        Ok(())
    }
}

/// `net_position` and `list_positions` element (spec 3.7). Timestamps are nanoseconds; the tx hash
/// lives in D9, the contract keeps `updated_at_block` (K-15).
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionView {
    pub epoch: EpochId,
    pub account_id: AccountId,
    pub net: I128,
    pub role: PositionRole,
    pub status: PositionStatus,
    pub amount: U128,
    pub recovered: U128,
    pub payment_deadline: U64,
    pub updated_at_block: u64,
}

impl Position {
    pub fn to_view(&self, epoch: EpochId, account_id: AccountId) -> PositionView {
        PositionView {
            epoch,
            account_id,
            net: I128(self.net),
            role: self.role,
            status: self.status,
            amount: U128(self.amount),
            recovered: U128(self.recovered),
            payment_deadline: U64(self.payment_deadline),
            updated_at_block: self.updated_at_block,
        }
    }
}

impl X207Circle {
    /// Writes a D5 record and appends the account to the epoch's position index on first write.
    /// `commit_epoch` calls this for each non-zero participant.
    pub fn put_position(&mut self, epoch: EpochId, account_id: AccountId, position: Position) {
        let key = (epoch, account_id.clone());
        let is_new = !self.positions.contains_key(&key);
        self.positions.insert(key, position);
        if is_new {
            self.epoch_mut_or_panic(epoch).position_accounts.push(account_id);
        }
    }
}

impl X207Circle {
    /// `settle` action of `ft_on_transfer` (spec 4.1). Order of checks (spec 4.7): `ERR_UNKNOWN_EPOCH`,
    /// `ERR_WRONG_PHASE`, `ERR_PAYMENT_WINDOW_CLOSED`, `ERR_NO_POSITION`, `ERR_NOT_DUE`,
    /// `ERR_UNDERPAYMENT`. Returns the refund `amount - due`. One call changes D5, D2, D6 and D1.
    pub fn settle_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        epoch: EpochId,
        member_id: Option<AccountId>,
    ) -> PromiseOrValue<U128> {
        let account = member_id.unwrap_or_else(|| sender_id.clone());
        let e = self.epoch_or_panic(epoch);
        e.require_phase(EpochPhase::PaymentWindow);
        let deadline = e.payment_deadline.expect("payment_window epoch has payment_deadline");
        if env::block_timestamp() >= deadline {
            env::panic_str(&format!("ERR_PAYMENT_WINDOW_CLOSED: payment_deadline {deadline}"));
        }
        let key = (epoch, account.clone());
        let Some(position) = self.positions.get(&key) else {
            env::panic_str(&format!("ERR_NO_POSITION: {account} in epoch {epoch}"))
        };
        let due = position.amount;
        if position.status != PositionStatus::Due {
            env::panic_str(&format!("ERR_NOT_DUE: {account} is {:?}", position.status));
        }
        if amount.0 < due {
            env::panic_str(&format!("ERR_UNDERPAYMENT: sent {} due {due}", amount.0));
        }

        let block = env::block_height();
        self.positions
            .get_mut(&key)
            .expect("position checked above")
            .transition(PositionStatus::Paid, block)
            .unwrap_or_else(|reason| env::panic_str(&reason));
        let debt = self.outstanding_debt.get(&account).copied().unwrap_or(0);
        let debt = debt
            .checked_sub(due)
            .unwrap_or_else(|| env::panic_str("ERR_D2_ACCOUNTING: outstanding_debt below the residual due"));
        self.outstanding_debt.insert(account.clone(), debt);
        self.d6_deposit_epoch_pool(epoch, due);
        // Only settlement in cash raises `settled_history`; the limit follows at `finalize_epoch` (K-14).
        let member = self.members.get_mut(&account).unwrap_or_else(|| env::panic_str(&format!("ERR_NOT_MEMBER: {account}")));
        member.settled_history += due;
        let settled_history = member.settled_history;

        let refunded = amount.0 - due;
        ResidualPaid {
            epoch,
            account_id: account,
            sender_id,
            amount: U128(due),
            refunded: U128(refunded),
            settled_history: U128(settled_history),
        }
        .emit();
        PromiseOrValue::Value(U128(refunded))
    }
}

impl X207Circle {
    /// Step 1 of spec 4.5 for one debtor: stake first, then the reserve, the rest is `shortfall`.
    /// The member turns `suspended` with limit 0 and the due leaves `outstanding_debt`.
    fn default_debtor(&mut self, epoch: EpochId, account: &AccountId, block: u64) -> MemberDefaulted {
        let key = (epoch, account.clone());
        let due = self.positions.get(&key).expect("defaulting position exists").amount;
        let stake = self.members.get(account).map_or(0, |m| m.stake);
        let from_stake = due.min(stake);
        let from_reserve = (due - from_stake).min(self.reserve);
        let shortfall = due - from_stake - from_reserve;

        self.d6_stake_to_pool(epoch, from_stake);
        self.d6_reserve_to_pool(epoch, from_reserve);
        self.epoch_mut_or_panic(epoch).shortfall += shortfall;

        let member = self.members.get_mut(account).unwrap_or_else(|| env::panic_str(&format!("ERR_NOT_MEMBER: {account}")));
        member.stake -= from_stake;
        member.limit = 0;
        member.status = MemberStatus::Suspended;
        member.reactivate_epoch = None;
        let stake_left = member.stake;
        assert_limit_invariant(member);

        let debt = self.outstanding_debt.get(account).copied().unwrap_or(0);
        let debt = debt
            .checked_sub(due)
            .unwrap_or_else(|| env::panic_str("ERR_D2_ACCOUNTING: outstanding_debt below the defaulted due"));
        self.outstanding_debt.insert(account.clone(), debt);

        let position = self.positions.get_mut(&key).expect("defaulting position exists");
        position.transition(PositionStatus::Defaulted, block).unwrap_or_else(|reason| env::panic_str(&reason));
        position.recovered = from_stake + from_reserve;

        MemberDefaulted {
            epoch,
            account_id: account.clone(),
            due: U128(due),
            from_stake: U128(from_stake),
            from_reserve: U128(from_reserve),
            shortfall: U128(shortfall),
            stake: U128(stake_left),
            status: "suspended".into(),
            position_status: "defaulted".into(),
            outstanding_debt: U128(debt),
        }
    }

    /// Step 3 of spec 4.5: `limit = compute_limit` for every `active` member with the params
    /// snapshot of `epoch` (K-12). Returns the members whose limit changed (K-11).
    fn recompute_limits(&mut self, epoch: EpochId) -> Vec<LimitChange> {
        let params = self.epoch_or_panic(epoch).params.clone();
        let current_epoch = self.current_epoch;
        let accounts: Vec<AccountId> = self.member_index.iter().cloned().collect();
        let mut changes = Vec::new();
        for account in accounts {
            let member = self.members.get_mut(&account).expect("indexed member");
            member.settle_reactivation(current_epoch, &params);
            if member.status != MemberStatus::Active {
                continue;
            }
            let new_limit = decay_limit(
                compute_limit(member.stake, member.settled_history, &params),
                member.last_active_epoch,
                epoch,
                params.limit_decay,
            );
            if new_limit != member.limit {
                changes.push(LimitChange {
                    account_id: account,
                    old_limit: U128(member.limit),
                    new_limit: U128(new_limit),
                    status: "active".into(),
                });
                member.limit = new_limit;
            }
            assert_limit_invariant(member);
        }
        changes
    }
}

#[near]
impl X207Circle {
    /// Permissionless after the payment window (spec 4.5). Every `due` left becomes `defaulted`
    /// (stake, then reserve, into the epoch pool), active limits are recomputed, and the epoch is
    /// `finalized`. P1 `limit_decay` (CT-053) lowers the limit of idle members; the P2 haircut (CT-057) is not applied.
    pub fn finalize_epoch(&mut self, epoch: EpochId) {
        let now = env::block_timestamp();
        let block = env::block_height();
        self.epoch_mut_or_panic(epoch).finalize(now);

        let accounts: Vec<AccountId> = self.epoch_or_panic(epoch).position_accounts.iter().cloned().collect();
        let (mut defaulted, mut total_defaulted, mut claimable_final) = (Vec::new(), 0u128, 0u128);
        for account in accounts {
            let position = self.positions.get(&(epoch, account.clone())).expect("indexed position");
            match (position.role, position.status) {
                (PositionRole::Debtor, PositionStatus::Due) => {
                    total_defaulted += position.amount;
                    defaulted.push(self.default_debtor(epoch, &account, block));
                }
                (PositionRole::Creditor, PositionStatus::Claimable) => claimable_final += position.amount,
                _ => {}
            }
        }
        let changes = self.recompute_limits(epoch);

        let e = self.epoch_mut_or_panic(epoch);
        e.pool_final = e.pool;
        e.claimable_final = claimable_final;
        let (pool, shortfall, finalized_at) = (e.pool, e.shortfall, e.finalized_at.expect("finalized epoch has finalized_at"));
        let defaulted_count = defaulted.len() as u32;
        let limit_logs = chunked_logs(&changes, |changes, chunk, chunks| LimitsUpdated { epoch, changes, chunk, chunks });
        let finalized_log = EpochFinalized {
            epoch,
            finalized_at: U64(finalized_at),
            defaulted_count,
            total_defaulted: U128(total_defaulted),
            pool: U128(pool),
            shortfall: U128(shortfall),
        }
        .to_log();

        // near caps the total log of one call at 16 KiB. One `member_defaulted` per debtor stops fitting
        // at about 60 debtors, and a `finalize_epoch` that panics on the cap would lock every claim of the
        // epoch. So the debtor events are cut when the room runs out, `limits_updated` and `epoch_finalized`
        // always go out, and one plain line says how many were left out (the positions stay readable
        // through `list_positions`). Needs an ADR: the spec asks one event per debtor.
        let reserved: usize = limit_logs.iter().map(String::len).sum::<usize>() + finalized_log.len() + OMITTED_NOTE_ROOM;
        let mut room = LOG_LIMIT.saturating_sub(reserved);
        let mut omitted = 0u32;
        for event in defaulted {
            let log = event.to_log();
            if log.len() <= room {
                room -= log.len();
                env::log_str(&log);
            } else {
                omitted += 1;
            }
        }
        if omitted > 0 {
            env::log_str(&format!("x207: {omitted} member_defaulted events left out of the log limit, read list_positions({epoch})"));
        }
        for log in limit_logs {
            env::log_str(&log);
        }
        env::log_str(&finalized_log);
    }
}

fn destination_wire(destination: ClaimDestination) -> &'static str {
    match destination {
        ClaimDestination::Near => "near",
        ClaimDestination::Intents => "intents",
    }
}

#[near]
impl X207Circle {
    /// Creditor takes the residual of a finalized epoch (ADR-24). Order of checks (spec 4.7):
    /// `assert_one_yocto`, `ERR_GAS_TOO_LOW`, `ERR_UNKNOWN_EPOCH`, `ERR_NOT_FINALIZED`, `ERR_NO_POSITION`,
    /// `ERR_NOT_CREDITOR`, `ERR_NOT_CLAIMABLE`, `ERR_POOL_INSUFFICIENT`. The funds always go to the
    /// predecessor (to its Intents balance for `intents`, ADR-19); `on_claim_resolved` settles the outcome.
    #[payable]
    pub fn claim(&mut self, epoch: EpochId, destination: ClaimDestination) -> Promise {
        assert_one_yocto();
        if env::prepaid_gas() < CLAIM_MIN_GAS {
            env::panic_str(&format!("ERR_GAS_TOO_LOW: need {} TGas", CLAIM_MIN_GAS.as_tgas()));
        }
        let account = env::predecessor_account_id();
        let e = self.epoch_or_panic(epoch);
        if e.phase != EpochPhase::Finalized {
            env::panic_str(&format!("ERR_NOT_FINALIZED: epoch {epoch}"));
        }
        let Some(position) = self.positions.get(&(epoch, account.clone())) else {
            env::panic_str(&format!("ERR_NO_POSITION: {account} in epoch {epoch}"))
        };
        if position.role != PositionRole::Creditor {
            env::panic_str(&format!("ERR_NOT_CREDITOR: {account}"));
        }
        if position.status != PositionStatus::Claimable {
            env::panic_str(&format!("ERR_NOT_CLAIMABLE: {account} is {:?}", position.status));
        }
        // Full amount, or the frozen ratio when the pool came up short (K-10). Frozen values keep the
        // result independent of the order of claims.
        let amount = if e.shortfall > 0 {
            position.amount.saturating_mul(e.pool_final) / e.claimable_final.max(1)
        } else {
            position.amount
        };
        if amount > e.pool {
            env::panic_str(&format!("ERR_POOL_INSUFFICIENT: pay {amount} pool {}", e.pool));
        }

        let block = env::block_height();
        self.positions
            .get_mut(&(epoch, account.clone()))
            .expect("position checked above")
            .transition(PositionStatus::Claimed, block)
            .unwrap_or_else(|reason| env::panic_str(&reason));
        self.d6_pool_to_in_flight(epoch, amount);

        let usdt = self.usdt_token_id.clone();
        let transfer = match destination {
            ClaimDestination::Near => Promise::new(usdt).function_call(
                "ft_transfer".to_string(),
                json!({"receiver_id": account, "amount": U128(amount), "memo": null}).to_string().into_bytes(),
                NearToken::from_yoctonear(1),
                FT_TRANSFER_GAS,
            ),
            ClaimDestination::Intents => Promise::new(usdt).function_call(
                "ft_transfer_call".to_string(),
                json!({"receiver_id": self.intents_id, "amount": U128(amount), "memo": null, "msg": account.as_str()})
                    .to_string()
                    .into_bytes(),
                NearToken::from_yoctonear(1),
                FT_TRANSFER_CALL_GAS,
            ),
        };
        transfer.then(Self::ext(env::current_account_id()).with_static_gas(CLAIM_CALLBACK_GAS).on_claim_resolved(
            epoch,
            account,
            U128(amount),
            destination,
        ))
    }

    /// Callback of the claim transfer (spec 4.6). Never panics. Returns the amount that arrived.
    #[private]
    pub fn on_claim_resolved(
        &mut self,
        epoch: EpochId,
        account_id: AccountId,
        amount: U128,
        destination: ClaimDestination,
    ) -> U128 {
        let sent = amount.0;
        // `ft_transfer` has no value; `ft_transfer_call` returns the amount the receiver used.
        let used = match (env::promise_result_checked(0, 64), destination) {
            (Ok(_), ClaimDestination::Near) => sent,
            (Ok(bytes), ClaimDestination::Intents) => {
                near_sdk::serde_json::from_slice::<U128>(&bytes).map_or(0, |v| v.0.min(sent))
            }
            (Err(_), _) => 0,
        };
        let key = (epoch, account_id.clone());
        let block = env::block_height();
        let in_claimed_state = self.positions.get(&key).is_some_and(|p| p.status == PositionStatus::Claimed);
        if !in_claimed_state {
            return U128(0);
        }
        let wire = destination_wire(destination).to_string();
        if used > 0 {
            self.d6_in_flight_out(used);
        }
        if used == sent {
            ResidualClaimed { epoch, account_id, amount: U128(used), destination: wire, status: "claimed".into() }.emit();
            return U128(used);
        }
        let back = sent - used;
        self.d6_in_flight_to_pool(epoch, back);
        let position = self.positions.get_mut(&key).expect("position checked above");
        position.amount = position.amount.saturating_sub(used);
        position.revert_claim(block).unwrap_or_else(|_| unreachable!("checked claimed above"));
        if used > 0 {
            ResidualClaimed { epoch, account_id: account_id.clone(), amount: U128(used), destination: wire.clone(), status: "claimed".into() }.emit();
        }
        ResidualClaimFailed { epoch, account_id, amount: U128(back), destination: wire, status: "claimable".into() }.emit();
        U128(used)
    }
}

#[near]
impl X207Circle {
    /// D4 proposal of `epoch`; `edges` is empty after `commit_epoch`. `None` before the first submit.
    pub fn get_clearing_proposal(&self, epoch: EpochId) -> Option<ClearingProposal> {
        self.proposals.get(&epoch).cloned()
    }

    /// D5 record of `account_id` in `epoch`, which carries the residual bill (`amount`, and for a
    /// debtor `payment_deadline` = commit + `payment_window`). `None` for a net-zero participant, a
    /// non-participant, an unknown epoch, or an epoch not committed yet (K-23).
    pub fn net_position(&self, epoch: EpochId, account_id: AccountId) -> Option<PositionView> {
        self.positions.get(&(epoch, account_id.clone())).map(|p| p.to_view(epoch, account_id))
    }

    /// D5 records of `epoch` in the order `commit_epoch` wrote them. `limit` 0, an unknown epoch,
    /// or a `from_index` past the end gives `[]`.
    pub fn list_positions(&self, epoch: EpochId, from_index: u32, limit: u32) -> Vec<PositionView> {
        let Some(e) = self.epochs.get(&epoch) else { return vec![] };
        e.position_accounts
            .iter()
            .skip(from_index as usize)
            .take(limit as usize)
            .filter_map(|id| self.positions.get(&(epoch, id.clone())).map(|p| p.to_view(epoch, id.clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::json_types::{U128, U64};
    use near_sdk::serde_json::{self, json};
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    use super::*;
    use crate::params::Params;
    use crate::types::ClearingEdge;

    const ALL: [PositionStatus; 5] = [
        PositionStatus::Due,
        PositionStatus::Paid,
        PositionStatus::Defaulted,
        PositionStatus::Claimable,
        PositionStatus::Claimed,
    ];

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn circle() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        X207Circle::with_config(acc("dao.near"), "agents-main".into(), acc("usdt.near"), acc("intents.near"), Params::default(), 1_000)
    }

    fn position(role: PositionRole, status: PositionStatus, net: i128) -> Position {
        Position { net, role, status, amount: net.unsigned_abs(), recovered: 0, payment_deadline: 99, updated_at_block: 10 }
    }

    // TC-CT-012-01
    #[test]
    fn allowed_transitions() {
        use PositionStatus::*;
        let mut p = position(PositionRole::Debtor, Due, -35);
        p.transition(Paid, 11).unwrap();
        assert_eq!((p.status, p.updated_at_block), (Paid, 11));
        let mut p = position(PositionRole::Debtor, Due, -35);
        p.transition(Defaulted, 12).unwrap();
        assert_eq!(p.status, Defaulted);
        let mut c = position(PositionRole::Creditor, Claimable, 35);
        c.transition(Claimed, 13).unwrap();
        assert_eq!(c.status, Claimed);
        c.revert_claim(14).unwrap();
        assert_eq!((c.status, c.updated_at_block), (Claimable, 14));
    }

    // TC-CT-012-02
    #[test]
    fn all_other_transitions_are_rejected() {
        use PositionStatus::*;
        for (from, to) in [(Paid, Due), (Paid, Defaulted), (Defaulted, Paid), (Due, Claimed), (Claimable, Paid), (Claimed, Due)] {
            let mut p = position(PositionRole::Debtor, from, 0);
            let err = p.transition(to, 99).unwrap_err();
            assert!(err.starts_with("ERR_BAD_TRANSITION"), "{from:?} -> {to:?}");
            assert_eq!((p.status, p.updated_at_block), (from, 10), "state changed on a rejected move");
        }
    }

    #[test]
    fn exactly_three_forward_moves_exist() {
        let moves = ALL.iter().flat_map(|a| ALL.iter().map(move |b| (*a, *b))).filter(|(a, b)| a.can_move_to(*b)).count();
        assert_eq!(moves, 3);
        assert!(ALL.iter().flat_map(|a| ALL.iter().map(move |b| (*a, *b))).filter(|(a, b)| a.can_revert_to(*b)).count() == 1);
    }

    #[test]
    fn claimed_returns_only_through_revert_claim() {
        let mut c = position(PositionRole::Creditor, PositionStatus::Claimed, 35);
        assert!(c.transition(PositionStatus::Claimable, 1).is_err());
        let mut d = position(PositionRole::Debtor, PositionStatus::Paid, -35);
        assert!(d.revert_claim(1).is_err());
        assert_eq!(d.status, PositionStatus::Paid);
    }

    // TC-CT-012-03 (unit side: null before submit, edges kept in the summary)
    #[test]
    fn proposal_view() {
        let mut c = circle();
        assert_eq!(c.get_clearing_proposal(1), None);
        let proposal = ClearingProposal {
            epoch: 1,
            solver: acc("solver1.near"),
            edges: vec![ClearingEdge { debtor: acc("a.near"), creditor: acc("b.near"), cleared: U128(25) }],
            edge_count: 1,
            cleared_total: U128(25),
            submitted_at: U64(5),
            committed: false,
        };
        c.proposals.insert(1, proposal.clone());
        assert_eq!(c.get_clearing_proposal(1), Some(proposal));
        assert_eq!(c.get_clearing_proposal(2), None);
    }

    // TC-CT-012-04 (unit side): the view shows the new status and block
    #[test]
    fn status_change_shows_in_view() {
        let mut c = circle();
        c.put_position(1, acc("scraper.near"), position(PositionRole::Debtor, PositionStatus::Due, -35_000_000));
        let before = c.list_positions(1, 0, 10);
        assert_eq!((before[0].status, before[0].updated_at_block), (PositionStatus::Due, 10));
        c.positions.get_mut(&(1, acc("scraper.near"))).unwrap().transition(PositionStatus::Paid, 77).unwrap();
        let after = c.list_positions(1, 0, 10);
        assert_eq!((after[0].status, after[0].updated_at_block), (PositionStatus::Paid, 77));
    }

    // TC-CT-012-05 (unit side)
    #[test]
    fn list_positions_pages() {
        let mut c = circle();
        c.put_position(1, acc("scraper.near"), position(PositionRole::Debtor, PositionStatus::Due, -35));
        c.put_position(1, acc("gpu.near"), position(PositionRole::Creditor, PositionStatus::Claimable, 35));
        let all = c.list_positions(1, 0, 10);
        assert_eq!(all.iter().map(|p| p.account_id.as_str()).collect::<Vec<_>>(), ["scraper.near", "gpu.near"]);
        assert_eq!(c.list_positions(1, 1, 10).len(), 1);
        assert_eq!(c.list_positions(1, 1, 10)[0].account_id, acc("gpu.near"));
        assert!(c.list_positions(1, 0, 0).is_empty());
        assert!(c.list_positions(1, 5, 10).is_empty());
        assert!(c.list_positions(9, 0, 10).is_empty());
    }

    #[test]
    fn put_position_twice_keeps_one_index_entry() {
        let mut c = circle();
        c.put_position(1, acc("scraper.near"), position(PositionRole::Debtor, PositionStatus::Due, -35));
        c.put_position(1, acc("scraper.near"), position(PositionRole::Debtor, PositionStatus::Paid, -35));
        assert_eq!(c.list_positions(1, 0, 10).len(), 1);
    }

    // TC-CT-012-06
    #[test]
    fn enum_wire_names() {
        let names: Vec<_> = ALL.iter().map(|s| serde_json::to_value(s).unwrap()).collect();
        assert_eq!(names, [json!("due"), json!("paid"), json!("defaulted"), json!("claimable"), json!("claimed")]);
        assert_eq!(serde_json::to_value(PositionRole::Debtor).unwrap(), json!("debtor"));
        assert_eq!(serde_json::to_value(PositionRole::Creditor).unwrap(), json!("creditor"));
    }

    #[test]
    fn position_view_json_matches_spec_example() {
        let mut p = position(PositionRole::Debtor, PositionStatus::Due, -35_000_000);
        p.payment_deadline = 1_793_840_400_000_000_000;
        p.updated_at_block = 182_733_001;
        assert_eq!(
            serde_json::to_value(p.to_view(42, acc("scraper.near"))).unwrap(),
            json!({"epoch":42,"account_id":"scraper.near","net":"-35000000","role":"debtor","status":"due","amount":"35000000","recovered":"0","payment_deadline":"1793840400000000000","updated_at_block":182733001})
        );
    }

    // TC-CT-031-01, TC-CT-031-02
    #[test]
    fn tc_ct_031_01_02_debtor_and_creditor_views() {
        let mut c = circle();
        let mut debtor = position(PositionRole::Debtor, PositionStatus::Due, -35_000_000);
        debtor.payment_deadline = 1_000 + 1800 * 1_000_000_000;
        c.put_position(1, acc("scraper.near"), debtor);
        c.put_position(1, acc("gpu.near"), position(PositionRole::Creditor, PositionStatus::Claimable, 35_000_000));
        let d = serde_json::to_value(c.net_position(1, acc("scraper.near"))).unwrap();
        assert_eq!(
            (&d["net"], &d["role"], &d["status"], &d["amount"], &d["recovered"], &d["payment_deadline"]),
            (&json!("-35000000"), &json!("debtor"), &json!("due"), &json!("35000000"), &json!("0"), &json!("1800000001000"))
        );
        let g = serde_json::to_value(c.net_position(1, acc("gpu.near"))).unwrap();
        assert_eq!(
            (&g["net"], &g["role"], &g["status"], &g["amount"]),
            (&json!("35000000"), &json!("creditor"), &json!("claimable"), &json!("35000000"))
        );
    }

    // TC-CT-031-03, TC-CT-031-04, TC-CT-031-05: no record means null
    #[test]
    fn tc_ct_031_03_to_05_missing_record_is_null() {
        let mut c = circle();
        c.put_position(1, acc("scraper.near"), position(PositionRole::Debtor, PositionStatus::Due, -35_000_000));
        for (epoch, who) in [(1, "translator.near"), (1, "outsider.near"), (2, "scraper.near"), (99, "scraper.near")] {
            assert_eq!(serde_json::to_value(c.net_position(epoch, acc(who))).unwrap(), json!(null), "{epoch} {who}");
        }
    }

    // TC-CT-031-07
    #[test]
    fn tc_ct_031_07_defaulted_shows_recovered() {
        let mut c = circle();
        let mut p = position(PositionRole::Debtor, PositionStatus::Defaulted, -35_000_000);
        p.recovered = 35_000_000;
        c.put_position(1, acc("scraper.near"), p);
        let v = serde_json::to_value(c.net_position(1, acc("scraper.near"))).unwrap();
        assert_eq!((&v["status"], &v["recovered"]), (&json!("defaulted"), &json!("35000000")));
    }
}

#[cfg(test)]
mod settle_tests {
    use near_sdk::serde_json::{self, json};
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::{testing_env, PublicKey};

    use super::*;
    use crate::member::{Member, MemberStatus};
    use crate::params::Params;

    const CREDIT: u128 = 1_000_000;
    const SEC: u64 = 1_000_000_000;
    const T_COMMIT: u64 = 4_200 * SEC;
    const T_PAY: u64 = T_COMMIT + 1_800 * SEC;

    fn acc(s: &str) -> AccountId {
        format!("{s}.near").parse().unwrap()
    }

    fn call_as(token: &str, now: u64) {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc(token)).block_timestamp(now).block_height(77).build());
    }

    fn member(c: &mut X207Circle, name: &str) {
        c.members.insert(
            acc(name),
            Member {
                account_id: acc(name),
                pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse::<PublicKey>().unwrap(),
                terms_hash: "a".repeat(64),
                stake: 100 * CREDIT,
                limit: 80 * CREDIT,
                status: MemberStatus::Active,
                admitted: false,
                joined_at: 0,
                settled_history: 0,
                last_active_epoch: 1,
                exit_requested_epoch: None,
                withdraw_pending: false,
                reactivate_epoch: None,
            },
        );
    }

    /// S1 after `commit_epoch`: scraper owes 35 (`due`), gpu is owed 35, translator is net zero.
    fn s1() -> X207Circle {
        call_as("deployer", 0);
        let mut c = X207Circle::with_config(acc("dao"), "agents-main".into(), acc("usdt"), acc("intents"), Params::default(), 0);
        for n in ["scraper", "gpu", "translator"] {
            member(&mut c, n);
        }
        c.epochs.get_mut(&1).unwrap().start_clearing(3_600 * SEC);
        c.epochs.get_mut(&1).unwrap().start_payment_window(T_COMMIT);
        let position = |net: i128, role, status| Position {
            net,
            role,
            status,
            amount: net.unsigned_abs(),
            recovered: 0,
            payment_deadline: T_PAY,
            updated_at_block: 5,
        };
        c.put_position(1, acc("scraper"), position(-35 * CREDIT as i128, PositionRole::Debtor, PositionStatus::Due));
        c.put_position(1, acc("gpu"), position(35 * CREDIT as i128, PositionRole::Creditor, PositionStatus::Claimable));
        c.outstanding_debt.insert(acc("scraper"), 35 * CREDIT);
        c.total_stake = 300 * CREDIT;
        c
    }

    fn settle(c: &mut X207Circle, sender: &str, amount: u128, msg: serde_json::Value, now: u64) -> U128 {
        call_as("usdt", now);
        match c.ft_on_transfer(acc(sender), U128(amount), msg.to_string()) {
            PromiseOrValue::Value(v) => v,
            PromiseOrValue::Promise(_) => panic!("settle returns a value"),
        }
    }

    fn msg(epoch: u64) -> serde_json::Value {
        json!({"action": "settle", "epoch": epoch})
    }

    fn panics(f: impl FnOnce()) -> String {
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).expect_err("expected a panic");
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    #[test]
    fn tc_ct_032_01_exact_payment() {
        let mut c = s1();
        assert_eq!(settle(&mut c, "scraper", 35 * CREDIT, msg(1), T_COMMIT + SEC), U128(0));
        let p = c.positions.get(&(1, acc("scraper"))).unwrap();
        assert_eq!((p.status, p.updated_at_block), (PositionStatus::Paid, 77));
        assert_eq!(c.outstanding_debt(acc("scraper")), U128(0));
        assert_eq!(c.epochs.get(&1).unwrap().pool, 35 * CREDIT);
        assert_eq!(c.members.get(&acc("scraper")).unwrap().settled_history, 35 * CREDIT);
        let pool = c.pool_view();
        assert_eq!((pool.epoch_pools, pool.accounted), (U128(35 * CREDIT), U128(335 * CREDIT)));
        let log = get_logs().into_iter().find(|l| l.contains("residual_paid")).unwrap();
        let ev: serde_json::Value = serde_json::from_str(log.trim_start_matches("EVENT_JSON:")).unwrap();
        assert_eq!(
            ev["data"][0],
            json!({"epoch":1,"account_id":"scraper.near","sender_id":"scraper.near","amount":"35000000","refunded":"0","settled_history":"35000000"})
        );
    }

    #[test]
    fn tc_ct_032_02_overpayment_is_refunded() {
        let mut c = s1();
        assert_eq!(settle(&mut c, "scraper", 40 * CREDIT, msg(1), T_COMMIT + SEC), U128(5 * CREDIT));
        assert_eq!(c.epochs.get(&1).unwrap().pool, 35 * CREDIT);
        let log = get_logs().into_iter().find(|l| l.contains("residual_paid")).unwrap();
        assert!(log.contains("\"refunded\":\"5000000\"") && log.contains("\"amount\":\"35000000\""));
    }

    #[test]
    fn tc_ct_032_03_one_unit_short_is_rejected() {
        let mut c = s1();
        let text = panics(|| {
            settle(&mut c, "scraper", 35 * CREDIT - 1, msg(1), T_COMMIT + SEC);
        });
        assert!(text.contains("ERR_UNDERPAYMENT"), "{text}");
        assert_eq!(c.positions.get(&(1, acc("scraper"))).unwrap().status, PositionStatus::Due);
        assert_eq!(c.pool_view().epoch_pools, U128(0));
    }

    #[test]
    fn tc_ct_032_04_paid_by_someone_else() {
        let mut c = s1();
        let m = json!({"action": "settle", "epoch": 1, "member_id": "scraper.near"});
        assert_eq!(settle(&mut c, "outsider", 35 * CREDIT, m, T_COMMIT + SEC), U128(0));
        assert_eq!(c.positions.get(&(1, acc("scraper"))).unwrap().status, PositionStatus::Paid);
        let log = get_logs().into_iter().find(|l| l.contains("residual_paid")).unwrap();
        assert!(log.contains("\"sender_id\":\"outsider.near\"") && log.contains("\"account_id\":\"scraper.near\""));
    }

    #[test]
    fn tc_ct_032_05_wrong_token() {
        let mut c = s1();
        call_as("usdt_palsu", T_COMMIT + SEC);
        let text = panics(|| {
            let _ = c.ft_on_transfer(acc("scraper"), U128(35 * CREDIT), msg(1).to_string());
        });
        assert!(text.contains("ERR_WRONG_TOKEN"), "{text}");
    }

    #[test]
    fn tc_ct_032_06_unknown_epoch() {
        let mut c = s1();
        let text = panics(|| {
            settle(&mut c, "scraper", 35 * CREDIT, msg(99), T_COMMIT + SEC);
        });
        assert!(text.contains("ERR_UNKNOWN_EPOCH"), "{text}");
    }

    #[test]
    fn tc_ct_032_07_epoch_not_in_payment_window() {
        let mut c = s1();
        c.epochs.insert(2, crate::clearing::Epoch::open(2, 0, 9_000 * SEC, Params::default()));
        let open = panics(|| {
            settle(&mut c, "scraper", 35 * CREDIT, msg(2), T_COMMIT + SEC);
        });
        assert!(open.contains("ERR_WRONG_PHASE"), "{open}");
        let mut clearing = s1();
        clearing.epochs.insert(1, {
            let mut e = crate::clearing::Epoch::open(1, 0, 3_600 * SEC, Params::default());
            e.start_clearing(3_600 * SEC);
            e
        });
        let text = panics(|| {
            settle(&mut clearing, "scraper", 35 * CREDIT, msg(1), 3_700 * SEC);
        });
        assert!(text.contains("ERR_WRONG_PHASE"), "{text}");
    }

    #[test]
    fn tc_ct_032_08_deadline_boundary() {
        let mut c = s1();
        assert_eq!(settle(&mut c, "scraper", 35 * CREDIT, msg(1), T_PAY - 1), U128(0));
        let mut late = s1();
        let text = panics(|| {
            settle(&mut late, "scraper", 35 * CREDIT, msg(1), T_PAY);
        });
        assert!(text.contains("ERR_PAYMENT_WINDOW_CLOSED"), "{text}");
    }

    #[test]
    fn tc_ct_032_09_after_finalization() {
        let mut c = s1();
        c.epochs.get_mut(&1).unwrap().finalize(T_PAY);
        let text = panics(|| {
            settle(&mut c, "scraper", 35 * CREDIT, msg(1), T_PAY + SEC);
        });
        assert!(text.contains("ERR_WRONG_PHASE"), "{text}");
    }

    #[test]
    fn tc_ct_032_10_creditor_cannot_settle() {
        let mut c = s1();
        let text = panics(|| {
            settle(&mut c, "gpu", 35 * CREDIT, msg(1), T_COMMIT + SEC);
        });
        assert!(text.contains("ERR_NOT_DUE"), "{text}");
    }

    #[test]
    fn tc_ct_032_11_no_position() {
        let mut c = s1();
        for sender in ["translator", "outsider"] {
            let text = panics(|| {
                settle(&mut c, sender, 35 * CREDIT, msg(1), T_COMMIT + SEC);
            });
            assert!(text.contains("ERR_NO_POSITION"), "{sender}: {text}");
        }
    }

    #[test]
    fn tc_ct_032_12_double_payment() {
        let mut c = s1();
        settle(&mut c, "scraper", 35 * CREDIT, msg(1), T_COMMIT + SEC);
        let text = panics(|| {
            settle(&mut c, "scraper", 35 * CREDIT, msg(1), T_COMMIT + 2 * SEC);
        });
        assert!(text.contains("ERR_NOT_DUE"), "{text}");
        assert_eq!(c.epochs.get(&1).unwrap().pool, 35 * CREDIT);
    }

    #[test]
    fn tc_ct_032_13_epoch_missing_in_msg() {
        let mut c = s1();
        let text = panics(|| {
            settle(&mut c, "scraper", 35 * CREDIT, json!({"action": "settle"}), T_COMMIT + SEC);
        });
        assert!(text.contains("ERR_BAD_MSG"), "{text}");
    }

    #[test]
    fn tc_ct_032_14_limit_stays_until_finalize() {
        let mut c = s1();
        settle(&mut c, "scraper", 35 * CREDIT, msg(1), T_COMMIT + SEC);
        assert_eq!(c.limit(acc("scraper")), U128(80 * CREDIT));
    }
}

#[cfg(test)]
mod finalize_tests {
    use near_sdk::serde_json::{self, json, Value};
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::{testing_env, PublicKey};

    use super::*;
    use crate::clearing::{Epoch, EpochPhase};
    use crate::member::{Member, MemberStatus};
    use crate::params::Params;

    const CREDIT: u128 = 1_000_000;
    const SEC: u64 = 1_000_000_000;
    const T_COMMIT: u64 = 4_200 * SEC;
    const T_PAY: u64 = T_COMMIT + 1_800 * SEC;

    fn acc(s: &str) -> AccountId {
        format!("{s}.near").parse().unwrap()
    }

    fn at(who: &str, now: u64) {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc(who)).block_timestamp(now).block_height(88).build());
    }

    fn member(c: &mut X207Circle, name: &str, stake: u128, limit: u128) {
        c.members.insert(
            acc(name),
            Member {
                account_id: acc(name),
                pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse::<PublicKey>().unwrap(),
                terms_hash: "a".repeat(64),
                stake,
                limit,
                status: MemberStatus::Active,
                admitted: false,
                joined_at: 0,
                settled_history: 0,
                last_active_epoch: 1,
                exit_requested_epoch: None,
                withdraw_pending: false,
                reactivate_epoch: None,
            },
        );
        c.member_index.push(acc(name));
        c.member_count += 1;
        c.total_stake += stake;
    }

    fn position(net: i128, role: PositionRole, status: PositionStatus) -> Position {
        Position { net, role, status, amount: net.unsigned_abs(), recovered: 0, payment_deadline: T_PAY, updated_at_block: 5 }
    }

    /// A circle whose epoch 1 is in `payment_window` with three members of stake 100 and limit 80.
    fn base() -> X207Circle {
        at("deployer", 0);
        let mut c = X207Circle::with_config(acc("dao"), "agents-main".into(), acc("usdt"), acc("intents"), Params::default(), 0);
        for n in ["scraper", "gpu", "translator"] {
            member(&mut c, n, 100 * CREDIT, 80 * CREDIT);
        }
        c.epochs.get_mut(&1).unwrap().start_clearing(3_600 * SEC);
        c.epochs.get_mut(&1).unwrap().start_payment_window(T_COMMIT);
        c
    }

    /// S1 after `commit_epoch`: scraper owes 35 (`due`), gpu is owed 35, translator is net zero.
    fn s1() -> X207Circle {
        let mut c = base();
        c.put_position(1, acc("scraper"), position(-35 * CREDIT as i128, PositionRole::Debtor, PositionStatus::Due));
        c.put_position(1, acc("gpu"), position(35 * CREDIT as i128, PositionRole::Creditor, PositionStatus::Claimable));
        c.outstanding_debt.insert(acc("scraper"), 35 * CREDIT);
        c
    }

    fn pay(c: &mut X207Circle, who: &str, amount: u128) {
        at("usdt", T_COMMIT + SEC);
        let _ = c.ft_on_transfer(acc(who), U128(amount), json!({"action": "settle", "epoch": 1}).to_string());
    }

    fn fin(c: &mut X207Circle) {
        at("keeper", T_PAY);
        c.finalize_epoch(1);
    }

    fn events() -> Vec<Value> {
        get_logs().iter().filter_map(|l| l.strip_prefix("EVENT_JSON:")).map(|b| serde_json::from_str(b).unwrap()).collect()
    }

    fn named<'a>(all: &'a [Value], name: &str) -> Vec<&'a Value> {
        all.iter().filter(|e| e["event"] == name).collect()
    }

    fn panics(f: impl FnOnce()) -> String {
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).expect_err("expected a panic");
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    fn pos(c: &X207Circle, who: &str) -> Position {
        c.positions.get(&(1, acc(who))).unwrap().clone()
    }

    // TC-CT-033-01
    #[test]
    fn tc_ct_033_01_debtor_defaults() {
        let mut c = s1();
        fin(&mut c);
        let p = pos(&c, "scraper");
        assert_eq!((p.status, p.recovered, p.updated_at_block), (PositionStatus::Defaulted, 35 * CREDIT, 88));
        let m = c.members.get(&acc("scraper")).unwrap();
        assert_eq!((m.stake, m.limit, m.status), (65 * CREDIT, 0, MemberStatus::Suspended));
        assert_eq!(c.outstanding_debt(acc("scraper")), U128(0));
        let e = c.epochs.get(&1).unwrap();
        assert_eq!((e.pool, e.shortfall, e.phase, e.finalized_at), (35 * CREDIT, 0, EpochPhase::Finalized, Some(T_PAY)));
        assert_eq!(c.reserve, 0);
        c.assert_d6(None);
    }

    // TC-CT-033-02, TC-CT-034-06 (order of the log inside one call)
    #[test]
    fn tc_ct_033_02_events() {
        let mut c = s1();
        fin(&mut c);
        let all = events();
        assert_eq!(all.iter().map(|e| e["event"].as_str().unwrap()).collect::<Vec<_>>(), ["member_defaulted", "limits_updated", "epoch_finalized"]);
        let d = &all[0]["data"][0];
        assert_eq!(
            d,
            &json!({"epoch":1,"account_id":"scraper.near","due":"35000000","from_stake":"35000000","from_reserve":"0","shortfall":"0","stake":"65000000","status":"suspended","position_status":"defaulted","outstanding_debt":"0"})
        );
        let f = &all[2]["data"][0];
        assert_eq!((&f["defaulted_count"], &f["total_defaulted"], &f["pool"], &f["shortfall"]), (&json!(1), &json!("35000000"), &json!("35000000"), &json!("0")));
        assert_eq!(f["finalized_at"], json!(T_PAY.to_string()));
    }

    // TC-CT-033-03
    #[test]
    fn tc_ct_033_03_all_paid() {
        let mut c = s1();
        pay(&mut c, "scraper", 35 * CREDIT);
        fin(&mut c);
        let all = events();
        assert!(named(&all, "member_defaulted").is_empty());
        let f = &named(&all, "epoch_finalized")[0]["data"][0];
        assert_eq!((&f["defaulted_count"], &f["pool"]), (&json!(0), &json!("35000000")));
        assert_eq!(pos(&c, "scraper").status, PositionStatus::Paid);
        c.assert_d6(None);
    }

    // TC-CT-033-04
    #[test]
    fn tc_ct_033_04_exactly_at_payment_deadline() {
        let mut c = s1();
        at("keeper", T_PAY - 1);
        assert!(panics(|| c.finalize_epoch(1)).contains("ERR_PAYMENT_WINDOW_OPEN"));
        assert_eq!(c.epochs.get(&1).unwrap().phase, EpochPhase::PaymentWindow);
        assert_eq!(pos(&c, "scraper").status, PositionStatus::Due);
        fin(&mut c);
        assert_eq!(c.epochs.get(&1).unwrap().phase, EpochPhase::Finalized);
    }

    // TC-CT-033-05
    #[test]
    fn tc_ct_033_05_wrong_phase() {
        let mut open = base();
        open.epochs.insert(2, Epoch::open(2, 0, 9_999 * SEC, Params::default()));
        at("keeper", T_PAY);
        assert!(panics(|| open.finalize_epoch(2)).contains("ERR_WRONG_PHASE"));
        let mut clearing = base();
        clearing.epochs.insert(
            2,
            {
                let mut e = Epoch::open(2, 0, 10 * SEC, Params::default());
                e.start_clearing(11 * SEC);
                e
            },
        );
        assert!(panics(|| clearing.finalize_epoch(2)).contains("ERR_WRONG_PHASE"));
        let mut c = s1();
        fin(&mut c);
        assert!(panics(|| c.finalize_epoch(1)).contains("ERR_WRONG_PHASE"));
    }

    // TC-CT-033-06
    #[test]
    fn tc_ct_033_06_unknown_epoch() {
        let mut c = s1();
        at("keeper", T_PAY);
        assert!(panics(|| c.finalize_epoch(99)).contains("ERR_UNKNOWN_EPOCH"));
    }

    // TC-CT-033-07
    #[test]
    fn tc_ct_033_07_no_due_left() {
        let mut c = s1();
        fin(&mut c);
        assert!(c.list_positions(1, 0, 100).iter().all(|p| p.status != PositionStatus::Due));
    }

    // TC-CT-033-08: the status is what `register_*` checks (`active_member`, TC-CT-020-16..18)
    #[test]
    fn tc_ct_033_08_suspended_member_is_not_active() {
        let mut c = s1();
        fin(&mut c);
        assert_eq!(c.members.get(&acc("scraper")).unwrap().status, MemberStatus::Suspended);
        assert_eq!(c.headroom(acc("scraper")), U128(0));
        assert_eq!(c.limit(acc("scraper")), U128(0));
    }

    // TC-CT-033-09
    #[test]
    fn tc_ct_033_09_only_debtor_stake_is_cut() {
        let mut c = s1();
        fin(&mut c);
        for who in ["translator", "gpu"] {
            assert_eq!(c.members.get(&acc(who)).unwrap().stake, 100 * CREDIT, "{who}");
        }
        assert_eq!(c.total_stake, 265 * CREDIT);
    }

    // TC-CT-033-10
    #[test]
    fn tc_ct_033_10_reserve_and_shortfall() {
        let mut c = s1();
        c.members.get_mut(&acc("scraper")).unwrap().stake = 20 * CREDIT;
        c.members.get_mut(&acc("scraper")).unwrap().limit = 16 * CREDIT;
        c.total_stake -= 80 * CREDIT;
        c.reserve = 10 * CREDIT;
        fin(&mut c);
        let all = events();
        let d = &named(&all, "member_defaulted")[0]["data"][0];
        assert_eq!(
            (&d["from_stake"], &d["from_reserve"], &d["shortfall"], &d["stake"]),
            (&json!("20000000"), &json!("10000000"), &json!("5000000"), &json!("0"))
        );
        assert_eq!(&named(&all, "epoch_finalized")[0]["data"][0]["shortfall"], &json!("5000000"));
        let e = c.epochs.get(&1).unwrap();
        assert_eq!((e.pool, e.shortfall, c.reserve), (30 * CREDIT, 5 * CREDIT, 0));
        assert_eq!(pos(&c, "scraper").recovered, 30 * CREDIT);
        c.assert_d6(None);
    }

    // TC-CT-033-11
    #[test]
    fn tc_ct_033_11_two_debtors() {
        let mut c = base();
        c.put_position(1, acc("scraper"), position(-35 * CREDIT as i128, PositionRole::Debtor, PositionStatus::Due));
        c.put_position(1, acc("translator"), position(-10 * CREDIT as i128, PositionRole::Debtor, PositionStatus::Due));
        c.put_position(1, acc("gpu"), position(45 * CREDIT as i128, PositionRole::Creditor, PositionStatus::Claimable));
        c.outstanding_debt.insert(acc("scraper"), 35 * CREDIT);
        c.outstanding_debt.insert(acc("translator"), 10 * CREDIT);
        fin(&mut c);
        assert_eq!(c.members.get(&acc("scraper")).unwrap().stake, 65 * CREDIT);
        assert_eq!(c.members.get(&acc("translator")).unwrap().stake, 90 * CREDIT);
        assert_eq!(c.members.get(&acc("gpu")).unwrap().stake, 100 * CREDIT);
        let all = events();
        assert_eq!(named(&all, "member_defaulted").len(), 2);
        assert_eq!(c.epochs.get(&1).unwrap().pool, 45 * CREDIT);
        let f = &named(&all, "epoch_finalized")[0]["data"][0];
        assert_eq!((&f["defaulted_count"], &f["total_defaulted"]), (&json!(2), &json!("45000000")));
        c.assert_d6(None);
    }

    #[test]
    fn many_defaults_stay_inside_the_log_limit() {
        let mut c = base();
        let long = |i: usize| format!("{i:03}{}", "a".repeat(55));
        for i in 0..63 {
            let name = long(i);
            member(&mut c, &name, 100 * CREDIT, 80 * CREDIT);
            c.put_position(1, acc(&name), position(-(CREDIT as i128), PositionRole::Debtor, PositionStatus::Due));
            c.outstanding_debt.insert(acc(&name), CREDIT);
        }
        fin(&mut c);
        let logs = get_logs();
        assert!(logs.iter().map(String::len).sum::<usize>() <= 16 * 1024, "total log {}", logs.iter().map(String::len).sum::<usize>());
        let all = events();
        assert_eq!(named(&all, "limits_updated").len(), 1);
        let f = &named(&all, "epoch_finalized")[0]["data"][0];
        assert_eq!(f["defaulted_count"], json!(63));
        let shown = named(&all, "member_defaulted").len();
        assert!(shown < 63 && shown > 30, "{shown} debtor events fit");
        assert!(logs.iter().any(|l| l.contains(&format!("{} member_defaulted events left out", 63 - shown))));
        assert!(c.list_positions(1, 0, 100).iter().all(|p| p.status != PositionStatus::Due));
        c.assert_d6(None);
    }

    // TC-CT-033-12
    #[test]
    fn tc_ct_033_12_epoch_without_positions() {
        let mut c = base();
        fin(&mut c);
        let f = &named(&events(), "epoch_finalized")[0]["data"][0].clone();
        assert_eq!((&f["defaulted_count"], &f["pool"]), (&json!(0), &json!("0")));
    }

    #[test]
    fn finalize_freezes_pool_and_claimable_for_claim() {
        let mut c = s1();
        fin(&mut c);
        let e = c.epochs.get(&1).unwrap();
        assert_eq!((e.pool_final, e.claimable_final), (35 * CREDIT, 35 * CREDIT));
        assert_eq!(pos(&c, "gpu").status, PositionStatus::Claimable);
    }

    // TC-CT-034-01, TC-CT-034-02, TC-CT-034-05
    #[test]
    fn tc_ct_034_01_02_05_cash_settlement_raises_limit() {
        let mut c = s1();
        c.members.get_mut(&acc("scraper")).unwrap().settled_history = 0;
        pay(&mut c, "scraper", 35 * CREDIT);
        fin(&mut c);
        assert_eq!(c.limit(acc("scraper")), U128(83_500_000));
        assert_eq!(c.limit(acc("translator")), U128(80 * CREDIT));
        assert_eq!(c.limit(acc("gpu")), U128(80 * CREDIT));
        let all = events();
        let changes = &named(&all, "limits_updated")[0]["data"][0]["changes"];
        assert_eq!(changes, &json!([{"account_id":"scraper.near","old_limit":"80000000","new_limit":"83500000","status":"active"}]));
    }

    // TC-CT-034-03
    #[test]
    fn tc_ct_034_03_defaulted_member_not_in_changes() {
        let mut c = s1();
        fin(&mut c);
        let all = events();
        let limits = named(&all, "limits_updated");
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0]["data"][0]["changes"], json!([]));
        assert_eq!((limits[0]["data"][0]["chunk"].as_u64(), limits[0]["data"][0]["chunks"].as_u64()), (Some(0), Some(1)));
    }

    // TC-CT-034-04
    #[test]
    fn tc_ct_034_04_limit_never_above_stake() {
        for paid in [true, false] {
            let mut c = s1();
            c.members.get_mut(&acc("scraper")).unwrap().settled_history = 500 * CREDIT;
            if paid {
                pay(&mut c, "scraper", 35 * CREDIT);
            }
            fin(&mut c);
            for id in c.member_index.iter() {
                let m = c.members.get(id).unwrap();
                assert!(m.limit <= m.stake, "{id}");
            }
        }
    }

    // TC-CT-034-07
    #[test]
    fn tc_ct_034_07_uses_snapshot_params_of_the_epoch() {
        let mut c = s1();
        c.epochs.get_mut(&1).unwrap().params.limit_growth = 2_000;
        let mut next = Params::default();
        next.limit_growth = 0;
        c.epochs.insert(2, Epoch::open(2, T_COMMIT, 9_999 * SEC, next));
        c.current_epoch = 2;
        pay(&mut c, "scraper", 35 * CREDIT);
        fin(&mut c);
        // 100 * 80% + 35 * 20% = 87
        assert_eq!(c.limit(acc("scraper")), U128(87 * CREDIT));
    }

    // TC-CT-034-08: with the default `limit_decay` 0 an idle member keeps its limit
    #[test]
    fn tc_ct_034_08_default_limit_decay_changes_nothing() {
        assert_eq!(Params::default().limit_decay, 0);
        let mut c = s1();
        c.members.get_mut(&acc("translator")).unwrap().last_active_epoch = 0;
        fin(&mut c);
        assert_eq!(c.limit(acc("translator")), U128(80 * CREDIT));
    }

    // CT-053: `limit_decay` 1000 bps and a member idle since epoch 0 loses 10 % per idle epoch; active members do not
    #[test]
    fn ct_053_limit_decay_lowers_the_limit_of_an_idle_member() {
        let mut c = s1();
        c.epochs.get_mut(&1).unwrap().params.limit_decay = 1_000;
        c.members.get_mut(&acc("translator")).unwrap().last_active_epoch = 0;
        fin(&mut c);
        // epoch 1, last active 0: one idle epoch, 80 * 0.9
        assert_eq!(c.limit(acc("translator")), U128(72 * CREDIT));
        assert_eq!(c.limit(acc("gpu")), U128(80 * CREDIT), "gpu took part in epoch 1");
        let all = events();
        let changes = named(&all, "limits_updated");
        let list = changes[0]["data"][0]["changes"].as_array().unwrap();
        assert!(list.iter().any(|ch| ch["account_id"] == "translator.near" && ch["new_limit"] == "72000000"));
    }

    // TC-CT-034-09
    #[test]
    fn tc_ct_034_09_limits_updated_is_chunked() {
        let mut c = base();
        for i in 0..64 {
            member(&mut c, &format!("m{i}"), 100 * CREDIT, 50 * CREDIT);
        }
        fin(&mut c);
        let all = events();
        let limits = named(&all, "limits_updated");
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[0]["data"][0]["changes"].as_array().unwrap().len(), 50);
        assert_eq!(limits[1]["data"][0]["changes"].as_array().unwrap().len(), 14);
        assert_eq!((limits[1]["data"][0]["chunk"].as_u64(), limits[1]["data"][0]["chunks"].as_u64()), (Some(1), Some(2)));
    }
}

#[cfg(test)]
mod claim_tests {
    use near_sdk::serde_json::{self, json, Value};
    use near_sdk::mock::MockAction;
    use near_sdk::test_utils::{get_created_receipts, get_logs, VMContextBuilder};
    use near_sdk::{test_vm_config, testing_env, Gas, NearToken, PromiseResult, PublicKey, RuntimeFeesConfig};

    use super::*;
    use crate::clearing::Epoch;
    use crate::member::{Member, MemberStatus};
    use crate::params::Params;

    const CREDIT: u128 = 1_000_000;

    fn acc(s: &str) -> AccountId {
        format!("{s}.near").parse().unwrap()
    }

    fn ctx(caller: &str, deposit: u128, gas_tgas: u64) -> VMContextBuilder {
        let mut b = VMContextBuilder::new();
        b.predecessor_account_id(acc(caller))
            .current_account_id(acc("circle"))
            .attached_deposit(NearToken::from_yoctonear(deposit))
            .prepaid_gas(Gas::from_tgas(gas_tgas))
            .block_height(300);
        b
    }

    fn call_as(caller: &str) {
        testing_env!(ctx(caller, 1, 300).build());
    }

    fn member(c: &mut X207Circle, name: &str) {
        c.members.insert(
            acc(name),
            Member {
                account_id: acc(name),
                pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse::<PublicKey>().unwrap(),
                terms_hash: "a".repeat(64),
                stake: 100 * CREDIT,
                limit: 80 * CREDIT,
                status: MemberStatus::Active,
                admitted: false,
                joined_at: 0,
                settled_history: 0,
                last_active_epoch: 1,
                exit_requested_epoch: None,
                withdraw_pending: false,
                reactivate_epoch: None,
            },
        );
        c.member_index.push(acc(name));
        c.member_count += 1;
        c.total_stake += 100 * CREDIT;
    }

    fn creditor(amount: u128) -> Position {
        Position { net: amount as i128, role: PositionRole::Creditor, status: PositionStatus::Claimable, amount, recovered: 0, payment_deadline: 9, updated_at_block: 5 }
    }

    /// Epoch 1 `finalized` with pool 35 and gpu owed 35 (S1.fin without the clearing steps).
    fn s1_fin() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(0).build());
        let mut c = X207Circle::with_config(acc("dao"), "agents-main".into(), acc("usdt"), acc("intents"), Params::default(), 0);
        for n in ["scraper", "gpu", "translator"] {
            member(&mut c, n);
        }
        let e = c.epochs.get_mut(&1).unwrap();
        e.start_clearing(3_600_000_000_000);
        e.start_payment_window(4_200_000_000_000);
        e.finalize(6_000_000_000_000);
        c.put_position(1, acc("gpu"), creditor(35 * CREDIT));
        c.put_position(
            1,
            acc("scraper"),
            Position { net: -35 * CREDIT as i128, role: PositionRole::Debtor, status: PositionStatus::Paid, amount: 35 * CREDIT, recovered: 0, payment_deadline: 9, updated_at_block: 5 },
        );
        c.d6_deposit_epoch_pool(1, 35 * CREDIT);
        let e = c.epochs.get_mut(&1).unwrap();
        e.pool_final = 35 * CREDIT;
        e.claimable_final = 35 * CREDIT;
        c
    }

    fn panics(f: impl FnOnce()) -> String {
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).expect_err("expected a panic");
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    fn status(c: &X207Circle, who: &str) -> (PositionStatus, u128) {
        let p = c.positions.get(&(1, acc(who))).unwrap();
        (p.status, p.amount)
    }

    /// `(receiver, method, args)` of every function call the contract created.
    fn calls() -> Vec<(String, String, Value)> {
        get_created_receipts()
            .into_iter()
            .flat_map(|r| {
                let receiver = r.receiver_id.to_string();
                r.actions.into_iter().filter_map(move |a| match a {
                    MockAction::FunctionCallWeight { method_name, args, .. } => Some((
                        receiver.clone(),
                        String::from_utf8(method_name).unwrap(),
                        serde_json::from_slice(&args).unwrap(),
                    )),
                    _ => None,
                })
            })
            .collect()
    }

    fn resolve(c: &mut X207Circle, result: PromiseResult, amount: u128, destination: ClaimDestination) -> U128 {
        testing_env!(ctx("circle", 0, 300).build(), test_vm_config(), RuntimeFeesConfig::test(), Default::default(), vec![result]);
        c.on_claim_resolved(1, acc("gpu"), U128(amount), destination)
    }

    fn events() -> Vec<Value> {
        get_logs().iter().filter_map(|l| l.strip_prefix("EVENT_JSON:")).map(|b| serde_json::from_str(b).unwrap()).collect()
    }

    // TC-CT-035-01 (unit side): state moves before the transfer, the transfer is ft_transfer to the caller
    #[test]
    fn tc_ct_035_01_claim_to_near_starts_an_ft_transfer() {
        let mut c = s1_fin();
        call_as("gpu");
        let _ = c.claim(1, ClaimDestination::Near);
        assert_eq!(status(&c, "gpu"), (PositionStatus::Claimed, 35 * CREDIT));
        let (pool, in_flight) = (c.epochs.get(&1).unwrap().pool, c.in_flight);
        assert_eq!((pool, in_flight), (0, 35 * CREDIT));
        let calls = calls();
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[0].0.as_str(), calls[0].1.as_str()), ("usdt.near", "ft_transfer"));
        assert_eq!((&calls[0].2["receiver_id"], &calls[0].2["amount"]), (&json!("gpu.near"), &json!("35000000")));
        assert_eq!((calls[1].0.as_str(), calls[1].1.as_str()), ("circle.near", "on_claim_resolved"));
        assert_eq!(calls[1].2, json!({"epoch": 1, "account_id": "gpu.near", "amount": "35000000", "destination": "near"}));
        c.assert_d6(None);
    }

    // TC-CT-035-03 (unit side): intents goes through ft_transfer_call with msg = the creditor
    #[test]
    fn tc_ct_035_03_claim_to_intents_sends_account_as_msg() {
        let mut c = s1_fin();
        call_as("gpu");
        let _ = c.claim(1, ClaimDestination::Intents);
        let calls = calls();
        assert_eq!((calls[0].0.as_str(), calls[0].1.as_str()), ("usdt.near", "ft_transfer_call"));
        assert_eq!(
            (&calls[0].2["receiver_id"], &calls[0].2["amount"], &calls[0].2["msg"]),
            (&json!("intents.near"), &json!("35000000"), &json!("gpu.near"))
        );
        assert_eq!(calls[1].2["destination"], "intents");
    }

    // TC-CT-035-04
    #[test]
    fn tc_ct_035_04_before_finalization() {
        let mut c = s1_fin();
        c.epochs.get_mut(&1).unwrap().phase = EpochPhase::PaymentWindow;
        call_as("gpu");
        assert!(panics(|| drop(c.claim(1, ClaimDestination::Near))).contains("ERR_NOT_FINALIZED"));
        assert_eq!(status(&c, "gpu").0, PositionStatus::Claimable);
    }

    // TC-CT-035-05
    #[test]
    fn tc_ct_035_05_second_claim() {
        let mut c = s1_fin();
        call_as("gpu");
        let _ = c.claim(1, ClaimDestination::Near);
        call_as("gpu");
        assert!(panics(|| drop(c.claim(1, ClaimDestination::Near))).contains("ERR_NOT_CLAIMABLE"));
    }

    // TC-CT-035-06
    #[test]
    fn tc_ct_035_06_debtor_cannot_claim() {
        let mut c = s1_fin();
        call_as("scraper");
        assert!(panics(|| drop(c.claim(1, ClaimDestination::Near))).contains("ERR_NOT_CREDITOR"));
    }

    // TC-CT-035-07
    #[test]
    fn tc_ct_035_07_no_position() {
        let mut c = s1_fin();
        for who in ["translator", "outsider"] {
            call_as(who);
            assert!(panics(|| drop(c.claim(1, ClaimDestination::Near))).contains("ERR_NO_POSITION"), "{who}");
        }
    }

    // TC-CT-035-08
    #[test]
    fn tc_ct_035_08_needs_exactly_one_yocto() {
        let mut c = s1_fin();
        for deposit in [0, 2] {
            testing_env!(ctx("gpu", deposit, 300).build());
            let text = panics(|| drop(c.claim(1, ClaimDestination::Near)));
            assert!(text.contains("Requires attached deposit of exactly 1 yoctoNEAR"), "{deposit}: {text}");
        }
        assert_eq!(status(&c, "gpu").0, PositionStatus::Claimable);
    }

    // TC-CT-035-09
    #[test]
    fn tc_ct_035_09_gas_too_low() {
        let mut c = s1_fin();
        testing_env!(ctx("gpu", 1, 99).build());
        assert!(panics(|| drop(c.claim(1, ClaimDestination::Near))).contains("ERR_GAS_TOO_LOW"));
        assert_eq!(status(&c, "gpu").0, PositionStatus::Claimable);
        testing_env!(ctx("gpu", 1, 100).build());
        let _ = c.claim(1, ClaimDestination::Near);
    }

    // TC-CT-035-15
    #[test]
    fn tc_ct_035_15_pool_too_small_is_an_invariant_guard() {
        let mut c = s1_fin();
        c.d6_pool_to_in_flight(1, 15 * CREDIT);
        call_as("gpu");
        assert!(panics(|| drop(c.claim(1, ClaimDestination::Near))).contains("ERR_POOL_INSUFFICIENT"));
        assert_eq!(status(&c, "gpu").0, PositionStatus::Claimable);
    }

    // TC-CT-035-16
    #[test]
    fn tc_ct_035_16_pro_rata_when_shortfall() {
        let mut c = s1_fin();
        let e = c.epochs.get_mut(&1).unwrap();
        e.shortfall = 5 * CREDIT;
        e.pool_final = 30 * CREDIT;
        c.d6_pool_to_in_flight(1, 5 * CREDIT);
        c.d6_in_flight_out(5 * CREDIT);
        call_as("gpu");
        let _ = c.claim(1, ClaimDestination::Near);
        assert_eq!(calls()[0].2["amount"], json!("30000000"));
        assert_eq!(c.epochs.get(&1).unwrap().pool, 0);
    }

    // TC-CT-035-19
    #[test]
    fn tc_ct_035_19_unknown_epoch() {
        let mut c = s1_fin();
        call_as("gpu");
        assert!(panics(|| drop(c.claim(99, ClaimDestination::Near))).contains("ERR_UNKNOWN_EPOCH"));
    }

    // TC-CT-035-20
    #[test]
    fn tc_ct_035_20_ratio_does_not_depend_on_claim_order() {
        for order in [["a", "b"], ["b", "a"]] {
            let mut c = s1_fin();
            c.positions.remove(&(1, acc("gpu")));
            c.put_position(1, acc("a"), creditor(21 * CREDIT));
            c.put_position(1, acc("b"), creditor(14 * CREDIT));
            let e = c.epochs.get_mut(&1).unwrap();
            e.shortfall = 5 * CREDIT;
            e.pool_final = 30 * CREDIT;
            c.d6_pool_to_in_flight(1, 5 * CREDIT);
            c.d6_in_flight_out(5 * CREDIT);
            let mut paid = std::collections::BTreeMap::new();
            for who in order {
                call_as(who);
                let before = c.in_flight;
                let _ = c.claim(1, ClaimDestination::Near);
                paid.insert(who, c.in_flight - before);
            }
            assert_eq!((paid["a"], paid["b"]), (18 * CREDIT, 12 * CREDIT), "order {order:?}");
        }
    }

    // TC-CT-035-18
    #[test]
    fn tc_ct_035_18_claim_has_no_destination_account_argument() {
        let abi: Value = serde_json::from_str(include_str!("../../abi.json")).unwrap();
        let claim = abi["body"]["functions"].as_array().unwrap().iter().find(|f| f["name"] == "claim").unwrap();
        let args = claim["params"]["args"].as_array().unwrap();
        assert_eq!(args.iter().map(|a| a["name"].as_str().unwrap()).collect::<Vec<_>>(), ["epoch", "destination"]);
        assert_eq!(abi["body"]["functions"].as_array().unwrap().iter().filter(|f| f["name"] == "claim").count(), 1);
        assert_eq!(args[1]["type_schema"]["$ref"], json!("#/definitions/ClaimDestination"));
    }

    fn after_claim() -> X207Circle {
        let mut c = s1_fin();
        call_as("gpu");
        let _ = c.claim(1, ClaimDestination::Near);
        c
    }

    // TC-CT-035-01 callback success
    #[test]
    fn callback_success_near() {
        let mut c = after_claim();
        assert_eq!(resolve(&mut c, PromiseResult::Successful(vec![]), 35 * CREDIT, ClaimDestination::Near), U128(35 * CREDIT));
        assert_eq!((status(&c, "gpu"), c.in_flight, c.epochs.get(&1).unwrap().pool), ((PositionStatus::Claimed, 35 * CREDIT), 0, 0));
        let ev = events();
        assert_eq!(ev[0]["event"], "residual_claimed");
        assert_eq!(ev[0]["data"][0], json!({"epoch":1,"account_id":"gpu.near","amount":"35000000","destination":"near","status":"claimed"}));
        c.assert_d6(None);
    }

    // TC-CT-035-10 callback failure
    #[test]
    fn callback_failure_restores_status_and_pool() {
        let mut c = after_claim();
        assert_eq!(resolve(&mut c, PromiseResult::Failed, 35 * CREDIT, ClaimDestination::Near), U128(0));
        assert_eq!((status(&c, "gpu"), c.in_flight, c.epochs.get(&1).unwrap().pool), ((PositionStatus::Claimable, 35 * CREDIT), 0, 35 * CREDIT));
        let ev = events();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["data"][0], json!({"epoch":1,"account_id":"gpu.near","amount":"35000000","destination":"near","status":"claimable"}));
        assert_eq!(ev[0]["event"], "residual_claim_failed");
        // a claim after the failure works again
        call_as("gpu");
        let _ = c.claim(1, ClaimDestination::Near);
        assert_eq!(status(&c, "gpu").0, PositionStatus::Claimed);
    }

    // TC-CT-035-11
    #[test]
    fn callback_intents_partial_use() {
        let mut c = after_claim();
        let used = PromiseResult::Successful(json!("25000000").to_string().into_bytes());
        assert_eq!(resolve(&mut c, used, 35 * CREDIT, ClaimDestination::Intents), U128(25 * CREDIT));
        assert_eq!((status(&c, "gpu"), c.in_flight, c.epochs.get(&1).unwrap().pool), ((PositionStatus::Claimable, 10 * CREDIT), 0, 10 * CREDIT));
        let ev = events();
        assert_eq!(ev.iter().map(|e| e["event"].as_str().unwrap()).collect::<Vec<_>>(), ["residual_claimed", "residual_claim_failed"]);
        assert_eq!((&ev[0]["data"][0]["amount"], &ev[1]["data"][0]["amount"]), (&json!("25000000"), &json!("10000000")));
        assert_eq!(ev[0]["data"][0]["destination"], "intents");
        c.assert_d6(None);
    }

    // TC-CT-035-12
    #[test]
    fn callback_intents_returns_everything_or_fails() {
        for result in [PromiseResult::Successful(json!("0").to_string().into_bytes()), PromiseResult::Failed, PromiseResult::Successful(b"garbage".to_vec())] {
            let mut c = after_claim();
            assert_eq!(resolve(&mut c, result, 35 * CREDIT, ClaimDestination::Intents), U128(0));
            assert_eq!((status(&c, "gpu"), c.epochs.get(&1).unwrap().pool, c.in_flight), ((PositionStatus::Claimable, 35 * CREDIT), 35 * CREDIT, 0));
            assert_eq!(events()[0]["event"], "residual_claim_failed");
        }
    }

    #[test]
    fn callback_intents_full_use_and_overreport() {
        for reported in ["35000000", "99000000"] {
            let mut c = after_claim();
            let result = PromiseResult::Successful(json!(reported).to_string().into_bytes());
            assert_eq!(resolve(&mut c, result, 35 * CREDIT, ClaimDestination::Intents), U128(35 * CREDIT), "{reported}");
            assert_eq!((status(&c, "gpu").0, c.in_flight), (PositionStatus::Claimed, 0));
        }
    }

    // TC-CT-035-14: no panic on a position that is not `claimed` (a second callback)
    #[test]
    fn callback_does_not_panic_on_unexpected_state() {
        let mut c = s1_fin();
        assert_eq!(resolve(&mut c, PromiseResult::Failed, 35 * CREDIT, ClaimDestination::Near), U128(0));
        assert_eq!((status(&c, "gpu").0, c.epochs.get(&1).unwrap().pool), (PositionStatus::Claimable, 35 * CREDIT));
    }

    #[test]
    fn epoch_struct_unused_import_guard() {
        let _ = Epoch::open(2, 0, 1, Params::default());
    }
}
