//! `ft_on_transfer` (CT-017, spec 4.1): the entry for USDT sent with `ft_transfer_call`. The shared
//! checks and the `stake` action live here; the `settle` action is `settle_transfer` (CT-032).
//!
//! Every rejection is a panic, so the token's `ft_resolve_transfer` refunds the whole amount and
//! the contract state stays as it was.

use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::{assert_one_yocto, env, near, serde_json, AccountId, Gas, NearToken, Promise, PromiseOrValue};

use crate::clearing::EpochPhase;
use crate::events::{ExitRequested, StakeAdded, StakeWithdrawFailed, StakeWithdrawn, X207Event};
use crate::member::MemberStatus;
use crate::params::Admission;
use crate::risk::{assert_limit_invariant, compute_limit};
use crate::state::{X207Circle, X207CircleExt};
use crate::types::TransferMsg;

fn status_wire(status: MemberStatus) -> &'static str {
    match status {
        MemberStatus::Pending => "pending",
        MemberStatus::Active => "active",
        MemberStatus::Suspended => "suspended",
        MemberStatus::Exited => "exited",
    }
}

impl X207Circle {
    /// Adds `amount` to the stake of `member_id` (default: the sender). Order of checks (spec 4.7):
    /// `ERR_NOT_MEMBER`, `ERR_MEMBER_EXITED`, `ERR_EXITING`, `ERR_NOT_ADMITTED`.
    pub fn stake_from_transfer(&mut self, sender_id: AccountId, amount: u128, member_id: Option<AccountId>) {
        let account = member_id.unwrap_or_else(|| sender_id.clone());
        let params = self.epoch_or_panic(self.current_epoch).params.clone();
        let Some(member) = self.members.get(&account) else {
            env::panic_str(&format!("ERR_NOT_MEMBER: {account}"))
        };
        if member.status == MemberStatus::Exited {
            env::panic_str(&format!("ERR_MEMBER_EXITED: {account}"));
        }
        if member.exit_requested_epoch.is_some() {
            env::panic_str(&format!("ERR_EXITING: {account} asked to exit"));
        }
        if params.admission == Admission::Dao && !member.admitted {
            env::panic_str(&format!("ERR_NOT_ADMITTED: {account}"));
        }

        let member = self.members.get_mut(&account).expect("member checked above");
        member.stake = member.stake.checked_add(amount).unwrap_or_else(|| env::panic_str("ERR_STAKE_OVERFLOW"));
        if member.status == MemberStatus::Pending {
            member.status = MemberStatus::Active;
        }
        // A `suspended` member keeps limit 0 and its status until reactivation (spec 4.1, ADR-30).
        if member.status == MemberStatus::Active {
            member.limit = compute_limit(member.stake, member.settled_history, &params);
        }
        assert_limit_invariant(member);
        let event = StakeAdded {
            account_id: account,
            sender_id,
            amount: U128(amount),
            stake: U128(member.stake),
            limit: U128(member.limit),
            status: status_wire(member.status).to_string(),
        };
        self.d6_deposit_stake(amount);
        event.emit();
    }
}

#[near]
impl X207Circle {
    /// Called by the USDT contract. Returns the amount to refund: `0` when the whole amount was used.
    /// Order of checks: `ERR_WRONG_TOKEN`, `ERR_BAD_MSG`, `ERR_ZERO_AMOUNT`, then the action.
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> PromiseOrValue<U128> {
        let token = env::predecessor_account_id();
        if token != self.usdt_token_id {
            env::panic_str(&format!("ERR_WRONG_TOKEN: {token}"));
        }
        let msg: TransferMsg = serde_json::from_str(&msg).unwrap_or_else(|e| env::panic_str(&format!("ERR_BAD_MSG: {e}")));
        if amount.0 == 0 {
            env::panic_str("ERR_ZERO_AMOUNT");
        }
        match msg {
            TransferMsg::Stake { member_id } => {
                self.stake_from_transfer(sender_id, amount.0, member_id);
                PromiseOrValue::Value(U128(0))
            }
            TransferMsg::Settle { epoch, member_id } => self.settle_transfer(sender_id, amount, epoch, member_id),
        }
    }
}

