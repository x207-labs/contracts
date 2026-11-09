//! x207 account locker (CT-125, contract 0.0.18, P1). Design: `docs/adr-0.0.18.md`, part 1.
//!
//! The seller deploys this contract on the account that is for sale, calls `new` and deletes every access key of the account, all in ONE transaction
//! (a batch is atomic, so there is no moment with the locker and the seller's keys). From then on only the circle can move the keys:
//!
//! - `release(grant_id, pubkey)`: the circle hands the account to the buyer (a full-access key is added);
//! - `give_back(grant_id)`: the circle hands the account back to the seller (`return_pubkey`), for a cancelled or expired sale.
//!
//! `new` registers the lock at the circle (`account_on_lock(grant_id)`). If the circle refuses (grant not offered, wrong scope, ...) the callback
//! `on_registered` adds `return_pubkey`, so a refused registration never leaves an account without keys.
//!
//! The locker never reads keys or code of other accounts and holds no funds: the balance of the account moves with its keys. After `release` the code
//! stays on the account; its entry points need `predecessor == circle` and a state that cannot come back to `Held`, so it cannot change keys again.

use near_sdk::serde_json::json;
use near_sdk::{env, near, require, AccountId, Gas, NearToken, PanicOnDefault, Promise, PublicKey};

/// Gas of `circle.account_on_lock`: a few storage writes and one event.
const REGISTER_GAS: Gas = Gas::from_tgas(20);
/// Gas reserved for `on_registered`: one promise result and at most one `AddKey` action.
const CALLBACK_GAS: Gas = Gas::from_tgas(10);

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LockState {
    /// `new` ran; the circle has not answered yet.
    Pending,
    /// The circle holds the escrow of this account.
    Held,
    /// The keys went to the buyer.
    Released,
    /// The keys went back to the seller.
    Returned,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockerView {
    pub circle: AccountId,
    pub grant_id: u64,
    pub return_pubkey: PublicKey,
    pub state: LockState,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Locker {
    circle: AccountId,
    grant_id: u64,
    return_pubkey: PublicKey,
    state: LockState,
}

#[near]
impl Locker {
    /// Only the account itself may call it (the deploy transaction of the seller). Registers the lock at the circle. `ERR_LOCKER_INIT_SELF`.
    #[init]
    pub fn new(circle: AccountId, grant_id: u64, return_pubkey: PublicKey) -> Self {
        require!(
            env::predecessor_account_id() == env::current_account_id(),
            "ERR_LOCKER_INIT_SELF: only the account itself initialises its locker"
        );
        Promise::new(circle.clone())
            .function_call(
                "account_on_lock".to_string(),
                json!({ "grant_id": grant_id }).to_string().into_bytes(),
                NearToken::from_yoctonear(0),
                REGISTER_GAS,
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(CALLBACK_GAS)
                    .on_registered(),
            )
            .detach();
        Self {
            circle,
            grant_id,
            return_pubkey,
            state: LockState::Pending,
        }
    }

    /// Callback of `account_on_lock`. Accepted: `held` (unless the circle already moved the keys). Refused: the keys go back to the seller.
    #[private]
    pub fn on_registered(&mut self) {
        if self.state != LockState::Pending {
            return;
        }
        if env::promise_result_checked(0, 16).is_ok() {
            self.state = LockState::Held;
        } else {
            self.state = LockState::Returned;
            self.add_full_access_key(self.return_pubkey.clone());
        }
    }

    /// The circle hands the account to the buyer. `ERR_LOCKER_NOT_CIRCLE`, `ERR_LOCKER_GRANT`, `ERR_LOCKER_STATE`.
    pub fn release(&mut self, grant_id: u64, pubkey: PublicKey) {
        self.assert_circle_and_grant(grant_id);
        self.state = LockState::Released;
        self.add_full_access_key(pubkey);
    }

    /// The circle hands the account back to the seller (`return_pubkey`). Same checks as `release`.
    pub fn give_back(&mut self, grant_id: u64) {
        self.assert_circle_and_grant(grant_id);
        self.state = LockState::Returned;
        self.add_full_access_key(self.return_pubkey.clone());
    }

    pub fn get_state(&self) -> LockerView {
        LockerView {
            circle: self.circle.clone(),
            grant_id: self.grant_id,
            return_pubkey: self.return_pubkey.clone(),
            state: self.state,
        }
    }
}

impl Locker {
    /// `release` and `give_back` work in `pending` as well as `held`: the circle registers the lock one block before `on_registered` runs, and a sale
    /// that completes in between must not fail. A state `released` or `returned` is final.
    fn assert_circle_and_grant(&self, grant_id: u64) {
        require!(
            env::predecessor_account_id() == self.circle,
            format!("ERR_LOCKER_NOT_CIRCLE: only {} moves the keys", self.circle)
        );
        require!(
            grant_id == self.grant_id,
            format!(
                "ERR_LOCKER_GRANT: this locker belongs to grant {}",
                self.grant_id
            )
        );
        require!(
            matches!(self.state, LockState::Pending | LockState::Held),
            format!("ERR_LOCKER_STATE: {:?}", self.state)
        );
    }

    fn add_full_access_key(&self, key: PublicKey) {
        Promise::new(env::current_account_id())
            .add_full_access_key(key)
            .detach();
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{testing_env, PromiseResult};

    use super::*;

    const SELLER: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";
    const BUYER: &str = "ed25519:7Yk4CPJpnV2VAwYTvHdUKRrHSQbCE8SiJBB9UCjSQbVM";

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn key(s: &str) -> PublicKey {
        s.parse().unwrap()
    }

    fn as_caller(caller: &str) {
        testing_env!(VMContextBuilder::new()
            .current_account_id(acc("sold.testnet"))
            .predecessor_account_id(acc(caller))
            .build());
    }

    fn with_result(caller: &str, result: PromiseResult) {
        testing_env!(
            VMContextBuilder::new()
                .current_account_id(acc("sold.testnet"))
                .predecessor_account_id(acc(caller))
                .build(),
            near_sdk::test_vm_config(),
            near_sdk::RuntimeFeesConfig::test(),
            Default::default(),
            vec![result]
        );
    }

    fn fresh() -> Locker {
        as_caller("sold.testnet");
        Locker::new(acc("circle.testnet"), 7, key(SELLER))
    }

    fn held() -> Locker {
        let mut l = fresh();
        with_result("sold.testnet", PromiseResult::Successful(vec![]));
        l.on_registered();
        l
    }

    fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("expected a panic");
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    }

    // TC-CT-125-01: only the account itself may initialise the locker, and the state starts as pending
    #[test]
    fn tc_ct_125_01_init_is_self_only() {
        let l = fresh();
        assert_eq!(
            l.get_state(),
            LockerView {
                circle: acc("circle.testnet"),
                grant_id: 7,
                return_pubkey: key(SELLER),
                state: LockState::Pending
            }
        );
        as_caller("eve.testnet");
        let text = panic_text(|| {
            Locker::new(acc("circle.testnet"), 7, key(SELLER));
        });
        assert!(text.contains("ERR_LOCKER_INIT_SELF"), "{text}");
    }

    // TC-CT-125-02: the circle accepted: held; a refusal returns the keys to the seller
    #[test]
    fn tc_ct_125_02_registration_accepted_or_refused() {
        let l = held();
        assert_eq!(l.get_state().state, LockState::Held);
        let mut refused = fresh();
        with_result("sold.testnet", PromiseResult::Failed);
        refused.on_registered();
        assert_eq!(refused.get_state().state, LockState::Returned);
    }

    // TC-CT-125-03: a late second answer of the circle changes nothing (that `on_registered` is `#[private]` is the macro's check, covered in the sandbox test)
    #[test]
    fn tc_ct_125_03_late_second_answer_changes_nothing() {
        let mut l = held();
        with_result("sold.testnet", PromiseResult::Failed);
        l.on_registered(); // a late second answer changes nothing
        assert_eq!(l.get_state().state, LockState::Held);
    }

    // TC-CT-125-04: release by the circle for the right grant, in held
    #[test]
    fn tc_ct_125_04_release_by_the_circle() {
        let mut l = held();
        as_caller("circle.testnet");
        l.release(7, key(BUYER));
        assert_eq!(l.get_state().state, LockState::Released);
    }

    // TC-CT-125-05: release or give_back from anybody else (the account itself, a fake circle) fails
    #[test]
    fn tc_ct_125_05_other_callers_are_refused() {
        for caller in ["sold.testnet", "fake-circle.testnet", "seller.testnet"] {
            let mut l = held();
            as_caller(caller);
            let text = panic_text(std::panic::AssertUnwindSafe(|| l.release(7, key(BUYER))));
            assert!(text.contains("ERR_LOCKER_NOT_CIRCLE"), "{caller}: {text}");
            let text = panic_text(std::panic::AssertUnwindSafe(|| l.give_back(7)));
            assert!(text.contains("ERR_LOCKER_NOT_CIRCLE"), "{caller}: {text}");
            assert_eq!(l.get_state().state, LockState::Held);
        }
    }

    // TC-CT-125-06: wrong grant id
    #[test]
    fn tc_ct_125_06_wrong_grant() {
        let mut l = held();
        as_caller("circle.testnet");
        let text = panic_text(std::panic::AssertUnwindSafe(|| l.release(8, key(BUYER))));
        assert!(text.contains("ERR_LOCKER_GRANT"), "{text}");
        let text = panic_text(std::panic::AssertUnwindSafe(|| l.give_back(8)));
        assert!(text.contains("ERR_LOCKER_GRANT"), "{text}");
    }

    // TC-CT-125-07: a final state cannot be left: no second release, no give_back after release, no release after give_back
    #[test]
    fn tc_ct_125_07_final_states() {
        let mut l = held();
        as_caller("circle.testnet");
        l.release(7, key(BUYER));
        for _ in 0..2 {
            let text = panic_text(std::panic::AssertUnwindSafe(|| l.release(7, key(BUYER))));
            assert!(text.contains("ERR_LOCKER_STATE"), "{text}");
            let text = panic_text(std::panic::AssertUnwindSafe(|| l.give_back(7)));
            assert!(text.contains("ERR_LOCKER_STATE"), "{text}");
        }
        let mut back = held();
        as_caller("circle.testnet");
        back.give_back(7);
        assert_eq!(back.get_state().state, LockState::Returned);
        let text = panic_text(std::panic::AssertUnwindSafe(|| back.release(7, key(BUYER))));
        assert!(text.contains("ERR_LOCKER_STATE"), "{text}");
    }

    // TC-CT-125-08: a sale that completes before the callback ran (pending) is released, and the late callback does not undo it
    #[test]
    fn tc_ct_125_08_release_while_pending_survives_the_late_callback() {
        let mut l = fresh();
        as_caller("circle.testnet");
        l.release(7, key(BUYER));
        with_result("sold.testnet", PromiseResult::Successful(vec![]));
        l.on_registered();
        assert_eq!(l.get_state().state, LockState::Released);
    }
}
