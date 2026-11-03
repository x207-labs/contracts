//! CT-094 in near-sandbox: the bond of both sides of a swap, with an arbiter named in the signed terms. Four paths (two confirmations, the buyer wins, the seller wins, an
//! arbiter that is silent) and the refusals. The bond is a lock on the stake: no token leaves the circle in any path. Needs `cargo near build non-reproducible-wasm` first.
//! Run one test at a time on a loaded machine.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::funded::Funded;
use near_workspaces::types::NearToken;
use near_workspaces::Account;
use serde_json::{json, Value};
use x207_circle::deal::{Deal, DealState};

const STAKE: u128 = 500 * CREDIT;
/// Swap of 120 and 100 credit: the value is 120, above the threshold of 100; the bond of each side is 20% = 24 credit.
const LEGS: [(&str, &str, u128); 2] = [("alice", "bob", 120 * CREDIT), ("bob", "alice", 100 * CREDIT)];
const BOND: u128 = 24 * CREDIT;
const HOUR: u64 = 3600 * 1_000_000_000;
const AGREEMENT: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
const EVIDENCE: &str = "efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";

struct World {
    f: Funded,
    arb: Account,
}

async fn world() -> anyhow::Result<World> {
    let mut f = Funded::deploy(None).await?;
    for name in ["alice", "bob", "carol"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, STAKE).await?.is_success());
    }
    let arb = f.account("arb").await?;
    Ok(World { f, arb })
}

impl World {
    /// Commits the swap with the arbiter and the window of one day; returns the bundle id.
    async fn commit(&mut self) -> anyhow::Result<u64> {
        let now = self.f.circle.now_ns().await?;
        let r = self.f.circle.commit_bundle_with_deal(1, &LEGS, (self.arb.id().as_str(), Some(AGREEMENT), now + 24 * HOUR), &["alice", "bob"]).await?;
        assert!(r.is_success(), "{:?}", failure_text(&r));
        let log = events(&r);
        assert_eq!(log.iter().filter(|(n, d)| n == "bond_locked" && d["amount"] == BOND.to_string()).count(), 2);
        Ok(log.iter().find(|(n, _)| n == "bundle_committed").expect("bundle_committed").1["bundle_id"].as_u64().unwrap())
    }

    async fn call(&self, who: &str, method: &str, args: Value) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        let account = if who == "arb" { self.arb.clone() } else { self.f.accounts[who].clone() };
        Ok(self.f.circle.call_as(&account, method, args).await?)
    }

    async fn stake_of(&self, who: &str) -> anyhow::Result<u128> {
        Ok(self.f.member(who).await?["stake"].as_str().unwrap().parse()?)
    }

    async fn locked(&self, who: &str) -> anyhow::Result<u128> {
        Ok(self.f.circle.view("bond_locked", json!({"account_id": account_of(who)})).await?.as_str().unwrap().parse()?)
    }

    async fn usdt_in_circle(&self) -> anyhow::Result<u128> {
        self.f.balance(&self.f.usdt, self.f.circle.contract.id()).await
    }
}

