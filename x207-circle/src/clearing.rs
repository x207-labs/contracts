//! F3 clearing: epoch state machine, D4 proposals, `close_epoch`, `submit_clearing`, `commit_epoch`.

use near_sdk::json_types::{U128, U64};
use near_sdk::store::Vector;
use near_sdk::{near, AccountId};

use crate::params::Params;
use crate::state::StorageKey;
use crate::types::{ClearingEdge, EpochId};

/// Stored phases (spec section 8). `cutoff` and `commit` are time-derived and never stored.
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EpochPhase {
    Open,
    Clearing,
    PaymentWindow,
    Finalized,
}

/// Epoch record (spec 9.1, prefix `c`): phase, timestamps (ns), params snapshot, totals, pool.
/// The three per-epoch vectors (prefixes `E`, `T`, `S`) live inside the record, because a
/// `store::Vector` keeps its length in the value that contains it.
#[near(serializers = [borsh])]
pub struct Epoch {
    pub epoch: EpochId,
    pub phase: EpochPhase,
    pub opened_at: u64,
    pub cutoff_at: u64,
    pub closed_at: Option<u64>,
    pub proposal_deadline: Option<u64>,
    pub committed_at: Option<u64>,
    pub payment_deadline: Option<u64>,
    pub finalized_at: Option<u64>,
    pub params: Params,
    pub edge_count: u32,
    pub participant_count: u32,
    pub total_obligation: u128,
    pub total_cleared: u128,
    pub pool: u128,
    pub shortfall: u128,
    /// D2 snapshot: edge index -> `(payer, payee)`.
    pub edges: Vector<(AccountId, AccountId)>,
    /// D2 aggregate: participant index -> account.
    pub participants: Vector<AccountId>,
    /// D5: position index -> account.
    pub position_accounts: Vector<AccountId>,
}

impl Epoch {
    /// A fresh `open` epoch with empty vectors under the per-epoch prefixes.
    pub fn open(epoch: EpochId, opened_at: u64, cutoff_at: u64, params: Params) -> Self {
        Self {
            epoch,
            phase: EpochPhase::Open,
            opened_at,
            cutoff_at,
            closed_at: None,
            proposal_deadline: None,
            committed_at: None,
            payment_deadline: None,
            finalized_at: None,
            params,
            edge_count: 0,
            participant_count: 0,
            total_obligation: 0,
            total_cleared: 0,
            pool: 0,
            shortfall: 0,
            edges: Vector::new(StorageKey::EpochEdges(epoch)),
            participants: Vector::new(StorageKey::EpochParticipants(epoch)),
            position_accounts: Vector::new(StorageKey::EpochPositions(epoch)),
        }
    }
}

/// D4 proposal (spec 3.6, prefix `q`). `edges` is emptied by `commit_epoch`.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearingProposal {
    pub epoch: EpochId,
    pub solver: AccountId,
    pub edges: Vec<ClearingEdge>,
    pub edge_count: u32,
    pub cleared_total: U128,
    pub submitted_at: U64,
    pub committed: bool,
}
