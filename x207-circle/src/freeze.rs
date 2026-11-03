//! `freeze_member` (CT-095, ADR-43): the DAO can stop a member from starting anything new, after a timelock, and move no money. The member can still pay what it
//! owes, claim, add stake, leave and take its stake. Lifting the freeze is at once; applying it waits `FREEZE_TIMELOCK_NS`, which is the warning to the member.
//!
//! A freeze takes effect from the epoch in which `apply_freeze` runs: vouchers of that epoch still register until the cutoff (so a seller that already handed over
//! its goods can register the voucher of a frozen buyer, `register_in_bundle` of a committed bundle too); from the next epoch on `register_*` refuses a frozen payer
//! or payee. `commit_bundle`, `post_intent`, `offer_grant`, `accept_grant` and `complete_escrow` refuse a frozen account at once.

use near_sdk::borsh::{self, BorshDeserialize};
use near_sdk::json_types::U64;
use near_sdk::{env, near, require, AccountId};

use crate::events::{FreezeCancelled, FreezeProposed, MemberFrozen, MemberUnfrozen, X207Event};
use crate::state::{X207Circle, X207CircleExt};
use crate::types::EpochId;

pub const FREEZE_TIMELOCK_NS: u64 = 24 * 3600 * 1_000_000_000;
const PREFIX: &[u8] = b"x207_freeze:";

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FreezeState {
    Pending,
    Frozen,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshDeserialize, near_sdk::borsh::BorshSerialize)]
#[borsh(crate = "near_sdk::borsh")]
pub struct FreezeRecord {
    pub state: u8,
    pub eta: u64,
    pub reason_hash: Option<String>,
    /// the epoch in which `apply_freeze` ran (0 while pending)
    pub from_epoch: EpochId,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreezeView {
    pub state: FreezeState,
    pub eta: U64,
    pub reason_hash: Option<String>,
    pub from_epoch: EpochId,
}

fn key(account: &AccountId) -> Vec<u8> {
    [PREFIX, account.as_bytes()].concat()
}

pub fn freeze_of(account: &AccountId) -> Option<FreezeRecord> {
    env::storage_read(&key(account)).map(|b| borsh::from_slice(&b).expect("freeze record is valid borsh"))
}

fn save(account: &AccountId, record: &FreezeRecord) {
    env::storage_write(&key(account), &borsh::to_vec(record).expect("borsh"));
}

const PENDING: u8 = 0;
const FROZEN: u8 = 1;

/// `true` once `apply_freeze` has run and until `unfreeze_member`.
pub fn member_is_frozen(account: &AccountId) -> bool {
    freeze_of(account).is_some_and(|r| r.state == FROZEN)
}

/// Refuses a frozen account for something new (`ERR_FROZEN`).
pub fn assert_not_frozen(account: &AccountId, what: &str) {
    require!(!member_is_frozen(account), format!("ERR_FROZEN: {account} is frozen and cannot {what}"));
}

/// Refuses a frozen account only for a voucher of an epoch AFTER the one in which the freeze took effect. A seller that handed over its goods before the freeze can
/// still register the voucher of a frozen buyer until the cutoff of the epoch of the freeze.
pub fn assert_not_frozen_for_voucher(account: &AccountId, voucher_epoch: EpochId) {
    if let Some(r) = freeze_of(account) {
        require!(r.state != FROZEN || voucher_epoch <= r.from_epoch, format!("ERR_FROZEN: {account} is frozen since epoch {}, a voucher of epoch {voucher_epoch} is refused", r.from_epoch));
    }
}

#[near]
impl X207Circle {
    /// Owner only (the DAO, by a proposal). Starts the timelock; the member sees `freeze_proposed` and can settle, finish its deals or leave before it applies.
    /// `ERR_NOT_OWNER`, `ERR_NOT_MEMBER`, `ERR_BAD_HASH`, `ERR_FREEZE_PENDING`, `ERR_ALREADY_FROZEN`.
    pub fn propose_freeze(&mut self, account_id: AccountId, reason_hash: Option<String>) {
        self.assert_owner();
        require!(self.members.get(&account_id).is_some(), format!("ERR_NOT_MEMBER: {account_id}"));
        if let Some(h) = &reason_hash {
            require!(h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "ERR_BAD_HASH: reason_hash is 64 lowercase hex");
        }
        match freeze_of(&account_id) {
            Some(r) if r.state == PENDING => env::panic_str(&format!("ERR_FREEZE_PENDING: {account_id}, eta {}", r.eta)),
            Some(_) => env::panic_str(&format!("ERR_ALREADY_FROZEN: {account_id}")),
            None => {}
        }
        let eta = env::block_timestamp() + FREEZE_TIMELOCK_NS;
        save(&account_id, &FreezeRecord { state: PENDING, eta, reason_hash: reason_hash.clone(), from_epoch: 0 });
        FreezeProposed { account_id, eta: U64(eta), reason_hash }.emit();
    }