/// Gas of the exit transfer and its callback (spec section 14); final values come from CT-043.
pub const WITHDRAW_TRANSFER_GAS: Gas = Gas::from_tgas(15);
pub const WITHDRAW_CALLBACK_GAS: Gas = Gas::from_tgas(15);

#[near]
impl X207Circle {
    /// ADR-29: the member asks to leave at the end of the current epoch. Vouchers for later epochs
    /// and more stake are refused with `ERR_EXITING` from now on; the status stays until `exited`.
    #[payable]
    pub fn request_exit(&mut self) {
        assert_one_yocto();
        let account = env::predecessor_account_id();
        let epoch = self.current_epoch;
        let Some(member) = self.members.get_mut(&account) else { env::panic_str(&format!("ERR_NOT_MEMBER: {account}")) };
        if !matches!(member.status, MemberStatus::Active | MemberStatus::Suspended) || member.exit_requested_epoch.is_some() {
            env::panic_str(&format!("ERR_WRONG_STATUS: {account} is {}", status_wire(member.status)));
        }
        member.exit_requested_epoch = Some(epoch);
        ExitRequested { account_id: account, epoch }.emit();
    }

    /// Pays the stake back once the exit epoch is `finalized` and no debt remains. Order of checks:
    /// `ERR_NOT_MEMBER`, `ERR_MEMBER_EXITED`, `ERR_EXIT_NOT_READY` (no request), `ERR_WITHDRAW_IN_FLIGHT`,
    /// `ERR_EXIT_NOT_READY` (epoch not final), `ERR_HAS_DUE`. A stake of 0 exits at once.
    #[payable]
    pub fn withdraw_stake(&mut self) -> PromiseOrValue<bool> {
        assert_one_yocto();
        let account = env::predecessor_account_id();
        let Some(member) = self.members.get(&account) else { env::panic_str(&format!("ERR_NOT_MEMBER: {account}")) };
        if member.status == MemberStatus::Exited {
            env::panic_str(&format!("ERR_MEMBER_EXITED: {account}"));
        }
        let Some(exit_epoch) = member.exit_requested_epoch else {
            env::panic_str(&format!("ERR_EXIT_NOT_READY: {account} has not asked to exit"))
        };
        if member.withdraw_pending {
            env::panic_str(&format!("ERR_WITHDRAW_IN_FLIGHT: {account}"));
        }
        if self.epochs.get(&exit_epoch).map(|e| e.phase) != Some(EpochPhase::Finalized) {
            env::panic_str(&format!("ERR_EXIT_NOT_READY: epoch {exit_epoch} is not finalized"));
        }
        let debt = self.outstanding_debt.get(&account).copied().unwrap_or(0);
        if debt > 0 {
            env::panic_str(&format!("ERR_HAS_DUE: {account} owes {debt}"));
        }

        let member = self.members.get_mut(&account).expect("member checked above");
        let amount = member.stake;
        if amount == 0 {
            member.status = MemberStatus::Exited;
            member.limit = 0;
            StakeWithdrawn { account_id: account, amount: U128(0) }.emit();
            return PromiseOrValue::Value(true);
        }
        member.stake = 0;
        member.limit = 0;
        member.withdraw_pending = true;
        self.d6_stake_to_in_flight(amount);
        let transfer = Promise::new(self.usdt_token_id.clone()).function_call(
            "ft_transfer".to_string(),
            json!({"receiver_id": account, "amount": U128(amount), "memo": null}).to_string().into_bytes(),
            NearToken::from_yoctonear(1),
            WITHDRAW_TRANSFER_GAS,
        );
        PromiseOrValue::Promise(transfer.then(
            Self::ext(env::current_account_id()).with_static_gas(WITHDRAW_CALLBACK_GAS).on_withdraw_resolved(account, U128(amount)),
        ))
    }

