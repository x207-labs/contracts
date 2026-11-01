//! F3 clearing: epoch state machine, D4 proposals, `close_epoch`, `submit_clearing`, `commit_epoch`.

use near_sdk::json_types::{U128, U64};
use near_sdk::store::Vector;
use near_sdk::{env, near, require, AccountId};

use crate::params::Params;
use crate::state::{StorageKey, X207Circle, X207CircleExt};
use crate::types::{ClearingEdge, EpochId};

/// Stored phases (spec section 8). `cutoff` and `commit` are time-derived and never stored.
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EpochPhase {
    Open,
    Clearing,
    PaymentWindow,
    Finalized,
}

/// Phase as reported by `get_epoch`: the stored phase plus the time-derived `cutoff` and `commit`.
#[near(serializers = [json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EpochPhaseView {
    Open,
    Cutoff,
    Clearing,
    Commit,
    PaymentWindow,
    Finalized,
}

const NS_PER_SEC: u64 = 1_000_000_000;

/// `cutoff_at` of epoch N+1 (spec section 8, K-12): `prev_cutoff_at + L` if that is strictly
/// after `now`, otherwise `now + L`. `L` is `epoch_length` (seconds) of epoch N+1.
pub fn next_cutoff_at(prev_cutoff_at: u64, now: u64, epoch_length: u64) -> u64 {
    let length_ns = epoch_length * NS_PER_SEC;
    let candidate = prev_cutoff_at + length_ns;
    if candidate > now {
        candidate
    } else {
        now + length_ns
    }
}

/// Epoch record (spec 9.1, prefix `c`): phase, timestamps (ns), params snapshot, totals, pool.
/// The three per-epoch vectors (prefixes `E`, `T`, `S`) live inside the record, because a
/// `store::Vector` keeps its length in the value that contains it.
#[near(serializers = [borsh])]
pub struct Epoch {
    pub epoch: EpochId,
    pub phase: EpochPhase,
    pub opened_at: u64,
    pub cutoff_at: u64,
    pub closed_at: Option<u64>,
    pub proposal_deadline: Option<u64>,
    pub committed_at: Option<u64>,
    pub payment_deadline: Option<u64>,
    pub finalized_at: Option<u64>,
    pub params: Params,
    pub edge_count: u32,
    pub participant_count: u32,
    pub total_obligation: u128,
    pub total_cleared: u128,
    pub pool: u128,
    pub shortfall: u128,
    /// D2 snapshot: edge index -> `(payer, payee)`.
    pub edges: Vector<(AccountId, AccountId)>,
    /// D2 aggregate: participant index -> account.
    pub participants: Vector<AccountId>,
    /// D5: position index -> account.
    pub position_accounts: Vector<AccountId>,
}

impl Epoch {
    /// A fresh `open` epoch with empty vectors under the per-epoch prefixes.
    pub fn open(epoch: EpochId, opened_at: u64, cutoff_at: u64, params: Params) -> Self {
        Self {
            epoch,
            phase: EpochPhase::Open,
            opened_at,
            cutoff_at,
            closed_at: None,
            proposal_deadline: None,
            committed_at: None,
            payment_deadline: None,
            finalized_at: None,
            params,
            edge_count: 0,
            participant_count: 0,
            total_obligation: 0,
            total_cleared: 0,
            pool: 0,
            shortfall: 0,
            edges: Vector::new(StorageKey::EpochEdges(epoch)),
            participants: Vector::new(StorageKey::EpochParticipants(epoch)),
            position_accounts: Vector::new(StorageKey::EpochPositions(epoch)),
        }
    }
}

impl Epoch {
    /// Phase at `now`: `cutoff` when stored `open` and `now >= cutoff_at`; `commit` when stored
    /// `clearing` and `now >= proposal_deadline` (spec section 8).
    pub fn phase_at(&self, now: u64) -> EpochPhaseView {
        match self.phase {
            EpochPhase::Open if now >= self.cutoff_at => EpochPhaseView::Cutoff,
            EpochPhase::Open => EpochPhaseView::Open,
            EpochPhase::Clearing if self.proposal_deadline.is_some_and(|d| now >= d) => EpochPhaseView::Commit,
            EpochPhase::Clearing => EpochPhaseView::Clearing,
            EpochPhase::PaymentWindow => EpochPhaseView::PaymentWindow,
            EpochPhase::Finalized => EpochPhaseView::Finalized,
        }
    }

    /// Panics `ERR_WRONG_PHASE` unless the stored phase is `expected`.
    pub fn require_phase(&self, expected: EpochPhase) {
        require!(
            self.phase == expected,
            format!("ERR_WRONG_PHASE: epoch {} is {:?}, expected {:?}", self.epoch, self.phase, expected)
        );
    }

