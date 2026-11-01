//! D7 `Params`, ranges, defaults, `validate_params`, `set_params`.

use near_sdk::json_types::U128;
use near_sdk::near;

use crate::types::EpochId;

/// Spec 3.4. Durations are seconds; ratios are basis points. Ranges, defaults, `validate_params`,
/// and `get_params` belong to CT-007.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    pub epoch_length: u64,
    pub payment_window: u64,
    pub proposal_window: u64,
    pub base_limit_ratio: u16,
    pub limit_growth: u16,
    pub limit_cap: U128,
    pub limit_decay: u16,
    pub fees: Fees,
    pub admission: Admission,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fees {
    pub clearing_fee_bps: u16,
    pub solver_fee_bps: u16,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    Open,
    Dao,
}

/// Params written by `set_params`, applied by `close_epoch` from `effective_epoch` (spec `get_pending_params`).
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingParams {
    pub params: Params,
    pub effective_epoch: EpochId,
}