// TC-CT-094-01: path 1, two confirmations
#[tokio::test]
async fn two_confirmations_return_both_bonds_and_close_the_deal() -> anyhow::Result<()> {
    let mut w = world().await?;
    let usdt = w.usdt_in_circle().await?;
    let id = w.commit().await?;
    assert_eq!((w.locked("alice").await?, w.locked("bob").await?, w.locked("carol").await?), (BOND, BOND, 0));
    let deal = w.f.circle.view("get_deal", json!({"bundle_id": id})).await?;
    assert_eq!((deal["state"].as_str(), deal["bond_each"].as_str(), deal["arbiter"].as_str(), deal["agreement_hash"].as_str()), (Some("open"), Some(BOND.to_string().as_str()), Some(w.arb.id().as_str()), Some(AGREEMENT)));
    // while the bond is locked the member cannot leave
    let alice = w.f.accounts["alice"].clone();
    let exit = alice.call(w.f.circle.contract.id(), "request_exit").deposit(NearToken::from_yoctonear(1)).transact().await?;
    assert!(failure_text(&exit).expect("refused").contains("ERR_BOND_LOCKED"));
    let c1 = w.call("alice", "confirm_deal", json!({"bundle_id": id})).await?;
    assert!(c1.is_success(), "{:?}", failure_text(&c1));
    assert_eq!((w.locked("alice").await?, w.locked("bob").await?), (BOND, 0), "alice confirmed bob: bob's bond is free");
    assert!(failure_text(&w.call("alice", "confirm_deal", json!({"bundle_id": id})).await?).expect("refused").contains("ERR_DEAL_CLOSED"));
    assert!(w.call("bob", "confirm_deal", json!({"bundle_id": id})).await?.is_success());
    assert_eq!((w.locked("alice").await?, w.locked("bob").await?), (0, 0));
    assert_eq!(w.f.circle.view("get_deal", json!({"bundle_id": id})).await?, Value::Null);
    assert_eq!((w.stake_of("alice").await?, w.stake_of("bob").await?), (STAKE, STAKE), "a confirmation moves nothing");
    assert_eq!(w.usdt_in_circle().await?, usdt, "no token left the circle");
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-094-02: path 2, the buyer wins (alice complains about bob; the arbiter finds bob at fault)
#[tokio::test]
async fn the_complainant_wins_and_the_bond_of_the_accused_goes_to_its_stake() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.commit().await?;
    let usdt = w.usdt_in_circle().await?;
    let disputed = w.call("alice", "dispute_deal", json!({"bundle_id": id, "evidence_hash": EVIDENCE})).await?;
    assert!(disputed.is_success(), "{:?}", failure_text(&disputed));
    assert!(events(&disputed).iter().any(|(n, d)| n == "deal_disputed" && d["complainant"] == account_of("alice") && d["accused"] == account_of("bob")));
    // nobody but the arbiter rules, not the owner and not a party
    for who in ["alice", "bob", "dao", "carol"] {
        let account = if who == "dao" { w.f.account("dao").await? } else { w.f.accounts[who].clone() };
        assert!(failure_text(&w.f.circle.call_as(&account, "rule_deal", json!({"bundle_id": id, "verdict": "accused_guilty"})).await?).expect("refused").contains("ERR_NOT_ARBITER"), "{who}");
    }
    assert_eq!((w.stake_of("alice").await?, w.stake_of("bob").await?), (STAKE, STAKE), "refused rulings moved nothing");
    let ruled = w.call("arb", "rule_deal", json!({"bundle_id": id, "verdict": "accused_guilty"})).await?;
    assert!(ruled.is_success(), "{:?}", failure_text(&ruled));
    let log = events(&ruled);
    assert!(log.iter().any(|(n, d)| n == "bond_slashed" && d["from"] == account_of("bob") && d["to"] == account_of("alice") && d["amount"] == BOND.to_string()));
    assert_eq!((w.stake_of("alice").await?, w.stake_of("bob").await?), (STAKE + BOND, STAKE - BOND));
    assert_eq!((w.locked("alice").await?, w.locked("bob").await?), (0, 0));
    assert_eq!(w.f.circle.view("get_deal", json!({"bundle_id": id})).await?, Value::Null);
    assert_eq!(w.usdt_in_circle().await?, usdt, "no token left the circle");
    // the limit of the loser follows its stake: limit <= stake
    let bob = w.f.member("bob").await?;
    assert!(bob["limit"].as_str().unwrap().parse::<u128>()? <= bob["stake"].as_str().unwrap().parse::<u128>()?);
    w.f.assert_d6().await?;
    // the winner can leave with the bond that it won: ask to exit, nothing locked
    let alice = w.f.accounts["alice"].clone();
    assert!(alice.call(w.f.circle.contract.id(), "request_exit").deposit(NearToken::from_yoctonear(1)).transact().await?.is_success());
    Ok(())
}

