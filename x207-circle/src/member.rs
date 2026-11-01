//! F1 onboarding: D1 members, `join`, member views.

use near_sdk::json_types::{U128, U64};
use near_sdk::{near, AccountId, PublicKey};

use crate::state::{X207Circle, X207CircleExt};
use crate::types::EpochId;

/// Spec 3.5, ADR-26. JSON values are `snake_case`.
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemberStatus {
    Pending,
    Active,
    Suspended,
    Exited,
}

/// D1 record (spec 3.7 `MemberView` plus the internal fields of section 14). Amounts are native
/// `u128` in state; the view converts to `U128`.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub account_id: AccountId,
    pub pubkey_voucher: PublicKey,
    pub terms_hash: String,
    pub stake: u128,
    pub limit: u128,
    pub status: MemberStatus,
    pub admitted: bool,
    pub joined_at: u64,
    pub settled_history: u128,
    pub last_active_epoch: EpochId,
    pub exit_requested_epoch: Option<EpochId>,
    /// `true` between `withdraw_stake` and `on_withdraw_resolved` (K-07). Not shown in the view.
    pub withdraw_pending: bool,
    /// ADR-30, P1 (CT-079). Not shown in the view.
    pub reactivate_epoch: Option<EpochId>,
}

impl Member {
    pub fn to_view(&self) -> MemberView {
        MemberView {
            account_id: self.account_id.clone(),
            pubkey_voucher: self.pubkey_voucher.clone(),
            terms_hash: self.terms_hash.clone(),
            stake: U128(self.stake),
            limit: U128(self.limit),
            status: self.status,
            admitted: self.admitted,
            joined_at: U64(self.joined_at),
            settled_history: U128(self.settled_history),
            last_active_epoch: self.last_active_epoch,
            exit_requested_epoch: self.exit_requested_epoch,
        }
    }
}

/// View of D1 (spec 3.7). `withdraw_pending` and `reactivate_epoch` stay internal.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberView {
    pub account_id: AccountId,
    pub pubkey_voucher: PublicKey,
    pub terms_hash: String,
    pub stake: U128,
    pub limit: U128,
    pub status: MemberStatus,
    pub admitted: bool,
    pub joined_at: U64,
    pub settled_history: U128,
    pub last_active_epoch: EpochId,
    pub exit_requested_epoch: Option<EpochId>,
}

#[near]
impl X207Circle {
    pub fn member(&self, account_id: AccountId) -> Option<MemberView> {
        self.members.get(&account_id).map(Member::to_view)
    }

    /// Members in join order, `limit` per page; `limit` 0 or an `from_index` past the end gives `[]` (K-23).
    pub fn list_members(&self, from_index: u32, limit: u32) -> Vec<MemberView> {
        self.member_index
            .iter()
            .skip(from_index as usize)
            .take(limit as usize)
            .filter_map(|id| self.members.get(id).map(Member::to_view))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::json_types::U128;
    use near_sdk::serde_json::{self, json};
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{env, testing_env, PublicKey};

    use super::*;
    use crate::params::{Admission, Fees, Params};

    const KEY: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn circle() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        let params = Params {
            epoch_length: 3600,
            payment_window: 1800,
            proposal_window: 600,
            base_limit_ratio: 8000,
            limit_growth: 1000,
            limit_cap: U128(1_000_000_000),
            limit_decay: 0,
            fees: Fees { clearing_fee_bps: 0, solver_fee_bps: 0 },
            admission: Admission::Open,
        };
        X207Circle::with_config(
            acc("dao.near"),
            "agents-main".to_string(),
            acc("usdt.near"),
            acc("intents.near"),
            params,
            env::block_timestamp(),
        )
    }

    fn add(c: &mut X207Circle, id: &str, status: MemberStatus, joined_at: u64) {
        let account_id = acc(id);
        c.members.insert(
            account_id.clone(),
            Member {
                account_id: account_id.clone(),
                pubkey_voucher: KEY.parse::<PublicKey>().unwrap(),
                terms_hash: "9f".repeat(32),
                stake: 100_000_000,
                limit: 80_000_000,
                status,
                admitted: false,
                joined_at,
                settled_history: 0,
                last_active_epoch: 1,
                exit_requested_epoch: None,
                withdraw_pending: true,
                reactivate_epoch: Some(3),
            },
        );
        c.member_index.push(account_id);
        c.member_count += 1;
    }

    #[test]
    fn tc_ct_009_01_registered_member_returns_all_fields() {
        let mut c = circle();
        add(&mut c, "translator.near", MemberStatus::Active, 1_793_836_800_000_000_000);
        let v = serde_json::to_value(c.member(acc("translator.near")).unwrap()).unwrap();
        assert_eq!(
            v,
            json!({
                "account_id": "translator.near",
                "pubkey_voucher": KEY,
                "terms_hash": "9f".repeat(32),
                "stake": "100000000",
                "limit": "80000000",
                "status": "active",
                "admitted": false,
                "joined_at": "1793836800000000000",
                "settled_history": "0",
                "last_active_epoch": 1,
                "exit_requested_epoch": null
            })
        );
    }

    #[test]
    fn tc_ct_009_02_unregistered_account_is_null() {
        let mut c = circle();
        add(&mut c, "translator.near", MemberStatus::Active, 1);
        assert_eq!(serde_json::to_value(c.member(acc("outsider.near"))).unwrap(), serde_json::Value::Null);
    }

    #[test]
    fn tc_ct_009_03_status_serialization() {
        for (status, wire) in [
            (MemberStatus::Pending, "pending"),
            (MemberStatus::Active, "active"),
            (MemberStatus::Suspended, "suspended"),
            (MemberStatus::Exited, "exited"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), json!(wire));
            assert_eq!(serde_json::from_value::<MemberStatus>(json!(wire)).unwrap(), status);
        }
    }

    #[test]
    fn tc_ct_009_04_joined_at_is_nanosecond_string() {
        let mut c = circle();
        add(&mut c, "a.near", MemberStatus::Pending, 1_700_000_000_123_456_789);
        let v = serde_json::to_value(c.member(acc("a.near")).unwrap()).unwrap();
        assert_eq!(v["joined_at"], json!("1700000000123456789"));
    }

    #[test]
    fn tc_ct_009_05_list_members_pages() {
        let mut c = circle();
        for i in 0..6 {
            add(&mut c, &format!("m{i}.near"), MemberStatus::Active, i);
        }
        let ids = |v: Vec<MemberView>| v.into_iter().map(|m| m.account_id.to_string()).collect::<Vec<_>>();
        assert_eq!(ids(c.list_members(0, 4)), ["m0.near", "m1.near", "m2.near", "m3.near"]);
        assert_eq!(ids(c.list_members(4, 4)), ["m4.near", "m5.near"]);
        assert!(c.list_members(6, 4).is_empty());
    }

    #[test]
    fn tc_ct_009_06_list_members_limit_zero() {
        let mut c = circle();
        add(&mut c, "a.near", MemberStatus::Active, 1);
        assert!(c.list_members(0, 0).is_empty());
    }
}
