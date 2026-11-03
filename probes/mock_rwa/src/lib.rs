//! A real-world-asset token for the sandbox tests of the class A token escrow (CT-091): the NEP-141 mock of `mock_ft` plus the three things
//! that make such tokens refuse or shrink a transfer. With `set_whitelist_on(true)` both sides of a transfer must be whitelisted
//! (`set_whitelisted`); a frozen account (`set_frozen`) can neither send nor receive; `set_fee` takes a flat fee out of every transfer, so the
//! receiver gets less than the amount (a deflationary token). A refused transfer panics like the real ones. `mint` is open to everyone.

use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::store::{LookupMap, LookupSet};
use near_sdk::{assert_one_yocto, env, near, require, AccountId, Gas, NearToken, PanicOnDefault, PromiseOrValue};

const RESOLVE_GAS: Gas = Gas::from_tgas(10);
/// Kept back for the call itself and `ft_resolve_transfer`; the rest goes to the receiver, like the
/// reference NEP-141 implementation does, so a low prepaid gas starves `ft_on_transfer` as it would on USDT.
const RESERVED_GAS: Gas = Gas::from_tgas(15);

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MockRwa {
    balances: LookupMap<AccountId, u128>,
    total_supply: u128,
    /// Accounts that called `storage_unregister`: a transfer to them fails like on the real token.
    unregistered: LookupSet<AccountId>,
    whitelist_on: bool,
    whitelisted: LookupSet<AccountId>,
    frozen: LookupSet<AccountId>,
    /// Flat fee burned out of every transfer.
    fee: u128,
}

impl MockRwa {
    fn balance(&self, account: &AccountId) -> u128 {
        self.balances.get(account).copied().unwrap_or(0)
    }

    fn move_amount(&mut self, from: &AccountId, to: &AccountId, amount: u128) {
        require!(amount > 0, "The amount should be a positive number");
        let from_balance = self.balance(from);
        require!(from_balance >= amount, "The account doesn't have enough balance");
        require!(!self.unregistered.contains(to), "The account is not registered");
        require!(!self.frozen.contains(from) && !self.frozen.contains(to), "ERR_FROZEN: the account is frozen");
        require!(!self.whitelist_on || (self.whitelisted.contains(from) && self.whitelisted.contains(to)), "ERR_NOT_WHITELISTED: both sides must be whitelisted");
        let received = amount.saturating_sub(self.fee);
        self.balances.insert(from.clone(), from_balance - amount);
        let to_balance = self.balance(to);
        self.balances.insert(to.clone(), to_balance + received);
        self.total_supply -= amount - received;
    }
}

#[near]
impl MockRwa {
    #[init]
    pub fn new() -> Self {
        Self { balances: LookupMap::new(b"b"), total_supply: 0, unregistered: LookupSet::new(b"u"), whitelist_on: false, whitelisted: LookupSet::new(b"w"), frozen: LookupSet::new(b"z"), fee: 0 }
    }

    pub fn set_whitelist_on(&mut self, on: bool) {
        self.whitelist_on = on;
    }

    pub fn set_whitelisted(&mut self, account_id: AccountId, whitelisted: bool) {
        if whitelisted { self.whitelisted.insert(account_id); } else { self.whitelisted.remove(&account_id); }
    }

    pub fn set_frozen(&mut self, account_id: AccountId, frozen: bool) {
        if frozen { self.frozen.insert(account_id); } else { self.frozen.remove(&account_id); }
    }

    pub fn set_fee(&mut self, fee: U128) {
        self.fee = fee.0;
    }

    pub fn mint(&mut self, account_id: AccountId, amount: U128) {
        let balance = self.balance(&account_id);
        self.balances.insert(account_id, balance + amount.0);
        self.total_supply += amount.0;
    }

    /// Test only: takes `amount` away from `account_id` (and from the supply), to play a contract whose NEP-141 balance is short.
    pub fn burn(&mut self, account_id: AccountId, amount: U128) {
        let balance = self.balance(&account_id);
        require!(balance >= amount.0, "burn above balance");
        self.balances.insert(account_id, balance - amount.0);
        self.total_supply -= amount.0;
    }

    #[payable]
    pub fn storage_deposit(&mut self, account_id: Option<AccountId>, registration_only: Option<bool>) -> near_sdk::serde_json::Value {
        let _ = registration_only;
        self.unregistered.remove(&account_id.unwrap_or_else(env::predecessor_account_id));
        json!({"total": "1250000000000000000000", "available": "0"})
    }

    /// NEP-145: the caller (or `account_id`) can no longer receive tokens until `storage_deposit`.
    #[payable]
    pub fn storage_unregister(&mut self, force: Option<bool>) -> bool {
        let _ = force;
        self.unregistered.insert(env::predecessor_account_id());
        true
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
        require!(env::prepaid_gas() > RESERVED_GAS, "More gas is required");
        let receiver_gas = env::prepaid_gas().saturating_sub(RESERVED_GAS);
        self.move_amount(&sender_id, &receiver_id, amount.0);
        let call = near_sdk::Promise::new(receiver_id.clone()).function_call(
            "ft_on_transfer".to_string(),
            json!({"sender_id": sender_id, "amount": amount, "msg": msg}).to_string().into_bytes(),
            NearToken::from_yoctonear(0),
            receiver_gas,
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
