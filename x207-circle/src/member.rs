//! F1 onboarding: D1 members, `join`, member views.

use near_sdk::{near, AccountId, PublicKey};

use crate::types::EpochId;

/// Spec 3.5, ADR-26. JSON values are `snake_case`.
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemberStatus {
    Pending,
    Active,
    Suspended,
    Exited,
}

/// D1 record (spec 3.7 `MemberView` plus the internal fields of section 14). Amounts are native
/// `u128` in state; the view converts to `U128`.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub account_id: AccountId,
    pub pubkey_voucher: PublicKey,
    pub terms_hash: String,
    pub stake: u128,
    pub limit: u128,
    pub status: MemberStatus,
    pub admitted: bool,
    pub joined_at: u64,
    pub settled_history: u128,
    pub last_active_epoch: EpochId,
    pub exit_requested_epoch: Option<EpochId>,
    /// `true` between `withdraw_stake` and `on_withdraw_resolved` (K-07). Not shown in the view.
    pub withdraw_pending: bool,
    /// ADR-30, P1 (CT-079). Not shown in the view.
    pub reactivate_epoch: Option<EpochId>,
}
