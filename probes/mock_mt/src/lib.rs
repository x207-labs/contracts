//! A NEP-245 multi-token mock for the sandbox tests of the class A multi-token escrow (CT-092): `mt_transfer`, `mt_transfer_call` and the batch call with the
//! refund of `mt_resolve_transfer`, balances per `(account, token_id)`, and two switches to play a token that refuses a transfer (`block_receiver`,
//! `set_frozen`). `mint` is open to everyone. Never deployed outside near-sandbox.

use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::store::{LookupMap, LookupSet};
use near_sdk::{assert_one_yocto, env, near, require, AccountId, Gas, NearToken, PanicOnDefault, Promise, PromiseOrValue};

const RESOLVE_GAS: Gas = Gas::from_tgas(10);
const RESERVED_GAS: Gas = Gas::from_tgas(15);

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MockMt {
    balances: LookupMap<(AccountId, String), u128>,
    blocked: LookupSet<AccountId>,
    frozen: LookupSet<AccountId>,
}

impl MockMt {
    fn balance(&self, account: &AccountId, token_id: &str) -> u128 {
        self.balances.get(&(account.clone(), token_id.to_string())).copied().unwrap_or(0)
    }

    fn move_amount(&mut self, from: &AccountId, to: &AccountId, token_id: &str, amount: u128) {
        require!(amount > 0, "The amount should be a positive number");
        require!(!self.blocked.contains(to), "ERR_RECEIVER_BLOCKED");
        require!(!self.frozen.contains(from) && !self.frozen.contains(to), "ERR_FROZEN");
        let have = self.balance(from, token_id);
        require!(have >= amount, "The account doesn't have enough balance");
        self.balances.insert((from.clone(), token_id.to_string()), have - amount);
        let to_balance = self.balance(to, token_id);
        self.balances.insert((to.clone(), token_id.to_string()), to_balance + amount);
    }
}

#[near]
impl MockMt {
    #[init]
    pub fn new() -> Self {
        Self { balances: LookupMap::new(b"b"), blocked: LookupSet::new(b"k"), frozen: LookupSet::new(b"z") }
    }

    pub fn mint(&mut self, account_id: AccountId, token_id: String, amount: U128) {
        let balance = self.balance(&account_id, &token_id);
        self.balances.insert((account_id, token_id), balance + amount.0);
    }

    pub fn block_receiver(&mut self, account_id: AccountId, blocked: bool) {
        if blocked { self.blocked.insert(account_id); } else { self.blocked.remove(&account_id); }
    }

    pub fn set_frozen(&mut self, account_id: AccountId, frozen: bool) {
        if frozen { self.frozen.insert(account_id); } else { self.frozen.remove(&account_id); }
    }

    pub fn mt_balance_of(&self, account_id: AccountId, token_id: String) -> U128 {
        U128(self.balance(&account_id, &token_id))
    }

    #[payable]
    pub fn mt_transfer(&mut self, receiver_id: AccountId, token_id: String, amount: U128, approval: Option<(AccountId, u64)>, memo: Option<String>) {
        assert_one_yocto();
        let _ = (approval, memo);
        self.move_amount(&env::predecessor_account_id(), &receiver_id, &token_id, amount.0);
    }

    #[payable]
    pub fn mt_batch_transfer_call(
        &mut self, receiver_id: AccountId, token_ids: Vec<String>, amounts: Vec<U128>, approvals: Option<Vec<Option<(AccountId, u64)>>>, memo: Option<String>, msg: String,
    ) -> Promise {
        assert_one_yocto();
        let _ = (approvals, memo);
        require!(token_ids.len() == amounts.len() && !token_ids.is_empty(), "token_ids and amounts differ");
        require!(env::prepaid_gas() > RESERVED_GAS, "More gas is required");
        let sender_id = env::predecessor_account_id();
        for (token, amount) in token_ids.iter().zip(&amounts) {
            self.move_amount(&sender_id, &receiver_id, token, amount.0);
        }
        let call = Promise::new(receiver_id.clone()).function_call(
            "mt_on_transfer".to_string(),
            json!({"sender_id": sender_id, "previous_owner_ids": vec![sender_id.clone(); token_ids.len()], "token_ids": token_ids, "amounts": amounts, "msg": msg}).to_string().into_bytes(),
            NearToken::from_yoctonear(0),
            env::prepaid_gas().saturating_sub(RESERVED_GAS),
        );
        call.then(Self::ext(env::current_account_id()).with_static_gas(RESOLVE_GAS).mt_resolve_transfer(sender_id, receiver_id, token_ids, amounts))
    }

    #[payable]
    pub fn mt_transfer_call(
        &mut self, receiver_id: AccountId, token_id: String, amount: U128, approval: Option<(AccountId, u64)>, memo: Option<String>, msg: String,
    ) -> Promise {
        let _ = approval;
        self.mt_batch_transfer_call(receiver_id, vec![token_id], vec![amount], None, memo, msg)
    }

    /// NEP-245 resolve: the receiver answers with the unused amount per token; a failed receiver means everything is unused. Returns the amounts used.
    #[private]
    pub fn mt_resolve_transfer(&mut self, sender_id: AccountId, receiver_id: AccountId, token_ids: Vec<String>, amounts: Vec<U128>) -> PromiseOrValue<Vec<U128>> {
        let unused: Vec<u128> = match env::promise_result_checked(0, 4096) {
            Ok(bytes) => near_sdk::serde_json::from_slice::<Vec<U128>>(&bytes).map(|v| v.into_iter().map(|u| u.0).collect()).unwrap_or_else(|_| amounts.iter().map(|a| a.0).collect()),
            Err(_) => amounts.iter().map(|a| a.0).collect(),
        };
        let mut used = Vec::new();
        for (i, (token, amount)) in token_ids.iter().zip(&amounts).enumerate() {
            let want_back = unused.get(i).copied().unwrap_or(amount.0).min(amount.0);
            let refund = want_back.min(self.balance(&receiver_id, token));
            if refund > 0 {
                let r = self.balance(&receiver_id, token);
                self.balances.insert((receiver_id.clone(), token.clone()), r - refund);
                let s = self.balance(&sender_id, token);
                self.balances.insert((sender_id.clone(), token.clone()), s + refund);
            }
            used.push(U128(amount.0 - refund));
        }
        PromiseOrValue::Value(used)
    }
}
