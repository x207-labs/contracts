//! F2 vouchers: D2/D3, canonical bytes, `register_cumulative`, `register_voucher`.

use near_sdk::near;

/// D2 and D3 for one `(epoch, payer, payee)` (spec 9.1, prefix `p`):
/// `obligation = cumulative - cleared`; `last_seq` is `0` before the first registration.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PairState {
    pub cumulative: u128,
    pub last_seq: u64,
    pub cleared: u128,
}

/// D2 aggregate per `(epoch, account)` (spec 9.1, prefix `t`).
#[near(serializers = [borsh])]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParticipantTotals {
    pub owed_out: u128,
    pub owed_in: u128,
}
