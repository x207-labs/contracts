//! Shared wire types of spec section 3 that more than one module needs.

use near_sdk::json_types::U128;
use near_sdk::{near, AccountId};

pub type EpochId = u64;
pub type Seq = u64;

/// Most edges in one clearing proposal (spec 3.6, ADR-32 K-44). CT-004: 300 edges cost 38 Tgas to
/// verify and 92 Tgas to apply in the probe, about 11x below 1 PGas; CT-043 re-measures the final methods.
pub const MAX_EDGES_PER_TX: u32 = 300;

/// One clearing edge (spec 3.6). Valid when `0 < cleared <= obligation[(epoch, debtor, creditor)]`.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearingEdge {
    pub debtor: AccountId,
    pub creditor: AccountId,
    pub cleared: U128,
}

/// Where `claim` pays out (spec 3.7): the member's NEAR account, or its balance in `intents.near`.
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimDestination {
    Near,
    Intents,
}

/// `msg` of `ft_transfer_call` to this contract (spec 4.1): a JSON object tagged by `action`.
/// `member_id` lets an owner or a settlement worker pay on behalf of an agent; the default is the sender.
#[near(serializers = [json])]
#[serde(tag = "action", rename_all = "snake_case")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferMsg {
    Stake {
        member_id: Option<AccountId>,
    },
    Settle {
        epoch: EpochId,
        member_id: Option<AccountId>,
    },
}
