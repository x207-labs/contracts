//! CT-030: property tests of the clearing verifier on random graphs. A failing case prints its
//! minimal input and proptest writes the seed to `proptest-regressions/`, so it replays.
//! Run: `cargo test --test ct030_clearing_property`.

use std::collections::BTreeMap;

use near_sdk::json_types::U128;
use near_sdk::test_utils::VMContextBuilder;
use near_sdk::AccountId;
use proptest::prelude::*;
use x207_circle::clearing::EpochPhase;
use x207_circle::params::Params;
use x207_circle::types::ClearingEdge;
use x207_circle::voucher::PairState;
use x207_circle::X207Circle;

/// TC-CT-030-07: at least 1.000 graphs per run.
const CASES: u32 = 1_000;

fn acc(i: usize) -> AccountId {
    format!("m{i}.near").parse().unwrap()
}

/// A random graph built from cycles, plus a proposal made from fractions of those cycles.
#[derive(Debug, Clone)]
struct Graph {
    n: usize,
    /// `(debtor, creditor) -> obligation`
    obligations: BTreeMap<(usize, usize), u128>,
    /// `(debtor, creditor) -> cleared` of the valid proposal (only edges with `cleared > 0`).
    proposal: BTreeMap<(usize, usize), u128>,
}

fn graph() -> impl Strategy<Value = Graph> {
    (3usize..=20).prop_flat_map(|n| {
        let cycle = (proptest::collection::vec(0..n, 2..=6), 1u128..=1_000, 0u128..=100);
        let extra = (0..n, 0..n, 1u128..=1_000);
        (
            Just(n),
            proptest::collection::vec(cycle, 1..=8),
            proptest::collection::vec(extra, 0..=6),
        )
    })
    .prop_map(|(n, cycles, extras)| {
        let mut obligations: BTreeMap<(usize, usize), u128> = BTreeMap::new();
        let mut proposal: BTreeMap<(usize, usize), u128> = BTreeMap::new();
        for (nodes, amount, pct) in cycles {
            let mut seen = Vec::new();
            for v in nodes {
                if !seen.contains(&v) {
                    seen.push(v);
                }
            }
            if seen.len() < 2 {
                continue;
            }
            // `cleared` of this cycle is a share of `amount`, so Σ cleared <= obligation per edge.
            let cleared = amount * pct / 100;
            for i in 0..seen.len() {
                let key = (seen[i], seen[(i + 1) % seen.len()]);
                *obligations.entry(key).or_default() += amount;
                if cleared > 0 {
                    *proposal.entry(key).or_default() += cleared;
                }
            }
        }
        for (a, b, amount) in extras {
            if a != b {
                *obligations.entry((a, b)).or_default() += amount;
            }
        }
        Graph { n, obligations, proposal }
    })
}

fn circle(g: &Graph) -> X207Circle {
    // `testing_env!` keeps the previous storage; every case needs an empty one.
    near_sdk::env::set_blockchain_interface(near_sdk::MockedBlockchain::new(
        VMContextBuilder::new().block_timestamp(1_000).build(),
        near_sdk::test_vm_config(),
        near_sdk::RuntimeFeesConfig::test(),
        Default::default(),
        Default::default(),
        Default::default(),
        None,
    ));
    let mut c = X207Circle::with_config(
        "dao.near".parse().unwrap(),
        "agents-main".to_string(),
        "usdt.near".parse().unwrap(),
        "intents.near".parse().unwrap(),
        Params::default(),
        1_000,
    );
    for (&(d, k), &amount) in &g.obligations {
        c.pairs.insert((1, acc(d), acc(k)), PairState { cumulative: amount, last_seq: 1, cleared: 0 });
        c.epochs.get_mut(&1).unwrap().edges.push((acc(d), acc(k)));
    }
    c.epochs.get_mut(&1).unwrap().phase = EpochPhase::Clearing;
    c
}

fn edges(proposal: &BTreeMap<(usize, usize), u128>) -> Vec<ClearingEdge> {
    proposal
        .iter()
        .map(|(&(d, k), &cleared)| ClearingEdge { debtor: acc(d), creditor: acc(k), cleared: U128(cleared) })
        .collect()
}

fn reason(c: &X207Circle, edges: Vec<ClearingEdge>) -> String {
    let r = c.simulate_clearing(1, edges);
    assert!(!r.valid, "expected a rejection");
    r.reason.expect("rejection has a reason")
}

