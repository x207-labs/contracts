//! NEP-141 fungible token mock with `ft_transfer_call` and the standard `ft_resolve_transfer`
//! refund, so sandbox tests see the same refund behaviour as USDT (spec 4.1). `mint` is open to
//! everyone; NEP-145 `storage_deposit` accepts any account and charges nothing.

use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::store::LookupMap;
use near_sdk::{assert_one_yocto, env, near, require, AccountId, Gas, NearToken, PanicOnDefault, PromiseOrValue};

const RESOLVE_GAS: Gas = Gas::from_tgas(10);
const ON_TRANSFER_GAS: Gas = Gas::from_tgas(40);

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MockFt {
    balances: LookupMap<AccountId, u128>,
    total_supply: u128,
}

impl MockFt {
    fn balance(&self, account: &AccountId) -> u128 {
        self.balances.get(account).copied().unwrap_or(0)
    }

    fn move_amount(&mut self, from: &AccountId, to: &AccountId, amount: u128) {
        require!(amount > 0, "The amount should be a positive number");
        let from_balance = self.balance(from);
        require!(from_balance >= amount, "The account doesn't have enough balance");
        self.balances.insert(from.clone(), from_balance - amount);
        let to_balance = self.balance(to);
        self.balances.insert(to.clone(), to_balance + amount);
    }
}

#[near]
impl MockFt {
    #[init]
    pub fn new() -> Self {
        Self { balances: LookupMap::new(b"b"), total_supply: 0 }
    }

    pub fn mint(&mut self, account_id: AccountId, amount: U128) {
        let balance = self.balance(&account_id);
        self.balances.insert(account_id, balance + amount.0);
        self.total_supply += amount.0;
    }

    #[payable]
    pub fn storage_deposit(&mut self, account_id: Option<AccountId>, registration_only: Option<bool>) -> near_sdk::serde_json::Value {
        let _ = (account_id, registration_only);
        json!({"total": "1250000000000000000000", "available": "0"})
    }

    pub fn ft_total_supply(&self) -> U128 {
        U128(self.total_supply)
    }

    pub fn ft_balance_of(&self, account_id: AccountId) -> U128 {
        U128(self.balance(&account_id))
    }

    #[payable]
    pub fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>) {
        assert_one_yocto();
        let _ = memo;
        self.move_amount(&env::predecessor_account_id(), &receiver_id, amount.0);
    }

    #[payable]
    pub fn ft_transfer_call(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>, msg: String) -> PromiseOrValue<U128> {
        assert_one_yocto();
        let _ = memo;
        let sender_id = env::predecessor_account_id();
        self.move_amount(&sender_id, &receiver_id, amount.0);
        let call = near_sdk::Promise::new(receiver_id.clone()).function_call(
            "ft_on_transfer".to_string(),
            json!({"sender_id": sender_id, "amount": amount, "msg": msg}).to_string().into_bytes(),
            NearToken::from_yoctonear(0),
            ON_TRANSFER_GAS,
        );
        let resolve = Self::ext(env::current_account_id()).with_static_gas(RESOLVE_GAS).ft_resolve_transfer(sender_id, receiver_id, amount);
        PromiseOrValue::Promise(call.then(resolve))
    }

    /// Standard NEP-141 resolve: the receiver returns the unused amount; a failed receiver means
    /// everything is unused. Returns the amount that was used.
    #[private]
    pub fn ft_resolve_transfer(&mut self, sender_id: AccountId, receiver_id: AccountId, amount: U128) -> U128 {
        let unused = match env::promise_result_checked(0, 64) {
            Ok(bytes) => near_sdk::serde_json::from_slice::<U128>(&bytes).map(|v| v.0.min(amount.0)).unwrap_or(amount.0),
            Err(_) => amount.0,
        };
        let refund = unused.min(self.balance(&receiver_id));
        if refund > 0 {
            let receiver_balance = self.balance(&receiver_id);
            self.balances.insert(receiver_id, receiver_balance - refund);
            let sender_balance = self.balance(&sender_id);
            self.balances.insert(sender_id, sender_balance + refund);
        }
        U128(amount.0 - refund)
    }
}