    /// Callback of the exit transfer. Never panics. Success: `exited`. Failure: the stake and the
    /// limit come back (the limit as `compute_limit` of the restored stake) and the member may retry.
    #[private]
    pub fn on_withdraw_resolved(&mut self, account_id: AccountId, amount: U128) -> bool {
        let sent = amount.0;
        let ok = env::promise_result_checked(0, 64).is_ok();
        let epoch_params = self.epochs.get(&self.current_epoch).map(|e| e.params.clone());
        let Some(member) = self.members.get_mut(&account_id) else { return false };
        if !member.withdraw_pending {
            return false;
        }
        member.withdraw_pending = false;
        if ok {
            member.status = MemberStatus::Exited;
            self.d6_in_flight_out(sent);
            StakeWithdrawn { account_id, amount }.emit();
            return true;
        }
        member.stake = sent;
        if let (MemberStatus::Active, Some(params)) = (member.status, epoch_params) {
            member.limit = compute_limit(member.stake, member.settled_history, &params);
        }
        self.d6_in_flight_to_stake(sent);
        StakeWithdrawFailed { account_id, amount }.emit();
        false
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::serde_json::Value;
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::{testing_env, PublicKey};

    use super::*;
    use crate::member::Member;
    use crate::params::Params;

    const CREDIT: u128 = 1_000_000;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn as_caller(caller: &str) {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc(caller)).block_timestamp(1_000).build());
    }

    fn circle_with(params: Params) -> X207Circle {
        as_caller("deployer.near");
        X207Circle::with_config(acc("dao.near"), "agents-main".into(), acc("usdt.near"), acc("intents.near"), params, 1_000)
    }

    fn circle() -> X207Circle {
        circle_with(Params::default())
    }

    fn add(c: &mut X207Circle, id: &str, status: MemberStatus, stake: u128, limit: u128) {
        c.members.insert(
            acc(id),
            Member {
                account_id: acc(id),
                pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse::<PublicKey>().unwrap(),
                terms_hash: "a".repeat(64),
                stake,
                limit,
                status,
                admitted: false,
                joined_at: 1,
                settled_history: 0,
                last_active_epoch: 1,
                exit_requested_epoch: None,
                withdraw_pending: false,
                reactivate_epoch: None,
            },
        );
        c.member_index.push(acc(id));
        c.member_count += 1;
        c.d6_deposit_stake(stake);
    }

    fn pending() -> X207Circle {
        let mut c = circle();
        add(&mut c, "translator.near", MemberStatus::Pending, 0, 0);
        c
    }

    fn transfer(c: &mut X207Circle, token: &str, sender: &str, amount: u128, msg: &str) -> PromiseOrValue<U128> {
        as_caller(token);
        c.ft_on_transfer(acc(sender), U128(amount), msg.to_string())
    }

    /// A valid stake transfer; the whole amount is used, so nothing is refunded.
    fn stake(c: &mut X207Circle, sender: &str, amount: u128) {
        zero(transfer(c, "usdt.near", sender, amount, r#"{"action":"stake"}"#));
    }

    fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).unwrap_err();
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    fn rejected(c: &mut X207Circle, token: &str, sender: &str, amount: u128, msg: &str) -> String {
        panic_text(std::panic::AssertUnwindSafe(|| drop(transfer(c, token, sender, amount, msg))))
    }

    fn zero(result: PromiseOrValue<U128>) {
        match result {
            PromiseOrValue::Value(v) => assert_eq!(v.0, 0),
            PromiseOrValue::Promise(_) => panic!("expected a value"),
        }
    }

    fn view(c: &X207Circle, id: &str) -> crate::member::MemberView {
        c.member(acc(id)).unwrap()
    }

    // TC-CT-017-01
    #[test]
    fn valid_stake_activates_the_member() {
        let mut c = pending();
        let _ = get_logs();
        stake(&mut c, "translator.near", 100 * CREDIT);
        let m = view(&c, "translator.near");
        assert_eq!((m.status, m.stake.0, m.limit.0), (MemberStatus::Active, 100 * CREDIT, 80 * CREDIT));
        assert_eq!(c.total_stake, 100 * CREDIT);
        let log = get_logs().into_iter().find_map(|l| l.strip_prefix("EVENT_JSON:").map(str::to_string)).unwrap();
        let event: Value = serde_json::from_str(&log).unwrap();
        assert_eq!(event["event"], "stake_added");
        let data = &event["data"][0];
        assert_eq!(data["account_id"], "translator.near");
        assert_eq!(data["sender_id"], "translator.near");
        assert_eq!((data["amount"].as_str(), data["stake"].as_str(), data["limit"].as_str(), data["status"].as_str()), (Some("100000000"), Some("100000000"), Some("80000000"), Some("active")));
        c.assert_d6(Some(100 * CREDIT));
    }

    // TC-CT-017-02
    #[test]
    fn stake_on_behalf_of_an_agent() {
        let mut c = pending();
        let _ = get_logs();
        zero(transfer(&mut c, "usdt.near", "outsider.near", 100 * CREDIT, r#"{"action":"stake","member_id":"translator.near"}"#));
        assert_eq!(view(&c, "translator.near").stake.0, 100 * CREDIT);
        let log = get_logs().into_iter().find(|l| l.contains("stake_added")).unwrap();
        assert!(log.contains(r#""sender_id":"outsider.near""#) && log.contains(r#""account_id":"translator.near""#), "{log}");
    }

    // TC-CT-017-03
    #[test]
    fn extra_stake_raises_the_limit() {
        let mut c = pending();
        stake(&mut c, "translator.near", 100 * CREDIT);
        stake(&mut c, "translator.near", 50 * CREDIT);
        let m = view(&c, "translator.near");
        assert_eq!((m.status, m.stake.0, m.limit.0), (MemberStatus::Active, 150 * CREDIT, 120 * CREDIT));
        c.assert_d6(Some(150 * CREDIT));
    }

    // TC-CT-017-04, TC-CT-017-05
    #[test]
    fn only_the_usdt_contract_may_call() {
        let mut c = pending();
        for caller in ["usdt_palsu.near", "outsider.near"] {
            assert!(rejected(&mut c, caller, "translator.near", 100 * CREDIT, r#"{"action":"stake"}"#).contains("ERR_WRONG_TOKEN"), "{caller}");
        }
        assert_eq!(view(&c, "translator.near").stake.0, 0);
        assert_eq!(c.total_stake, 0);
    }

    // TC-CT-017-06, TC-CT-017-07
    #[test]
    fn unknown_action_or_bad_json() {
        let mut c = pending();
        for msg in [r#"{"action":"burn"}"#, "stake", "", r#"{"action":"stake","member_id":42}"#, "{}"] {
            assert!(rejected(&mut c, "usdt.near", "translator.near", CREDIT, msg).contains("ERR_BAD_MSG"), "{msg:?}");
        }
        assert_eq!(c.total_stake, 0);
    }

    // TC-CT-017-08, TC-CT-017-09
    #[test]
    fn stake_needs_a_member() {
        let mut c = pending();
        assert!(rejected(&mut c, "usdt.near", "outsider.near", CREDIT, r#"{"action":"stake"}"#).contains("ERR_NOT_MEMBER"));
        assert!(rejected(&mut c, "usdt.near", "translator.near", CREDIT, r#"{"action":"stake","member_id":"outsider.near"}"#).contains("ERR_NOT_MEMBER"));
    }

    // TC-CT-017-10
    #[test]
    fn zero_amount_is_rejected() {
        let mut c = pending();
        assert!(rejected(&mut c, "usdt.near", "translator.near", 0, r#"{"action":"stake"}"#).contains("ERR_ZERO_AMOUNT"));
    }

    // TC-CT-017-11
    #[test]
    fn dao_admission_blocks_an_unadmitted_member() {
        let mut c = circle_with(Params { admission: Admission::Dao, ..Params::default() });
        add(&mut c, "translator.near", MemberStatus::Pending, 0, 0);
        assert!(rejected(&mut c, "usdt.near", "translator.near", 100 * CREDIT, r#"{"action":"stake"}"#).contains("ERR_NOT_ADMITTED"));
        assert_eq!(view(&c, "translator.near").status, MemberStatus::Pending);
        c.members.get_mut(&acc("translator.near")).unwrap().admitted = true;
        stake(&mut c, "translator.near", 100 * CREDIT);
        assert_eq!(view(&c, "translator.near").status, MemberStatus::Active);
    }

    // TC-CT-017-12
    #[test]
    fn suspended_member_adds_stake_but_stays_suspended() {
        let mut c = circle();
        add(&mut c, "scraper.near", MemberStatus::Suspended, 65 * CREDIT, 0);
        stake(&mut c, "scraper.near", 50 * CREDIT);
        let m = view(&c, "scraper.near");
        assert_eq!((m.status, m.stake.0, m.limit.0), (MemberStatus::Suspended, 115 * CREDIT, 0));
        c.assert_d6(Some(115 * CREDIT));
    }

    // TC-CT-017-13
    #[test]
    fn exited_member_cannot_stake() {
        let mut c = circle();
        add(&mut c, "gpu.near", MemberStatus::Exited, 0, 0);
        assert!(rejected(&mut c, "usdt.near", "gpu.near", 10 * CREDIT, r#"{"action":"stake"}"#).contains("ERR_MEMBER_EXITED"));
    }

    // TC-CT-017-14
    #[test]
    fn one_unit_of_stake_gives_limit_zero() {
        let mut c = pending();
        stake(&mut c, "translator.near", 1);
        let m = view(&c, "translator.near");
        assert_eq!((m.status, m.stake.0, m.limit.0), (MemberStatus::Active, 1, 0));
    }

    // K-06: no stake while an exit is requested
    #[test]
    fn stake_during_exit_is_rejected() {
        let mut c = circle();
        add(&mut c, "gpu.near", MemberStatus::Active, 100 * CREDIT, 80 * CREDIT);
        c.members.get_mut(&acc("gpu.near")).unwrap().exit_requested_epoch = Some(1);
        assert!(rejected(&mut c, "usdt.near", "gpu.near", 10 * CREDIT, r#"{"action":"stake"}"#).contains("ERR_EXITING"));
        assert_eq!(view(&c, "gpu.near").stake.0, 100 * CREDIT);
    }

    // limit is capped by `limit_cap` and never above stake (I-01, I-02)
    #[test]
    fn limit_respects_cap_and_stake() {
        let mut c = circle_with(Params { limit_cap: U128(50 * CREDIT), ..Params::default() });
        add(&mut c, "translator.near", MemberStatus::Pending, 0, 0);
        stake(&mut c, "translator.near", 100 * CREDIT);
        assert_eq!(view(&c, "translator.near").limit.0, 50 * CREDIT);
    }

    // order of checks (spec 4.7): token, msg, amount, then the member
    #[test]
    fn check_order() {
        let mut c = pending();
        assert!(rejected(&mut c, "outsider.near", "x.near", 0, "not json").contains("ERR_WRONG_TOKEN"));
        assert!(rejected(&mut c, "usdt.near", "x.near", 0, "not json").contains("ERR_BAD_MSG"));
        assert!(rejected(&mut c, "usdt.near", "x.near", 0, r#"{"action":"stake"}"#).contains("ERR_ZERO_AMOUNT"));
    }
}

#[cfg(test)]
mod exit_tests {
    use near_sdk::mock::MockAction;
    use near_sdk::serde_json::{self, Value};
    use near_sdk::test_utils::{get_created_receipts, get_logs, VMContextBuilder};
    use near_sdk::{test_vm_config, testing_env, PromiseResult, PublicKey, RuntimeFeesConfig};

    use super::*;
    use crate::member::Member;
    use crate::params::Params;
    use crate::settlement::{Position, PositionRole, PositionStatus};

    const CREDIT: u128 = 1_000_000;

    fn acc(s: &str) -> AccountId {
        format!("{s}.near").parse().unwrap()
    }

    fn ctx(caller: &str, deposit: u128) -> VMContextBuilder {
        let mut b = VMContextBuilder::new();
        b.predecessor_account_id(acc(caller)).current_account_id(acc("circle")).attached_deposit(NearToken::from_yoctonear(deposit));
        b
    }

    fn call_as(caller: &str) {
        testing_env!(ctx(caller, 1).build());
    }

    fn add(c: &mut X207Circle, name: &str, status: MemberStatus, stake: u128, limit: u128) {
        c.members.insert(
            acc(name),
            Member {
                account_id: acc(name),
                pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse::<PublicKey>().unwrap(),
                terms_hash: "a".repeat(64),
                stake,
                limit,
                status,
                admitted: false,
                joined_at: 1,
                settled_history: 0,
                last_active_epoch: 1,
                exit_requested_epoch: None,
                withdraw_pending: false,
                reactivate_epoch: None,
            },
        );
        c.member_index.push(acc(name));
        c.member_count += 1;
        c.d6_deposit_stake(stake);
    }

    fn circle() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(0).build());
        let mut c = X207Circle::with_config(acc("dao"), "agents-main".into(), acc("usdt"), acc("intents"), Params::default(), 0);
        add(&mut c, "coder", MemberStatus::Active, 100 * CREDIT, 80 * CREDIT);
        add(&mut c, "scraper", MemberStatus::Active, 100 * CREDIT, 80 * CREDIT);
        add(&mut c, "pending", MemberStatus::Pending, 0, 0);
        c
    }

    /// Epoch 1 forced to `finalized`.
    fn finalize_epoch_one(c: &mut X207Circle) {
        let e = c.epochs.get_mut(&1).unwrap();
        e.start_clearing(3_600_000_000_000);
        e.start_payment_window(4_200_000_000_000);
        e.finalize(6_000_000_000_000);
    }

    fn panics(f: impl FnOnce()) -> String {
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).expect_err("expected a panic");
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    fn events() -> Vec<Value> {
        get_logs().iter().filter_map(|l| l.strip_prefix("EVENT_JSON:")).map(|b| serde_json::from_str(b).unwrap()).collect()
    }

    fn calls() -> Vec<(String, String, Value)> {
        get_created_receipts()
            .into_iter()
            .flat_map(|r| {
                let receiver = r.receiver_id.to_string();
                r.actions.into_iter().filter_map(move |a| match a {
                    MockAction::FunctionCallWeight { method_name, args, .. } => {
                        Some((receiver.clone(), String::from_utf8(method_name).unwrap(), serde_json::from_slice(&args).unwrap()))
                    }
                    _ => None,
                })
            })
            .collect()
    }

    /// `coder` asked to exit in epoch 1 and epoch 1 is finalized.
    fn ready() -> X207Circle {
        let mut c = circle();
        call_as("coder");
        c.request_exit();
        finalize_epoch_one(&mut c);
        c
    }

    fn withdraw(c: &mut X207Circle, who: &str) -> PromiseOrValue<bool> {
        call_as(who);
        c.withdraw_stake()
    }

    fn resolve(c: &mut X207Circle, result: PromiseResult, amount: u128) -> bool {
        testing_env!(ctx("circle", 0).build(), test_vm_config(), RuntimeFeesConfig::test(), Default::default(), vec![result]);
        c.on_withdraw_resolved(acc("coder"), U128(amount))
    }

    // TC-CT-077-01
    #[test]
    fn tc_ct_077_01_request_exit() {
        let mut c = circle();
        call_as("coder");
        c.request_exit();
        let m = c.members.get(&acc("coder")).unwrap();
        assert_eq!((m.exit_requested_epoch, m.status), (Some(1), MemberStatus::Active));
        let ev = events();
        assert_eq!((ev[0]["event"].as_str(), &ev[0]["data"][0]), (Some("exit_requested"), &serde_json::json!({"account_id": "coder.near", "epoch": 1})));
    }

    // TC-CT-077-02
    #[test]
    fn tc_ct_077_02_needs_one_yocto() {
        let mut c = circle();
        for deposit in [0, 2] {
            testing_env!(ctx("coder", deposit).build());
            assert!(panics(|| c.request_exit()).contains("Requires attached deposit of exactly 1 yoctoNEAR"));
        }
        assert_eq!(c.members.get(&acc("coder")).unwrap().exit_requested_epoch, None);
    }

    // TC-CT-077-03, TC-CT-077-14
    #[test]
    fn tc_ct_077_03_14_not_a_member() {
        let mut c = circle();
        call_as("outsider");
        assert!(panics(|| c.request_exit()).contains("ERR_NOT_MEMBER"));
        assert!(panics(|| drop(c.withdraw_stake())).contains("ERR_NOT_MEMBER"));
    }

    // TC-CT-077-04, TC-CT-077-05
    #[test]
    fn tc_ct_077_04_05_status_rules() {
        let mut c = circle();
        call_as("pending");
        assert!(panics(|| c.request_exit()).contains("ERR_WRONG_STATUS"));
        c.members.get_mut(&acc("scraper")).unwrap().status = MemberStatus::Suspended;
        call_as("scraper");
        c.request_exit();
        assert_eq!(c.members.get(&acc("scraper")).unwrap().exit_requested_epoch, Some(1));
        call_as("scraper");
        assert!(panics(|| c.request_exit()).contains("ERR_WRONG_STATUS"));
    }

    // TC-CT-077-07, TC-CT-077-08, TC-CT-077-09
    #[test]
    fn tc_ct_077_07_08_09_exit_epoch_must_be_final() {
        let mut c = circle();
        assert!(panics(|| drop(withdraw(&mut c, "coder"))).contains("ERR_EXIT_NOT_READY"), "no request");
        call_as("coder");
        c.request_exit();
        assert!(panics(|| drop(withdraw(&mut c, "coder"))).contains("ERR_EXIT_NOT_READY"), "epoch open");
        let e = c.epochs.get_mut(&1).unwrap();
        e.start_clearing(3_600_000_000_000);
        e.start_payment_window(4_200_000_000_000);
        assert!(panics(|| drop(withdraw(&mut c, "coder"))).contains("ERR_EXIT_NOT_READY"), "payment_window");
        c.epochs.get_mut(&1).unwrap().finalize(6_000_000_000_000);
        assert!(matches!(withdraw(&mut c, "coder"), PromiseOrValue::Promise(_)));
    }

    // TC-CT-077-10: stake left after a default is paid back
    #[test]
    fn tc_ct_077_10_leftover_stake_after_default() {
        let mut c = circle();
        call_as("scraper");
        c.request_exit();
        finalize_epoch_one(&mut c);
        let m = c.members.get_mut(&acc("scraper")).unwrap();
        m.stake = 65 * CREDIT;
        m.limit = 0;
        m.status = MemberStatus::Suspended;
        c.total_stake -= 35 * CREDIT;
        c.put_position(
            1,
            acc("scraper"),
            Position { net: -35 * CREDIT as i128, role: PositionRole::Debtor, status: PositionStatus::Defaulted, amount: 35 * CREDIT, recovered: 35 * CREDIT, payment_deadline: 9, updated_at_block: 5 },
        );
        assert!(matches!(withdraw(&mut c, "scraper"), PromiseOrValue::Promise(_)));
        let c0 = &calls()[0];
        assert_eq!((c0.0.as_str(), c0.1.as_str(), &c0.2["receiver_id"], &c0.2["amount"]), ("usdt.near", "ft_transfer", &serde_json::json!("scraper.near"), &serde_json::json!("65000000")));
    }

    // TC-CT-077-11
    #[test]
    fn tc_ct_077_11_debt_in_another_epoch_blocks() {
        let mut c = ready();
        c.outstanding_debt.insert(acc("coder"), 10 * CREDIT);
        assert!(panics(|| drop(withdraw(&mut c, "coder"))).contains("ERR_HAS_DUE"));
        assert_eq!(c.members.get(&acc("coder")).unwrap().stake, 100 * CREDIT);
        assert!(!c.members.get(&acc("coder")).unwrap().withdraw_pending);
    }

    // TC-CT-077-12, TC-CT-077-19
    #[test]
    fn tc_ct_077_12_19_withdraw_success() {
        let mut c = ready();
        let before = c.accounted();
        assert!(matches!(withdraw(&mut c, "coder"), PromiseOrValue::Promise(_)));
        let calls = calls();
        assert_eq!((calls[0].0.as_str(), calls[0].1.as_str(), &calls[0].2["amount"]), ("usdt.near", "ft_transfer", &serde_json::json!("100000000")));
        assert_eq!((calls[1].1.as_str(), &calls[1].2["amount"]), ("on_withdraw_resolved", &serde_json::json!("100000000")));
        let m = c.members.get(&acc("coder")).unwrap();
        assert_eq!((m.stake, m.limit, m.withdraw_pending), (0, 0, true));
        assert_eq!((c.in_flight, c.total_stake, c.accounted()), (100 * CREDIT, 100 * CREDIT, before), "K-08: the sum of D6 does not change");

        assert!(resolve(&mut c, PromiseResult::Successful(vec![]), 100 * CREDIT));
        let m = c.members.get(&acc("coder")).unwrap();
        assert_eq!((m.status, m.stake, m.limit, m.withdraw_pending), (MemberStatus::Exited, 0, 0, false));
        assert_eq!((c.in_flight, c.total_stake), (0, 100 * CREDIT));
        let ev = events();
        assert_eq!((ev[0]["event"].as_str(), &ev[0]["data"][0]), (Some("stake_withdrawn"), &serde_json::json!({"account_id": "coder.near", "amount": "100000000"})));
        c.assert_d6(None);
    }

    // TC-CT-077-13
    #[test]
    fn tc_ct_077_13_failed_transfer_restores_stake_and_limit() {
        let mut c = ready();
        let _ = withdraw(&mut c, "coder");
        assert!(!resolve(&mut c, PromiseResult::Failed, 100 * CREDIT));
        let m = c.members.get(&acc("coder")).unwrap();
        assert_eq!((m.stake, m.limit, m.status, m.exit_requested_epoch, m.withdraw_pending), (100 * CREDIT, 80 * CREDIT, MemberStatus::Active, Some(1), false));
        assert_eq!((c.in_flight, c.total_stake), (0, 200 * CREDIT));
        assert_eq!(events()[0]["event"], "stake_withdraw_failed");
        c.assert_d6(None);
        assert!(matches!(withdraw(&mut c, "coder"), PromiseOrValue::Promise(_)), "may try again");
    }

    // TC-CT-077-15
    #[test]
    fn tc_ct_077_15_after_exit() {
        let mut c = ready();
        let _ = withdraw(&mut c, "coder");
        resolve(&mut c, PromiseResult::Successful(vec![]), 100 * CREDIT);
        assert!(panics(|| drop(withdraw(&mut c, "coder"))).contains("ERR_MEMBER_EXITED"));
    }

    // TC-CT-077-16
    #[test]
    fn tc_ct_077_16_second_withdraw_before_the_callback() {
        let mut c = ready();
        let _ = withdraw(&mut c, "coder");
        assert!(panics(|| drop(withdraw(&mut c, "coder"))).contains("ERR_WITHDRAW_IN_FLIGHT"));
        assert_eq!(c.in_flight, 100 * CREDIT);
    }

    // TC-CT-077-21
    #[test]
    fn tc_ct_077_21_no_more_stake_while_exiting() {
        let mut c = circle();
        call_as("coder");
        c.request_exit();
        testing_env!(ctx("usdt", 0).build());
        let text = panics(|| drop(c.ft_on_transfer(acc("coder"), U128(10 * CREDIT), r#"{"action":"stake"}"#.to_string())));
        assert!(text.contains("ERR_EXITING"), "{text}");
        assert_eq!(c.members.get(&acc("coder")).unwrap().stake, 100 * CREDIT);
    }

    // TC-CT-077-22
    #[test]
    fn tc_ct_077_22_empty_stake_exits_without_a_promise() {
        let mut c = ready();
        let m = c.members.get_mut(&acc("coder")).unwrap();
        c.total_stake -= m.stake;
        m.stake = 0;
        m.limit = 0;
        m.status = MemberStatus::Suspended;
        c.epochs.get_mut(&1).unwrap().shortfall = 5 * CREDIT;
        match withdraw(&mut c, "coder") {
            PromiseOrValue::Value(done) => assert!(done),
            PromiseOrValue::Promise(_) => panic!("no promise for a stake of 0"),
        }
        assert!(calls().is_empty());
        assert_eq!(c.members.get(&acc("coder")).unwrap().status, MemberStatus::Exited);
        let ev = events();
        assert_eq!((ev[0]["event"].as_str(), ev[0]["data"][0]["amount"].as_str()), (Some("stake_withdrawn"), Some("0")));
    }

    // TC-CT-077-20: an exited account stays registered, so `join` refuses it
    #[test]
    fn tc_ct_077_20_exited_member_stays_registered() {
        let mut c = ready();
        let _ = withdraw(&mut c, "coder");
        resolve(&mut c, PromiseResult::Successful(vec![]), 100 * CREDIT);
        assert!(c.members.get(&acc("coder")).is_some());
        assert!(c.member_index.iter().any(|a| a == &acc("coder")));
    }
}
