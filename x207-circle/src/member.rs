//! F1 onboarding: D1 members, `join`, member views.

use near_sdk::json_types::{U128, U64};
use near_sdk::{env, near, AccountId, CurveType, NearToken, Promise, PublicKey};

use crate::events::{MemberJoined, X207Event};
use crate::state::{X207Circle, X207CircleExt};
use crate::types::EpochId;

/// Minimum attached deposit of `join` (spec 9.3). Not refunded on exit.
/// Covers the storage that one member can make the contract hold (security review 3, I-3, measured in `tests/ct062_storage_per_member.rs`): the worst case is 31282 bytes =
/// 0.3128 NEAR (64 intents 6594 bytes, 16 open grants with a scope of 1024 bytes 24288 bytes, the member row) plus, since 0.0.12, 8 open deals and a freeze record (2954 bytes):
/// 34236 bytes = 0.3424 NEAR. 0.35 NEAR is that rounded up.
pub const JOIN_STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(350);
/// Most registered members, including `exited` ones (spec 9.3, tuned in the gas item).
pub const MAX_MEMBERS: u32 = 64;

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
    /// ADR-30 (CT-079): a `suspended` member whose `reactivate_epoch` has been opened (`current_epoch >= it`) reads
    /// as `active` without being written. `close_epoch` does not walk the members (K-36).
    pub fn effective_status(&self, current_epoch: EpochId) -> MemberStatus {
        match (self.status, self.reactivate_epoch) {
            (MemberStatus::Suspended, Some(at)) if current_epoch >= at => MemberStatus::Active,
            (status, _) => status,
        }
    }

    /// Writes the effective status the first time the member is touched by a state change, with the limit from the
    /// stake and the reset history (the limit stays 0 while the member is still `suspended`).
    pub fn settle_reactivation(&mut self, current_epoch: EpochId, params: &crate::params::Params) {
        if self.effective_status(current_epoch) == MemberStatus::Active && self.status != MemberStatus::Active {
            self.status = MemberStatus::Active;
            self.reactivate_epoch = None;
            self.limit = crate::deal::bound_limit(crate::risk::compute_limit(self.stake, self.settled_history, params), self.stake, &self.account_id);
        }
    }

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

impl X207Circle {
    /// The member as a read sees it: a reactivation whose epoch has opened is applied (not written).
    pub fn member_now(&self, member: &Member) -> Member {
        let mut member = member.clone();
        if let Some(epoch) = self.epochs.get(&self.current_epoch) {
            member.settle_reactivation(self.current_epoch, &epoch.params);
        }
        member
    }
}

#[near]
impl X207Circle {
    /// Registers the caller as a `pending` member (spec 4.7 order): deposit, `circle_id`, key curve,
    /// `terms_hash` (64 lowercase hex), not yet registered, member cap. Stake and admission come later.
    /// The deposit above `JOIN_STORAGE_DEPOSIT` goes back to the caller in the same receipt chain.
    #[payable]
    pub fn join(&mut self, circle_id: String, pubkey_voucher: PublicKey, terms_hash: String) -> MemberView {
        let deposit = env::attached_deposit();
        if deposit < JOIN_STORAGE_DEPOSIT {
            env::panic_str(&format!(
                "ERR_INSUFFICIENT_STORAGE_DEPOSIT: attached {deposit}, need {JOIN_STORAGE_DEPOSIT}"
            ));
        }
        if circle_id != self.circle_id {
            env::panic_str(&format!("ERR_WRONG_CIRCLE_ID: {circle_id}"));
        }
        if pubkey_voucher.curve_type() != CurveType::ED25519 {
            env::panic_str("ERR_BAD_PUBKEY: pubkey_voucher must be an ed25519 key");
        }
        if terms_hash.len() != 64 || !terms_hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            env::panic_str("ERR_BAD_TERMS_HASH: terms_hash must be 64 lowercase hex characters");
        }
        let account_id = env::predecessor_account_id();
        if self.members.contains_key(&account_id) {
            env::panic_str(&format!("ERR_ALREADY_MEMBER: {account_id}"));
        }
        if self.member_count >= MAX_MEMBERS {
            env::panic_str(&format!("ERR_MAX_MEMBERS: {MAX_MEMBERS}"));
        }

        let joined_at = env::block_timestamp();
        let member = Member {
            account_id: account_id.clone(),
            pubkey_voucher: pubkey_voucher.clone(),
            terms_hash: terms_hash.clone(),
            stake: 0,
            limit: 0,
            status: MemberStatus::Pending,
            admitted: false,
            joined_at,
            settled_history: 0,
            last_active_epoch: self.current_epoch,
            exit_requested_epoch: None,
            withdraw_pending: false,
            reactivate_epoch: None,
        };
        let view = member.to_view();
        self.members.insert(account_id.clone(), member);
        self.member_index.push(account_id.clone());
        self.member_count += 1;
        MemberJoined {
            account_id: account_id.clone(),
            pubkey_voucher: String::from(&pubkey_voucher),
            terms_hash,
            status: "pending".to_string(),
            joined_at: U64(joined_at),
        }
        .emit();

