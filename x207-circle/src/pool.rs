//! D6 accounting (CT-010): stake per member, reserve, settlement pool per epoch, solver fees, and
//! USDT in flight. Invariant I-04 (spec 10):
//! `total_stake + reserve + Σ epoch pools + Σ solver fees + in_flight = USDT balance of the contract`,
//! checked once all receipts of a transaction are done.
//!
//! Every D6 mutation in other modules goes through the helpers below. Moves between components keep
//! the sum; only `deposit_*` and `in_flight_out` change it, matching USDT that enters or leaves.

use near_sdk::json_types::U128;
use near_sdk::{env, near};

use crate::state::{X207Circle, X207CircleExt};
use crate::types::EpochId;

/// View `get_pool` (spec section 5). `accounted` is the sum of the five components.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolView {
    pub total_stake: U128,
    pub reserve: U128,
    pub epoch_pools: U128,
    pub solver_fees: U128,
    pub in_flight: U128,
    pub accounted: U128,
}

fn add(a: u128, b: u128) -> u128 {
    a.checked_add(b).unwrap_or_else(|| env::panic_str("ERR_D6_ACCOUNTING: overflow"))
}

fn sub(a: u128, b: u128) -> u128 {
    a.checked_sub(b).unwrap_or_else(|| env::panic_str("ERR_D6_ACCOUNTING: underflow"))
}

impl X207Circle {
    pub fn pool_view(&self) -> PoolView {
        PoolView {
            total_stake: U128(self.total_stake),
            reserve: U128(self.reserve),
            epoch_pools: U128(self.epoch_pools),
            solver_fees: U128(self.solver_fees_total),
            in_flight: U128(self.in_flight),
            accounted: U128(self.accounted()),
        }
    }

    pub fn accounted(&self) -> u128 {
        [self.reserve, self.epoch_pools, self.solver_fees_total, self.in_flight]
            .into_iter()
            .fold(self.total_stake, add)
    }

    fn epoch_pool(&self, epoch: EpochId) -> u128 {
        self.epochs.get(&epoch).unwrap_or_else(|| env::panic_str("ERR_UNKNOWN_EPOCH")).pool
    }

    fn set_epoch_pool(&mut self, epoch: EpochId, pool: u128) {
        self.epochs.get_mut(&epoch).unwrap_or_else(|| env::panic_str("ERR_UNKNOWN_EPOCH")).pool = pool;
    }

    fn pool_up(&mut self, epoch: EpochId, amount: u128) {
        let pool = add(self.epoch_pool(epoch), amount);
        self.epoch_pools = add(self.epoch_pools, amount);
        self.set_epoch_pool(epoch, pool);
    }

    fn pool_down(&mut self, epoch: EpochId, amount: u128) {
        let pool = sub(self.epoch_pool(epoch), amount);
        self.epoch_pools = sub(self.epoch_pools, amount);
        self.set_epoch_pool(epoch, pool);
    }

    /// USDT enters as stake. The caller updates `Member.stake`.
    pub fn d6_deposit_stake(&mut self, amount: u128) {
        self.total_stake = add(self.total_stake, amount);
    }

    /// USDT enters as a residual payment for `epoch`.
    pub fn d6_deposit_epoch_pool(&mut self, epoch: EpochId, amount: u128) {
        self.pool_up(epoch, amount);
    }

    /// Default: stake of the debtor moves to the epoch pool. The caller updates `Member.stake`.
    pub fn d6_stake_to_pool(&mut self, epoch: EpochId, amount: u128) {
        self.total_stake = sub(self.total_stake, amount);
        self.pool_up(epoch, amount);
    }

    /// Default: reserve covers what stake could not.
    pub fn d6_reserve_to_pool(&mut self, epoch: EpochId, amount: u128) {
        self.reserve = sub(self.reserve, amount);
        self.pool_up(epoch, amount);
    }

    /// `claim`: pool of `epoch` goes in flight until the callback.
    pub fn d6_pool_to_in_flight(&mut self, epoch: EpochId, amount: u128) {
        self.pool_down(epoch, amount);
        self.in_flight = add(self.in_flight, amount);
    }

    /// Failed `claim` transfer: the amount returns to the pool of `epoch`.
    pub fn d6_in_flight_to_pool(&mut self, epoch: EpochId, amount: u128) {
        self.in_flight = sub(self.in_flight, amount);
        self.pool_up(epoch, amount);
    }

