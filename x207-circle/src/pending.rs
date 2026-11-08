//! CT-123 (0.0.18, A-CT-01 of the audit of 2026-11-08): a `pending` member with no stake can leave, so that unused rows cannot take every slot of
//! `MAX_MEMBERS` for good. Admission is checked at the stake, after the join, so a check in `join_evm` would not help: the way out is removal.
//!
//! - `remove_pending(account_id)`: the owner (the DAO) removes a `pending` member at any time; anyone may remove one that has been `pending` for
//!   `PENDING_GC_AFTER_NS` and was never admitted.
//! - A join into a full circle (`join`, `join_evm`) removes the oldest such row first (`make_room`), so the cap is a real count of live members.
//!
//! Only a row with no stake, no debt and no open withdrawal is removable, so nothing of a member is lost. The sponsor of an EVM member gets the join
//! deposit back (`refund_sponsor`).

use near_sdk::{env, near, require, AccountId};

use crate::events::{MemberRemoved, X207Event};
use crate::member::{MemberStatus, MAX_MEMBERS};
use crate::state::{X207Circle, X207CircleExt};

/// A `pending`, never admitted member without stake is garbage once it has waited this long (7 days).
pub const PENDING_GC_AFTER_NS: u64 = 7 * 24 * 3_600 * 1_000_000_000;

impl X207Circle {
    /// True for a row that is only a slot: `pending`, no stake, no debt, no withdrawal in flight.
    fn is_empty_pending(&self, account_id: &AccountId) -> bool {
        self.members.get(account_id).is_some_and(|m| {
            m.status == MemberStatus::Pending
                && m.stake == 0
                && !m.withdraw_pending
                && m.exit_requested_epoch.is_none()
                && self.outstanding_debt.get(account_id).copied().unwrap_or(0) == 0
        })
    }

    /// Removes the row. The caller has checked `is_empty_pending`.
    fn drop_pending(&mut self, account_id: &AccountId, by_owner: bool) {
        self.members.remove(account_id);
        let position = self.member_index.iter().position(|a| a == account_id);
        if let Some(index) = position {
            self.member_index.swap_remove(index as u32);
        }
        self.member_count = self.member_count.saturating_sub(1);
        crate::evm::refund_sponsor(account_id);
        crate::evm::forget_signer(account_id);
        MemberRemoved {
            account_id: account_id.clone(),
            by_owner,
        }
        .emit();
    }

    /// A join needs a free slot. In a full circle the oldest `pending` row that nobody admitted and that is past `PENDING_GC_AFTER_NS` is removed;
    /// without one the join fails with `ERR_MAX_MEMBERS`.
    pub(crate) fn make_room(&mut self) {
        if self.member_count < MAX_MEMBERS {
            return;
        }
        let now = env::block_timestamp();
        let victim = self
            .member_index
            .iter()
            .find(|id| {
                self.is_empty_pending(id)
                    && self.members.get(*id).is_some_and(|m| {
                        !m.admitted && now >= m.joined_at.saturating_add(PENDING_GC_AFTER_NS)
                    })
            })
            .cloned();
        match victim {
            Some(id) => self.drop_pending(&id, false),
            None => env::panic_str(&format!("ERR_MAX_MEMBERS: {MAX_MEMBERS}")),
        }
    }
}

