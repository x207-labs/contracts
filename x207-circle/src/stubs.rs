//! Interface stubs of CT-068: the P0 methods and views whose implementation items are not done yet,
//! with the final signatures of spec sections 4, 5, and 14, so `abi.json` can be frozen first.
//!
//! The lane that implements a method deletes its stub in the same commit and adds the real method
//! (anywhere in the crate, with the same name, argument names, and types). A stub only panics
//! `ERR_NOT_IMPLEMENTED`; it never changes state. Argument names are part of the ABI, so they keep
//! their spec names even when unused.

#![allow(unused_variables)]

use near_sdk::json_types::U128;
use near_sdk::{env, near, AccountId, PromiseOrValue};

use crate::state::{X207Circle, X207CircleExt};
use crate::types::{ClaimDestination, EpochId};

fn not_implemented(method: &str) -> ! {
    env::panic_str(&format!("ERR_NOT_IMPLEMENTED: {method}"))
}

#[near]
impl X207Circle {
    // F3 clearing and F6 default.
    pub fn finalize_epoch(&mut self, epoch: EpochId) {
        not_implemented("finalize_epoch")
    }

    // F4 settlement.
    #[payable]
    pub fn claim(&mut self, epoch: EpochId, destination: ClaimDestination) -> near_sdk::Promise {
        not_implemented("claim")
    }

    #[private]
    pub fn on_claim_resolved(
        &mut self,
        epoch: EpochId,
        account_id: AccountId,
        amount: U128,
        destination: ClaimDestination,
    ) -> U128 {
        not_implemented("on_claim_resolved")
    }

    // ADR-29 exit.
    #[payable]
    pub fn request_exit(&mut self) {
        not_implemented("request_exit")
    }

    #[payable]
    pub fn withdraw_stake(&mut self) -> PromiseOrValue<bool> {
        not_implemented("withdraw_stake")
    }

    #[private]
    pub fn on_withdraw_resolved(&mut self, account_id: AccountId, amount: U128) -> bool {
        not_implemented("on_withdraw_resolved")
    }
}