// TC-CT-094-03: path 3, the seller wins (alice complains, the arbiter finds the complaint false: half of her bond goes to bob)
#[tokio::test]
async fn a_false_complaint_costs_half_of_the_bond_of_the_complainant() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.commit().await?;
    assert!(w.call("alice", "dispute_deal", json!({"bundle_id": id, "evidence_hash": EVIDENCE})).await?.is_success());
    let ruled = w.call("arb", "rule_deal", json!({"bundle_id": id, "verdict": "complaint_false"})).await?;
    assert!(ruled.is_success(), "{:?}", failure_text(&ruled));
    assert_eq!((w.stake_of("alice").await?, w.stake_of("bob").await?), (STAKE - BOND / 2, STAKE + BOND / 2));
    assert_eq!((w.locked("alice").await?, w.locked("bob").await?), (0, 0));
    assert!(events(&ruled).iter().any(|(n, d)| n == "bond_released" && d["account_id"] == account_of("alice") && d["amount"] == (BOND / 2).to_string() && d["reason"] == "ruling"));
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-094-04: path 4, the arbiter says nothing: both bonds are released and nobody is punished
#[tokio::test]
async fn a_silent_arbiter_punishes_nobody() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.commit().await?;
    assert!(w.call("bob", "dispute_deal", json!({"bundle_id": id, "evidence_hash": EVIDENCE})).await?.is_success());
    let carol = "carol";
    assert!(failure_text(&w.call(carol, "settle_deal", json!({"bundle_id": id})).await?).expect("refused").contains("ERR_DEAL_NOT_DUE"));
    // the dispute is moved eight days back (the record is rewritten, as the upgrade timelock is in ct055)
    let view = w.f.circle.view("get_deal", json!({"bundle_id": id})).await?;
    let deal = Deal {
        parties: [account_of("alice").parse()?, account_of("bob").parse()?],
        arbiter: view["arbiter"].as_str().unwrap().parse()?,
        agreement_hash: Some(AGREEMENT.into()),
        confirm_by: view["confirm_by"].as_str().unwrap().parse()?,
        bond_each: BOND,
        locked: [true, true],
        state: DealState::Disputed,
        complainant: Some(1),
        disputed_at: 1,
        evidence_hash: Some(EVIDENCE.into()),
    };
    let key = [vec![b'd'], id.to_le_bytes().to_vec()].concat();
    w.f.circle.worker.patch_state(w.f.circle.contract.id(), &key, &near_sdk::borsh::to_vec(&deal)?).await?;
    // too late for the arbiter, on time for anybody
    assert!(failure_text(&w.call("arb", "rule_deal", json!({"bundle_id": id, "verdict": "accused_guilty"})).await?).expect("refused").contains("ERR_RULING_WINDOW"));
    let settled = w.call(carol, "settle_deal", json!({"bundle_id": id})).await?;
    assert!(settled.is_success(), "{:?}", failure_text(&settled));
    assert!(events(&settled).iter().filter(|(n, d)| n == "bond_released" && d["reason"] == "arbiter_silent").count() == 2);
    assert_eq!((w.stake_of("alice").await?, w.stake_of("bob").await?), (STAKE, STAKE));
    assert_eq!((w.locked("alice").await?, w.locked("bob").await?), (0, 0));
    assert_eq!(w.f.circle.view("get_deal", json!({"bundle_id": id})).await?, Value::Null);
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-094-05: no dispute and the window ends: anybody releases the bonds
#[tokio::test]
async fn bonds_are_released_when_the_window_ends_without_a_dispute() -> anyhow::Result<()> {
    let mut w = world().await?;
    let now = w.f.circle.now_ns().await?;
    let r = w.f.circle.commit_bundle_with_deal(1, &LEGS, (w.arb.id().as_str(), None, now + HOUR + 60_000_000_000), &["alice", "bob"]).await?;
    assert!(r.is_success(), "{:?}", failure_text(&r));
    let id = events(&r).iter().find(|(n, _)| n == "bundle_committed").unwrap().1["bundle_id"].as_u64().unwrap();
    assert!(failure_text(&w.call("carol", "settle_deal", json!({"bundle_id": id})).await?).expect("refused").contains("ERR_DEAL_NOT_DUE"));
    let confirm_by: u64 = w.f.circle.view("get_deal", json!({"bundle_id": id})).await?["confirm_by"].as_str().unwrap().parse()?;
    w.f.circle.advance_to(confirm_by).await?;
    let settled = w.call("carol", "settle_deal", json!({"bundle_id": id})).await?;
    assert!(settled.is_success(), "{:?}", failure_text(&settled));
    assert!(events(&settled).iter().any(|(n, d)| n == "bond_released" && d["reason"] == "elapsed"));
    assert_eq!((w.locked("alice").await?, w.locked("bob").await?), (0, 0));
    Ok(())
}