    /// `open -> clearing` (`close_epoch`): needs `now >= cutoff_at`; opens the proposal window from
    /// the epoch's own params snapshot.
    pub fn start_clearing(&mut self, now: u64) {
        self.require_phase(EpochPhase::Open);
        require!(now >= self.cutoff_at, format!("ERR_BEFORE_CUTOFF: cutoff_at {}", self.cutoff_at));
        self.phase = EpochPhase::Clearing;
        self.closed_at = Some(now);
        self.proposal_deadline = Some(now + self.params.proposal_window * NS_PER_SEC);
    }

    /// `clearing -> payment_window` (`commit_epoch`): needs `now >= proposal_deadline`.
    pub fn start_payment_window(&mut self, now: u64) {
        self.require_phase(EpochPhase::Clearing);
        let deadline = self.proposal_deadline.expect("clearing epoch has proposal_deadline");
        require!(now >= deadline, format!("ERR_PROPOSAL_WINDOW_OPEN: proposal_deadline {}", deadline));
        self.phase = EpochPhase::PaymentWindow;
        self.committed_at = Some(now);
        self.payment_deadline = Some(now + self.params.payment_window * NS_PER_SEC);
    }

    /// `payment_window -> finalized` (`finalize_epoch`): needs `now >= payment_deadline`.
    pub fn finalize(&mut self, now: u64) {
        self.require_phase(EpochPhase::PaymentWindow);
        let deadline = self.payment_deadline.expect("payment_window epoch has payment_deadline");
        require!(now >= deadline, format!("ERR_PAYMENT_WINDOW_OPEN: payment_deadline {}", deadline));
        self.phase = EpochPhase::Finalized;
        self.finalized_at = Some(now);
    }

    pub fn to_view(&self, now: u64) -> EpochView {
        EpochView {
            epoch: self.epoch,
            phase: self.phase_at(now),
            opened_at: U64(self.opened_at),
            cutoff_at: U64(self.cutoff_at),
            closed_at: self.closed_at.map(U64),
            proposal_deadline: self.proposal_deadline.map(U64),
            committed_at: self.committed_at.map(U64),
            payment_deadline: self.payment_deadline.map(U64),
            finalized_at: self.finalized_at.map(U64),
            edge_count: self.edge_count,
            participant_count: self.participant_count,
            total_obligation: U128(self.total_obligation),
            total_cleared: U128(self.total_cleared),
            pool: U128(self.pool),
            shortfall: U128(self.shortfall),
            params: self.params.clone(),
        }
    }
}

/// `get_epoch` result (spec section 5). Timestamps are nanoseconds.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpochView {
    pub epoch: EpochId,
    pub phase: EpochPhaseView,
    pub opened_at: U64,
    pub cutoff_at: U64,
    pub closed_at: Option<U64>,
    pub proposal_deadline: Option<U64>,
    pub committed_at: Option<U64>,
    pub payment_deadline: Option<U64>,
    pub finalized_at: Option<U64>,
    pub edge_count: u32,
    pub participant_count: u32,
    pub total_obligation: U128,
    pub total_cleared: U128,
    pub pool: U128,
    pub shortfall: U128,
    pub params: Params,
}

impl X207Circle {
    /// The epoch record, or panics `ERR_UNKNOWN_EPOCH`.
    pub fn epoch_or_panic(&self, epoch: EpochId) -> &Epoch {
        self.epochs.get(&epoch).unwrap_or_else(|| env::panic_str(&format!("ERR_UNKNOWN_EPOCH: {epoch}")))
    }

    pub fn epoch_mut_or_panic(&mut self, epoch: EpochId) -> &mut Epoch {
        self.epochs.get_mut(&epoch).unwrap_or_else(|| env::panic_str(&format!("ERR_UNKNOWN_EPOCH: {epoch}")))
    }
}

#[near]
impl X207Circle {
    /// `None` = `current_epoch` (the newest epoch created). The reported phase may be the derived
    /// `cutoff` or `commit` (K-28).
    pub fn get_epoch(&self, epoch: Option<EpochId>) -> Option<EpochView> {
        let id = epoch.unwrap_or(self.current_epoch);
        self.epochs.get(&id).map(|e| e.to_view(env::block_timestamp()))
    }
}

/// D4 proposal (spec 3.6, prefix `q`). `edges` is emptied by `commit_epoch`.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearingProposal {
    pub epoch: EpochId,
    pub solver: AccountId,
    pub edges: Vec<ClearingEdge>,
    pub edge_count: u32,
    pub cleared_total: U128,
    pub submitted_at: U64,
    pub committed: bool,
}

