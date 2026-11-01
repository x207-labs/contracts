//! Contract root `X207Circle` (CT-006) and the `interface_version` view.
//!
//! Collections follow spec section 9.1. Each process module owns the record type and the
//! `#[near] impl X207Circle` block for its collections; this file only wires storage.

use near_sdk::borsh::BorshSerialize;
use near_sdk::json_types::Base58CryptoHash;
use near_sdk::store::{LookupMap, Vector};
use near_sdk::{near, AccountId, BorshStorageKey, PanicOnDefault};

use crate::clearing::{ClearingProposal, Epoch};
use crate::member::Member;
use crate::params::Params;
use crate::settlement::Position;
use crate::types::EpochId;
use crate::voucher::{PairState, ParticipantTotals};
use crate::INTERFACE_VERSION;

/// Storage prefixes (spec 9.1). The first byte of every family is distinct, so a per-epoch key
/// (`E` + epoch) can never equal a key of another family. Variants without data are the ASCII
/// letter of the spec; per-epoch variants append the little-endian `EpochId`.
#[derive(BorshStorageKey, BorshSerialize, Clone, Copy, Debug, PartialEq, Eq)]
#[borsh(use_discriminant = true, crate = "near_sdk::borsh")]
#[repr(u8)]
pub enum StorageKey {
    /// D1 `AccountId -> Member`.
    Members = b'm',
    /// D1 member index -> `AccountId`.
    MemberIndex = b'M',
    /// D2 `AccountId -> outstanding_debt`.
    OutstandingDebt = b'o',
    /// D2/D3 `(EpochId, payer, payee) -> PairState`.
    Pairs = b'p',
    /// D2 snapshot: edge index -> `(payer, payee)`, per epoch.
    EpochEdges(EpochId) = b'E',
    /// D2 aggregate `(EpochId, AccountId) -> ParticipantTotals`.
    Totals = b't',
    /// D2 aggregate: participant index -> `AccountId`, per epoch.
    EpochParticipants(EpochId) = b'T',
    /// D7 snapshot, D4 summary, D6 epoch pool: `EpochId -> Epoch`.
    Epochs = b'c',
    /// D4 `EpochId -> ClearingProposal`.
    Proposals = b'q',
    /// D5 `(EpochId, AccountId) -> Position`.
    Positions = b's',
    /// D5 position index -> `AccountId`, per epoch.
    EpochPositions(EpochId) = b'S',
    /// D6 `AccountId -> solver fee balance` (P1).
    SolverFees = b'f',
    /// D8 `CryptoHash -> IntentRecord` (P2).
    Intents = b'i',
    /// D8 `u64 -> Bundle` (P2).
    Bundles = b'b',
    /// D8 `(EpochId, AccountId) -> reserved headroom` (P2).
    BundleReserves = b'r',
}

/// Params written by `set_params`, applied by `close_epoch` from `effective_epoch` (spec `get_pending_params`).
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingParams {
    pub params: Params,
    pub effective_epoch: EpochId,
}

