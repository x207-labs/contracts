//! Stateful property test (security review, "Not tested"): random sequences of calls through the interface of the contract, in the unit-test VM and without the sandbox.
//!
//! Every call runs the way a transaction does: the state is read from storage, the method runs, and the state is written back only if the method did not panic; a panic
//! drops everything it wrote (the storage is restored from the copy taken before the call). After every call these hold:
//! - a panic carries an `ERR_` code (the contract refuses; it does not trip an `assert!`, an `expect` or an overflow);
//! - `limit <= stake` for every member (I-01), and a payer that just registered a voucher owes no more than its limit;
//! - for every epoch the debtors owe exactly what the creditors are owed (I-09);
//! - the USDT that the contract booked (`get_pool().accounted`) equals the USDT that came in minus what went out, and `total_stake` is the sum of the stakes (D6).
//! Run: `cargo test --lib stateful_tests`. A failing sequence is shrunk by proptest and its seed is kept in `proptest-regressions/`.

use std::collections::{BTreeMap, HashMap};
use std::panic::{catch_unwind, AssertUnwindSafe};

use ed25519_dalek::{Signer, SigningKey};
use near_sdk::json_types::U128;
use near_sdk::serde_json::{self, json};
use near_sdk::test_utils::VMContextBuilder;
use near_sdk::{env, AccountId, CurveType, Gas, MockedBlockchain, NearToken, PromiseOrValue, PromiseResult, PublicKey, RuntimeFeesConfig, VMContext};
use proptest::prelude::*;

use crate::canonical::{voucher_signing_hash, Voucher};
use crate::clearing::EpochPhaseView;
use crate::grant::{AssetClassEntry, GrantInput, RevokeReason};
use crate::params::Params;
use crate::settlement::{PositionRole, PositionStatus};
use crate::types::ClearingEdge;
use crate::X207Circle;

const CREDIT: u128 = 1_000_000;
const MEMBERS: usize = 4;
const CIRCLE: &str = "circle.near";

fn acc(i: usize) -> AccountId {
    format!("m{i}.near").parse().unwrap()
}

fn named(s: &str) -> AccountId {
    s.parse().unwrap()
}

fn key(i: usize) -> SigningKey {
    SigningKey::from_bytes(&[i as u8 + 1; 32])
}

fn pubkey(i: usize) -> PublicKey {
    PublicKey::from_parts(CurveType::ED25519, key(i).verifying_key().to_bytes().to_vec()).unwrap()
}

struct World {
    now: u64,
    height: u64,
    storage: HashMap<Vec<u8>, Vec<u8>>,
    /// USDT that came in through `ft_on_transfer` minus what a claim paid out.
    usdt: u128,
    cumulative: BTreeMap<(usize, usize, u64), (u64, u128)>,
    /// Calls the contract refused, by code, for the report of a failing case.
    refused: BTreeMap<String, u32>,
    /// Operations by name: how many the contract accepted and how many it refused.
    accepted: BTreeMap<String, u32>,
    rejected: BTreeMap<String, u32>,
}

fn context(w: &World, predecessor: &str, deposit: NearToken) -> VMContext {
    VMContextBuilder::new()
        .current_account_id(named(CIRCLE))
        .predecessor_account_id(named(predecessor))
        .signer_account_id(named(predecessor))
        .attached_deposit(deposit)
        .prepaid_gas(Gas::from_tgas(300))
        .block_timestamp(w.now)
        .block_height(w.height)
        .build()
}

fn install(ctx: VMContext, storage: HashMap<Vec<u8>, Vec<u8>>, results: Vec<PromiseResult>) {
    env::set_blockchain_interface(MockedBlockchain::new(ctx, near_sdk::test_vm_config(), RuntimeFeesConfig::test(), results, storage, Default::default(), None));
}

fn take() -> HashMap<Vec<u8>, Vec<u8>> {
    near_sdk::mock::with_mocked_blockchain(|b| b.take_storage())
}

fn panic_message(e: Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
}

