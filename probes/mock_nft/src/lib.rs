//! NEP-171 non-fungible token mock: `nft_mint` is open, `nft_transfer`, `nft_transfer_call` with the standard
//! `nft_resolve_transfer` (a receiver that returns `true` or fails gets the token back). `block_receiver` makes later transfers
//! TO that account fail, so a test can make the delivery of the escrow fail like a real token with an unregistered receiver.

use near_sdk::serde_json::json;
use near_sdk::store::{LookupMap, LookupSet};
use near_sdk::{assert_one_yocto, env, near, require, AccountId, Gas, NearToken, PanicOnDefault, PromiseOrValue};

const RESOLVE_GAS: Gas = Gas::from_tgas(10);
const RESERVED_GAS: Gas = Gas::from_tgas(15);

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MockNft {
    owners: LookupMap<String, AccountId>,
    blocked: LookupSet<AccountId>,
}

#[near(serializers = [json])]
pub struct Token {
    pub token_id: String,
    pub owner_id: AccountId,
}

#[near]
impl MockNft {
    #[init]
    pub fn new() -> Self {
        Self { owners: LookupMap::new(b"o"), blocked: LookupSet::new(b"b") }
    }

    pub fn nft_mint(&mut self, token_id: String, owner_id: AccountId) {
        require!(self.owners.get(&token_id).is_none(), "token exists");
        self.owners.insert(token_id, owner_id);
    }

    pub fn block_receiver(&mut self, account_id: AccountId) {
        self.blocked.insert(account_id);
    }

    pub fn nft_token(&self, token_id: String) -> Option<Token> {
        self.owners.get(&token_id).map(|owner_id| Token { token_id, owner_id: owner_id.clone() })
    }

    #[payable]
    pub fn nft_transfer(&mut self, receiver_id: AccountId, token_id: String, approval_id: Option<u64>, memo: Option<String>) {
        assert_one_yocto();
        let _ = (approval_id, memo);
        self.move_token(&env::predecessor_account_id(), &receiver_id, &token_id);
    }

    #[payable]
    pub fn nft_transfer_call(
        &mut self,
        receiver_id: AccountId,
        token_id: String,
        approval_id: Option<u64>,
        memo: Option<String>,
        msg: String,
    ) -> PromiseOrValue<bool> {
        assert_one_yocto();
        let _ = (approval_id, memo);
        require!(env::prepaid_gas() > RESERVED_GAS, "More gas is required");
        let sender_id = env::predecessor_account_id();
        self.move_token(&sender_id, &receiver_id, &token_id);
        let call = near_sdk::Promise::new(receiver_id.clone()).function_call(
            "nft_on_transfer".to_string(),
            json!({"sender_id": sender_id, "previous_owner_id": sender_id, "token_id": token_id, "msg": msg}).to_string().into_bytes(),
            NearToken::from_yoctonear(0),
            env::prepaid_gas().saturating_sub(RESERVED_GAS),
        );
        let resolve = Self::ext(env::current_account_id()).with_static_gas(RESOLVE_GAS).nft_resolve_transfer(sender_id, receiver_id, token_id);
        PromiseOrValue::Promise(call.then(resolve))
    }

    /// Standard NEP-171 resolve: `true` from the receiver (return the token) or a failed receiver gives the token back. Returns whether
    /// the token stayed with the receiver.
    #[private]
    pub fn nft_resolve_transfer(&mut self, previous_owner_id: AccountId, receiver_id: AccountId, token_id: String) -> bool {
        let return_it = match env::promise_result_checked(0, 16) {
            Ok(bytes) => near_sdk::serde_json::from_slice::<bool>(&bytes).unwrap_or(true),
            Err(_) => true,
        };
        if return_it && self.owners.get(&token_id) == Some(&receiver_id) {
            self.owners.insert(token_id, previous_owner_id);
            return false;
        }
        true
    }
}

impl MockNft {
    fn move_token(&mut self, from: &AccountId, to: &AccountId, token_id: &String) {
        require!(self.owners.get(token_id) == Some(from), "Sender must be the token owner");
        require!(!self.blocked.contains(to), "The receiver cannot receive tokens");
        self.owners.insert(token_id.clone(), to.clone());
    }
}
