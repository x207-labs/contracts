//! `withdraw_intents_balance` (BE-034, spec 18): brings the USDT that came in through `mt_on_transfer` from the multi-token balance at
//! Intents to the NEP-141 account of the circle.
//!
//! `mt_on_transfer` books the paid residual in the epoch pool (D6) at once, but the tokens stay at Intents as `nep141:<usdt>`. The contract keeps
//! the sum of what it booked that way (`intents_balance`) and asks Intents for exactly that sum: `ft_withdraw` to its own account with 1 yocto,
//! then a private callback. One withdrawal at a time: while one is in flight a second call is `ERR_WITHDRAW_IN_FLIGHT`. A failed withdrawal (or one that answers `0`) books nothing
//! and can be called again. Nothing here touches the pool: a `claim` to `near` that finds too little NEP-141 balance fails through the existing
//! path (`residual_claim_failed`, the position is `claimable` again) and can be repeated after the withdrawal.

use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::{env, near, require, Gas, NearToken, Promise};

use crate::events::{IntentsWithdrawn, X207Event};
use crate::state::{X207Circle, X207CircleExt};

const CREDIT_KEY: &[u8] = b"x207_intents_credit";
const IN_FLIGHT_KEY: &[u8] = b"x207_intents_withdrawing";
/// Gas of `ft_withdraw` at Intents (it makes the `ft_transfer` and resolves it) and of the callback; the call needs both and some room.
pub const INTENTS_WITHDRAW_GAS: Gas = Gas::from_tgas(80);
pub const INTENTS_WITHDRAW_CALLBACK_GAS: Gas = Gas::from_tgas(20);
pub const INTENTS_WITHDRAW_MIN_GAS: Gas = Gas::from_tgas(130);

fn read(key: &[u8]) -> u128 {
    env::storage_read(key).map_or(0, |b| u128::from_le_bytes(b.try_into().expect("16 bytes")))
}

fn write(key: &[u8], value: u128) {
    if value == 0 {
        env::storage_remove(key);
    } else {
        env::storage_write(key, &value.to_le_bytes());
    }
}

/// Adds what `mt_on_transfer` kept to the balance that still sits at Intents.
pub(crate) fn add_credit(amount: u128) {
    write(CREDIT_KEY, read(CREDIT_KEY) + amount);
}

#[near]
impl X207Circle {
    /// Permissionless. Moves the whole booked Intents balance to the NEP-141 account of the circle. `ERR_NOTHING_TO_WITHDRAW`,
    /// `ERR_WITHDRAW_IN_FLIGHT`, `ERR_GAS_TOO_LOW`.
    pub fn withdraw_intents_balance(&mut self) -> Promise {
        require!(
            env::prepaid_gas() >= INTENTS_WITHDRAW_MIN_GAS,
            format!(
                "ERR_GAS_TOO_LOW: need {} TGas",
                INTENTS_WITHDRAW_MIN_GAS.as_tgas()
            )
        );
        require!(
            read(IN_FLIGHT_KEY) == 0,
            "ERR_WITHDRAW_IN_FLIGHT: a withdrawal from Intents has not resolved yet"
        );
        let amount = read(CREDIT_KEY);
        require!(
            amount > 0,
            "ERR_NOTHING_TO_WITHDRAW: no USDT is booked at Intents"
        );
        write(IN_FLIGHT_KEY, amount);
        Promise::new(self.intents_id.clone())
            .function_call(
                "ft_withdraw".to_string(),
                json!({"token": self.usdt_token_id, "receiver_id": env::current_account_id(), "amount": U128(amount), "memo": null, "msg": null})
                    .to_string()
                    .into_bytes(),
                NearToken::from_yoctonear(1),
                INTENTS_WITHDRAW_GAS,
            )
            .then(Self::ext(env::current_account_id()).with_static_gas(INTENTS_WITHDRAW_CALLBACK_GAS).on_intents_withdrawn(U128(amount)))
    }

