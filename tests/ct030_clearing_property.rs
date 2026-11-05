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
use x207_circle::voucher::{PairState, ParticipantTotals};
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
    graph_between(3, 20)
}

/// 2 to 30 members, as in the review of the verifier (security review, "Not checked").
fn wide() -> impl Strategy<Value = Graph> {
    graph_between(2, 30)
}

fn graph_between(low: usize, high: usize) -> impl Strategy<Value = Graph> {
    (low..=high)
        .prop_flat_map(|n| {
            let cycle = (
                proptest::collection::vec(0..n, 2..=6),
                1u128..=1_000,
                0u128..=100,
            );
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
            Graph {
                n,
                obligations,
                proposal,
            }
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
        c.pairs.insert(
            (1, acc(d), acc(k)),
            PairState {
                cumulative: amount,
                last_seq: 1,
                cleared: 0,
            },
        );
        c.epochs.get_mut(&1).unwrap().edges.push((acc(d), acc(k)));
    }
    c.epochs.get_mut(&1).unwrap().phase = EpochPhase::Clearing;
    c
}

fn edges(proposal: &BTreeMap<(usize, usize), u128>) -> Vec<ClearingEdge> {
    proposal
        .iter()
        .map(|(&(d, k), &cleared)| ClearingEdge {
            debtor: acc(d),
            creditor: acc(k),
            cleared: U128(cleared),
        })
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

/// `circle(g)` with the D2 totals and participants that `register_cumulative` keeps, so `commit_epoch` can run.
fn circle_with_totals(g: &Graph) -> X207Circle {
    let mut c = circle(g);
    c.epochs.get_mut(&1).unwrap().phase = EpochPhase::Open;
    for (&(d, k), &amount) in &g.obligations {
        let e = c.epochs.get_mut(&1).unwrap();
        e.edge_count += 1;
        e.total_obligation += amount;
        for (member, out, inn) in [(d, amount, 0), (k, 0, amount)] {
            let key = (1, acc(member));
            let is_new = c.totals.get(&key).is_none();
            let mut totals = c
                .totals
                .get(&key)
                .cloned()
                .unwrap_or_else(ParticipantTotals::default);
            totals.owed_out += out;
            totals.owed_in += inn;
            c.totals.insert(key, totals);
            if is_new {
                let e = c.epochs.get_mut(&1).unwrap();
                e.participants.push(acc(member));
                e.participant_count += 1;
            }
        }
        let debt = c.outstanding_debt.get(&acc(d)).copied().unwrap_or(0);
        c.outstanding_debt.insert(acc(d), debt + amount);
    }
    c
}

fn at(ns: u64, predecessor: &str) {
    near_sdk::testing_env!(VMContextBuilder::new()
        .predecessor_account_id(predecessor.parse().unwrap())
        .block_timestamp(ns)
        .block_height(500)
        .build());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    // TC-CT-030-02 with the real steps: close, submit the valid proposal, commit. Every net position is the same
    // before and after (I-08), the positions add up (I-09) and each pair keeps `obligation = cumulative - cleared`.
    #[test]
    fn net_positions_survive_the_real_commit(g in graph()) {
        prop_assume!(!g.proposal.is_empty());
        let mut c = circle_with_totals(&g);
        let before_net = net(g.n, &g.obligations);
        let cutoff = c.get_epoch(Some(1)).unwrap().cutoff_at.0;
        at(cutoff, "keeper.near");
        c.close_epoch(1);
        let deadline = c.get_epoch(Some(1)).unwrap().proposal_deadline.unwrap().0;
        at(cutoff + 1, "solver1.near");
        c.submit_clearing(1, edges(&g.proposal));
        at(deadline, "keeper.near");
        c.commit_epoch(1);

        let positions = c.list_positions(1, 0, 100);
        let (mut due, mut claimable) = (0i128, 0i128);
        for (i, expected) in before_net.iter().enumerate() {
            let position = positions.iter().find(|p| p.account_id == acc(i));
            let got = position.map_or(0, |p| p.net.0);
            prop_assert_eq!(got, *expected, "member {}", i);
            if got < 0 { due -= got } else { claimable += got }
        }
        prop_assert_eq!(due, claimable);
        for (&(d, k), &o) in &g.obligations {
            let pair = c.obligation(1, acc(d), acc(k)).unwrap();
            prop_assert_eq!(pair.obligation.0, o - g.proposal.get(&(d, k)).copied().unwrap_or(0));
        }
    }

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

    // TC-CT-030-02, arithmetic part: applying the proposal keeps every net position (I-08) and Σ due = Σ claimable (I-09).
    // The same property through the real `close_epoch`, `submit_clearing` and `commit_epoch` is `net_positions_survive_the_real_commit`.
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
        let changed = if cleared < o { cleared + 1 } else { cleared.saturating_sub(1) };
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

// ---- the verifier against an independent statement of its rules, mutated and random proposals, and the order of submissions ----

type Proposal = Vec<(usize, usize, u128)>;

/// The rules of spec 4.7 written again from the text, without the code of the contract: the first failing rule in the order of the spec, or the total.
fn oracle(g: &Graph, proposal: &Proposal) -> Result<u128, &'static str> {
    if proposal.is_empty() {
        return Err("ERR_EMPTY_PROPOSAL");
    }
    if proposal.len() > 300 {
        return Err("ERR_TOO_MANY_EDGES");
    }
    let mut seen = std::collections::HashSet::new();
    let (mut credit, mut debit) = (
        BTreeMap::<usize, u128>::new(),
        BTreeMap::<usize, u128>::new(),
    );
    let mut total = 0u128;
    for &(d, k, cleared) in proposal {
        if cleared == 0 {
            return Err("ERR_ZERO_CLEARED");
        }
        if !seen.insert((d, k)) {
            return Err("ERR_DUPLICATE_EDGE");
        }
        let Some(&obligation) = g.obligations.get(&(d, k)) else {
            return Err("ERR_EDGE_NOT_IN_SNAPSHOT");
        };
        if cleared > obligation {
            return Err("ERR_CLEARED_EXCEEDS_OBLIGATION");
        }
        total += cleared;
        *credit.entry(k).or_default() += cleared;
        *debit.entry(d).or_default() += cleared;
    }
    let members: std::collections::BTreeSet<usize> =
        credit.keys().chain(debit.keys()).copied().collect();
    if members
        .iter()
        .any(|m| credit.get(m).copied().unwrap_or(0) != debit.get(m).copied().unwrap_or(0))
    {
        return Err("ERR_UNBALANCED_MEMBER");
    }
    Ok(total)
}

fn wire(proposal: &Proposal) -> Vec<ClearingEdge> {
    proposal
        .iter()
        .map(|&(d, k, cleared)| ClearingEdge {
            debtor: acc(d),
            creditor: acc(k),
            cleared: U128(cleared),
        })
        .collect()
}

#[derive(Debug, Clone)]
enum Mutation {
    Bump(usize, u128),
    Exceed(usize),
    Drop(usize),
    Duplicate(usize),
    Zero(usize),
    Reverse(usize),
    Add(usize, usize, u128),
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        (any::<usize>(), 1u128..=3).prop_map(|(i, d)| Mutation::Bump(i, d)),
        any::<usize>().prop_map(Mutation::Exceed),
        any::<usize>().prop_map(Mutation::Drop),
        any::<usize>().prop_map(Mutation::Duplicate),
        any::<usize>().prop_map(Mutation::Zero),
        any::<usize>().prop_map(Mutation::Reverse),
        (0usize..30, 0usize..30, 0u128..=50).prop_map(|(a, b, c)| Mutation::Add(a, b, c)),
    ]
}

fn mutate(mut p: Proposal, g: &Graph, m: &Mutation) -> Proposal {
    if p.is_empty() {
        return p;
    }
    match *m {
        Mutation::Bump(i, d) => {
            let i = i % p.len();
            p[i].2 += d;
        }
        Mutation::Exceed(i) => {
            let i = i % p.len();
            p[i].2 = g.obligations.get(&(p[i].0, p[i].1)).copied().unwrap_or(0) + 1;
        }
        Mutation::Drop(i) => {
            p.remove(i % p.len());
        }
        Mutation::Duplicate(i) => p.push(p[i % p.len()]),
        Mutation::Zero(i) => {
            let i = i % p.len();
            p[i].2 = 0;
        }
        Mutation::Reverse(i) => {
            let i = i % p.len();
            p[i] = (p[i].1, p[i].0, p[i].2);
        }
        Mutation::Add(a, b, c) => p.push((a % g.n, b % g.n, c)),
    }
    p
}

/// A proposal for `g`: its valid one, or a random one, and then up to three mutations.
fn candidate(g: &Graph, use_raw: bool, raw: &Proposal, muts: &[Mutation]) -> Proposal {
    let mut p: Proposal = if use_raw {
        raw.iter().map(|&(a, b, c)| (a % g.n, b % g.n, c)).collect()
    } else {
        g.proposal.iter().map(|(&(d, k), &c)| (d, k, c)).collect()
    };
    for m in muts {
        p = mutate(p, g, m);
    }
    p
}

fn code(reason: &str) -> &str {
    reason.split(':').next().unwrap_or(reason)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    // TC-CT-030-09: the verifier accepts exactly what the rules of the spec accept, and says the same first failure, for 2 to 30 members, valid, mutated and random proposals.
    #[test]
    fn the_verifier_agrees_with_an_independent_oracle(
        g in wide(), muts in proptest::collection::vec(mutation(), 0..=3),
        raw in proptest::collection::vec((0usize..30, 0usize..30, 0u128..=2_000), 0..=40), use_raw in any::<bool>(),
    ) {
        let proposal = candidate(&g, use_raw, &raw, &muts);
        let c = circle(&g);
        let got = c.simulate_clearing(1, wire(&proposal));
        match oracle(&g, &proposal) {
            Ok(total) => {
                prop_assert!(got.valid, "the oracle accepts, the verifier says {:?}", got.reason);
                prop_assert_eq!(got.cleared_total.0, total);
            }
            Err(expected) => {
                prop_assert!(!got.valid, "the oracle says {}, the verifier accepted {:?}", expected, proposal);
                prop_assert_eq!(code(got.reason.as_deref().unwrap_or("")), expected);
            }
        }
    }

    // TC-CT-030-10: whatever proposal is accepted, through the real close, submit and commit: no obligation goes up, nothing is cleared beyond its edge, every net position is the
    // sum of the vouchers whatever the solver sent, and what is due is what is claimable.
    #[test]
    fn an_accepted_proposal_never_raises_an_obligation_or_moves_a_net_position(
        g in wide(), muts in proptest::collection::vec(mutation(), 0..=3),
        raw in proptest::collection::vec((0usize..30, 0usize..30, 0u128..=2_000), 0..=40), use_raw in any::<bool>(),
    ) {
        let proposal = candidate(&g, use_raw, &raw, &muts);
        // a proposal that the rules refuse is the other test's business; no assume here, so that the many refusals do not abort the run
        if oracle(&g, &proposal).is_err() {
            return Ok(());
        }
        let mut c = circle_with_totals(&g);
        let cutoff = c.get_epoch(Some(1)).unwrap().cutoff_at.0;
        at(cutoff, "keeper.near");
        c.close_epoch(1);
        let deadline = c.get_epoch(Some(1)).unwrap().proposal_deadline.unwrap().0;
        at(cutoff + 1, "solver1.near");
        c.submit_clearing(1, wire(&proposal));
        at(deadline, "keeper.near");
        c.commit_epoch(1);
        let cleared: BTreeMap<(usize, usize), u128> = proposal.iter().map(|&(d, k, x)| ((d, k), x)).collect();
        let (mut after, mut due, mut claimable) = (BTreeMap::new(), 0i128, 0i128);
        for (&(d, k), &o) in &g.obligations {
            let now = c.obligation(1, acc(d), acc(k)).unwrap().obligation.0;
            prop_assert!(now <= o, "the obligation of {}->{} went up", d, k);
            prop_assert_eq!(now, o - cleared.get(&(d, k)).copied().unwrap_or(0));
            after.insert((d, k), now);
        }
        let positions = c.list_positions(1, 0, 100);
        for (i, expected) in net(g.n, &g.obligations).iter().enumerate() {
            let got = positions.iter().find(|p| p.account_id == acc(i)).map_or(0, |p| p.net.0);
            prop_assert_eq!(got, *expected, "member {}", i);
            if got < 0 { due -= got } else { claimable += got }
        }
        prop_assert_eq!(net(g.n, &after), net(g.n, &g.obligations));
        prop_assert_eq!(due, claimable);
    }
}

/// Cycles and several proposals made of shares of the same cycles: all valid, and of different size.
fn rounds() -> impl Strategy<Value = (Graph, Vec<Proposal>)> {
    (2usize..=30).prop_flat_map(|n| {
        let cycle = (proptest::collection::vec(0..n, 2..=6), 2u128..=1_000).prop_flat_map(
            |(nodes, amount)| {
                (
                    Just(nodes),
                    Just(amount),
                    proptest::collection::vec(0..=amount, 5),
                )
            },
        );
        proptest::collection::vec(cycle, 1..=6).prop_map(move |cycles| {
            let mut obligations: BTreeMap<(usize, usize), u128> = BTreeMap::new();
            let mut proposals: Vec<BTreeMap<(usize, usize), u128>> = vec![BTreeMap::new(); 5];
            for (nodes, amount, shares) in cycles {
                let mut seen = Vec::new();
                for v in nodes {
                    if !seen.contains(&v) {
                        seen.push(v);
                    }
                }
                if seen.len() < 2 {
                    continue;
                }
                for i in 0..seen.len() {
                    let key = (seen[i], seen[(i + 1) % seen.len()]);
                    *obligations.entry(key).or_default() += amount;
                    for (round, share) in shares.iter().enumerate() {
                        if *share > 0 {
                            *proposals[round].entry(key).or_default() += share;
                        }
                    }
                }
            }
            let rounds = proposals
                .into_iter()
                .filter(|p| !p.is_empty())
                .map(|p| p.into_iter().map(|((d, k), x)| (d, k, x)).collect())
                .collect();
            (
                Graph {
                    n,
                    obligations,
                    proposal: BTreeMap::new(),
                },
                rounds,
            )
        })
    })
}

fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> Option<String> {
    std::panic::catch_unwind(f).err().map(|e| {
        e.downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    // TC-CT-030-11 (spec 4.3): of valid proposals sent one after another, only a larger cleared_total, or the same total with fewer edges, replaces the stored one; the stored total never goes down.
    #[test]
    fn only_a_better_valid_proposal_replaces_the_stored_one((g, proposals) in rounds()) {
        prop_assume!(!proposals.is_empty());
        let mut c = circle(&g);
        let cutoff = c.get_epoch(Some(1)).unwrap().cutoff_at.0;
        c.epochs.get_mut(&1).unwrap().phase = EpochPhase::Open;
        at(cutoff, "keeper.near");
        c.close_epoch(1);
        let mut stored: Option<(u128, usize)> = None;
        for (i, proposal) in proposals.iter().enumerate() {
            at(cutoff + 1 + i as u64, "solver1.near");
            let total: u128 = proposal.iter().map(|e| e.2).sum();
            let better = stored.is_none_or(|(t, n)| total > t || (total == t && proposal.len() < n));
            let outcome = panic_text(std::panic::AssertUnwindSafe(|| {
                c.submit_clearing(1, wire(proposal));
            }));
            if better {
                prop_assert!(outcome.is_none(), "a better proposal was refused: {:?}", outcome);
                stored = Some((total, proposal.len()));
            } else {
                prop_assert!(outcome.is_some_and(|m| m.contains("ERR_PROPOSAL_NOT_BETTER")), "a proposal that is not better was taken");
            }
            let now = c.get_clearing_proposal(1).unwrap();
            prop_assert_eq!((now.cleared_total.0, now.edge_count as usize), stored.unwrap());
        }
    }
}

// TC-CT-030-07
#[test]
#[allow(clippy::assertions_on_constants)]
fn tc_ct_030_07_runs_at_least_1000_cases() {
    assert!(CASES >= 1_000);
}