    /// `withdraw_stake`: stake goes in flight until the callback. The caller updates `Member.stake`.
    pub fn d6_stake_to_in_flight(&mut self, amount: u128) {
        self.total_stake = sub(self.total_stake, amount);
        self.in_flight = add(self.in_flight, amount);
    }

    /// Failed `withdraw_stake` transfer: the stake returns. The caller updates `Member.stake`.
    pub fn d6_in_flight_to_stake(&mut self, amount: u128) {
        self.in_flight = sub(self.in_flight, amount);
        self.total_stake = add(self.total_stake, amount);
    }

    /// Transfer succeeded: USDT left the contract.
    pub fn d6_in_flight_out(&mut self, amount: u128) {
        self.in_flight = sub(self.in_flight, amount);
    }

    /// Checks I-04 against the real USDT balance (when given) and the stored sums against the
    /// records: `total_stake = Σ Member.stake` and `epoch_pools = Σ Epoch.pool`.
    pub fn assert_d6(&self, usdt_balance: Option<u128>) {
        let stake: u128 = self.member_index.iter().map(|id| self.members.get(id).expect("indexed member").stake).sum();
        assert_eq!(self.total_stake, stake, "total_stake != Σ member.stake");
        let pools: u128 = (1..=self.current_epoch).filter_map(|e| self.epochs.get(&e)).map(|e| e.pool).sum();
        assert_eq!(self.epoch_pools, pools, "epoch_pools != Σ epoch.pool");
        if let Some(balance) = usdt_balance {
            assert_eq!(self.accounted(), balance, "accounted != USDT balance");
        }
    }
}