/// Net position per member: Σ owed to it minus Σ it owes.
fn net(n: usize, obligations: &BTreeMap<(usize, usize), u128>) -> Vec<i128> {
    let mut net = vec![0i128; n];
    for (&(d, k), &o) in obligations {
        net[d] -= o as i128;
        net[k] += o as i128;
    }
    net
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    // TC-CT-030-01
    #[test]
    fn valid_proposal_from_cycles_passes(g in graph()) {
        prop_assume!(!g.proposal.is_empty());
        let c = circle(&g);
        let r = c.simulate_clearing(1, edges(&g.proposal));
        prop_assert!(r.valid, "{:?}", r.reason);
        prop_assert_eq!(r.cleared_total.0, g.proposal.values().sum::<u128>());
        prop_assert_eq!(r.edge_count as usize, g.proposal.len());
    }

    // TC-CT-030-02 (arithmetic model of the apply step; the real commit step is a separate item): applying the proposal keeps every net position (I-08) and Σ due = Σ claimable (I-09).
    #[test]
    fn net_positions_survive_applying(g in graph()) {
        prop_assume!(!g.proposal.is_empty());
        let c = circle(&g);
        prop_assert!(c.simulate_clearing(1, edges(&g.proposal)).valid);
        let after: BTreeMap<(usize, usize), u128> = g
            .obligations
            .iter()
            .map(|(k, &o)| (*k, o - g.proposal.get(k).copied().unwrap_or(0)))
            .collect();
        let (before_net, after_net) = (net(g.n, &g.obligations), net(g.n, &after));
        prop_assert_eq!(&before_net, &after_net);
        let due: i128 = after_net.iter().filter(|v| **v < 0).map(|v| -v).sum();
        let claimable: i128 = after_net.iter().filter(|v| **v > 0).sum();
        prop_assert_eq!(due, claimable);
    }

    // TC-CT-030-03
    #[test]
    fn cleared_above_obligation_rejected(g in graph(), pick in any::<prop::sample::Index>()) {
        prop_assume!(!g.proposal.is_empty());
        let c = circle(&g);
        let mut e = edges(&g.proposal);
        let i = pick.index(e.len());
        let o = g.obligations[&(
            e[i].debtor.as_str()[1..e[i].debtor.as_str().find('.').unwrap()].parse().unwrap(),
            e[i].creditor.as_str()[1..e[i].creditor.as_str().find('.').unwrap()].parse().unwrap(),
        )];
        e[i].cleared = U128(o + 1);
        prop_assert!(reason(&c, e).starts_with("ERR_CLEARED_EXCEEDS_OBLIGATION"));
    }

    // TC-CT-030-04
    #[test]
    fn unbalancing_one_edge_rejected(g in graph(), pick in any::<prop::sample::Index>()) {
        prop_assume!(!g.proposal.is_empty());
        let keys: Vec<_> = g.proposal.keys().copied().collect();
        let key = keys[pick.index(keys.len())];
        let (cleared, o) = (g.proposal[&key], g.obligations[&key]);
        let changed = if cleared < o { cleared + 1 } else if cleared > 1 { cleared - 1 } else { 0 };
        prop_assume!(changed > 0);
        let mut p = g.proposal.clone();
        p.insert(key, changed);
        let c = circle(&g);
        prop_assert!(reason(&c, edges(&p)).starts_with("ERR_UNBALANCED_MEMBER"));
    }

    // TC-CT-030-05
    #[test]
    fn foreign_edge_rejected(g in graph(), a in 0usize..20, b in 0usize..20) {
        prop_assume!(!g.proposal.is_empty());
        let (a, b) = (a % g.n, b % g.n);
        prop_assume!(a != b && !g.obligations.contains_key(&(a, b)));
        let mut e = edges(&g.proposal);
        e.push(ClearingEdge { debtor: acc(a), creditor: acc(b), cleared: U128(1) });
        let c = circle(&g);
        let r = reason(&c, e);
        prop_assert!(r.starts_with("ERR_EDGE_NOT_IN_SNAPSHOT"), "{r}");
    }

    // TC-CT-030-06
    #[test]
    fn duplicate_edge_rejected(g in graph(), pick in any::<prop::sample::Index>()) {
        prop_assume!(!g.proposal.is_empty());
        let mut e = edges(&g.proposal);
        let dup = e[pick.index(e.len())].clone();
        e.push(dup);
        let c = circle(&g);
        prop_assert!(reason(&c, e).starts_with("ERR_DUPLICATE_EDGE"));
    }

    // TC-CT-030-08: the per-edge zero check runs before the balance check.
    #[test]
    fn zero_edge_rejected_before_balance(g in graph(), pick in any::<prop::sample::Index>()) {
        prop_assume!(!g.proposal.is_empty());
        let mut e = edges(&g.proposal);
        let i = pick.index(e.len());
        e[i].cleared = U128(0);
        let c = circle(&g);
        prop_assert!(reason(&c, e).starts_with("ERR_ZERO_CLEARED"));
    }
}

// TC-CT-030-07
#[test]
fn tc_ct_030_07_runs_at_least_1000_cases() {
    assert!(CASES >= 1_000);
}
