//! Shared wire types of spec section 3 that more than one module needs.

use near_sdk::json_types::U128;
use near_sdk::{near, AccountId};

pub type EpochId = u64;
pub type Seq = u64;

/// One clearing edge (spec 3.6). Valid when `0 < cleared <= obligation[(epoch, debtor, creditor)]`.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearingEdge {
    pub debtor: AccountId,
    pub creditor: AccountId,
    pub cleared: U128,
}
