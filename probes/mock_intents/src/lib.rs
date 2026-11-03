//! `ft_on_transfer` receiver that records what the circle sends on a claim to Intents (ADR-19)
//! and answers by a mode set by the test: use everything, hand part of it back, or fail.

use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::{env, near, AccountId, Gas, NearToken, PanicOnDefault, Promise};

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MockIntents {
    mode: String,
    refund: u128,
    last_msg: Option<String>,
    last_sender: Option<AccountId>,
    received: u128,
}

#[near]
impl MockIntents {
    #[init]
    pub fn new() -> Self {
        Self { mode: "use_all".into(), refund: 0, last_msg: None, last_sender: None, received: 0 }
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
        Promise::new(receiver_id).function_call(
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