    /// Anybody, once the timelock has run out. `ERR_FREEZE_NOT_PENDING`, `ERR_FREEZE_TIMELOCK`.
    pub fn apply_freeze(&mut self, account_id: AccountId) {
        let Some(mut r) = freeze_of(&account_id).filter(|r| r.state == PENDING) else { env::panic_str(&format!("ERR_FREEZE_NOT_PENDING: {account_id}")) };
        let now = env::block_timestamp();
        require!(now >= r.eta, format!("ERR_FREEZE_TIMELOCK: eta {} now {now}", r.eta));
        r.state = FROZEN;
        r.from_epoch = self.current_epoch;
        save(&account_id, &r);
        MemberFrozen { account_id, from_epoch: self.current_epoch }.emit();
    }

    /// Owner only, while the freeze is pending. `ERR_NOT_OWNER`, `ERR_FREEZE_NOT_PENDING`.
    pub fn cancel_freeze(&mut self, account_id: AccountId) {
        self.assert_owner();
        require!(freeze_of(&account_id).is_some_and(|r| r.state == PENDING), format!("ERR_FREEZE_NOT_PENDING: {account_id}"));
        env::storage_remove(&key(&account_id));
        FreezeCancelled { account_id }.emit();
    }

    /// Owner only, at once: lifting a restriction needs no waiting. `ERR_NOT_OWNER`, `ERR_NOT_FROZEN`.
    pub fn unfreeze_member(&mut self, account_id: AccountId) {
        self.assert_owner();
        require!(member_is_frozen(&account_id), format!("ERR_NOT_FROZEN: {account_id}"));
        env::storage_remove(&key(&account_id));
        MemberUnfrozen { account_id }.emit();
    }

    pub fn get_freeze(&self, account_id: AccountId) -> Option<FreezeView> {
        freeze_of(&account_id).map(|r| FreezeView {
            state: if r.state == FROZEN { FreezeState::Frozen } else { FreezeState::Pending },
            eta: U64(r.eta),
            reason_hash: r.reason_hash,
            from_epoch: r.from_epoch,
        })
    }

    pub fn is_frozen(&self, account_id: AccountId) -> bool {
        member_is_frozen(&account_id)
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::{testing_env, PublicKey};

    use super::*;
    use crate::member::{Member, MemberStatus};
    use crate::params::Params;

    const T0: u64 = 1_000;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn as_caller(caller: &str, ts: u64) {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc(caller)).block_timestamp(ts).build());
    }

    fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("expected a panic");
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    fn circle() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(T0).build());
        let mut c = X207Circle::with_config(acc("dao.near"), "agents-main".into(), acc("usdt.near"), acc("intents.near"), Params::default(), T0);
        let member = Member {
            account_id: acc("bob.near"),
            pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse::<PublicKey>().unwrap(),
            terms_hash: "a".repeat(64),
            stake: 100,
            limit: 80,
            status: MemberStatus::Active,
            admitted: false,
            joined_at: T0,
            settled_history: 0,
            last_active_epoch: 1,
            exit_requested_epoch: None,
            withdraw_pending: false,
            reactivate_epoch: None,
        };
        c.members.insert(acc("bob.near"), member);
        c.member_index.push(acc("bob.near"));
        c.member_count += 1;
        c
    }

    // TC-CT-095-U01: propose -> timelock -> apply, and the views and events on the way
    #[test]
    fn a_freeze_waits_for_the_timelock_and_then_applies_from_the_current_epoch() {
        let mut c = circle();
        as_caller("dao.near", T0);
        c.propose_freeze(acc("bob.near"), Some("ab".repeat(32)));
        let eta = T0 + FREEZE_TIMELOCK_NS;
        assert_eq!(c.get_freeze(acc("bob.near")), Some(FreezeView { state: FreezeState::Pending, eta: U64(eta), reason_hash: Some("ab".repeat(32)), from_epoch: 0 }));
        assert!(!c.is_frozen(acc("bob.near")), "pending is not frozen");
        assert!(get_logs().iter().any(|l| l.contains("freeze_proposed") && l.contains(&format!("\"eta\":\"{eta}\""))));
        as_caller("anybody.near", eta - 1);
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.apply_freeze(acc("bob.near")))).contains("ERR_FREEZE_TIMELOCK"));
        as_caller("anybody.near", eta);
        c.apply_freeze(acc("bob.near"));
        assert!(c.is_frozen(acc("bob.near")));
        assert_eq!(c.get_freeze(acc("bob.near")).map(|v| (v.state, v.from_epoch)), Some((FreezeState::Frozen, c.current_epoch)));
        assert!(get_logs().iter().any(|l| l.contains("member_frozen")));
    }

    // TC-CT-095-U02: who may do what, and the refusals
    #[test]
    fn only_the_owner_proposes_cancels_and_lifts_and_every_wrong_state_is_refused() {
        let mut c = circle();
        as_caller("mallory.near", T0);
        for f in [
            Box::new(|c: &mut X207Circle| c.propose_freeze(acc("bob.near"), None)) as Box<dyn Fn(&mut X207Circle)>,
            Box::new(|c: &mut X207Circle| c.cancel_freeze(acc("bob.near"))),
            Box::new(|c: &mut X207Circle| c.unfreeze_member(acc("bob.near"))),
        ] {
            assert!(panic_text(std::panic::AssertUnwindSafe(|| f(&mut c))).contains("ERR_NOT_OWNER"));
        }
        as_caller("dao.near", T0);
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.propose_freeze(acc("ghost.near"), None))).contains("ERR_NOT_MEMBER"));
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.propose_freeze(acc("bob.near"), Some("XYZ".into())))).contains("ERR_BAD_HASH"));
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.apply_freeze(acc("bob.near")))).contains("ERR_FREEZE_NOT_PENDING"));
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.cancel_freeze(acc("bob.near")))).contains("ERR_FREEZE_NOT_PENDING"));
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.unfreeze_member(acc("bob.near")))).contains("ERR_NOT_FROZEN"));
        c.propose_freeze(acc("bob.near"), None);
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.propose_freeze(acc("bob.near"), None))).contains("ERR_FREEZE_PENDING"));
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.unfreeze_member(acc("bob.near")))).contains("ERR_NOT_FROZEN"), "a pending freeze is cancelled, not lifted");
        c.cancel_freeze(acc("bob.near"));
        assert_eq!(c.get_freeze(acc("bob.near")), None);
        assert!(get_logs().iter().any(|l| l.contains("freeze_cancelled")));
        c.propose_freeze(acc("bob.near"), None);
        as_caller("anybody.near", T0 + FREEZE_TIMELOCK_NS);
        c.apply_freeze(acc("bob.near"));
        as_caller("dao.near", T0 + FREEZE_TIMELOCK_NS);
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.propose_freeze(acc("bob.near"), None))).contains("ERR_ALREADY_FROZEN"));
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.cancel_freeze(acc("bob.near")))).contains("ERR_FREEZE_NOT_PENDING"));
        c.unfreeze_member(acc("bob.near"));
        assert!(!c.is_frozen(acc("bob.near")) && c.get_freeze(acc("bob.near")).is_none());
        assert!(get_logs().iter().any(|l| l.contains("member_unfrozen")));
    }

    // TC-CT-095-U03: the rule for vouchers: refused only after the epoch of the freeze, never while pending; the other things are refused at once
    #[test]
    fn vouchers_of_the_epoch_of_the_freeze_pass_and_those_of_later_epochs_do_not() {
        let mut c = circle();
        as_caller("dao.near", T0);
        c.propose_freeze(acc("bob.near"), None);
        assert_eq!(panic_text_ok(|| assert_not_frozen_for_voucher(&acc("bob.near"), 99)), "ok", "pending blocks nothing");
        assert_eq!(panic_text_ok(|| assert_not_frozen(&acc("bob.near"), "x")), "ok");
        as_caller("anybody.near", T0 + FREEZE_TIMELOCK_NS);
        c.apply_freeze(acc("bob.near"));
        let from = c.current_epoch;
        assert_eq!(panic_text_ok(|| assert_not_frozen_for_voucher(&acc("bob.near"), from)), "ok", "the epoch of the freeze still registers");
        assert!(panic_text(|| assert_not_frozen_for_voucher(&acc("bob.near"), from + 1)).contains("ERR_FROZEN"));
        assert!(panic_text(|| assert_not_frozen(&acc("bob.near"), "post an intent")).contains("ERR_FROZEN"));
        assert_eq!(panic_text_ok(|| assert_not_frozen(&acc("alice.near"), "x")), "ok", "an account that is not frozen is not looked at");
    }

    // TC-CT-095-U04 (review C2): a freeze applied while epoch 4 is clearing is recorded in the open epoch 5, so a voucher of epoch 5 still registers and one of epoch 6 does not
    #[test]
    fn a_freeze_applied_while_an_epoch_clears_is_recorded_in_the_epoch_that_is_open() {
        let mut c = circle();
        c.current_epoch = 5;
        as_caller("dao.near", T0);
        c.propose_freeze(acc("bob.near"), None);
        as_caller("anybody.near", T0 + FREEZE_TIMELOCK_NS);
        c.apply_freeze(acc("bob.near"));
        assert_eq!(c.get_freeze(acc("bob.near")).map(|v| v.from_epoch), Some(5));
        assert_eq!(panic_text_ok(|| assert_not_frozen_for_voucher(&acc("bob.near"), 5)), "ok");
        assert!(panic_text(|| assert_not_frozen_for_voucher(&acc("bob.near"), 6)).contains("ERR_FROZEN"));
    }

    fn panic_text_ok(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        match std::panic::catch_unwind(f) {
            Ok(()) => "ok".into(),
            Err(e) => e.downcast_ref::<String>().cloned().unwrap_or_default(),
        }
    }
}
