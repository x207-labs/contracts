//! CT-004 gas probe. Mimics the storage shape of spec 9.1 (pairs, participant totals, positions,
//! one proposal blob per epoch) and the two transactions of clearing:
//! `probe_verify` ~ `submit_clearing` and `probe_apply` ~ `commit_epoch`.

use std::collections::{HashMap, HashSet};

use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::store::{LookupMap, Vector};
use near_sdk::{env, near, AccountId, PanicOnDefault};

const EPOCH: u64 = 1;

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Edge {
    pub debtor: AccountId,
    pub creditor: AccountId,
    pub cleared: U128,
}

#[near(serializers = [borsh])]
#[derive(Clone)]
struct PairState {
    cumulative: u128,
    last_seq: u64,
    cleared: u128,
}

#[near(serializers = [borsh])]
#[derive(Clone)]
struct Totals {
    owed_out: u128,
    owed_in: u128,
}

#[near(serializers = [borsh])]
#[derive(Clone)]
struct Position {
    net: i128,
    role: u8,
    status: u8,
    amount: u128,
    recovered: u128,
    payment_deadline: u64,
    updated_at_block: u64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Probe {
    pairs: LookupMap<(u64, AccountId, AccountId), PairState>,
    totals: LookupMap<(u64, AccountId), Totals>,
    positions: LookupMap<(u64, AccountId), Position>,
    proposal: LookupMap<u64, Vec<Edge>>,
    participants: Vector<AccountId>,
}

#[near]
impl Probe {
    #[init]
    pub fn new() -> Self {
        Self {
            pairs: LookupMap::new(b"p"),
            totals: LookupMap::new(b"t"),
            positions: LookupMap::new(b"s"),
            proposal: LookupMap::new(b"q"),
            participants: Vector::new(b"T"),
        }
    }

    /// Seeds one obligation per edge (`cleared` here is the obligation). Not measured.
    pub fn seed(&mut self, edges: Vec<Edge>) {
        for edge in edges {
            let amount = edge.cleared.0;
            self.pairs.insert((EPOCH, edge.debtor.clone(), edge.creditor.clone()), PairState { cumulative: amount, last_seq: 1, cleared: 0 });
            for (account, out, inn) in [(&edge.debtor, amount, 0), (&edge.creditor, 0, amount)] {
                let key = (EPOCH, account.clone());
                let mut totals = self.totals.get(&key).cloned().unwrap_or_else(|| {
                    self.participants.push(account.clone());
                    Totals { owed_out: 0, owed_in: 0 }
                });
                totals.owed_out += out;
                totals.owed_in += inn;
                self.totals.insert(key, totals);
            }
        }
    }

    /// `submit_clearing`: validate every edge against the snapshot, check the per-member balance,
    /// replace the stored proposal. Returns `cleared_total`.
    pub fn probe_verify(&mut self, edges: Vec<Edge>) -> U128 {
        assert!(!edges.is_empty(), "ERR_EMPTY_PROPOSAL");
        let mut seen = HashSet::new();
        let mut balance: HashMap<AccountId, i128> = HashMap::new();
        let mut total = 0u128;
        for edge in &edges {
            let cleared = edge.cleared.0;
            assert!(cleared > 0, "ERR_ZERO_CLEARED");
            assert!(seen.insert((&edge.debtor, &edge.creditor)), "ERR_DUPLICATE_EDGE");
            let pair = self.pairs.get(&(EPOCH, edge.debtor.clone(), edge.creditor.clone())).expect("ERR_EDGE_NOT_IN_SNAPSHOT");
            assert!(cleared <= pair.cumulative - pair.cleared, "ERR_CLEARED_EXCEEDS_OBLIGATION");
            *balance.entry(edge.creditor.clone()).or_default() += cleared as i128;
            *balance.entry(edge.debtor.clone()).or_default() -= cleared as i128;
            total += cleared;
        }
        assert!(balance.values().all(|b| *b == 0), "ERR_UNBALANCED_MEMBER");
        self.proposal.insert(EPOCH, edges);
        U128(total)
    }

    /// Emits `count` logs of `size` bytes each (limit experiment).
    pub fn probe_logs(&self, count: u32, size: u32) {
        for _ in 0..count {
            env::log_str(&"x".repeat(size as usize));
        }
    }

    /// `commit_epoch`: apply the stored proposal to the pairs, write one position per participant,
    /// emit the event logs in chunks of 50.
    pub fn probe_apply(&mut self) -> u32 {
        let edges = self.proposal.remove(&EPOCH).expect("proposal");
        for edge in &edges {
            let key = (EPOCH, edge.debtor.clone(), edge.creditor.clone());
            let mut pair = self.pairs.get(&key).cloned().expect("pair");
            pair.cleared += edge.cleared.0;
            self.pairs.insert(key, pair);
        }
        let mut entries = Vec::new();
        for account in &self.participants {
            let totals = self.totals.get(&(EPOCH, account.clone())).cloned().expect("totals");
            let net = totals.owed_in as i128 - totals.owed_out as i128;
            if net != 0 {
                self.positions.insert(
                    (EPOCH, account.clone()),
                    Position { net, role: 1, status: 1, amount: net.unsigned_abs(), recovered: 0, payment_deadline: 1, updated_at_block: env::block_height() },
                );
            }
            entries.push(json!({"account_id": account, "net": net.to_string(), "role": "none", "status": null, "amount": "0", "outstanding_debt": "0"}));
        }
        // Near caps the total log length of one call at 16 KiB; emit what fits, for gas only.
        let mut budget = 16_000usize;
        let mut emit = |line: String| {
            if line.len() <= budget {
                budget -= line.len();
                env::log_str(&line);
            }
        };
        for chunk in edges.chunks(50) {
            emit(format!("EVENT_JSON:{}", json!({"standard": "x207", "version": "0.0.1", "event": "clearing_applied", "data": [{"edges": chunk}]})));
        }
        for chunk in entries.chunks(50) {
            emit(format!("EVENT_JSON:{}", json!({"standard": "x207", "version": "0.0.1", "event": "epoch_committed", "data": [{"positions": chunk}]})));
        }
        edges.len() as u32
    }
}

