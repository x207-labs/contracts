//! F3 clearing verifier (spec 3.6, 4.3): O(n) validation of a proposal and the view
//! `simulate_clearing`. `verify_clearing` and `beats_stored` are shared with `submit_clearing`.

use std::collections::{BTreeMap, HashSet};

use near_sdk::json_types::U128;
use near_sdk::{near, AccountId};

use crate::clearing::EpochPhase;
use crate::state::{X207Circle, X207CircleExt};
use crate::types::{ClearingEdge, EpochId, MAX_EDGES_PER_TX};

/// `simulate_clearing` result (spec section 5). `reason` carries the `ERR_` code and detail.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulationResult {
    pub valid: bool,
    pub cleared_total: U128,
    pub edge_count: u32,
    pub better_than_current: bool,
    pub reason: Option<String>,
}

impl X207Circle {
    /// Validates `edges` against the D2 snapshot of `epoch` and returns `cleared_total`, or the
    /// `ERR_KODE: detail` text of the first failing rule. Order (spec 4.7): `ERR_UNKNOWN_EPOCH`,
    /// `ERR_WRONG_PHASE`, `ERR_EMPTY_PROPOSAL`, `ERR_TOO_MANY_EDGES`, then per edge in input order
    /// `ERR_ZERO_CLEARED`, `ERR_DUPLICATE_EDGE`, `ERR_EDGE_NOT_IN_SNAPSHOT`,
    /// `ERR_CLEARED_EXCEEDS_OBLIGATION`, then `ERR_UNBALANCED_MEMBER`. Reads only; the time check
    /// `ERR_PROPOSAL_WINDOW_CLOSED` belongs to `submit_clearing`.
    pub fn verify_clearing(&self, epoch: EpochId, edges: &[ClearingEdge]) -> Result<u128, String> {
        let Some(e) = self.epochs.get(&epoch) else {
            return Err(format!("ERR_UNKNOWN_EPOCH: {epoch}"));
        };
        if e.phase != EpochPhase::Clearing {
            return Err(format!(
                "ERR_WRONG_PHASE: epoch {epoch} is {:?}, expected Clearing",
                e.phase
            ));
        }
        if edges.is_empty() {
            return Err("ERR_EMPTY_PROPOSAL: no edges".to_string());
        }
        if edges.len() > MAX_EDGES_PER_TX as usize {
            return Err(format!(
                "ERR_TOO_MANY_EDGES: {} > {MAX_EDGES_PER_TX}",
                edges.len()
            ));
        }
        let mut seen: HashSet<(&AccountId, &AccountId)> = HashSet::with_capacity(edges.len());
        let mut net: BTreeMap<&AccountId, (u128, u128)> = BTreeMap::new();
        let mut total: u128 = 0;
        for edge in edges {
            let (debtor, creditor, cleared) = (&edge.debtor, &edge.creditor, edge.cleared.0);
            if cleared == 0 {
                return Err(format!("ERR_ZERO_CLEARED: {debtor} -> {creditor}"));
            }
            if !seen.insert((debtor, creditor)) {
                return Err(format!("ERR_DUPLICATE_EDGE: {debtor} -> {creditor}"));
            }
            let Some(pair) = self.pairs.get(&(epoch, debtor.clone(), creditor.clone())) else {
                return Err(format!("ERR_EDGE_NOT_IN_SNAPSHOT: {debtor} -> {creditor}"));
            };
            if cleared > pair.obligation() {
                return Err(format!(
                    "ERR_CLEARED_EXCEEDS_OBLIGATION: {debtor} -> {creditor} cleared {cleared} obligation {}",
                    pair.obligation()
                ));
            }
            // Sums are bounded by `total_obligation` (a u128 that registration already added up).
            total = total
                .checked_add(cleared)
                .ok_or("ERR_CLEARED_EXCEEDS_OBLIGATION: total overflow")?;
            net.entry(creditor).or_default().0 += cleared;
            net.entry(debtor).or_default().1 += cleared;
        }
        if let Some((account, (inn, out))) = net.iter().find(|(_, (inn, out))| inn != out) {
            return Err(format!(
                "ERR_UNBALANCED_MEMBER: {account} in {inn} out {out}"
            ));
        }
        Ok(total)
    }

    /// Spec 4.3: larger `cleared_total` wins; on a tie fewer edges win; otherwise the stored
    /// proposal stays. `true` when no proposal is stored for `epoch`.
    pub fn beats_stored(&self, epoch: EpochId, cleared_total: u128, edge_count: u32) -> bool {
        match self.proposals.get(&epoch) {
            None => true,
            Some(stored) => {
                cleared_total > stored.cleared_total.0
                    || (cleared_total == stored.cleared_total.0 && edge_count < stored.edge_count)
            }
        }
    }
}