        let excess = deposit.saturating_sub(JOIN_STORAGE_DEPOSIT);
        if !excess.is_zero() {
            Promise::new(account_id).transfer(excess).detach();
        }
        view
    }

    pub fn member(&self, account_id: AccountId) -> Option<MemberView> {
        self.members.get(&account_id).map(|m| self.member_now(m).to_view())
    }

    /// Members in join order, `limit` per page; `limit` 0 or an `from_index` past the end gives `[]` (K-23).
    pub fn list_members(&self, from_index: u32, limit: u32) -> Vec<MemberView> {
        self.member_index
            .iter()
            .skip(from_index as usize)
            .take(limit as usize)
            .filter_map(|id| self.members.get(id).map(|m| self.member_now(m).to_view()))
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
            max_search_share_bps: 500,
            bond_threshold: near_sdk::json_types::U128(100_000_000),
            bond_bps: 2000,
            prune_delay_epochs: 2,
            relayer_fee: near_sdk::json_types::U128(0),
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

#[cfg(test)]
mod join_tests {
    use near_sdk::serde_json::{self, json};
    use near_sdk::test_utils::{get_created_receipts, get_logs, VMContextBuilder};
    use near_sdk::testing_env;

    use super::*;
    use crate::params::Params;

    const KEY: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";
    const SECP: &str = "secp256k1:qMoRgcoXai4mBPsdbHi1wfyxF9TdbPCF4qSDQTRP3TfescSRoUdSx6nmeQoN3aiwGzwMyGXAb1gUjBTv5AY8DXj";
    const HASH: &str = "9f2c0000000000000000000000000000000000000000000000000000000000e1";

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn key(s: &str) -> PublicKey {
        s.parse().unwrap()
    }

    /// `F0`: fresh circle `agents-main`; the next call is made by `caller` with `deposit`.
    fn circle(caller: &str, deposit: NearToken) -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        let c = X207Circle::with_config(
            acc("dao.near"),
            "agents-main".to_string(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            1_000,
        );
        as_caller(caller, deposit);
        c
    }

    fn as_caller(caller: &str, deposit: NearToken) {
        testing_env!(VMContextBuilder::new()
            .block_timestamp(5_000)
            .predecessor_account_id(acc(caller))
            .attached_deposit(deposit)
            .build());
    }

    fn join_ok(c: &mut X207Circle) -> MemberView {
        c.join("agents-main".to_string(), key(KEY), HASH.to_string())
    }

    // TC-CT-016-01
    #[test]
    fn tc_ct_016_01_valid_join_exact_deposit() {
        let mut c = circle("translator.near", JOIN_STORAGE_DEPOSIT);
        let v = serde_json::to_value(join_ok(&mut c)).unwrap();
        assert_eq!(v["status"], json!("pending"));
        assert_eq!(v["admitted"], json!(false));
        assert_eq!((&v["stake"], &v["limit"]), (&json!("0"), &json!("0")));
        assert_eq!(v["account_id"], json!("translator.near"));
        assert_eq!(v["joined_at"], json!("5000"));
        assert_eq!(c.member_count, 1);
        assert_eq!(c.member(acc("translator.near")).unwrap().status, MemberStatus::Pending);
        let log = get_logs().into_iter().find(|l| l.contains("member_joined")).expect("member_joined event");
        let e: serde_json::Value = serde_json::from_str(log.trim_start_matches("EVENT_JSON:")).unwrap();
        assert_eq!(
            e["data"][0],
            json!({"account_id":"translator.near","pubkey_voucher":KEY,"terms_hash":HASH,"status":"pending","joined_at":"5000"})
        );
        assert!(get_created_receipts().is_empty(), "exact deposit has nothing to refund");
    }

    // TC-CT-016-02 (unit part): excess is sent back to the caller
    #[test]
    fn tc_ct_016_02_excess_deposit_refunded() {
        let mut c = circle("translator.near", NearToken::from_near(1));
        join_ok(&mut c);
        let receipts = get_created_receipts();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].receiver_id, acc("translator.near"));
        assert!(format!("{:?}", receipts[0].actions).contains(&(NearToken::from_near(1).as_yoctonear() - JOIN_STORAGE_DEPOSIT.as_yoctonear()).to_string()));
    }

    // TC-CT-016-03
    #[test]
    #[should_panic(expected = "ERR_INSUFFICIENT_STORAGE_DEPOSIT")]
    fn tc_ct_016_03_one_yocto_short() {
        let mut c = circle("translator.near", NearToken::from_yoctonear(JOIN_STORAGE_DEPOSIT.as_yoctonear() - 1));
        join_ok(&mut c);
    }

    // TC-CT-016-04
    #[test]
    #[should_panic(expected = "ERR_INSUFFICIENT_STORAGE_DEPOSIT")]
    fn tc_ct_016_04_no_deposit() {
        let mut c = circle("translator.near", NearToken::from_yoctonear(0));
        join_ok(&mut c);
    }

    // TC-CT-016-05
    #[test]
    #[should_panic(expected = "ERR_WRONG_CIRCLE_ID")]
    fn tc_ct_016_05_wrong_circle_id() {
        let mut c = circle("translator.near", JOIN_STORAGE_DEPOSIT);
        c.join("agents-test".to_string(), key(KEY), HASH.to_string());
    }

    // TC-CT-016-06
    #[test]
    #[should_panic(expected = "ERR_BAD_PUBKEY")]
    fn tc_ct_016_06_not_ed25519() {
        let mut c = circle("translator.near", JOIN_STORAGE_DEPOSIT);
        c.join("agents-main".to_string(), key(SECP), HASH.to_string());
    }

    // TC-CT-016-07 (K-16): a broken key never reaches the contract code
    #[test]
    fn tc_ct_016_07_broken_key_fails_deserialization() {
        let err = serde_json::from_value::<PublicKey>(json!("ed25519:xyz")).unwrap_err().to_string();
        assert!(!err.contains("ERR_"), "{err}");
    }

    // TC-CT-016-08
    #[test]
    fn tc_ct_016_08_bad_terms_hash_cases() {
        let cases = [
            String::new(),
            "a".repeat(63),
            "a".repeat(65),
            format!("{}g", "a".repeat(63)),
            HASH.to_uppercase(),
        ];
        for bad in cases {
            let mut c = circle("translator.near", JOIN_STORAGE_DEPOSIT);
            let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                c.join("agents-main".to_string(), key(KEY), bad.clone());
            }))
            .expect_err("expected a panic");
            let msg = err
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            assert!(msg.contains("ERR_BAD_TERMS_HASH"), "{bad:?}: {msg}");
            assert_eq!(c.member_count, 0);
        }
    }

    // TC-CT-016-09
    #[test]
    #[should_panic(expected = "ERR_ALREADY_MEMBER")]
    fn tc_ct_016_09_double_join() {
        let mut c = circle("translator.near", JOIN_STORAGE_DEPOSIT);
        join_ok(&mut c);
        join_ok(&mut c);
    }

    #[test]
    fn tc_ct_016_09_first_entry_untouched_after_double_join() {
        let mut c = circle("translator.near", JOIN_STORAGE_DEPOSIT);
        let first = join_ok(&mut c);
        as_caller("translator.near", JOIN_STORAGE_DEPOSIT);
        let other = key("ed25519:7Yk4CPJpnV2VAwYTvHdUKRrHSQbCE8SiJBB9UCjSQbVM");
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.join("agents-main".to_string(), other, "b".repeat(64));
        }));
        assert!(res.is_err());
        assert_eq!(c.member(acc("translator.near")).unwrap(), first);
        assert_eq!(c.member_count, 1);
    }

    // TC-CT-016-10
    #[test]
    fn tc_ct_016_10_max_members_boundary() {
        let mut c = circle("m0.near", JOIN_STORAGE_DEPOSIT);
        for i in 0..MAX_MEMBERS {
            as_caller(&format!("m{i}.near"), JOIN_STORAGE_DEPOSIT);
            join_ok(&mut c);
        }
        assert_eq!(c.member_count, MAX_MEMBERS);
        as_caller("late.near", JOIN_STORAGE_DEPOSIT);
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            join_ok(&mut c);
        }))
        .expect_err("expected a panic");
        let msg = err
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(msg.contains("ERR_MAX_MEMBERS"), "{msg}");
        assert_eq!(c.member_count, MAX_MEMBERS);
    }

    // TC-CT-016-12 (K-23): the contract cannot read access keys, so the same key is accepted.
    #[test]
    fn tc_ct_016_12_voucher_key_may_equal_access_key() {
        let mut c = circle("translator.near", JOIN_STORAGE_DEPOSIT);
        assert_eq!(join_ok(&mut c).pubkey_voucher, key(KEY));
    }

    // Order of spec 4.7: deposit is checked before circle_id.
    #[test]
    #[should_panic(expected = "ERR_INSUFFICIENT_STORAGE_DEPOSIT")]
    fn deposit_checked_before_circle_id() {
        let mut c = circle("translator.near", NearToken::from_yoctonear(0));
        c.join("agents-test".to_string(), key(KEY), HASH.to_string());
    }
}