#[near]
impl X207Circle {
    pub fn get_pool(&self) -> PoolView {
        self.pool_view()
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{testing_env, AccountId, PublicKey};

    use super::*;
    use crate::member::{Member, MemberStatus};
    use crate::params::Params;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn circle() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        X207Circle::with_config(
            acc("dao.near"),
            "agents-main".to_string(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            1_000,
        )
    }

    fn add_member(c: &mut X207Circle, id: &str, stake: u128) {
        let member = Member {
            account_id: acc(id),
            pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse::<PublicKey>().unwrap(),
            terms_hash: "a".repeat(64),
            stake,
            limit: stake * 8 / 10,
            status: MemberStatus::Active,
            admitted: false,
            joined_at: 1_000,
            settled_history: 0,
            last_active_epoch: 1,
            exit_requested_epoch: None,
            withdraw_pending: false,
            reactivate_epoch: None,
        };
        c.members.insert(acc(id), member);
        c.member_index.push(acc(id));
        c.member_count += 1;
        c.d6_deposit_stake(stake);
    }

    /// Spec example `get_pool` after one settle: stake 300, epoch pool 35.
    fn funded() -> X207Circle {
        let mut c = circle();
        for id in ["translator.near", "scraper.near", "gpu.near"] {
            add_member(&mut c, id, 100);
        }
        c
    }

    // TC-CT-010-01
    #[test]
    fn pool_is_zero_after_init() {
        let c = circle();
        let view = c.get_pool();
        assert_eq!(view, PoolView { total_stake: U128(0), reserve: U128(0), epoch_pools: U128(0), solver_fees: U128(0), in_flight: U128(0), accounted: U128(0) });
        c.assert_d6(Some(0));
    }

    #[test]
    fn view_json_matches_spec_example() {
        let mut c = funded();
        c.d6_deposit_epoch_pool(1, 35);
        let json = near_sdk::serde_json::to_value(c.get_pool()).unwrap();
        assert_eq!(
            json,
            near_sdk::serde_json::json!({"total_stake":"300","reserve":"0","epoch_pools":"35","solver_fees":"0","in_flight":"0","accounted":"335"})
        );
    }

    // TC-CT-010-02
    #[test]
    fn stake_raises_total_and_accounted() {
        let c = funded();
        assert_eq!((c.get_pool().total_stake.0, c.get_pool().accounted.0), (300, 300));
        c.assert_d6(Some(300));
    }

    // TC-CT-010-03
    #[test]
    fn settle_raises_epoch_pool() {
        let mut c = funded();
        c.d6_deposit_epoch_pool(1, 35);
        assert_eq!((c.get_pool().epoch_pools.0, c.get_pool().accounted.0), (35, 335));
        assert_eq!(c.epochs.get(&1).unwrap().pool, 35);
        c.assert_d6(Some(335));
    }

    // TC-CT-010-04
    #[test]
    fn default_moves_stake_to_pool() {
        let mut c = funded();
        c.members.get_mut(&acc("scraper.near")).unwrap().stake -= 35;
        c.d6_stake_to_pool(1, 35);
        let view = c.get_pool();
        assert_eq!((view.total_stake.0, view.epoch_pools.0, view.accounted.0), (265, 35, 300));
        c.assert_d6(Some(300));
    }

    #[test]
    fn reserve_covers_remainder() {
        let mut c = funded();
        c.reserve = 10;
        c.d6_reserve_to_pool(1, 10);
        assert_eq!((c.reserve, c.epoch_pools, c.accounted()), (0, 10, 310));
    }

    // TC-CT-010-05 (accounting part; the real claim flow is CT-035)
    #[test]
    fn claim_completes_and_leaves_contract() {
        let mut c = funded();
        c.d6_deposit_epoch_pool(1, 35);
        c.d6_pool_to_in_flight(1, 35);
        c.d6_in_flight_out(35);
        let view = c.get_pool();
        assert_eq!((view.epoch_pools.0, view.in_flight.0, view.accounted.0), (0, 0, 300));
        c.assert_d6(Some(300));
    }

    // TC-CT-010-08
    #[test]
    fn claim_in_flight_keeps_component_sum() {
        let mut c = funded();
        c.d6_deposit_epoch_pool(1, 35);
        let before = c.accounted();
        c.d6_pool_to_in_flight(1, 35);
        let view = c.get_pool();
        assert_eq!((view.in_flight.0, view.epoch_pools.0), (35, 0));
        assert_eq!(c.accounted(), before);
        assert_eq!(c.epochs.get(&1).unwrap().pool, 0);
    }

    #[test]
    fn failed_claim_restores_pool() {
        let mut c = funded();
        c.d6_deposit_epoch_pool(1, 35);
        c.d6_pool_to_in_flight(1, 35);
        c.d6_in_flight_to_pool(1, 35);
        assert_eq!((c.epoch_pools, c.in_flight, c.accounted()), (35, 0, 335));
        c.assert_d6(Some(335));
    }

    #[test]
    fn withdraw_round_trip_keeps_sum() {
        let mut c = funded();
        c.members.get_mut(&acc("gpu.near")).unwrap().stake = 0;
        c.d6_stake_to_in_flight(100);
        assert_eq!((c.total_stake, c.in_flight, c.accounted()), (200, 100, 300));
        c.members.get_mut(&acc("gpu.near")).unwrap().stake = 100;
        c.d6_in_flight_to_stake(100);
        assert_eq!((c.total_stake, c.in_flight, c.accounted()), (300, 0, 300));
        c.assert_d6(Some(300));
    }

    #[test]
    fn pools_are_tracked_per_epoch() {
        let mut c = funded();
        c.epochs.insert(2, crate::clearing::Epoch::open(2, 2_000, 9_000, Params::default()));
        c.current_epoch = 2;
        c.d6_deposit_epoch_pool(1, 5);
        c.d6_deposit_epoch_pool(2, 7);
        assert_eq!((c.epochs.get(&1).unwrap().pool, c.epochs.get(&2).unwrap().pool, c.epoch_pools), (5, 7, 12));
        c.assert_d6(Some(312));
    }

    #[test]
    #[should_panic(expected = "ERR_D6_ACCOUNTING: underflow")]
    fn underflow_panics() {
        let mut c = funded();
        c.d6_pool_to_in_flight(1, 1);
    }

    #[test]
    #[should_panic(expected = "ERR_D6_ACCOUNTING: overflow")]
    fn overflow_panics() {
        let mut c = circle();
        c.d6_deposit_stake(u128::MAX);
        c.d6_deposit_epoch_pool(1, 1);
        let _ = c.accounted();
    }

    #[test]
    #[should_panic(expected = "accounted != USDT balance")]
    fn assert_d6_detects_balance_drift() {
        funded().assert_d6(Some(299));
    }

    #[test]
    #[should_panic(expected = "total_stake != Σ member.stake")]
    fn assert_d6_detects_stake_drift() {
        let mut c = funded();
        c.members.get_mut(&acc("gpu.near")).unwrap().stake += 1;
        c.assert_d6(None);
    }
}