// TC-CT-094-06: refusals at the commit
#[tokio::test]
async fn the_commit_refuses_what_the_rules_forbid_and_locks_nothing() -> anyhow::Result<()> {
    let mut w = world().await?;
    let now = w.f.circle.now_ns().await?;
    let dao = w.f.account("dao").await?;
    // at the threshold without an arbiter
    let plain = w.f.circle.commit_bundle(1, &LEGS, &["alice", "bob"]).await?;
    assert!(failure_text(&plain).expect("refused").contains("ERR_BOND_REQUIRED"));
    // an arbiter that is a party, the owner, a window too short or too long
    for (arbiter, window, code) in [("alice", 24 * HOUR, "ERR_ARBITER_INVALID"), (dao.id().as_str(), 24 * HOUR, "ERR_ARBITER_INVALID"), (w.arb.id().as_str(), HOUR / 2, "ERR_DEAL_WINDOW"), (w.arb.id().as_str(), 15 * 24 * HOUR, "ERR_DEAL_WINDOW")] {
        let who = if arbiter == "alice" { account_of("alice") } else { arbiter.to_string() };
        let r = w.f.circle.commit_bundle_with_deal(1, &LEGS, (&who, None, now + window), &["alice", "bob"]).await?;
        assert!(failure_text(&r).expect("refused").contains(code), "{arbiter} {window}: {:?}", failure_text(&r));
    }
    // a swap below the threshold needs no arbiter
    let small = w.f.circle.commit_bundle(1, &[("alice", "bob", 50 * CREDIT), ("bob", "alice", 40 * CREDIT)], &["alice", "bob"]).await?;
    assert!(small.is_success(), "{:?}", failure_text(&small));
    assert_eq!((w.locked("alice").await?, w.locked("bob").await?), (0, 0));
    // a ring of three above the threshold needs none in 0.0.12, and an arbiter on a ring is refused
    let ring = [("alice", "bob", 120 * CREDIT), ("bob", "carol", 120 * CREDIT), ("carol", "alice", 120 * CREDIT)];
    let r = w.f.circle.commit_bundle_with_deal(1, &ring, (w.arb.id().as_str(), None, now + 24 * HOUR), &["alice", "bob", "carol"]).await?;
    assert!(failure_text(&r).expect("refused").contains("ERR_BAD_BUNDLE"));
    let r = w.f.circle.commit_bundle(1, &ring, &["alice", "bob", "carol"]).await?;
    assert!(r.is_success(), "{:?}", failure_text(&r));
    // the signatures must cover the deal terms: signing the plain terms but sending an arbiter is refused
    let (plain, hash) = w.f.circle.bundle_terms(1, &[("alice", "bob", 130 * CREDIT), ("bob", "alice", 100 * CREDIT)]);
    let sigs: Vec<Value> = ["alice", "bob"].iter().map(|s| json!({"account_id": account_of(s), "signature": w.f.circle.sign_terms(s, &plain, hash)})).collect();
    let legs: Vec<Value> = [("alice", "bob", 130 * CREDIT), ("bob", "alice", 100 * CREDIT)].iter().map(|(a, b, m)| json!({"payer": account_of(a), "payee": account_of(b), "max_amount": m.to_string()})).collect();
    let bad = w.f.circle.call_keeper("commit_bundle", json!({"bundle": {"epoch": 1, "legs": legs, "arbiter": w.arb.id(), "confirm_by": (now + 24 * HOUR).to_string()}, "signatures": sigs})).await?;
    assert!(failure_text(&bad).expect("refused").contains("ERR_BUNDLE_SIGNATURES"), "{:?}", failure_text(&bad));
    assert_eq!((w.locked("alice").await?, w.locked("bob").await?), (0, 0));
    Ok(())
}

// TC-CT-094-07: the bond must come from the slack between stake and limit
#[tokio::test]
async fn a_member_without_enough_stake_above_its_limit_cannot_enter_the_deal() -> anyhow::Result<()> {
    let mut f = Funded::deploy(None).await?;
    // 150 credit of stake: limit 120, slack 30; a bond of 24 fits once, not twice
    for name in ["alice", "bob"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 150 * CREDIT).await?.is_success());
    }
    let arb = f.account("arb").await?;
    let now = f.circle.now_ns().await?;
    let first = f.circle.commit_bundle_with_deal(1, &LEGS, (arb.id().as_str(), None, now + 24 * HOUR), &["alice", "bob"]).await?;
    assert!(first.is_success(), "{:?}", failure_text(&first));
    let second = f.circle.commit_bundle_with_deal(1, &[("alice", "bob", 121 * CREDIT), ("bob", "alice", 100 * CREDIT)], (arb.id().as_str(), None, now + 24 * HOUR), &["alice", "bob"]).await?;
    assert!(failure_text(&second).expect("refused").contains("ERR_BOND_INSUFFICIENT_STAKE") || failure_text(&second).expect("refused").contains("ERR_BUNDLE_HEADROOM"), "{:?}", failure_text(&second));
    Ok(())
}