impl World {
    fn new() -> World {
        let mut w = World { now: 1_000, height: 100, storage: HashMap::new(), usdt: 0, cumulative: BTreeMap::new(), refused: BTreeMap::new(), accepted: BTreeMap::new(), rejected: BTreeMap::new() };
        install(context(&w, "deployer.near", NearToken::from_yoctonear(0)), HashMap::new(), vec![]);
        let c = X207Circle::with_config(named("dao.near"), "agents-main".into(), named("usdt.near"), named("intents.near"), Params::default(), w.now);
        env::state_write(&c);
        drop(c);
        w.storage = take();
        w
    }

    /// One transaction: the state comes from storage, `f` runs, a panic reverts everything. `Err` is the message of the panic.
    fn call<R>(&mut self, predecessor: &str, deposit: NearToken, results: Vec<PromiseResult>, f: impl FnOnce(&mut X207Circle) -> R) -> Result<R, String> {
        self.height += 1;
        install(context(self, predecessor, deposit), self.storage.clone(), results);
        let mut c: X207Circle = env::state_read().expect("state");
        match catch_unwind(AssertUnwindSafe(|| f(&mut c))) {
            Ok(r) => {
                env::state_write(&c);
                drop(c);
                self.storage = take();
                Ok(r)
            }
            Err(e) => {
                drop(c);
                let message = panic_message(e);
                let code = message.find("ERR_").map_or("(no ERR_ code)", |i| message[i..].split(|ch: char| !(ch.is_ascii_uppercase() || ch == '_')).next().unwrap_or(""));
                *self.refused.entry(code.to_string()).or_default() += 1;
                Err(message)
            }
        }
    }

    fn view<R>(&mut self, f: impl FnOnce(&X207Circle) -> R) -> R {
        install(context(self, "viewer.near", NearToken::from_yoctonear(0)), self.storage.clone(), vec![]);
        let c: X207Circle = env::state_read().expect("state");
        let r = f(&c);
        drop(c);
        r
    }

    /// The epoch that is in the clearing phase: `close_epoch` opens the next epoch at once, so it is the one before the running epoch.
    fn clearing_epoch(&mut self) -> u64 {
        let running = self.epoch().epoch;
        [running.saturating_sub(1).max(1), running]
            .into_iter()
            .find(|e| self.view(|c| c.get_epoch(Some(*e)).is_some_and(|v| matches!(v.phase, EpochPhaseView::Clearing | EpochPhaseView::Commit))))
            .unwrap_or(running)
    }

    fn epoch(&mut self) -> crate::clearing::EpochView {
        self.view(|c| c.get_epoch(None).expect("running epoch"))
    }
}

#[derive(Debug, Clone)]
enum Op {
    Join(usize),
    Stake(usize, u32),
    Register(usize, usize, u32),
    PostIntent(usize, u8),
    RemoveIntent(usize, u8),
    SetAssetClass,
    OfferGrant(usize),
    AcceptGrant(usize, u8),
    RevokeGrant(usize, u8, u8),
    Advance(u8),
    CloseEpoch,
    SubmitCycle(usize, usize),
    SubmitBad(usize, usize, u32),
    CommitEpoch,
    Settle(usize),
    Finalize,
    Claim(usize, bool),
    Prune(u32),
    /// Two vouchers between two members, one each way, so that a cycle exists to be cleared.
    PairTrade(usize, usize, u32, u32),
    /// The best cycle proposal the obligations allow: every pair that owes both ways, cleared by the smaller amount.
    SubmitBest,
    /// A stretch of an epoch in the order of the process: how many steps of it run.
    Flow(u8),
}