#[near]
impl X207Circle {
    /// Runs the `submit_clearing` verifier without writing state. Rule violations come back in
    /// `reason`, never as a panic; `better_than_current` follows spec 4.3.
    pub fn simulate_clearing(&self, epoch: EpochId, edges: Vec<ClearingEdge>) -> SimulationResult {
        let edge_count = edges.len() as u32;
        match self.verify_clearing(epoch, &edges) {
            Ok(total) => SimulationResult {
                valid: true,
                cleared_total: U128(total),
                edge_count,
                better_than_current: self.beats_stored(epoch, total, edge_count),
                reason: None,
            },
            Err(reason) => SimulationResult {
                valid: false,
                cleared_total: U128(0),
                edge_count,
                better_than_current: false,
                reason: Some(reason),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::json_types::U64;
    use near_sdk::serde_json::{self, json};
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{env, testing_env};

    use super::*;
    use crate::clearing::ClearingProposal;
    use crate::params::Params;
    use crate::voucher::PairState;

    const CREDIT: u128 = 1_000_000;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn edge(d: &str, c: &str, cleared: u128) -> ClearingEdge {
        ClearingEdge {
            debtor: acc(d),
            creditor: acc(c),
            cleared: U128(cleared),
        }
    }

    /// Epoch 1 in phase `clearing` with the given `(payer, payee, cumulative)` pairs.
    fn clearing_circle(pairs: &[(&str, &str, u128)]) -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        let mut c = X207Circle::with_config(
            acc("dao.near"),
            "agents-main".to_string(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            env::block_timestamp(),
        );
        for (payer, payee, cumulative) in pairs {
            c.pairs.insert(
                (1, acc(payer), acc(payee)),
                PairState {
                    cumulative: *cumulative,
                    last_seq: 1,
                    cleared: 0,
                },
            );
            c.epochs
                .get_mut(&1)
                .unwrap()
                .edges
                .push((acc(payer), acc(payee)));
        }
        let e = c.epochs.get_mut(&1).unwrap();
        e.phase = EpochPhase::Clearing;
        c
    }

    /// Scenario S1 of the test case file: scraper->gpu 60, gpu->translator 25, translator->scraper 25.
    fn s1() -> X207Circle {
        clearing_circle(&[
            ("scraper.near", "gpu.near", 60 * CREDIT),
            ("gpu.near", "translator.near", 25 * CREDIT),
            ("translator.near", "scraper.near", 25 * CREDIT),
        ])
    }

    fn p_s1() -> Vec<ClearingEdge> {
        vec![
            edge("scraper.near", "gpu.near", 25 * CREDIT),
            edge("gpu.near", "translator.near", 25 * CREDIT),
            edge("translator.near", "scraper.near", 25 * CREDIT),
        ]
    }

    fn reason(c: &X207Circle, edges: Vec<ClearingEdge>) -> String {
        let r = c.simulate_clearing(1, edges);
        assert!(!r.valid);
        assert_eq!(r.cleared_total, U128(0));
        assert!(!r.better_than_current);
        r.reason.expect("invalid result has a reason")
    }

    // TC-CT-026-01 (unit part; the sandbox run goes through real registration)
    #[test]
    fn tc_ct_026_01_valid_proposal() {
        let r = s1().simulate_clearing(1, p_s1());
        assert_eq!(
            serde_json::to_value(r).unwrap(),
            json!({"valid":true,"cleared_total":"75000000","edge_count":3,"better_than_current":true,"reason":null})
        );
    }

    // TC-CT-026-02
    #[test]
    fn tc_ct_026_02_empty_proposal() {
        assert!(reason(&s1(), vec![]).starts_with("ERR_EMPTY_PROPOSAL"));
    }

    // TC-CT-026-03: the count is checked before the content of any edge (K-17)
    #[test]
    fn tc_ct_026_03_too_many_edges() {
        let junk = vec![edge("nobody.near", "nobody.near", 0); MAX_EDGES_PER_TX as usize + 1];
        assert!(reason(&s1(), junk).starts_with("ERR_TOO_MANY_EDGES"));
    }

    // TC-CT-026-04
    #[test]
    fn tc_ct_026_04_duplicate_pair() {
        let edges = vec![
            edge("gpu.near", "translator.near", 10 * CREDIT),
            edge("gpu.near", "translator.near", 10 * CREDIT),
        ];
        assert!(reason(&s1(), edges).starts_with("ERR_DUPLICATE_EDGE"));
    }

    // TC-CT-026-05
    #[test]
    fn tc_ct_026_05_edge_outside_snapshot() {
        assert!(
            reason(&s1(), vec![edge("gpu.near", "scraper.near", 5 * CREDIT)])
                .starts_with("ERR_EDGE_NOT_IN_SNAPSHOT")
        );
    }

    // TC-CT-026-06
    #[test]
    fn tc_ct_026_06_exceeds_obligation() {
        let mut edges = p_s1();
        edges[1].cleared = U128(25 * CREDIT + 1);
        assert!(reason(&s1(), edges).starts_with("ERR_CLEARED_EXCEEDS_OBLIGATION"));
    }

    // TC-CT-026-07
    #[test]
    fn tc_ct_026_07_exactly_the_obligation() {
        let r = s1().simulate_clearing(1, p_s1());
        assert!(r.valid, "{:?}", r.reason);
    }

    // TC-CT-026-08 (K-02)
    #[test]
    fn tc_ct_026_08_zero_cleared() {
        let mut edges = p_s1();
        edges[0].cleared = U128(0);
        assert!(reason(&s1(), edges).starts_with("ERR_ZERO_CLEARED"));
    }

    // TC-CT-026-09
    #[test]
    fn tc_ct_026_09_unbalanced_member_is_named() {
        let msg = reason(&s1(), vec![edge("scraper.near", "gpu.near", 25 * CREDIT)]);
        assert!(msg.starts_with("ERR_UNBALANCED_MEMBER"), "{msg}");
        assert!(msg.contains("gpu.near"), "{msg}");
    }

    // TC-CT-026-10: S2 with P-opt stored, simulate P-2c
    #[test]
    fn tc_ct_026_10_not_better_than_stored() {
        let mut c = clearing_circle(&[
            ("summarizer.near", "analyst.near", 15 * CREDIT),
            ("analyst.near", "summarizer.near", 15 * CREDIT),
            ("analyst.near", "coder.near", 10 * CREDIT),
            ("coder.near", "summarizer.near", 10 * CREDIT),
        ]);
        let p_opt = vec![
            edge("summarizer.near", "analyst.near", 15 * CREDIT),
            edge("analyst.near", "summarizer.near", 5 * CREDIT),
            edge("analyst.near", "coder.near", 10 * CREDIT),
            edge("coder.near", "summarizer.near", 10 * CREDIT),
        ];
        c.proposals.insert(
            1,
            ClearingProposal {
                epoch: 1,
                solver: acc("solver2.near"),
                edge_count: 4,
                cleared_total: U128(40 * CREDIT),
                edges: p_opt,
                submitted_at: U64(1),
                committed: false,
            },
        );
        let p_2c = vec![
            edge("summarizer.near", "analyst.near", 15 * CREDIT),
            edge("analyst.near", "summarizer.near", 15 * CREDIT),
        ];
        let r = c.simulate_clearing(1, p_2c);
        assert!(r.valid && !r.better_than_current, "{r:?}");
        assert_eq!(r.cleared_total, U128(30 * CREDIT));
    }

    // Spec 4.3 tie-break (shared with submit_clearing): same total, fewer edges wins; same edges loses.
    #[test]
    fn beats_stored_tie_break() {
        let mut c = s1();
        let stored = |total: u128, n: u32| ClearingProposal {
            epoch: 1,
            solver: acc("solver1.near"),
            edges: vec![],
            edge_count: n,
            cleared_total: U128(total),
            submitted_at: U64(1),
            committed: false,
        };
        assert!(c.beats_stored(1, 1, 1));
        c.proposals.insert(1, stored(30, 3));
        assert!(c.beats_stored(1, 30, 2));
        assert!(!c.beats_stored(1, 30, 3));
        assert!(!c.beats_stored(1, 30, 4));
        assert!(c.beats_stored(1, 31, 9));
        assert!(!c.beats_stored(1, 29, 1));
    }

    // TC-CT-026-11 (K-03)
    #[test]
    fn tc_ct_026_11_wrong_phase_and_unknown_epoch() {
        let mut c = s1();
        assert!(c
            .simulate_clearing(99, p_s1())
            .reason
            .unwrap()
            .starts_with("ERR_UNKNOWN_EPOCH"));
        c.epochs.get_mut(&1).unwrap().phase = EpochPhase::Open;
        let r = c.simulate_clearing(1, p_s1());
        assert!(!r.valid);
        assert!(r.reason.unwrap().starts_with("ERR_WRONG_PHASE"));
    }

    // TC-CT-026-12 (K-16): a negative amount never reaches the verifier
    #[test]
    fn tc_ct_026_12_negative_cleared_fails_deserialization() {
        let err = serde_json::from_value::<ClearingEdge>(
            json!({"debtor":"gpu.near","creditor":"translator.near","cleared":"-1"}),
        )
        .unwrap_err()
        .to_string();
        assert!(!err.contains("ERR_"), "{err}");
    }

    // Per-edge order: zero is reported before duplicate, a later bad edge does not hide an earlier one.
    #[test]
    fn first_failing_edge_in_input_order_wins() {
        let edges = vec![
            edge("gpu.near", "scraper.near", 5 * CREDIT),
            edge("gpu.near", "translator.near", 0),
        ];
        assert!(reason(&s1(), edges).starts_with("ERR_EDGE_NOT_IN_SNAPSHOT"));
    }

    // TC-CT-026-13 (unit approximation): the view takes `&self`, so it cannot write; state reads equal before and after.
    #[test]
    fn simulate_does_not_change_state() {
        let c = s1();
        let before = c.obligation(1, acc("gpu.near"), acc("translator.near"));
        let _ = c.simulate_clearing(1, p_s1());
        assert_eq!(
            c.obligation(1, acc("gpu.near"), acc("translator.near")),
            before
        );
        assert!(c.proposals.get(&1).is_none());
    }
}