#[near]
impl X207Circle {
    /// Removes a `pending` member that has no stake, no debt and no withdrawal in flight, gives the join deposit of an EVM member back to its sponsor
    /// and frees the slot (`member_count` goes down). The owner may do it at any time; anyone else only for a member that was never admitted and has
    /// been `pending` for `PENDING_GC_AFTER_NS`. `ERR_NOT_MEMBER`, `ERR_NOT_PENDING`, `ERR_PENDING_TOO_YOUNG`, `ERR_NOT_OWNER`.
    pub fn remove_pending(&mut self, account_id: AccountId) {
        let by_owner = env::predecessor_account_id() == self.owner_id;
        require!(
            self.members.contains_key(&account_id),
            format!("ERR_NOT_MEMBER: {account_id}")
        );
        require!(
            self.is_empty_pending(&account_id),
            format!("ERR_NOT_PENDING: {account_id} is not a pending member without stake")
        );
        if !by_owner {
            let m = self.members.get(&account_id).expect("member checked above");
            require!(
                !m.admitted && env::block_timestamp() >= m.joined_at.saturating_add(PENDING_GC_AFTER_NS),
                format!("ERR_PENDING_TOO_YOUNG: only the owner removes {account_id} before it has been pending for 7 days or after it was admitted")
            );
        }
        self.drop_pending(&account_id, by_owner);
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::{testing_env, NearToken, PublicKey};

    use super::*;
    use crate::member::JOIN_STORAGE_DEPOSIT;
    use crate::params::Params;

    const KEY: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";
    const HASH: &str = "9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f";

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn as_caller(caller: &str, deposit: NearToken, now_ns: u64) {
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc(caller))
            .attached_deposit(deposit)
            .block_timestamp(now_ns)
            .build());
    }

    fn full_circle() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        let mut c = X207Circle::with_config(
            acc("dao.near"),
            "agents-main".to_string(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            1_000,
        );
        for i in 0..MAX_MEMBERS {
            as_caller(
                &format!("m{i}.near"),
                JOIN_STORAGE_DEPOSIT,
                5_000 + u64::from(i),
            );
            c.join(
                "agents-main".to_string(),
                KEY.parse::<PublicKey>().unwrap(),
                HASH.to_string(),
            );
        }
        assert_eq!(c.member_count, MAX_MEMBERS);
        c
    }

    fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("expected a panic");
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    }

    // TC-CT-123-01: the owner removes a pending member at any time; the slot, the index and the event follow
    #[test]
    fn tc_ct_123_01_owner_removes_pending_and_frees_the_slot() {
        let mut c = full_circle();
        as_caller("dao.near", NearToken::from_yoctonear(0), 6_000);
        c.remove_pending(acc("m3.near"));
        assert_eq!(c.member_count, MAX_MEMBERS - 1);
        assert!(c.member(acc("m3.near")).is_none());
        assert_eq!(c.member_index.len(), MAX_MEMBERS - 1);
        assert!(!c.member_index.iter().any(|a| a == &acc("m3.near")));
        assert!(get_logs()
            .iter()
            .any(|l| l.contains("member_removed") && l.contains("\"by_owner\":true")));
        // the slot is usable at once
        as_caller("late.near", JOIN_STORAGE_DEPOSIT, 7_000);
        c.join(
            "agents-main".to_string(),
            KEY.parse::<PublicKey>().unwrap(),
            HASH.to_string(),
        );
        assert_eq!(c.member_count, MAX_MEMBERS);
    }

    // TC-CT-123-02: a stranger may not remove a young row, an unknown account, or a member that has a stake or is not pending
    #[test]
    fn tc_ct_123_02_refusals() {
        let mut c = full_circle();
        as_caller("eve.near", NearToken::from_yoctonear(0), 6_000);
        let young = panic_text(std::panic::AssertUnwindSafe(|| {
            c.remove_pending(acc("m3.near"))
        }));
        assert!(young.contains("ERR_PENDING_TOO_YOUNG"), "{young}");
        let unknown = panic_text(std::panic::AssertUnwindSafe(|| {
            c.remove_pending(acc("nobody.near"))
        }));
        assert!(unknown.contains("ERR_NOT_MEMBER"), "{unknown}");
        c.members.get_mut(&acc("m4.near")).unwrap().stake = 1;
        as_caller("dao.near", NearToken::from_yoctonear(0), 6_000);
        let staked = panic_text(std::panic::AssertUnwindSafe(|| {
            c.remove_pending(acc("m4.near"))
        }));
        assert!(staked.contains("ERR_NOT_PENDING"), "{staked}");
        c.members.get_mut(&acc("m5.near")).unwrap().status = MemberStatus::Active;
        let active = panic_text(std::panic::AssertUnwindSafe(|| {
            c.remove_pending(acc("m5.near"))
        }));
        assert!(active.contains("ERR_NOT_PENDING"), "{active}");
        c.outstanding_debt.insert(acc("m6.near"), 5);
        let debt = panic_text(std::panic::AssertUnwindSafe(|| {
            c.remove_pending(acc("m6.near"))
        }));
        assert!(debt.contains("ERR_NOT_PENDING"), "{debt}");
        assert_eq!(c.member_count, MAX_MEMBERS);
    }

    // TC-CT-123-03: after 7 days anyone may remove a never-admitted pending row; an admitted one stays for the owner
    #[test]
    fn tc_ct_123_03_garbage_collection_after_seven_days() {
        let mut c = full_circle();
        c.members.get_mut(&acc("m7.near")).unwrap().admitted = true;
        let later = 5_000 + PENDING_GC_AFTER_NS + 100;
        as_caller("eve.near", NearToken::from_yoctonear(0), later);
        c.remove_pending(acc("m3.near"));
        assert_eq!(c.member_count, MAX_MEMBERS - 1);
        assert!(get_logs()
            .iter()
            .any(|l| l.contains("member_removed") && l.contains("\"by_owner\":false")));
        let admitted = panic_text(std::panic::AssertUnwindSafe(|| {
            c.remove_pending(acc("m7.near"))
        }));
        assert!(admitted.contains("ERR_PENDING_TOO_YOUNG"), "{admitted}");
    }

    // TC-CT-123-04 (A-CT-01): 64 pending rows do not block a new join once the oldest is stale; before that the circle is full
    #[test]
    fn tc_ct_123_04_slot_burn_does_not_lock_the_circle() {
        let mut c = full_circle();
        as_caller("late.near", JOIN_STORAGE_DEPOSIT, 6_000);
        let full = panic_text(std::panic::AssertUnwindSafe(|| {
            c.join(
                "agents-main".to_string(),
                KEY.parse::<PublicKey>().unwrap(),
                HASH.to_string(),
            );
        }));
        assert!(full.contains("ERR_MAX_MEMBERS"), "{full}");
        c.members.get_mut(&acc("m0.near")).unwrap().stake = 1; // a row with stake is never evicted
        as_caller(
            "late.near",
            JOIN_STORAGE_DEPOSIT,
            5_000 + PENDING_GC_AFTER_NS + 1_000,
        );
        c.join(
            "agents-main".to_string(),
            KEY.parse::<PublicKey>().unwrap(),
            HASH.to_string(),
        );
        assert_eq!(c.member_count, MAX_MEMBERS);
        assert!(c.member(acc("late.near")).is_some());
        assert!(
            c.member(acc("m0.near")).is_some(),
            "the row with stake stays"
        );
        assert!(
            c.member(acc("m1.near")).is_none(),
            "the oldest empty pending row was removed"
        );
        assert!(get_logs()
            .iter()
            .any(|l| l.contains("member_removed") && l.contains("m1.near")));
    }
}
