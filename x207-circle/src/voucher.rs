//! F2 vouchers: D2/D3, canonical bytes, `register_cumulative`, `register_voucher`.

use near_sdk::json_types::U128;
use near_sdk::{near, AccountId};

use crate::state::{X207Circle, X207CircleExt};
use crate::types::EpochId;

/// D2 and D3 for one `(epoch, payer, payee)` (spec 9.1, prefix `p`):
/// `obligation = cumulative - cleared`; `last_seq` is `0` before the first registration.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PairState {
    pub cumulative: u128,
    pub last_seq: u64,
    pub cleared: u128,
}

/// D2 aggregate per `(epoch, account)` (spec 9.1, prefix `t`).
#[near(serializers = [borsh])]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParticipantTotals {
    pub owed_out: u128,
    pub owed_in: u128,
}

impl PairState {
    /// `cumulative - cleared`; `cleared <= cumulative` always holds (spec 3.6).
    pub fn obligation(&self) -> u128 {
        self.cumulative - self.cleared
    }
}

/// `obligation` and `list_obligations` result (spec section 5).
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObligationView {
    pub epoch: EpochId,
    pub debtor: AccountId,
    pub creditor: AccountId,
    pub cumulative: U128,
    pub last_seq: u64,
    pub cleared: U128,
    pub obligation: U128,
}

impl ObligationView {
    fn new(epoch: EpochId, debtor: AccountId, creditor: AccountId, pair: &PairState) -> Self {
        Self {
            epoch,
            debtor,
            creditor,
            cumulative: U128(pair.cumulative),
            last_seq: pair.last_seq,
            cleared: U128(pair.cleared),
            obligation: U128(pair.obligation()),
        }
    }
}

#[near]
impl X207Circle {
    /// D2 `outstanding_debt`; `0` for an unknown account.
    pub fn outstanding_debt(&self, account_id: AccountId) -> U128 {
        U128(self.outstanding_debt.get(&account_id).copied().unwrap_or(0))
    }

    /// D3 `last_seq` of `(payer, payee, epoch)`; `0` before the first registration.
    pub fn last_seq(&self, payer: AccountId, payee: AccountId, epoch: EpochId) -> u64 {
        self.pairs.get(&(epoch, payer, payee)).map_or(0, |p| p.last_seq)
    }

    /// D2 and D3 of one pair, or `None` before the first registration.
    pub fn obligation(&self, epoch: EpochId, debtor: AccountId, creditor: AccountId) -> Option<ObligationView> {
        let pair = self.pairs.get(&(epoch, debtor.clone(), creditor.clone()))?;
        Some(ObligationView::new(epoch, debtor, creditor, pair))
    }

    /// Pairs of `epoch` in first-registration order (the D2 snapshot index); `limit` 0, an unknown
    /// epoch, or a `from_index` past the end gives `[]`.
    pub fn list_obligations(&self, epoch: EpochId, from_index: u32, limit: u32) -> Vec<ObligationView> {
        let Some(e) = self.epochs.get(&epoch) else { return vec![] };
        e.edges
            .iter()
            .skip(from_index as usize)
            .take(limit as usize)
            .filter_map(|(debtor, creditor)| self.obligation(epoch, debtor.clone(), creditor.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::serde_json::{self, json};
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{env, testing_env};

    use super::*;
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
            env::block_timestamp(),
        )
    }

    /// Stands in for registration: one entry in the pair map and one in the epoch edge index.
    fn put(c: &mut X207Circle, epoch: EpochId, payer: &str, payee: &str, pair: PairState) {
        c.pairs.insert((epoch, acc(payer), acc(payee)), pair);
        c.epochs.get_mut(&epoch).unwrap().edges.push((acc(payer), acc(payee)));
    }

    // TC-CT-011-05
    #[test]
    fn tc_ct_011_05_missing_pair() {
        let c = circle();
        assert_eq!(c.last_seq(acc("gpu.near"), acc("scraper.near"), 1), 0);
        assert_eq!(serde_json::to_value(c.obligation(1, acc("gpu.near"), acc("scraper.near"))).unwrap(), json!(null));
        assert_eq!(c.outstanding_debt(acc("gpu.near")), U128(0));
    }

    // TC-CT-011-01 (view shape), TC-CT-011-06 (obligation = cumulative - cleared)
    #[test]
    fn tc_ct_011_06_obligation_is_cumulative_minus_cleared() {
        let mut c = circle();
        put(&mut c, 1, "scraper.near", "gpu.near", PairState { cumulative: 60_000_000, last_seq: 3, cleared: 25_000_000 });
        let v = serde_json::to_value(c.obligation(1, acc("scraper.near"), acc("gpu.near"))).unwrap();
        assert_eq!(
            v,
            json!({"epoch":1,"debtor":"scraper.near","creditor":"gpu.near","cumulative":"60000000",
                   "last_seq":3,"cleared":"25000000","obligation":"35000000"})
        );
        assert_eq!(c.last_seq(acc("scraper.near"), acc("gpu.near"), 1), 3);
    }

    // TC-CT-011-02, TC-CT-011-03: one entry per pair per epoch, epochs independent
    #[test]
    fn tc_ct_011_03_epochs_are_separate_entries() {
        let mut c = circle();
        c.epochs.insert(2, crate::clearing::Epoch::open(2, 2_000, 9_000, Params::default()));
        put(&mut c, 1, "translator.near", "scraper.near", PairState { cumulative: 25_000_000, last_seq: 2, cleared: 0 });
        put(&mut c, 2, "translator.near", "scraper.near", PairState { cumulative: 5_000_000, last_seq: 1, cleared: 0 });
        assert_eq!(c.list_obligations(1, 0, 10).len(), 1);
        assert_eq!(c.list_obligations(2, 0, 10).len(), 1);
        assert_eq!(c.obligation(1, acc("translator.near"), acc("scraper.near")).unwrap().cumulative, U128(25_000_000));
        assert_eq!(c.obligation(2, acc("translator.near"), acc("scraper.near")).unwrap().cumulative, U128(5_000_000));
        assert_eq!(c.last_seq(acc("translator.near"), acc("scraper.near"), 2), 1);
    }

    // TC-CT-011-04 (view reads the stored per-account total)
    #[test]
    fn tc_ct_011_04_outstanding_debt_view() {
        let mut c = circle();
        c.outstanding_debt.insert(acc("translator.near"), 55_000_000);
        assert_eq!(c.outstanding_debt(acc("translator.near")), U128(55_000_000));
    }

    // TC-CT-011-07
    #[test]
    fn tc_ct_011_07_list_obligations_pages() {
        let mut c = circle();
        for i in 0..6u64 {
            put(&mut c, 1, &format!("p{i}.near"), "q.near", PairState { cumulative: 1, last_seq: i + 1, cleared: 0 });
        }
        assert_eq!(c.list_obligations(1, 0, 4).len(), 4);
        let tail = c.list_obligations(1, 4, 4);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].debtor, acc("p4.near"));
        assert!(c.list_obligations(1, 6, 4).is_empty());
        assert!(c.list_obligations(1, 0, 0).is_empty());
        assert!(c.list_obligations(9, 0, 4).is_empty());
    }
}