    /// Callback of the withdrawal. Never panics. `ft_withdraw` answers with the amount that really left Intents (the reference contract gives the
    /// balance back and answers `0` when its transfer fails), so that figure is what is booked: a failed call, a `0` and an answer that is not an
    /// amount all book nothing and leave the withdrawal open for a new call. Returns the amount that left Intents.
    #[private]
    pub fn on_intents_withdrawn(&mut self, amount: U128) -> U128 {
        write(IN_FLIGHT_KEY, 0);
        let left = match env::promise_result_checked(0, 64) {
            Ok(bytes) => {
                near_sdk::serde_json::from_slice::<U128>(&bytes).map_or(0, |v| v.0.min(amount.0))
            }
            Err(_) => 0,
        };
        if left == 0 {
            return U128(0);
        }
        write(CREDIT_KEY, read(CREDIT_KEY).saturating_sub(left));
        IntentsWithdrawn { amount: U128(left) }.emit();
        U128(left)
    }

    /// USDT that `mt_on_transfer` booked and that still sits at Intents, not yet withdrawn.
    pub fn intents_balance(&self) -> U128 {
        U128(read(CREDIT_KEY))
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::json_types::U128;
    use near_sdk::serde_json;
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::{test_vm_config, testing_env, AccountId, PromiseResult, RuntimeFeesConfig};

    use super::*;
    use crate::params::Params;

    const CREDIT: u128 = 1_000_000;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    /// A circle with `booked` at Intents and a withdrawal of that amount in flight, inside an environment where promise 0 answered `result`.
    fn in_flight(booked: u128, result: PromiseResult) -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        let c = X207Circle::with_config(
            acc("dao.near"),
            "agents-main".into(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            1_000,
        );
        let ctx = VMContextBuilder::new()
            .current_account_id(acc("circle.near"))
            .predecessor_account_id(acc("circle.near"))
            .build();
        testing_env!(
            ctx,
            test_vm_config(),
            RuntimeFeesConfig::test(),
            Default::default(),
            vec![result]
        );
        write(CREDIT_KEY, 0);
        add_credit(booked);
        write(IN_FLIGHT_KEY, booked);
        c
    }

    fn answer(text: &str) -> PromiseResult {
        PromiseResult::Successful(text.as_bytes().to_vec())
    }

    fn events() -> Vec<String> {
        get_logs()
            .iter()
            .filter_map(|l| l.strip_prefix("EVENT_JSON:"))
            .map(|b| {
                serde_json::from_str::<serde_json::Value>(b).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// The three answers that must book nothing: the call failed, the answer is not a number, the answer is `0`.
    #[test]
    fn a_withdrawal_that_did_not_leave_books_nothing_and_can_be_called_again() {
        let failures = [
            ("failed", PromiseResult::Failed),
            ("not a number", answer("garbage")),
            ("empty", answer("")),
            ("a string that is not an amount", answer("\"abc\"")),
            ("zero", answer("\"0\"")),
        ];
        for (name, result) in failures {
            let mut c = in_flight(40 * CREDIT, result);
            assert_eq!(c.on_intents_withdrawn(U128(40 * CREDIT)), U128(0), "{name}");
            assert_eq!(
                c.intents_balance(),
                U128(40 * CREDIT),
                "{name}: the balance stays booked"
            );
            assert_eq!(
                read(IN_FLIGHT_KEY),
                0,
                "{name}: the withdrawal is open for a new call"
            );
            assert!(events().is_empty(), "{name}: no intents_withdrawn event");
        }
    }

    #[test]
    fn what_ft_withdraw_answers_is_what_is_booked_out() {
        for (reported, left) in [
            ("\"40000000\"", 40 * CREDIT),
            ("\"15000000\"", 15 * CREDIT),
            ("\"99000000\"", 40 * CREDIT),
        ] {
            let mut c = in_flight(40 * CREDIT, answer(reported));
            assert_eq!(
                c.on_intents_withdrawn(U128(40 * CREDIT)),
                U128(left),
                "{reported}"
            );
            assert_eq!(c.intents_balance(), U128(40 * CREDIT - left), "{reported}");
            assert_eq!(
                (read(IN_FLIGHT_KEY), events()),
                (0, vec!["intents_withdrawn".to_string()]),
                "{reported}"
            );
        }
    }
}
