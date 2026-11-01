//! `ft_on_transfer` (CT-017, spec 4.1): the entry for USDT sent with `ft_transfer_call`. The shared
//! checks and the `stake` action live here; the `settle` action is `settle_transfer` (CT-032).
//!
//! Every rejection is a panic, so the token's `ft_resolve_transfer` refunds the whole amount and
//! the contract state stays as it was.

use near_sdk::json_types::U128;
use near_sdk::{env, near, serde_json, AccountId, PromiseOrValue};

use crate::events::{StakeAdded, X207Event};
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
