//! NEP-297 events (spec sections 6 and 14). Log format:
//! `EVENT_JSON:{"standard":"x207","version":"0.0.2","event":"<name>","data":[{..}]}`.
//!
//! Status, role, mode, and destination fields are plain strings: they are wire values taken from
//! enums owned by other modules (`"none"` and `null` in `PositionEntry` have no enum counterpart).

use near_sdk::json_types::{I128, U128, U64};
use near_sdk::serde::Serialize;
use near_sdk::serde_json::json;
use near_sdk::{env, AccountId};

use crate::types::{ClearingEdge, EpochId, Seq};
use crate::INTERFACE_VERSION;

pub const EVENT_STANDARD: &str = "x207";

/// Maximum list items in one log; longer lists are emitted in chunks (spec section 6, K-19).
pub const MAX_EVENT_ITEMS: usize = 50;

/// Frozen schema of one event. `items` lists the fields of the list field, if the event has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventSchema {
    pub name: &'static str,
    pub fields: &'static [&'static str],
    pub items: &'static [&'static str],
}

const fn schema(name: &'static str, fields: &'static [&'static str]) -> EventSchema {
    EventSchema { name, fields, items: &[] }
}

/// The 17 P0 events of interface `0.0.1` (K-20) and the P1 events that joined with a patch bump: `member_admitted`
/// (CT-018, `0.0.2`), `member_reactivated` (CT-079, `0.0.2`), `solver_fee_claimed` and `solver_fee_claim_failed` (CT-052, `0.0.2`), `upgrade_proposed` and `upgrade_applied` (CT-055, `0.0.3`). The other P1/P2 events join when their items are built.
pub const EVENT_SCHEMAS: [EventSchema; 39] = [
    schema("member_joined", &["account_id", "pubkey_voucher", "terms_hash", "status", "joined_at"]),
    schema("stake_added", &["account_id", "sender_id", "amount", "stake", "limit", "status"]),
    schema(
        "voucher_registered",
        &["payer", "payee", "epoch", "seq", "cumulative_amount", "delta", "obligation", "outstanding_debt", "mode"],
    ),
    schema(
        "epoch_cutoff",
        &[
            "epoch",
            "closed_at",
            "proposal_deadline",
            "edge_count",
            "participant_count",
            "total_obligation",
            "next_epoch",
            "next_cutoff_at",
        ],
    ),
    schema(
        "clearing_submitted",
        &["epoch", "solver", "cleared_total", "edge_count", "replaced_solver", "replaced_cleared_total"],
    ),
    EventSchema {
        name: "clearing_applied",
        fields: &["epoch", "solver", "cleared_total", "edges", "clearing_fee", "solver_fee", "chunk", "chunks"],
        items: &["debtor", "creditor", "cleared"],
    },
    EventSchema {
        name: "epoch_committed",
        fields: &[
            "epoch",
            "committed_at",
            "payment_deadline",
            "total_due",
            "total_claimable",
            "positions",
            "chunk",
            "chunks",
        ],
        items: &["account_id", "net", "role", "status", "amount", "outstanding_debt"],
    },
    schema("residual_paid", &["epoch", "account_id", "sender_id", "amount", "refunded", "settled_history"]),
    schema(
        "member_defaulted",
        &[
            "epoch",
            "account_id",
            "due",
            "from_stake",
            "from_reserve",
            "shortfall",
            "stake",
            "status",
            "position_status",
            "outstanding_debt",
        ],
    ),
    EventSchema {
        name: "limits_updated",
        fields: &["epoch", "changes", "chunk", "chunks"],
        items: &["account_id", "old_limit", "new_limit", "status"],
    },
    schema("epoch_finalized", &["epoch", "finalized_at", "defaulted_count", "total_defaulted", "pool", "shortfall"]),
    schema("residual_claimed", &["epoch", "account_id", "amount", "destination", "status"]),
    schema("residual_claim_failed", &["epoch", "account_id", "amount", "destination", "status"]),
    schema("params_updated", &["old", "new", "changed", "effective_epoch"]),
    schema("exit_requested", &["account_id", "epoch"]),
    schema("stake_withdrawn", &["account_id", "amount"]),
    schema("stake_withdraw_failed", &["account_id", "amount"]),
    schema("member_admitted", &["account_id", "status"]),
    schema("member_reactivated", &["account_id", "stake", "limit", "effective_epoch"]),
    schema("solver_fee_claimed", &["account_id", "amount"]),
    schema("solver_fee_claim_failed", &["account_id", "amount"]),
    schema("upgrade_proposed", &["code_hash", "eta"]),
    schema("upgrade_applied", &["code_hash", "applied_at"]),
    schema("intent_posted", &["intent_hash", "account_id", "posted_at"]),
    schema("asset_class_set", &["asset_type", "class"]),
    schema("grant_offered", &["grant_id", "asset_type", "grantor", "class", "unit", "unit_price", "expires_at"]),
    schema("grant_activated", &["grant_id", "grantee", "delivery_pubkey"]),
    schema("grant_revoked", &["grant_id", "reason"]),
    schema("escrow_held", &["grant_id", "nft_contract", "token_id"]),
    schema("ft_escrow_held", &["grant_id", "ft_contract", "amount"]),
    schema("freeze_proposed", &["account_id", "eta", "reason_hash"]),
    schema("member_frozen", &["account_id", "from_epoch"]),
    schema("freeze_cancelled", &["account_id"]),
    schema("member_unfrozen", &["account_id"]),
    schema("mt_escrow_held", &["grant_id", "mt_contract", "token_id", "amount"]),
    schema("escrow_failed", &["grant_id", "step", "epoch", "payer", "payee", "delta", "reverted"]),
    EventSchema {
        name: "bundle_committed",
        fields: &["bundle_id", "terms_hash", "epoch", "legs", "solver"],
        items: &["payer", "payee", "max_amount"],
    },
    schema("intents_withdrawn", &["amount"]),
    schema("intent_removed", &["intent_hash", "account_id"]),
];

pub trait X207Event: Serialize {
    const NAME: &'static str;

    fn to_log(&self) -> String {
        let envelope = json!({
            "standard": EVENT_STANDARD,
            "version": INTERFACE_VERSION,
            "event": Self::NAME,
            "data": [self],
        });
        format!("EVENT_JSON:{envelope}")
    }

    fn emit(&self) {
        env::log_str(&self.to_log());
    }
}

macro_rules! event {
    ($name:literal, $ty:ident { $($field:ident: $field_ty:ty),+ $(,)? }) => {
        #[derive(Serialize, Debug, Clone)]
        #[serde(crate = "near_sdk::serde")]
        pub struct $ty {
            $(pub $field: $field_ty,)+
        }

        impl X207Event for $ty {
            const NAME: &'static str = $name;
        }
    };
}

event!("member_joined", MemberJoined {
    account_id: AccountId,
    pubkey_voucher: String,
    terms_hash: String,
    status: String,
    joined_at: U64,
});

event!("stake_added", StakeAdded {
    account_id: AccountId,
    sender_id: AccountId,
    amount: U128,
    stake: U128,
    limit: U128,
    status: String,
});

event!("voucher_registered", VoucherRegistered {
    payer: AccountId,
    payee: AccountId,
    epoch: EpochId,
    seq: Seq,
    cumulative_amount: U128,
    delta: U128,
    obligation: U128,
    outstanding_debt: U128,
    mode: String,
});

event!("epoch_cutoff", EpochCutoff {
    epoch: EpochId,
    closed_at: U64,
    proposal_deadline: U64,
    edge_count: u32,
    participant_count: u32,
    total_obligation: U128,
    next_epoch: EpochId,
    next_cutoff_at: U64,
});

event!("clearing_submitted", ClearingSubmitted {
    epoch: EpochId,
    solver: AccountId,
    cleared_total: U128,
    edge_count: u32,
    replaced_solver: Option<AccountId>,
    replaced_cleared_total: Option<U128>,
});

event!("clearing_applied", ClearingApplied {
    epoch: EpochId,
    solver: Option<AccountId>,
    cleared_total: U128,
    edges: Vec<ClearingEdge>,
    clearing_fee: U128,
    solver_fee: U128,
    chunk: u32,
    chunks: u32,
});

/// One element of `epoch_committed.positions[]`. Net-zero participants carry `role` `"none"`,
/// `status` `null`, `amount` `"0"` (K-35).
#[derive(Serialize, Debug, Clone)]
#[serde(crate = "near_sdk::serde")]
pub struct PositionEntry {
    pub account_id: AccountId,
    pub net: I128,
    pub role: String,
    pub status: Option<String>,
    pub amount: U128,
    pub outstanding_debt: U128,
}

event!("epoch_committed", EpochCommitted {
    epoch: EpochId,
    committed_at: U64,
    payment_deadline: U64,
    total_due: U128,
    total_claimable: U128,
    positions: Vec<PositionEntry>,
    chunk: u32,
    chunks: u32,
});

event!("residual_paid", ResidualPaid {
    epoch: EpochId,
    account_id: AccountId,
    sender_id: AccountId,
    amount: U128,
    refunded: U128,
    settled_history: U128,
});

event!("member_defaulted", MemberDefaulted {
    epoch: EpochId,
    account_id: AccountId,
    due: U128,
    from_stake: U128,
    from_reserve: U128,
    shortfall: U128,
    stake: U128,
    status: String,
    position_status: String,
    outstanding_debt: U128,
});

/// One element of `limits_updated.changes[]`: only `active` members whose limit changed (K-11).
#[derive(Serialize, Debug, Clone)]
#[serde(crate = "near_sdk::serde")]
pub struct LimitChange {
    pub account_id: AccountId,
    pub old_limit: U128,
    pub new_limit: U128,
    pub status: String,
}

event!("limits_updated", LimitsUpdated {
    epoch: EpochId,
    changes: Vec<LimitChange>,
    chunk: u32,
    chunks: u32,
});

event!("epoch_finalized", EpochFinalized {
    epoch: EpochId,
    finalized_at: U64,
    defaulted_count: u32,
    total_defaulted: U128,
    pool: U128,
    shortfall: U128,
});

event!("residual_claimed", ResidualClaimed {
    epoch: EpochId,
    account_id: AccountId,
    amount: U128,
    destination: String,
    status: String,
});

event!("intents_withdrawn", IntentsWithdrawn {
    amount: U128,
});

event!("residual_claim_failed", ResidualClaimFailed {
    epoch: EpochId,
    account_id: AccountId,
    amount: U128,
    destination: String,
    status: String,
});

/// `old` and `new` are `Params`; generic so this schema does not depend on `params.rs`.
#[derive(Serialize, Debug, Clone)]
#[serde(crate = "near_sdk::serde")]
pub struct ParamsUpdated<P: Serialize> {
    pub old: P,
    pub new: P,
    pub changed: Vec<String>,
    pub effective_epoch: EpochId,
}

impl<P: Serialize> X207Event for ParamsUpdated<P> {
    const NAME: &'static str = "params_updated";
}

event!("exit_requested", ExitRequested { account_id: AccountId, epoch: EpochId });
event!("stake_withdrawn", StakeWithdrawn { account_id: AccountId, amount: U128 });
event!("stake_withdraw_failed", StakeWithdrawFailed { account_id: AccountId, amount: U128 });
event!("member_admitted", MemberAdmitted { account_id: AccountId, status: String });
event!("intent_posted", IntentPosted { intent_hash: near_sdk::json_types::Base58CryptoHash, account_id: AccountId, posted_at: U64 });
event!("intent_removed", IntentRemoved { intent_hash: near_sdk::json_types::Base58CryptoHash, account_id: AccountId });
event!("asset_class_set", AssetClassSet { asset_type: String, class: crate::grant::AssetClass });
event!("grant_offered", GrantOffered { grant_id: u64, asset_type: String, grantor: AccountId, class: crate::grant::AssetClass, unit: String, unit_price: U128, expires_at: U64 });
event!("grant_activated", GrantActivated { grant_id: u64, grantee: AccountId, delivery_pubkey: Option<near_sdk::PublicKey> });
event!("grant_revoked", GrantRevoked { grant_id: u64, reason: crate::grant::RevokeReason });
event!("escrow_held", EscrowHeld { grant_id: u64, nft_contract: AccountId, token_id: String });
event!("ft_escrow_held", FtEscrowHeld { grant_id: u64, ft_contract: AccountId, amount: U128 });
event!("mt_escrow_held", MtEscrowHeld { grant_id: u64, mt_contract: AccountId, token_id: String, amount: U128 });
event!("freeze_proposed", FreezeProposed { account_id: AccountId, eta: U64, reason_hash: Option<String> });
event!("member_frozen", MemberFrozen { account_id: AccountId, from_epoch: EpochId });
event!("freeze_cancelled", FreezeCancelled { account_id: AccountId });
event!("member_unfrozen", MemberUnfrozen { account_id: AccountId });
event!("escrow_failed", EscrowFailed { grant_id: u64, step: crate::escrow::EscrowStep, epoch: EpochId, payer: AccountId, payee: AccountId, delta: U128, reverted: bool });
event!("bundle_committed", BundleCommitted { bundle_id: u64, terms_hash: String, epoch: EpochId, legs: Vec<crate::bundle::BundleLeg>, solver: AccountId });
event!("upgrade_proposed", UpgradeProposed { code_hash: near_sdk::json_types::Base58CryptoHash, eta: U64 });
event!("upgrade_applied", UpgradeApplied { code_hash: near_sdk::json_types::Base58CryptoHash, applied_at: U64 });
event!("solver_fee_claimed", SolverFeeClaimed { account_id: AccountId, amount: U128 });
event!("solver_fee_claim_failed", SolverFeeClaimFailed { account_id: AccountId, amount: U128 });
event!("member_reactivated", MemberReactivated { account_id: AccountId, stake: U128, limit: U128, effective_epoch: EpochId });

/// Logs of one event per chunk of at most `MAX_EVENT_ITEMS` items, in order. An empty list still
/// gives one event with `chunk` 0 and `chunks` 1 (K-19). `build` repeats the scalar fields in every chunk.
pub fn chunked_logs<T: Clone, E: X207Event>(items: &[T], build: impl Fn(Vec<T>, u32, u32) -> E) -> Vec<String> {
    let chunks = items.len().div_ceil(MAX_EVENT_ITEMS).max(1) as u32;
    if items.is_empty() {
        return vec![build(Vec::new(), 0, chunks).to_log()];
    }
    items.chunks(MAX_EVENT_ITEMS).enumerate().map(|(index, chunk)| build(chunk.to_vec(), index as u32, chunks).to_log()).collect()
}

/// Emits `chunked_logs` in the same receipt.
pub fn emit_chunked<T: Clone, E: X207Event>(items: &[T], build: impl Fn(Vec<T>, u32, u32) -> E) {
    for log in chunked_logs(items, build) {
        env::log_str(&log);
    }
}

/// Emits `clearing_applied` with only the edges that have `cleared > 0` (ADR-31).
pub fn emit_clearing_applied(
    epoch: EpochId,
    solver: Option<AccountId>,
    cleared_total: U128,
    edges: &[ClearingEdge],
    clearing_fee: U128,
    solver_fee: U128,
) {
    let edges: Vec<ClearingEdge> = edges.iter().filter(|edge| edge.cleared.0 > 0).cloned().collect();
    emit_chunked(&edges, |edges, chunk, chunks| ClearingApplied {
        epoch,
        solver: solver.clone(),
        cleared_total,
        edges,
        clearing_fee,
        solver_fee,
        chunk,
        chunks,
    });
}

/// Serializes `event` and returns its `data[0]` object, for unit tests of event emitters.
#[cfg(test)]
pub fn event_data<E: X207Event>(event: &E) -> near_sdk::serde_json::Value {
    let log = event.to_log();
    let body = log.strip_prefix("EVENT_JSON:").expect("EVENT_JSON prefix");
    let parsed: near_sdk::serde_json::Value = near_sdk::serde_json::from_str(body).expect("valid JSON");
    parsed["data"][0].clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::serde_json::{self, Value};
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::testing_env;

    fn acc(name: &str) -> AccountId {
        name.parse().unwrap()
    }

    fn edge(n: usize, cleared: u128) -> ClearingEdge {
        ClearingEdge { debtor: acc(&format!("d{n}.near")), creditor: acc(&format!("c{n}.near")), cleared: U128(cleared) }
    }

    fn position(account: &str, net: i128, role: &str, status: Option<&str>, amount: u128, debt: u128) -> PositionEntry {
        PositionEntry {
            account_id: acc(account),
            net: I128(net),
            role: role.to_string(),
            status: status.map(str::to_string),
            amount: U128(amount),
            outstanding_debt: U128(debt),
        }
    }

    fn change(account: AccountId) -> LimitChange {
        LimitChange { account_id: account, old_limit: U128(80_000_000), new_limit: U128(83_500_000), status: "active".into() }
    }

    /// One sample log per P0 event, keyed by event name.
    fn samples() -> Vec<(&'static str, String)> {
        let t = U64(1_793_836_800_000_000_000);
        let a = acc("a.near");
        let b = acc("b.near");
        vec![
            (MemberJoined::NAME, MemberJoined { account_id: a.clone(), pubkey_voucher: "ed25519:7Yk".into(), terms_hash: "9f".repeat(32), status: "pending".into(), joined_at: t }.to_log()),
            (StakeAdded::NAME, StakeAdded { account_id: a.clone(), sender_id: b.clone(), amount: U128(1), stake: U128(1), limit: U128(0), status: "active".into() }.to_log()),
            (VoucherRegistered::NAME, VoucherRegistered { payer: a.clone(), payee: b.clone(), epoch: 42, seq: 137, cumulative_amount: U128(2_740_000), delta: U128(20_000), obligation: U128(2_740_000), outstanding_debt: U128(2_740_000), mode: "cumulative".into() }.to_log()),
            (EpochCutoff::NAME, EpochCutoff { epoch: 42, closed_at: t, proposal_deadline: t, edge_count: 30, participant_count: 6, total_obligation: U128(5), next_epoch: 43, next_cutoff_at: t }.to_log()),
            (ClearingSubmitted::NAME, ClearingSubmitted { epoch: 42, solver: a.clone(), cleared_total: U128(5), edge_count: 3, replaced_solver: None, replaced_cleared_total: None }.to_log()),
            (ClearingApplied::NAME, ClearingApplied { epoch: 42, solver: None, cleared_total: U128(0), edges: vec![edge(1, 5)], clearing_fee: U128(0), solver_fee: U128(0), chunk: 0, chunks: 1 }.to_log()),
            (EpochCommitted::NAME, EpochCommitted { epoch: 42, committed_at: t, payment_deadline: t, total_due: U128(35), total_claimable: U128(35), positions: vec![position("s.near", -35, "debtor", Some("due"), 35, 35)], chunk: 0, chunks: 1 }.to_log()),
            (ResidualPaid::NAME, ResidualPaid { epoch: 42, account_id: a.clone(), sender_id: b.clone(), amount: U128(35), refunded: U128(0), settled_history: U128(35) }.to_log()),
            (MemberDefaulted::NAME, MemberDefaulted { epoch: 42, account_id: a.clone(), due: U128(35), from_stake: U128(35), from_reserve: U128(0), shortfall: U128(0), stake: U128(65), status: "suspended".into(), position_status: "defaulted".into(), outstanding_debt: U128(0) }.to_log()),
            (LimitsUpdated::NAME, LimitsUpdated { epoch: 42, changes: vec![change(a.clone())], chunk: 0, chunks: 1 }.to_log()),
            (EpochFinalized::NAME, EpochFinalized { epoch: 42, finalized_at: t, defaulted_count: 1, total_defaulted: U128(35), pool: U128(35), shortfall: U128(0) }.to_log()),
            (ResidualClaimed::NAME, ResidualClaimed { epoch: 42, account_id: a.clone(), amount: U128(35), destination: "near".into(), status: "claimed".into() }.to_log()),
            (IntentsWithdrawn::NAME, IntentsWithdrawn { amount: U128(35) }.to_log()),
            (ResidualClaimFailed::NAME, ResidualClaimFailed { epoch: 42, account_id: a.clone(), amount: U128(35), destination: "intents".into(), status: "claimable".into() }.to_log()),
            ("params_updated", ParamsUpdated { old: json!({"epoch_length": 3600}), new: json!({"epoch_length": 7200}), changed: vec!["epoch_length".to_string()], effective_epoch: 43 }.to_log()),
            (ExitRequested::NAME, ExitRequested { account_id: a.clone(), epoch: 42 }.to_log()),
            (StakeWithdrawn::NAME, StakeWithdrawn { account_id: a.clone(), amount: U128(0) }.to_log()),
            (StakeWithdrawFailed::NAME, StakeWithdrawFailed { account_id: a.clone(), amount: U128(1) }.to_log()),
            (MemberAdmitted::NAME, MemberAdmitted { account_id: a.clone(), status: "pending".into() }.to_log()),
            (MemberReactivated::NAME, MemberReactivated { account_id: a.clone(), stake: U128(115), limit: U128(92), effective_epoch: 43 }.to_log()),
            (SolverFeeClaimed::NAME, SolverFeeClaimed { account_id: a.clone(), amount: U128(375_000) }.to_log()),
            (SolverFeeClaimFailed::NAME, SolverFeeClaimFailed { account_id: a.clone(), amount: U128(375_000) }.to_log()),
            (UpgradeProposed::NAME, UpgradeProposed { code_hash: [7u8; 32].into(), eta: t }.to_log()),
            (IntentRemoved::NAME, IntentRemoved { intent_hash: [3u8; 32].into(), account_id: a.clone() }.to_log()),
            (IntentPosted::NAME, IntentPosted { intent_hash: [3u8; 32].into(), account_id: a.clone(), posted_at: t }.to_log()),
            (AssetClassSet::NAME, AssetClassSet { asset_type: "x:y".into(), class: crate::grant::AssetClass::B }.to_log()),
            (GrantOffered::NAME, GrantOffered { grant_id: 1, asset_type: "x:y".into(), grantor: a.clone(), class: crate::grant::AssetClass::B, unit: "call".into(), unit_price: U128(1), expires_at: t }.to_log()),
            (GrantActivated::NAME, GrantActivated { grant_id: 1, grantee: b.clone(), delivery_pubkey: None }.to_log()),
            (GrantRevoked::NAME, GrantRevoked { grant_id: 1, reason: crate::grant::RevokeReason::Grantor }.to_log()),
            (EscrowHeld::NAME, EscrowHeld { grant_id: 1, nft_contract: b.clone(), token_id: "t1".into() }.to_log()),
            (FreezeProposed::NAME, FreezeProposed { account_id: a.clone(), eta: t, reason_hash: None }.to_log()),
            (MemberFrozen::NAME, MemberFrozen { account_id: a.clone(), from_epoch: 42 }.to_log()),
            (FreezeCancelled::NAME, FreezeCancelled { account_id: a.clone() }.to_log()),
            (MemberUnfrozen::NAME, MemberUnfrozen { account_id: a.clone() }.to_log()),
            (MtEscrowHeld::NAME, MtEscrowHeld { grant_id: 1, mt_contract: b.clone(), token_id: "nep141:x.near".into(), amount: U128(5) }.to_log()),
            (FtEscrowHeld::NAME, FtEscrowHeld { grant_id: 1, ft_contract: b.clone(), amount: U128(5) }.to_log()),
            (EscrowFailed::NAME, EscrowFailed { grant_id: 1, step: crate::escrow::EscrowStep::Complete, epoch: 42, payer: a.clone(), payee: b.clone(), delta: U128(5), reverted: true }.to_log()),
            (BundleCommitted::NAME, BundleCommitted { bundle_id: 1, terms_hash: "ab".repeat(32), epoch: 42, legs: vec![crate::bundle::BundleLeg { payer: a.clone(), payee: b.clone(), max_amount: U128(10) }], solver: b.clone() }.to_log()),
            (UpgradeApplied::NAME, UpgradeApplied { code_hash: [7u8; 32].into(), applied_at: t }.to_log()),
        ]
    }

    fn parse(log: &str) -> Value {
        let body = log.strip_prefix("EVENT_JSON:").expect("EVENT_JSON prefix");
        serde_json::from_str(body).expect("valid JSON")
    }

    fn data(log: &str) -> Value {
        parse(log)["data"][0].clone()
    }

    fn sorted_keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        keys
    }

    fn sorted(fields: &[&str]) -> Vec<String> {
        let mut keys: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
        keys.sort();
        keys
    }

    // TC-CT-013-01
    #[test]
    fn envelope_format_for_every_p0_event() {
        let samples = samples();
        assert_eq!(samples.len(), EVENT_SCHEMAS.len());
        for (name, log) in &samples {
            let parsed = parse(log);
            assert_eq!(parsed["standard"], "x207");
            assert_eq!(parsed["version"], crate::INTERFACE_VERSION);
            assert_eq!(parsed["event"], *name);
            let data = parsed["data"].as_array().expect("data array");
            assert_eq!(data.len(), 1);
            assert!(data[0].is_object());
        }
    }

    // TC-CT-013-02
    #[test]
    fn data_fields_match_spec_exactly() {
        for (name, log) in samples() {
            let schema = EVENT_SCHEMAS.iter().find(|s| s.name == name).expect("schema");
            let payload = data(&log);
            assert_eq!(sorted_keys(&payload), sorted(schema.fields), "{name}");
            for list in ["edges", "positions", "changes"] {
                if let Some(items) = payload.get(list) {
                    assert_eq!(sorted_keys(&items[0]), sorted(schema.items), "{name}.{list}");
                }
            }
        }
    }

    // TC-CT-013-03
    #[test]
    fn field_types_on_the_wire() {
        let logs = samples();
        let by_name = |name: &str| data(&logs.iter().find(|(n, _)| *n == name).unwrap().1);

        let voucher = by_name("voucher_registered");
        assert!(voucher["epoch"].is_u64());
        assert!(voucher["seq"].is_u64());
        for field in ["cumulative_amount", "delta", "obligation", "outstanding_debt"] {
            assert!(voucher[field].is_string(), "{field}");
        }

        let cutoff = by_name("epoch_cutoff");
        for field in ["closed_at", "proposal_deadline", "next_cutoff_at"] {
            assert_eq!(cutoff[field], "1793836800000000000", "{field}");
        }
        assert_eq!(by_name("member_joined")["joined_at"], "1793836800000000000");

        let committed = by_name("epoch_committed");
        assert_eq!(committed["positions"][0]["net"], "-35");
        assert_eq!(committed["total_due"], "35");
    }

    // TC-CT-013-04
    #[test]
    fn voucher_registered_uses_snake_case() {
        let logs = samples();
        let log = &logs.iter().find(|(n, _)| *n == "voucher_registered").unwrap().1;
        assert_eq!(data(log)["cumulative_amount"], "2740000");
        assert!(!log.contains("cumulativeAmount"));
    }

    // TC-CT-013-05
    #[test]
    fn outstanding_debt_and_net_zero_positions() {
        let event = EpochCommitted {
            epoch: 1,
            committed_at: U64(1),
            payment_deadline: U64(2),
            total_due: U128(35_000_000),
            total_claimable: U128(35_000_000),
            positions: vec![
                position("scraper.near", -35_000_000, "debtor", Some("due"), 35_000_000, 35_000_000),
                position("gpu.near", 35_000_000, "creditor", Some("claimable"), 35_000_000, 0),
                position("translator.near", 0, "none", None, 0, 0),
            ],
            chunk: 0,
            chunks: 1,
        };
        let payload = event_data(&event);
        assert_eq!(payload["positions"][0]["outstanding_debt"], "35000000");
        let zero = &payload["positions"][2];
        assert_eq!(zero["role"], "none");
        assert!(zero["status"].is_null());
        assert_eq!(zero["amount"], "0");
        assert_eq!(zero["outstanding_debt"], "0");

        let logs = samples();
        let defaulted = data(&logs.iter().find(|(n, _)| *n == "member_defaulted").unwrap().1);
        assert_eq!(defaulted["outstanding_debt"], "0");
    }

    // TC-CT-013-06
    #[test]
    fn clearing_applied_drops_zero_edges() {
        testing_env!(VMContextBuilder::new().build());
        emit_clearing_applied(1, Some(acc("solver.near")), U128(25), &[edge(1, 25), edge(2, 0)], U128(0), U128(0));
        let logs = get_logs();
        assert_eq!(logs.len(), 1);
        let edges = data(&logs[0])["edges"].as_array().unwrap().clone();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["cleared"], "25");
    }

    fn limit_changes(n: usize) -> Vec<LimitChange> {
        (0..n).map(|i| change(acc(&format!("m{i}.near")))).collect()
    }

    fn emit_limits(changes: &[LimitChange]) {
        emit_chunked(changes, |changes, chunk, chunks| LimitsUpdated { epoch: 7, changes, chunk, chunks });
    }

    // TC-CT-013-07
    #[test]
    fn chunking_at_fifty_items() {
        testing_env!(VMContextBuilder::new().build());
        emit_limits(&limit_changes(50));
        let logs = get_logs();
        assert_eq!(logs.len(), 1);
        let payload = data(&logs[0]);
        assert_eq!((payload["chunk"].as_u64(), payload["chunks"].as_u64()), (Some(0), Some(1)));
        assert_eq!(payload["changes"].as_array().unwrap().len(), 50);

        testing_env!(VMContextBuilder::new().build());
        emit_limits(&limit_changes(51));
        let logs = get_logs();
        assert_eq!(logs.len(), 2);
        for (index, log) in logs.iter().enumerate() {
            let payload = data(log);
            assert_eq!(payload["chunk"].as_u64(), Some(index as u64));
            assert_eq!(payload["chunks"].as_u64(), Some(2));
            assert_eq!(payload["epoch"].as_u64(), Some(7));
        }
        assert_eq!(data(&logs[0])["changes"].as_array().unwrap().len(), 50);
        assert_eq!(data(&logs[1])["changes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn empty_list_still_emits_one_chunk() {
        testing_env!(VMContextBuilder::new().build());
        emit_limits(&[]);
        let logs = get_logs();
        assert_eq!(logs.len(), 1);
        let payload = data(&logs[0]);
        assert_eq!(payload["changes"], json!([]));
        assert_eq!((payload["chunk"].as_u64(), payload["chunks"].as_u64()), (Some(0), Some(1)));
    }

    // TC-CT-013-08
    #[test]
    fn fifty_items_with_longest_accounts_fit_in_a_log() {
        const LOG_LIMIT: usize = 16 * 1024;
        let long = |i: usize| acc(&format!("{i:02}{}", "a".repeat(62)));
        assert_eq!(long(0).len(), 64);
        let max = U128(u128::MAX);

        let changes: Vec<LimitChange> = (0..50)
            .map(|i| LimitChange { account_id: long(i), old_limit: max, new_limit: max, status: "active".into() })
            .collect();
        let limits = LimitsUpdated { epoch: u64::MAX, changes, chunk: 0, chunks: 1 }.to_log();
        assert!(limits.len() <= LOG_LIMIT, "limits_updated {}", limits.len());

        let positions: Vec<PositionEntry> = (0..50)
            .map(|i| PositionEntry {
                account_id: long(i),
                net: I128(i128::MIN),
                role: "creditor".into(),
                status: Some("claimable".into()),
                amount: max,
                outstanding_debt: max,
            })
            .collect();
        let committed = EpochCommitted {
            epoch: u64::MAX,
            committed_at: U64(u64::MAX),
            payment_deadline: U64(u64::MAX),
            total_due: max,
            total_claimable: max,
            positions,
            chunk: 0,
            chunks: 1,
        }
        .to_log();
        assert!(committed.len() <= LOG_LIMIT, "epoch_committed {}", committed.len());

        let edges: Vec<ClearingEdge> =
            (0..50).map(|i| ClearingEdge { debtor: long(i), creditor: long(i + 50), cleared: max }).collect();
        let applied = ClearingApplied {
            epoch: u64::MAX,
            solver: Some(long(99)),
            cleared_total: max,
            edges,
            clearing_fee: max,
            solver_fee: max,
            chunk: 0,
            chunks: 1,
        }
        .to_log();
        assert!(applied.len() <= LOG_LIMIT, "clearing_applied {}", applied.len());
    }

    // TC-CT-013-09, TC-CT-068-07
    #[test]
    fn frozen_event_list() {
        let names: Vec<&str> = EVENT_SCHEMAS.iter().map(|s| s.name).collect();
        for p1 in [] {
            assert!(!names.contains(&p1), "{p1}");
        }
        assert_eq!(
            names,
            [
                "member_joined", "stake_added", "voucher_registered", "epoch_cutoff", "clearing_submitted",
                "clearing_applied", "epoch_committed", "residual_paid", "member_defaulted", "limits_updated",
                "epoch_finalized", "residual_claimed", "residual_claim_failed", "params_updated", "exit_requested",
                "stake_withdrawn", "stake_withdraw_failed", "member_admitted", "member_reactivated", "solver_fee_claimed",
                "solver_fee_claim_failed", "upgrade_proposed", "upgrade_applied",
                "intent_posted", "asset_class_set", "grant_offered", "grant_activated", "grant_revoked", "escrow_held", "ft_escrow_held", "freeze_proposed", "member_frozen", "freeze_cancelled", "member_unfrozen", "mt_escrow_held", "escrow_failed", "bundle_committed",
                "intents_withdrawn", "intent_removed",
            ]
        );
    }
}