#[cfg(test)]
mod tests {
    use near_sdk::json_types::U128;
    use near_sdk::serde_json::{self, json};
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    use super::*;
    use crate::params::{Admission, Fees};

    const SEC: u64 = NS_PER_SEC;

    fn params() -> Params {
        Params {
            epoch_length: 3600,
            payment_window: 1800,
            proposal_window: 600,
            base_limit_ratio: 8000,
            limit_growth: 1000,
            limit_cap: U128(1_000_000_000),
            limit_decay: 0,
            fees: Fees { clearing_fee_bps: 0, solver_fee_bps: 0 },
            admission: Admission::Open,
        }
    }

    fn at(ns: u64) {
        testing_env!(VMContextBuilder::new().block_timestamp(ns).build());
    }

    fn circle(now: u64) -> X207Circle {
        at(now);
        X207Circle::with_config(
            "dao.near".parse().unwrap(),
            "agents-main".to_string(),
            "usdt.near".parse().unwrap(),
            "intents.near".parse().unwrap(),
            params(),
            now,
        )
    }

    fn epoch1(c: &mut X207Circle) -> &mut Epoch {
        c.epochs.get_mut(&1).unwrap()
    }

    #[test]
    fn tc_ct_014_01_epoch_one_after_init() {
        let c = circle(1_000);
        let v = serde_json::to_value(c.get_epoch(None).unwrap()).unwrap();
        assert_eq!(v["epoch"], json!(1));
        assert_eq!(v["phase"], json!("open"));
        assert_eq!(v["opened_at"], json!("1000"));
        assert_eq!(v["cutoff_at"], json!((1_000 + 3600 * SEC).to_string()));
        for f in ["closed_at", "proposal_deadline", "committed_at", "payment_deadline", "finalized_at"] {
            assert_eq!(v[f], serde_json::Value::Null, "{f}");
        }
        assert_eq!(v["params"]["epoch_length"], json!(3600));
    }

    #[test]
    fn tc_ct_014_02_derived_cutoff_at_boundary() {
        let c = circle(0);
        let t_cut = 3600 * SEC;
        at(t_cut - 1);
        assert_eq!(c.get_epoch(Some(1)).unwrap().phase, EpochPhaseView::Open);
        at(t_cut);
        assert_eq!(c.get_epoch(Some(1)).unwrap().phase, EpochPhaseView::Cutoff);
    }

    #[test]
    fn tc_ct_014_03_derived_commit_at_boundary() {
        let mut c = circle(0);
        let t_cut = 3600 * SEC;
        epoch1(&mut c).start_clearing(t_cut);
        let t_prop = t_cut + 600 * SEC;
        at(t_prop - 1);
        assert_eq!(c.get_epoch(Some(1)).unwrap().phase, EpochPhaseView::Clearing);
        at(t_prop);
        assert_eq!(c.get_epoch(Some(1)).unwrap().phase, EpochPhaseView::Commit);
    }

    #[test]
    fn tc_ct_014_04_full_cycle_phases_and_timestamps() {
        let mut c = circle(0);
        let e = epoch1(&mut c);
        e.start_clearing(3600 * SEC);
        assert_eq!((e.phase, e.closed_at, e.proposal_deadline), (EpochPhase::Clearing, Some(3600 * SEC), Some(4200 * SEC)));
        e.start_payment_window(4200 * SEC);
        assert_eq!((e.phase, e.committed_at, e.payment_deadline), (EpochPhase::PaymentWindow, Some(4200 * SEC), Some(6000 * SEC)));
        e.finalize(6000 * SEC);
        assert_eq!((e.phase, e.finalized_at), (EpochPhase::Finalized, Some(6000 * SEC)));
        at(6000 * SEC);
        assert_eq!(c.get_epoch(Some(1)).unwrap().phase, EpochPhaseView::Finalized);
    }

    #[test]
    #[should_panic(expected = "ERR_WRONG_PHASE")]
    fn tc_ct_014_05_close_after_finalized_rejected() {
        let mut c = circle(0);
        let e = epoch1(&mut c);
        e.start_clearing(3600 * SEC);
        e.start_payment_window(4200 * SEC);
        e.finalize(6000 * SEC);
        e.start_clearing(7000 * SEC);
    }

    #[test]
    #[should_panic(expected = "ERR_WRONG_PHASE")]
    fn tc_ct_014_05_commit_after_finalized_rejected() {
        let mut c = circle(0);
        let e = epoch1(&mut c);
        e.start_clearing(3600 * SEC);
        e.start_payment_window(4200 * SEC);
        e.finalize(6000 * SEC);
        e.start_payment_window(7000 * SEC);
    }

