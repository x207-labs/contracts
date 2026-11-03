//! `ft_on_transfer` receiver that records what the circle sends on a claim to Intents (ADR-19)
//! and answers by a mode set by the test: use everything, hand part of it back, or fail.

use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::store::LookupMap;
use near_sdk::{assert_one_yocto, env, near, require, AccountId, Gas, NearToken, PanicOnDefault, Promise};

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MockIntents {
    mode: String,
    refund: u128,
    last_msg: Option<String>,
    last_sender: Option<AccountId>,
    received: u128,
    /// Multi-token balances held for accounts (what `mt_on_transfer` leaves at Intents). The USDT behind them is the NEP-141 balance of this
    /// contract: a test sends it here first, as a deposit to Intents would.
    balances: LookupMap<AccountId, u128>,
}

#[near]
impl MockIntents {
    #[init]
    pub fn new() -> Self {
        Self { mode: "use_all".into(), refund: 0, last_msg: None, last_sender: None, received: 0, balances: LookupMap::new(b"b") }
    }

    /// `use_all`: nothing is returned. `refund`: `refund` is handed back (capped at the amount).
    /// `panic`: `ft_on_transfer` fails, so the token refunds everything.
    pub fn set_mode(&mut self, mode: String, refund: Option<U128>) {
        assert!(matches!(mode.as_str(), "use_all" | "refund" | "panic"), "unknown mode");
        self.mode = mode;
        self.refund = refund.map_or(0, |r| r.0);
    }

    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> U128 {
        if self.mode == "panic" {
            env::panic_str("mock intents refuses the transfer");
        }
        self.last_msg = Some(msg);
        self.last_sender = Some(sender_id);
        let back = if self.mode == "refund" { self.refund.min(amount.0) } else { 0 };
        self.received += amount.0 - back;
        U128(back)
    }

    /// Plays `intents.near` paying a circle in the multi-token (NEP-245): calls `mt_on_transfer` on `receiver_id` with this contract as the
    /// predecessor. The call returns the receiver's answer (the amounts handed back). `previous_owner_id` is whoever the test says paid.
    pub fn mt_transfer_call(&mut self, receiver_id: AccountId, previous_owner_id: AccountId, token_id: String, amount: U128, msg: String) -> Promise {
        Promise::new(receiver_id.clone()).function_call(
            "mt_on_transfer".to_string(),
            json!({
                "sender_id": previous_owner_id,
                "previous_owner_ids": [previous_owner_id],
                "token_ids": [token_id],
                "amounts": [amount],
                "msg": msg,
            })
            .to_string()
            .into_bytes(),
            NearToken::from_yoctonear(0),
            Gas::from_tgas(100),
        )
        .then(Self::ext(env::current_account_id()).with_static_gas(Gas::from_tgas(10)).mt_resolve(receiver_id, amount))
    }

    /// NEP-245 resolve: what the receiver kept (`amount` minus the refund it returned) becomes its balance here; a failed receiver keeps nothing.
    #[private]
    pub fn mt_resolve(&mut self, receiver_id: AccountId, amount: U128) -> Vec<U128> {
        let refund = match env::promise_result_checked(0, 4096) {
            Ok(bytes) => near_sdk::serde_json::from_slice::<Vec<U128>>(&bytes).ok().and_then(|v| v.first().copied()).map_or(amount.0, |r| r.0.min(amount.0)),
            Err(_) => amount.0,
        };
        let kept = amount.0 - refund;
        let balance = self.balances.get(&receiver_id).copied().unwrap_or(0);
        self.balances.insert(receiver_id, balance + kept);
        vec![U128(refund)]
    }

    pub fn balance_of(&self, account_id: AccountId) -> U128 {
        U128(self.balances.get(&account_id).copied().unwrap_or(0))
    }

    /// `ft_withdraw` of `intents.near`: the caller takes `amount` of its balance out as the NEP-141 `token`, sent to `receiver_id`.
    /// The balance is given back when the NEP-141 transfer fails.
    #[payable]
    pub fn ft_withdraw(&mut self, token: AccountId, receiver_id: AccountId, amount: U128, memo: Option<String>, msg: Option<String>) -> Promise {
        assert_one_yocto();
        let _ = (memo, msg);
        let caller = env::predecessor_account_id();
        let balance = self.balances.get(&caller).copied().unwrap_or(0);
        require!(balance >= amount.0, "insufficient balance at intents");
        self.balances.insert(caller.clone(), balance - amount.0);
        Promise::new(token)
            .function_call(
                "ft_transfer".to_string(),
                json!({"receiver_id": receiver_id, "amount": amount, "memo": null}).to_string().into_bytes(),
                NearToken::from_yoctonear(1),
                Gas::from_tgas(15),
            )
            .then(Self::ext(env::current_account_id()).with_static_gas(Gas::from_tgas(10)).withdraw_resolve(caller, amount))
    }

    #[private]
    pub fn withdraw_resolve(&mut self, account_id: AccountId, amount: U128) -> U128 {
        if env::promise_result_checked(0, 64).is_err() {
            // like the reference contract: the balance is given back and the answer is 0
            let balance = self.balances.get(&account_id).copied().unwrap_or(0);
            self.balances.insert(account_id, balance + amount.0);
            return U128(0);
        }
        amount
    }

    pub fn last_msg(&self) -> Option<String> {
        self.last_msg.clone()
    }

    pub fn last_sender(&self) -> Option<AccountId> {
        self.last_sender.clone()
    }

    pub fn received(&self) -> U128 {
        U128(self.received)
    }
}