fn op() -> impl Strategy<Value = Op> {
    let m = 0..MEMBERS;
    prop_oneof![
        3 => m.clone().prop_map(Op::Join),
        4 => (m.clone(), 1u32..=300).prop_map(|(a, x)| Op::Stake(a, x)),
        8 => (m.clone(), m.clone(), 1u32..=120).prop_map(|(a, b, x)| Op::Register(a, b, x)),
        1 => (m.clone(), any::<u8>()).prop_map(|(a, h)| Op::PostIntent(a, h)),
        1 => (m.clone(), any::<u8>()).prop_map(|(a, h)| Op::RemoveIntent(a, h)),
        1 => Just(Op::SetAssetClass),
        2 => m.clone().prop_map(Op::OfferGrant),
        2 => (m.clone(), 1u8..=3).prop_map(|(a, id)| Op::AcceptGrant(a, id)),
        1 => (m.clone(), 1u8..=3, 0u8..=4).prop_map(|(a, id, r)| Op::RevokeGrant(a, id, r)),
        6 => (0u8..=4).prop_map(Op::Advance),
        3 => Just(Op::CloseEpoch),
        4 => (m.clone(), m.clone()).prop_map(|(a, b)| Op::SubmitCycle(a, b)),
        1 => (m.clone(), m.clone(), 1u32..=50).prop_map(|(a, b, x)| Op::SubmitBad(a, b, x)),
        3 => Just(Op::CommitEpoch),
        4 => m.clone().prop_map(Op::Settle),
        3 => Just(Op::Finalize),
        4 => (m.clone(), any::<bool>()).prop_map(|(a, ok)| Op::Claim(a, ok)),
        2 => (1u32..=30).prop_map(Op::Prune),
        4 => (m.clone(), m.clone(), 1u32..=60, 1u32..=60).prop_map(|(a, b, x, y)| Op::PairTrade(a, b, x, y)),
        2 => Just(Op::SubmitBest),
        6 => (1u8..=13).prop_map(Op::Flow),
    ]
}

fn signed_voucher(payer: usize, payee: usize, epoch: u64, seq: u64, cumulative: u128) -> (Voucher, String) {
    let voucher = Voucher {
        circle: named(CIRCLE), payer: acc(payer), payee: acc(payee), epoch, seq, cumulative_amount: U128(cumulative), unit: "credit".into(), resource: "r".into(),
    };
    let signature = format!("ed25519:{}", near_sdk::bs58::encode(key(payer).sign(&voucher_signing_hash(&voucher)).to_bytes()).into_string());
    (voucher, signature)
}

/// Runs one operation. The USDT that moves is booked in `w.usdt`. Returns `Err(message)` for a refusal.
fn run(w: &mut World, op: &Op) -> Result<(), String> {
    let result = run_inner(w, op);
    let name = format!("{op:?}").split(['(', ' ']).next().unwrap_or("").to_string();
    *(if result.is_ok() { &mut w.accepted } else { &mut w.rejected }).entry(name).or_default() += 1;
    result
}