    #[test]
    #[should_panic(expected = "ERR_WRONG_PHASE")]
    fn tc_ct_014_05_finalize_twice_rejected() {
        let mut c = circle(0);
        let e = epoch1(&mut c);
        e.start_clearing(3600 * SEC);
        e.start_payment_window(4200 * SEC);
        e.finalize(6000 * SEC);
        e.finalize(7000 * SEC);
    }

    #[test]
    #[should_panic(expected = "ERR_WRONG_PHASE")]
    fn tc_ct_014_06_commit_in_open_rejected() {
        let mut c = circle(0);
        epoch1(&mut c).start_payment_window(9_999 * SEC);
    }

    #[test]
    #[should_panic(expected = "ERR_WRONG_PHASE")]
    fn tc_ct_014_06_finalize_in_open_rejected() {
        let mut c = circle(0);
        epoch1(&mut c).finalize(9_999 * SEC);
    }

    #[test]
    #[should_panic(expected = "ERR_WRONG_PHASE")]
    fn tc_ct_014_06_submit_phase_check_in_open_rejected() {
        let c = circle(0);
        c.epoch_or_panic(1).require_phase(EpochPhase::Clearing);
    }

    #[test]
    #[should_panic(expected = "ERR_BEFORE_CUTOFF")]
    fn close_before_cutoff_rejected() {
        let mut c = circle(0);
        epoch1(&mut c).start_clearing(3600 * SEC - 1);
    }

    #[test]
    #[should_panic(expected = "ERR_PROPOSAL_WINDOW_OPEN")]
    fn commit_before_proposal_deadline_rejected() {
        let mut c = circle(0);
        let e = epoch1(&mut c);
        e.start_clearing(3600 * SEC);
        e.start_payment_window(4200 * SEC - 1);
    }

    #[test]
    #[should_panic(expected = "ERR_PAYMENT_WINDOW_OPEN")]
    fn finalize_before_payment_deadline_rejected() {
        let mut c = circle(0);
        let e = epoch1(&mut c);
        e.start_clearing(3600 * SEC);
        e.start_payment_window(4200 * SEC);
        e.finalize(6000 * SEC - 1);
    }

    #[test]
    fn tc_ct_014_08_unknown_epoch_is_null() {
        let c = circle(0);
        assert!(c.get_epoch(Some(99)).is_none());
    }

    #[test]
    #[should_panic(expected = "ERR_UNKNOWN_EPOCH")]
    fn unknown_epoch_helper_panics() {
        let c = circle(0);
        c.epoch_or_panic(99);
    }

    #[test]
    fn tc_ct_014_12_next_cutoff_keeps_cadence() {
        let l = 3600 * SEC;
        let prev = 10 * l;
        assert_eq!(next_cutoff_at(prev, prev + 100 * SEC, 3600), prev + l);
    }

    #[test]
    fn tc_ct_014_13_next_cutoff_when_late_and_at_boundary() {
        let l = 3600 * SEC;
        let prev = 10 * l;
        assert_eq!(next_cutoff_at(prev, prev + l + SEC, 3600), prev + l + SEC + l);
        assert_eq!(next_cutoff_at(prev, prev + l, 3600), prev + l + l);
        assert_eq!(next_cutoff_at(prev, prev + l - 1, 3600), prev + l);
    }

    #[test]
    fn tc_ct_014_10_snapshot_is_per_epoch() {
        let mut c = circle(0);
        let mut p2 = params();
        p2.epoch_length = 7200;
        c.epochs.insert(2, Epoch::open(2, 10, 20, p2));
        assert_eq!(c.get_epoch(Some(1)).unwrap().params.epoch_length, 3600);
        assert_eq!(c.get_epoch(Some(2)).unwrap().params.epoch_length, 7200);
    }

    #[test]
    fn tc_ct_014_11_overlapping_epochs_keep_own_phase() {
        let mut c = circle(0);
        c.epochs.insert(2, Epoch::open(2, 3600 * SEC, 7200 * SEC, params()));
        c.current_epoch = 2;
        let e1 = epoch1(&mut c);
        e1.start_clearing(3600 * SEC);
        e1.start_payment_window(4200 * SEC);
        assert_eq!(c.epochs.get(&2).unwrap().phase, EpochPhase::Open);
        at(4200 * SEC);
        assert_eq!(c.get_epoch(None).unwrap().epoch, 2);
        assert_eq!(c.get_epoch(Some(1)).unwrap().phase, EpochPhaseView::PaymentWindow);
    }
}
