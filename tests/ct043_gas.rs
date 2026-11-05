//! CT-043: gas of the final methods in near-sandbox, on the real contract with real members.
//! Table: `cargo test --test ct043_gas -- --nocapture --test-threads=1 | grep GAS`.
//! Needs `cargo near build non-reproducible-wasm` first. The sandbox gas price is 1e8 yocto per gas;
//! the NEAR price for the cost per voucher is an assumption (`NEAR_USD`, default 3.0).

mod common;

use common::circle::{account_of, failure_text, CREDIT};
use common::flow::{edge, finalize, s1_committed_on};
use common::funded::Funded;
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::{Gas, NearToken};
use serde_json::{json, Value};

const LIMIT_TGAS: f64 = 1000.0; // 1 PGas, the per-transaction limit
const MARGIN: f64 = 0.5; // each method must stay below half of the limit

fn tgas(result: &ExecutionFinalResult) -> f64 {
    result.total_gas_burnt.as_gas() as f64 / 1e12
}

fn report(label: &str, tgas: f64) {
    println!("GAS {label}: {tgas:.1} Tgas");
    assert!(
        tgas < LIMIT_TGAS * MARGIN,
        "{label} uses {tgas:.1} Tgas, more than {} Tgas",
        LIMIT_TGAS * MARGIN
    );
}

fn name(i: usize) -> String {
    format!("m{i:02}")
}

async fn members(f: &mut Funded, n: usize) -> anyhow::Result<()> {
    for i in 0..n {
        let n = name(i);
        let account = f.account(&n).await?;
        let join = f.join(&n, Funded::join_deposit()).await?;
        assert!(join.is_success(), "join {n}: {:?}", failure_text(&join));
        let stake = f.stake(&account, 100 * CREDIT).await?;
        assert!(stake.is_success(), "stake {n}: {:?}", failure_text(&stake));
    }
    Ok(())
}

async fn storage_usage(f: &Funded) -> anyhow::Result<u64> {
    Ok(f.circle
        .worker
        .view_account(f.circle.contract.id())
        .await?
        .storage_usage)
}

/// Ring edges `i -> (i + k) % n` for `k` in `1..=offsets`, one voucher each of 1 credit.
async fn register_ring(
    f: &mut Funded,
    n: usize,
    offsets: usize,
) -> anyhow::Result<(f64, Vec<Value>)> {
    let mut first = None;
    let mut edges = Vec::new();
    for k in 1..=offsets {
        for i in 0..n {
            let (payer, payee) = (name(i), name((i + k) % n));
            let r = f.circle.register(&payer, &payee, 1, CREDIT).await?;
            assert!(
                r.is_success(),
                "register {payer}->{payee}: {:?}",
                failure_text(&r)
            );
            first.get_or_insert(tgas(&r));
            edges.push(edge(&payer, &payee, CREDIT));
        }
    }
    Ok((first.unwrap(), edges))
}

async fn close_submit_commit(
    f: &Funded,
    edges: Option<Vec<Value>>,
) -> anyhow::Result<(Option<f64>, f64)> {
    let c = &f.circle;
    let solver = c.worker.dev_create_account().await?;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    let close = c.call_keeper("close_epoch", json!({"epoch": 1})).await?;
    assert!(close.is_success(), "{:?}", failure_text(&close));
    let mut submit_gas = None;
    if let Some(edges) = edges {
        let r = c
            .call_as(
                &solver,
                "submit_clearing",
                json!({"epoch": 1, "edges": edges}),
            )
            .await?;
        assert!(r.is_success(), "submit: {:?}", failure_text(&r));
        submit_gas = Some(tgas(&r));
    }
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?)
        .await?;
    let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "commit: {:?}", failure_text(&commit));
    Ok((submit_gas, tgas(&commit)))
}

// TC-CT-043-01, -02, -03: edges 3, 12 and 30 among 6 agents (3 agents for 3 edges)
async fn ring(n: usize, offsets: usize) -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    members(&mut f, n).await?;
    let (first, edges) = register_ring(&mut f, n, offsets).await?;
    let count = edges.len();
    if count == 3 {
        // a second voucher of the pair: a continuing registration
        let again = f.circle.register(&name(0), &name(1), 1, 2 * CREDIT).await?;
        assert!(again.is_success(), "{:?}", failure_text(&again));
        report("register_cumulative new pair", first);
        report("register_cumulative continuing pair", tgas(&again));
    }
    let (submit, commit) = close_submit_commit(&f, Some(edges)).await?;
    report(&format!("submit_clearing {count} edges"), submit.unwrap());
    report(
        &format!("commit_epoch {count} edges, {n} participants"),
        commit,
    );
    Ok(())
}

#[tokio::test]
async fn gas_edges_3() -> anyhow::Result<()> {
    ring(3, 1).await
}

#[tokio::test]
async fn gas_edges_12() -> anyhow::Result<()> {
    ring(6, 2).await
}

