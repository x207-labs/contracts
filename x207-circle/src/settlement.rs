//! F4 settlement: D5 positions, D6 accounting, `ft_on_transfer`, `finalize_epoch`, `claim`.

use near_sdk::near;

/// Spec 3.5. Debtor: `due -> paid | defaulted`. Creditor: `claimable <-> claimed` (ADR-24).
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PositionStatus {
    Due,
    Paid,
    Defaulted,
    Claimable,
    Claimed,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PositionRole {
    Debtor,
    Creditor,
}

/// D5 record per `(epoch, account)` (spec 9.1, prefix `s`); the key carries epoch and account.
/// Net-zero participants have no record.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub net: i128,
    pub role: PositionRole,
    pub status: PositionStatus,
    pub amount: u128,
    pub recovered: u128,
    pub payment_deadline: u64,
    pub updated_at_block: u64,
}
