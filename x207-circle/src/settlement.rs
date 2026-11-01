//! F4 settlement: D5 positions, D6 accounting, `ft_on_transfer`, `finalize_epoch`, `claim`.

use near_sdk::json_types::{I128, U128, U64};
use near_sdk::{near, AccountId};

use crate::clearing::ClearingProposal;
use crate::state::{X207Circle, X207CircleExt};
use crate::types::EpochId;

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