fn run_inner(w: &mut World, op: &Op) -> Result<(), String> {
    let none = NearToken::from_yoctonear(0);
    match *op {
        Op::Join(m) => w.call(&format!("m{m}.near"), NearToken::from_near(1), vec![], |c| {
            c.join("agents-main".into(), pubkey(m), "a".repeat(64));
        }),
        Op::Stake(m, x) => {
            let amount = x as u128 * CREDIT;
            let back = w.call("usdt.near", none, vec![], |c| c.ft_on_transfer(acc(m), U128(amount), r#"{"action":"stake"}"#.into()))?;
            if let PromiseOrValue::Value(refund) = back {
                w.usdt += amount - refund.0;
            }
            Ok(())
        }
        Op::Register(p, q, x) => {
            let epoch = w.epoch().epoch;
            let (seq, cumulative) = w.cumulative.get(&(p, q, epoch)).copied().unwrap_or((0, 0));
            let (voucher, signature) = signed_voucher(p, q, epoch, seq + 1, cumulative + x as u128 * CREDIT);
            let next = (seq + 1, cumulative + x as u128 * CREDIT);
            w.call("relayer.near", none, vec![], |c| {
                c.register_cumulative(voucher, signature);
            })?;
            w.cumulative.insert((p, q, epoch), next);
            Ok(())
        }
        Op::PostIntent(m, h) => w.call(&format!("m{m}.near"), none, vec![], |c| c.post_intent([h; 32].into())),
        Op::RemoveIntent(m, h) => w.call(&format!("m{m}.near"), none, vec![], |c| c.remove_intent([h; 32].into())),
        Op::SetAssetClass => w.call("dao.near", none, vec![], |c| {
            let entry: AssetClassEntry = serde_json::from_value(json!({
                "asset_type": "near-rpc:access", "class": "B", "delivery_methods": ["near_access_key"], "accepted_proofs": [], "terms_hash": "ab".repeat(32)
            }))
            .unwrap();
            c.set_asset_class("near-rpc:access".into(), entry)
        }),
        Op::OfferGrant(m) => {
            let expires = w.now + 3_600_000_000_000;
            w.call(&format!("m{m}.near"), none, vec![], |c| {
                let input: GrantInput = serde_json::from_value(json!({
                    "asset_type": "near-rpc:access", "delivery_method": "near_access_key",
                    "limits": {"scope": "{\"receiver_id\":\"api.near\",\"method_names\":[\"get\"],\"allowance\":\"1\"}", "quota": null, "expires_at": expires.to_string()},
                    "unit": "call", "unit_price": "1000", "operator": null, "proof_hash": null, "proof_kind": null
                }))
                .unwrap();
                c.offer_grant(input);
            })
        }
        Op::AcceptGrant(m, _) => {
            // the first offered grant of somebody else: the ids are the order in which the grants were offered
            let id = (1..=12u64).find(|id| w.view(|c| c.get_grant(*id)).is_some_and(|g| g.status == crate::grant::GrantStatus::Offered && g.grantor != acc(m)));
            w.call(&format!("m{m}.near"), none, vec![], |c| c.accept_grant(id.unwrap_or(1), None))
        }
        Op::RevokeGrant(m, id, r) => {
            let reason = [RevokeReason::Grantor, RevokeReason::Grantee, RevokeReason::Operator, RevokeReason::Expired, RevokeReason::Defaulted][r as usize % 5];
            w.call(&format!("m{m}.near"), none, vec![], |c| c.revoke_grant(id as u64, reason))
        }
        Op::Advance(kind) => {
            // the next deadline of that kind in any epoch that is still waiting for it
            let running = w.epoch();
            let epochs: Vec<crate::clearing::EpochView> = (1..=running.epoch).filter_map(|e| w.view(|c| c.get_epoch(Some(e)))).collect();
            let next = |pick: &dyn Fn(&crate::clearing::EpochView) -> Option<u64>| epochs.iter().filter_map(pick).filter(|d| *d > w.now).min();
            let target = match kind {
                0 => running.cutoff_at.0 + 1,
                1 => next(&|e| e.proposal_deadline.map(|d| d.0)).map_or(w.now + 60_000_000_000, |d| d + 1),
                2 => next(&|e| e.payment_deadline.map(|d| d.0)).map_or(w.now + 60_000_000_000, |d| d + 1),
                3 => w.now + 60_000_000_000,
                _ => w.now + 3_600_000_000_000,
            };
            w.now = w.now.max(target);
            Ok(())
        }
        Op::CloseEpoch => {
            let epoch = w.epoch().epoch;
            w.call("keeper.near", none, vec![], |c| c.close_epoch(epoch))
        }
        Op::SubmitCycle(a, b) => {
            let epoch = w.clearing_epoch();
            let all = w.view(|c| c.list_obligations(epoch, 0, 1_000));
            let obligation = |d: usize, k: usize| all.iter().find(|o| o.debtor == acc(d) && o.creditor == acc(k)).map_or(0, |o| o.obligation.0);
            let cleared = obligation(a, b).min(obligation(b, a));
            let edges = vec![
                ClearingEdge { debtor: acc(a), creditor: acc(b), cleared: U128(cleared) },
                ClearingEdge { debtor: acc(b), creditor: acc(a), cleared: U128(cleared) },
            ];
            w.call("solver.near", none, vec![], |c| {
                c.submit_clearing(epoch, edges);
            })
        }
        Op::SubmitBad(a, b, x) => {
            let epoch = w.clearing_epoch();
            let edges = vec![ClearingEdge { debtor: acc(a), creditor: acc(b), cleared: U128(x as u128 * CREDIT) }];
            w.call("solver.near", none, vec![], |c| {
                c.submit_clearing(epoch, edges);
            })
        }
        Op::CommitEpoch => {
            let epoch = w.epoch().epoch;
            // the running epoch is the new one after `close_epoch`; the epoch to commit is the one before it when it is still clearing
            let target = if epoch > 1 { epoch - 1 } else { epoch };
            w.call("keeper.near", none, vec![], |c| c.commit_epoch(target)).or_else(|_| w.call("keeper.near", none, vec![], |c| c.commit_epoch(epoch)))
        }
        Op::Settle(m) => {
            let epoch = w.epoch().epoch;
            let due = [epoch.saturating_sub(1).max(1), epoch].into_iter().find_map(|e| {
                w.view(|c| c.net_position(e, acc(m))).filter(|p| p.status == PositionStatus::Due && p.amount.0 > 0).map(|p| (e, p.amount.0))
            });
            let Some((e, amount)) = due else {
                return w.call("usdt.near", none, vec![], |c| c.ft_on_transfer(acc(m), U128(CREDIT), json!({"action": "settle", "epoch": 1}).to_string())).map(|_| ());
            };
            let back = w.call("usdt.near", none, vec![], |c| c.ft_on_transfer(acc(m), U128(amount), json!({"action": "settle", "epoch": e}).to_string()))?;
            if let PromiseOrValue::Value(refund) = back {
                w.usdt += amount - refund.0;
            }
            Ok(())
        }
        Op::Finalize => {
            let epoch = w.epoch().epoch;
            let target = if epoch > 1 { epoch - 1 } else { epoch };
            w.call("keeper.near", none, vec![], |c| c.finalize_epoch(target)).or_else(|_| w.call("keeper.near", none, vec![], |c| c.finalize_epoch(epoch)))
        }
        Op::Claim(m, ok) => {
            let epoch = w.epoch().epoch;
            let found = [epoch.saturating_sub(1).max(1), epoch].into_iter().find_map(|e| {
                let shortfall = w.view(|c| c.get_epoch(Some(e)).map_or(0, |v| v.shortfall.0));
                w.view(|c| c.net_position(e, acc(m)))
                    .filter(|p| p.role == PositionRole::Creditor && p.status == PositionStatus::Claimable && shortfall == 0)
                    .map(|p| (e, p.amount.0))
            });
            let Some((e, amount)) = found else {
                return w.call(&format!("m{m}.near"), NearToken::from_yoctonear(1), vec![], |c| {
                    c.claim(epoch, crate::types::ClaimDestination::Near);
                });
            };
            w.call(&format!("m{m}.near"), NearToken::from_yoctonear(1), vec![], |c| {
                c.claim(e, crate::types::ClaimDestination::Near);
            })?;
            let result = if ok { PromiseResult::Successful(vec![]) } else { PromiseResult::Failed };
            let paid = w.call(CIRCLE, none, vec![result], |c| c.on_claim_resolved(e, acc(m), U128(amount), crate::types::ClaimDestination::Near))?;
            w.usdt -= paid.0;
            Ok(())
        }
        Op::PairTrade(..) | Op::Flow(_) => unreachable!("expanded before it runs"),
        Op::SubmitBest => {
            let epoch = w.clearing_epoch();
            let all = w.view(|c| c.list_obligations(epoch, 0, 1_000));
            let obligation = |d: &AccountId, k: &AccountId| all.iter().find(|o| &o.debtor == d && &o.creditor == k).map_or(0, |o| o.obligation.0);
            let mut edges = vec![];
            for o in &all {
                let back = obligation(&o.creditor, &o.debtor);
                let cleared = o.obligation.0.min(back);
                if cleared > 0 {
                    edges.push(ClearingEdge { debtor: o.debtor.clone(), creditor: o.creditor.clone(), cleared: U128(cleared) });
                }
            }
            w.call("solver.near", none, vec![], |c| {
                c.submit_clearing(epoch, edges);
            })
        }
        Op::Prune(n) => {
            let epoch = w.epoch().epoch;
            w.call("anyone.near", none, vec![], |c| {
                c.prune_epoch(epoch.saturating_sub(1).max(1), n);
            })
        }
    }
}

/// The composite operations become plain ones; a run is a list of plain operations.
fn expand(op: Op) -> Vec<Op> {
    match op {
        Op::PairTrade(a, b, x, y) => vec![Op::Register(a, b, x), Op::Register(b, a, y)],
        Op::Flow(steps) => {
            let mut all = vec![Op::Advance(0), Op::CloseEpoch, Op::SubmitBest, Op::Advance(1), Op::CommitEpoch];
            all.extend((0..MEMBERS).map(Op::Settle));
            all.extend([Op::Advance(2), Op::Finalize]);
            all.extend((0..MEMBERS).map(|m| Op::Claim(m, true)));
            all.push(Op::Prune(30));
            all.truncate(steps as usize);
            all
        }
        other => vec![other],
    }
}

fn check(w: &mut World, after: &Op, accepted: bool) -> Result<(), TestCaseError> {
    let (members, pool) = w.view(|c| (c.list_members(0, 100), c.get_pool()));
    let mut stake = 0u128;
    for m in &members {
        prop_assert!(m.limit.0 <= m.stake.0, "I-01 after {:?}: {} has limit {} above stake {}", after, m.account_id, m.limit.0, m.stake.0);
        stake += m.stake.0;
    }
    prop_assert_eq!(pool.total_stake.0, stake, "total_stake after {:?}", after);
    prop_assert_eq!(pool.accounted.0, w.usdt, "D6 after {:?}: the pool books {} and {} came in", after, pool.accounted.0, w.usdt);
    let last = w.epoch().epoch;
    for epoch in 1..=last {
        let positions = w.view(|c| c.list_positions(epoch, 0, 100));
        let due: u128 = positions.iter().filter(|p| p.role == PositionRole::Debtor).map(|p| p.amount.0).sum();
        let claimable: u128 = positions.iter().filter(|p| p.role == PositionRole::Creditor).map(|p| p.amount.0).sum();
        prop_assert_eq!(due, claimable, "I-09 in epoch {} after {:?}", epoch, after);
    }
    // right after a registration that the contract accepted, the payer owes no more than its limit (later the limit may fall, at a default or a recount)
    if let (Op::Register(p, _, _), true) = (after, accepted) {
        let (debt, limit) = w.view(|c| (c.outstanding_debt(acc(*p)).0, c.limit(acc(*p)).0));
        prop_assert!(debt <= limit, "{} owes {} above its limit {}", acc(*p), debt, limit);
    }
    Ok(())
}

// TC-STATEFUL-01: any order of calls keeps the invariants, and every refusal is an `ERR_` code. The run is driven by hand, so that the coverage can be
// asserted at the end: a run that the contract refused call by call would pass for the wrong reason.
#[test]
fn random_sequences_of_calls_keep_the_invariants() {
    use proptest::test_runner::{Config, TestRunner};
    let cases: u32 = std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    let mut runner = TestRunner::new(Config { cases, ..Config::default() });
    let accepted = std::cell::RefCell::new(BTreeMap::<String, u32>::new());
    let rejected = std::cell::RefCell::new(BTreeMap::<String, u32>::new());
    let refusals = std::cell::RefCell::new(BTreeMap::<String, u32>::new());
    let result = runner.run(&proptest::collection::vec(op(), 20..=90), |ops| {
        let mut w = World::new();
        // start with members that have stake, so that the sequences get past the first gates
        for m in 0..MEMBERS {
            let _ = run(&mut w, &Op::Join(m));
            let _ = run(&mut w, &Op::Stake(m, 100));
        }
        let _ = run(&mut w, &Op::SetAssetClass);
        for operation in ops.into_iter().flat_map(expand) {
            let accepted = match run(&mut w, &operation) {
                Ok(()) => true,
                Err(message) => {
                    prop_assert!(message.contains("ERR_"), "a refusal without an ERR_ code after {:?}: {}", operation, message);
                    false
                }
            };
            check(&mut w, &operation, accepted)?;
        }
        for (k, v) in &w.accepted {
            *accepted.borrow_mut().entry(k.clone()).or_default() += v;
        }
        for (k, v) in &w.rejected {
            *rejected.borrow_mut().entry(k.clone()).or_default() += v;
        }
        for (k, v) in &w.refused {
            *refusals.borrow_mut().entry(k.clone()).or_default() += v;
        }
        Ok(())
    });
    let report = format!("accepted {:?}\nrefused {:?}\nerror codes {:?}\n", accepted.borrow(), rejected.borrow(), refusals.borrow());
    if let Ok(path) = std::env::var("X207_STATEFUL_STATS") {
        std::fs::write(path, &report).unwrap();
    }
    if let Err(e) = result {
        panic!("{e}\n{report}");
    }
    // coverage: the sequences must get through the main steps of an epoch, not only be refused
    for name in ["Register", "CloseEpoch", "SubmitBest", "CommitEpoch", "Settle", "Finalize", "Claim", "Prune", "OfferGrant", "AcceptGrant"] {
        assert!(accepted.borrow().get(name).copied().unwrap_or(0) > 0, "no {name} was ever accepted in {cases} sequences:\n{report}");
    }
}

#[test]
fn the_harness_gets_a_real_epoch_through_the_whole_cycle() {
    // a fixed run through join, stake, vouchers, clearing, commit, settle, finalize and claim, so that the property test does not pass by refusing everything
    let mut w = World::new();
    for m in 0..3 {
        run(&mut w, &Op::Join(m)).unwrap();
        run(&mut w, &Op::Stake(m, 100)).unwrap();
    }
    for (p, q, x) in [(0, 1, 60), (1, 2, 25), (2, 0, 25)] {
        run(&mut w, &Op::Register(p, q, x)).unwrap();
        check(&mut w, &Op::Register(p, q, x), true).unwrap();
    }
    run(&mut w, &Op::Advance(0)).unwrap();
    run(&mut w, &Op::CloseEpoch).unwrap();
    let all = w.view(|c| c.list_obligations(1, 0, 100));
    assert_eq!(all.len(), 3);
    let edges = vec![
        ClearingEdge { debtor: acc(0), creditor: acc(1), cleared: U128(25 * CREDIT) },
        ClearingEdge { debtor: acc(1), creditor: acc(2), cleared: U128(25 * CREDIT) },
        ClearingEdge { debtor: acc(2), creditor: acc(0), cleared: U128(25 * CREDIT) },
    ];
    w.now += 1_000_000_000;
    w.call("solver.near", NearToken::from_yoctonear(0), vec![], |c| {
        c.submit_clearing(1, edges);
    })
    .unwrap();
    let deadline = w.view(|c| c.get_epoch(Some(1)).unwrap().proposal_deadline.unwrap().0);
    w.now = deadline;
    w.call("keeper.near", NearToken::from_yoctonear(0), vec![], |c| c.commit_epoch(1)).unwrap();
    assert!(matches!(w.view(|c| c.get_epoch(Some(1)).unwrap().phase), EpochPhaseView::PaymentWindow));
    run(&mut w, &Op::Settle(0)).unwrap();
    check(&mut w, &Op::Settle(0), true).unwrap();
    w.now = w.view(|c| c.get_epoch(Some(1)).unwrap().payment_deadline.unwrap().0) + 1;
    w.call("keeper.near", NearToken::from_yoctonear(0), vec![], |c| c.finalize_epoch(1)).unwrap();
    run(&mut w, &Op::Claim(1, true)).unwrap();
    check(&mut w, &Op::Claim(1, true), true).unwrap();
    run(&mut w, &Op::Prune(30)).unwrap();
    check(&mut w, &Op::Prune(30), true).unwrap();
}