/// Pending contract upgrade (spec `get_upgrade`, CT-055).
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingUpgrade {
    pub code_hash: Base58CryptoHash,
    pub eta: u64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct X207Circle {
    pub owner_id: AccountId,
    pub circle_id: String,
    pub usdt_token_id: AccountId,
    pub intents_id: AccountId,
    pub current_epoch: EpochId,
    pub pending_params: Option<PendingParams>,
    /// D6: sum of member stakes.
    pub total_stake: u128,
    /// D6: reserve fund.
    pub reserve: u128,
    /// D6: USDT in `ft_transfer` promises that have not resolved yet.
    pub in_flight: u128,
    /// D6: running total of `Epoch.pool` over all epochs (CT-010).
    pub epoch_pools: u128,
    /// D6: running total of the `solver_fees` balances (CT-010, P1 fills it).
    pub solver_fees_total: u128,
    pub member_count: u32,
    pub upgrade: Option<PendingUpgrade>,
    /// D1.
    pub members: LookupMap<AccountId, Member>,
    pub member_index: Vector<AccountId>,
    /// D2.
    pub outstanding_debt: LookupMap<AccountId, u128>,
    /// D2 and D3, one entry per `(epoch, payer, payee)`.
    pub pairs: LookupMap<(EpochId, AccountId, AccountId), PairState>,
    pub totals: LookupMap<(EpochId, AccountId), ParticipantTotals>,
    /// D7 snapshot, D4 summary, D6 epoch pool.
    pub epochs: LookupMap<EpochId, Epoch>,
    /// D4.
    pub proposals: LookupMap<EpochId, ClearingProposal>,
    /// D5.
    pub positions: LookupMap<(EpochId, AccountId), Position>,
    /// D6 solver fee balances (P1).
    pub solver_fees: LookupMap<AccountId, u128>,
}

impl X207Circle {
    /// Builds the root with empty collections and epoch `1` already `open`. `params` must already
    /// be validated (CT-007); `new` (CT-008) is the `#[init]` wrapper around this.
    pub fn with_config(
        owner_id: AccountId,
        circle_id: String,
        usdt_token_id: AccountId,
        intents_id: AccountId,
        params: Params,
        now_ns: u64,
    ) -> Self {
        let mut epochs = LookupMap::new(StorageKey::Epochs);
        let cutoff_at = now_ns + params.epoch_length * 1_000_000_000;
        epochs.insert(1, Epoch::open(1, now_ns, cutoff_at, params));
        Self {
            owner_id,
            circle_id,
            usdt_token_id,
            intents_id,
            current_epoch: 1,
            pending_params: None,
            total_stake: 0,
            reserve: 0,
            in_flight: 0,
            epoch_pools: 0,
            solver_fees_total: 0,
            member_count: 0,
            upgrade: None,
            members: LookupMap::new(StorageKey::Members),
            member_index: Vector::new(StorageKey::MemberIndex),
            outstanding_debt: LookupMap::new(StorageKey::OutstandingDebt),
            pairs: LookupMap::new(StorageKey::Pairs),
            totals: LookupMap::new(StorageKey::Totals),
            epochs,
            proposals: LookupMap::new(StorageKey::Proposals),
            positions: LookupMap::new(StorageKey::Positions),
            solver_fees: LookupMap::new(StorageKey::SolverFees),
        }
    }
}

#[near]
impl X207Circle {
    pub fn interface_version(&self) -> String {
        INTERFACE_VERSION.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use near_sdk::borsh;
    use near_sdk::json_types::U128;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{env, testing_env, PublicKey};

    use super::*;
    use crate::clearing::EpochPhase;
    use crate::member::MemberStatus;
    use crate::params::{Admission, Fees};
    use crate::settlement::{PositionRole, PositionStatus};
    use crate::types::ClearingEdge;

    const ALL_KEYS: [StorageKey; 15] = [
        StorageKey::Members,
        StorageKey::MemberIndex,
        StorageKey::OutstandingDebt,
        StorageKey::Pairs,
        StorageKey::EpochEdges(1),
        StorageKey::Totals,
        StorageKey::EpochParticipants(1),
        StorageKey::Epochs,
        StorageKey::Proposals,
        StorageKey::Positions,
        StorageKey::EpochPositions(1),
        StorageKey::SolverFees,
        StorageKey::Intents,
        StorageKey::Bundles,
        StorageKey::BundleReserves,
    ];

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn params() -> Params {
        Params {
            epoch_length: 3600,
            payment_window: 1800,
            proposal_window: 600,
            base_limit_ratio: 8000,
            limit_growth: 1000,
            limit_cap: U128(1_000_000_000),
            limit_decay: 0,
            fees: Fees { clearing_fee_bps: 0, solver_fee_bps: 0 },
            admission: Admission::Open,
        }
    }

    fn circle() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        X207Circle::with_config(
            acc("dao.near"),
            "agents-main".to_string(),
            acc("usdt.near"),
            acc("intents.near"),
            params(),
            env::block_timestamp(),
        )
    }

    fn member(id: &str) -> Member {
        Member {
            account_id: acc(id),
            pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse::<PublicKey>().unwrap(),
            terms_hash: "a".repeat(64),
            stake: 100,
            limit: 80,
            status: MemberStatus::Active,
            admitted: false,
            joined_at: 1_000,
            settled_history: 0,
            last_active_epoch: 1,
            exit_requested_epoch: None,
            withdraw_pending: false,
            reactivate_epoch: None,
        }
    }

    fn prefix_bytes(k: StorageKey) -> Vec<u8> {
        borsh::to_vec(&k).unwrap()
    }

    #[test]
    fn tc_ct_006_01_prefixes_are_unique() {
        let all: HashSet<Vec<u8>> = ALL_KEYS.iter().map(|k| prefix_bytes(*k)).collect();
        assert_eq!(all.len(), ALL_KEYS.len());
        assert_eq!(prefix_bytes(StorageKey::Members), b"m");
        assert_eq!(prefix_bytes(StorageKey::MemberIndex), b"M");
        assert_eq!(prefix_bytes(StorageKey::EpochEdges(1)), [b'E', 1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn tc_ct_006_02_epoch_prefixes_do_not_collide() {
        for family in [
            StorageKey::EpochEdges as fn(EpochId) -> StorageKey,
            StorageKey::EpochParticipants,
            StorageKey::EpochPositions,
        ] {
            assert_ne!(prefix_bytes(family(1)), prefix_bytes(family(2)));
        }
        let mut c = circle();
        c.epochs.insert(2, Epoch::open(2, 2_000, 9_000, params()));
        let e1 = c.epochs.get_mut(&1).unwrap();
        e1.edges.push((acc("a.near"), acc("b.near")));
        e1.edges.push((acc("b.near"), acc("c.near")));
        e1.participants.push(acc("a.near"));
        e1.position_accounts.push(acc("a.near"));
        let e2 = c.epochs.get_mut(&2).unwrap();
        e2.edges.push((acc("x.near"), acc("y.near")));
        e2.participants.push(acc("x.near"));
        e2.participants.push(acc("y.near"));
        c.epochs.flush();
        let e1 = c.epochs.get(&1).unwrap();
        assert_eq!((e1.edges.len(), e1.participants.len(), e1.position_accounts.len()), (2, 1, 1));
        assert_eq!(e1.edges.get(1).unwrap().1, acc("c.near"));
        let e2 = c.epochs.get(&2).unwrap();
        assert_eq!((e2.edges.len(), e2.participants.len(), e2.position_accounts.len()), (1, 2, 0));
        assert_eq!(e2.edges.get(0).unwrap().0, acc("x.near"));
    }

    #[test]
    fn tc_ct_006_03_first_byte_distinct_per_family() {
        let firsts: HashSet<u8> = ALL_KEYS.iter().map(|k| prefix_bytes(*k)[0]).collect();
        assert_eq!(firsts.len(), ALL_KEYS.len());
    }

    #[test]
    fn tc_ct_006_04_all_collections_round_trip() {
        let mut c = circle();
        let (a, b) = (acc("a.near"), acc("b.near"));

        c.members.insert(a.clone(), member("a.near"));
        c.member_index.push(a.clone());
        assert_eq!(c.members.get(&a).unwrap().stake, 100);
        assert_eq!(c.member_index.get(0), Some(&a));

        c.outstanding_debt.insert(a.clone(), 7);
        assert_eq!(c.outstanding_debt.get(&a), Some(&7));

        let pair = PairState { cumulative: 10, last_seq: 3, cleared: 4 };
        c.pairs.insert((1, a.clone(), b.clone()), pair.clone());
        assert_eq!(c.pairs.get(&(1, a.clone(), b.clone())), Some(&pair));
        assert_eq!(c.pairs.get(&(1, b.clone(), a.clone())), None);

        let totals = ParticipantTotals { owed_out: 10, owed_in: 2 };
        c.totals.insert((1, a.clone()), totals.clone());
        assert_eq!(c.totals.get(&(1, a.clone())), Some(&totals));

        let e = c.epochs.get(&1).unwrap();
        assert_eq!((e.phase, e.cutoff_at, e.params.clone()), (EpochPhase::Open, 1_000 + 3600 * 1_000_000_000, params()));

        let proposal = ClearingProposal {
            epoch: 1,
            solver: acc("solver.near"),
            edges: vec![ClearingEdge { debtor: a.clone(), creditor: b.clone(), cleared: U128(5) }],
            edge_count: 1,
            cleared_total: U128(5),
            submitted_at: near_sdk::json_types::U64(1),
            committed: false,
        };
        c.proposals.insert(1, proposal.clone());
        assert_eq!(c.proposals.get(&1), Some(&proposal));

        let position = Position {
            net: -35,
            role: PositionRole::Debtor,
            status: PositionStatus::Due,
            amount: 35,
            recovered: 0,
            payment_deadline: 99,
            updated_at_block: 12,
        };
        c.positions.insert((1, a.clone()), position.clone());
        assert_eq!(c.positions.get(&(1, a.clone())), Some(&position));

        c.solver_fees.insert(acc("solver.near"), 3);
        assert_eq!(c.solver_fees.get(&acc("solver.near")), Some(&3));
    }

    #[test]
    fn tc_ct_006_05_root_fields() {
        let c = circle();
        assert_eq!(c.owner_id, acc("dao.near"));
        assert_eq!(c.circle_id, "agents-main");
        assert_eq!(c.usdt_token_id, acc("usdt.near"));
        assert_eq!(c.intents_id, acc("intents.near"));
        assert_eq!(c.current_epoch, 1);
        assert!(c.pending_params.is_none() && c.upgrade.is_none());
        assert_eq!((c.total_stake, c.reserve, c.in_flight, c.member_count), (0, 0, 0, 0));
        assert_eq!(c.interface_version(), "0.0.1");
    }
}