#[tokio::test]
async fn gas_edges_30() -> anyhow::Result<()> {
    ring(6, 5).await
}

// TC-CT-043-03, -05: MAX_MEMBERS members, 63 debtors default against one creditor, then finalize
#[tokio::test]
async fn gas_finalize_with_max_members() -> anyhow::Result<()> {
    const MAX_MEMBERS: usize = 64;
    // 64 joins, stakes and 63 vouchers are about 400 transactions, which must fit in epoch 1 even on a busy machine
    let mut params = Funded::params_short();
    params["epoch_length"] = json!(3000);
    let mut f = Funded::deploy(Some(params)).await?;
    members(&mut f, MAX_MEMBERS).await?;
    for i in 1..MAX_MEMBERS {
        let r = f.circle.register(&name(i), &name(0), 1, CREDIT).await?;
        assert!(r.is_success(), "{:?}", failure_text(&r));
    }
    let (_, commit) = close_submit_commit(&f, None).await?;
    report(
        &format!("commit_epoch 63 edges, {MAX_MEMBERS} participants"),
        commit,
    );
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    report(
        &format!("finalize_epoch {MAX_MEMBERS} members, 63 defaults"),
        tgas(&fin),
    );
    Ok(())
}

// TC-CT-043-04: claim to a NEAR account and to Intents, at exactly CLAIM_MIN_GAS
#[tokio::test]
async fn gas_claim() -> anyhow::Result<()> {
    for destination in ["near", "intents"] {
        let f = s1_committed_on(Funded::deploy_with_intents(Some(Funded::params_short())).await?)
            .await?;
        let fin = finalize(&f.circle).await?;
        assert!(fin.is_success(), "{:?}", failure_text(&fin));
        let r = f.accounts["gpu"]
            .call(f.circle.contract.id(), "claim")
            .args_json(json!({"epoch": 1, "destination": destination}))
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(100))
            .transact()
            .await?;
        assert!(
            r.is_success(),
            "claim {destination} with 100 Tgas: {:?}",
            failure_text(&r)
        );
        assert!(r.failures().is_empty(), "{:?}", r.failures());
        assert!(
            events_include_claim(&r),
            "{destination}: no residual_claimed in the log"
        );
        report(
            &format!("claim {destination} incl. callback (prepaid 100 Tgas)"),
            tgas(&r),
        );
    }
    Ok(())
}

fn events_include_claim(r: &ExecutionFinalResult) -> bool {
    r.logs().iter().any(|l| l.contains("residual_claimed"))
}

// TC-CT-043-06: cost of one voucher against the nominal of 0.02 USD
#[tokio::test]
async fn cost_per_voucher() -> anyhow::Result<()> {
    let near_usd: f64 = std::env::var("NEAR_USD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3.0);
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    members(&mut f, 2).await?;
    let (first, _) = register_ring(&mut f, 2, 1).await?;
    let again = f.circle.register(&name(0), &name(1), 1, 2 * CREDIT).await?;
    let gas = tgas(&again) * 1e12;
    let near = gas * 1e8 / 1e24; // sandbox gas price: 1e8 yocto per gas
    let usd = near * near_usd;
    println!("GAS cost per voucher (continuing): {:.2} Tgas = {near:.6} NEAR = {usd:.6} USD at {near_usd} USD/NEAR; {:.2}% of a 0.02 USD payment (first voucher {first:.1} Tgas)", gas / 1e12, usd / 0.02 * 100.0);
    assert!(
        usd < 0.02,
        "a voucher must cost less than the payment it covers"
    );
    Ok(())
}

// TC-CT-043-07: storage per entry
#[tokio::test]
async fn storage_per_entry() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for n in ["m00", "m01"] {
        f.account(n).await?;
    }
    let s0 = storage_usage(&f).await?;
    assert!(f.join("m00", Funded::join_deposit()).await?.is_success());
    let s1 = storage_usage(&f).await?;
    assert!(f.join("m01", Funded::join_deposit()).await?.is_success());
    let s2 = storage_usage(&f).await?;
    let member_bytes = s2 - s1;
    println!(
        "GAS storage: member {member_bytes} bytes (first join {} bytes incl. the member index)",
        s1 - s0
    );
    let per_byte = 10_000_000_000_000_000_000u128; // 1e19 yocto per byte
    let member_cost = member_bytes as u128 * per_byte;
    assert!(
        member_cost <= Funded::join_deposit().as_yoctonear(),
        "a member costs {member_cost} yocto, the join deposit is {}",
        Funded::join_deposit().as_yoctonear()
    );
    for n in ["m00", "m01"] {
        let account = f.accounts[n].clone();
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    let s3 = storage_usage(&f).await?;
    let first = f.circle.register("m00", "m01", 1, CREDIT).await?;
    assert!(first.is_success());
    let s4 = storage_usage(&f).await?;
    println!("GAS storage: first voucher of a new pair {} bytes (pair, two participants, epoch counters)", s4 - s3);
    let _ = account_of("m00");
    Ok(())
}
