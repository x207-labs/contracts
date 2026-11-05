//! CT-004: gas of clearing verification (`submit_clearing`) and application (`commit_epoch`)
//! against the number of edges, in near-sandbox. Probe: `probes/ct004_clearing_gas`.
//! Build: `cargo near build non-reproducible-wasm --manifest-path probes/ct004_clearing_gas/Cargo.toml`.
//! Table: `cargo test --test ct004_clearing_gas -- --nocapture`.

use near_workspaces::types::Gas;
use near_workspaces::Contract;
use serde_json::{json, Value};

mod common;
use x207_circle::types::MAX_EDGES_PER_TX;

const MAX_MEMBERS: usize = 64;
const TX_GAS_LIMIT_TGAS: f64 = 1000.0;

/// Longest valid account id: 64 characters.
fn account(index: usize) -> String {
    format!("{index:03}{}", "a".repeat(61))
}

/// `members` accounts, offsets `1..` of a ring: edge `i -> (i + k) % members` for every `i`. Each
/// offset is a permutation, so every member has equal in and out amounts (balanced). `n` edges are
/// taken offset by offset; a partial last offset is closed by dropping to whole offsets.
fn graph(members: usize, n: usize) -> Vec<Value> {
    assert!(n.is_multiple_of(members) && members > 1);
    let mut edges = Vec::new();
    for k in 1..=n / members {
        assert!(k < members, "too many edges for {members} members");
        for i in 0..members {
            edges.push(json!({"debtor": account(i), "creditor": account((i + k) % members), "cleared": "1000000"}));
        }
    }
    edges
}

async fn fresh() -> anyhow::Result<Contract> {
    let worker = near_workspaces::sandbox().await?;
    let wasm = std::fs::read(common::probe_wasm("ct004_clearing_gas"))?;
    let contract = worker.dev_deploy(&wasm).await?;
    contract.call("new").transact().await?.into_result()?;
    Ok(contract)
}

fn tgas(burnt: near_workspaces::result::ExecutionFinalResult) -> f64 {
    burnt.total_gas_burnt.as_gas() as f64 / 1e12
}

async fn seed(contract: &Contract, edges: &[Value]) -> anyhow::Result<()> {
    for chunk in edges.chunks(100) {
        contract
            .call("seed")
            .args_json(json!({"edges": chunk}))
            .gas(Gas::from_tgas(300))
            .transact()
            .await?
            .into_result()?;
    }
    Ok(())
}

/// Returns `(verify_tgas, apply_tgas)` or `None` when the call ran out of gas at `attach` Tgas.
async fn measure(members: usize, n: usize, attach: u64) -> anyhow::Result<(f64, Option<f64>)> {
    let contract = fresh().await?;
    let edges = graph(members, n);
    seed(&contract, &edges).await?;
    let verify = contract
        .call("probe_verify")
        .args_json(json!({"edges": edges}))
        .gas(Gas::from_tgas(attach))
        .transact()
        .await?;
    if !verify.is_success() {
        return Ok((f64::INFINITY, None));
    }
    let verify_gas = tgas(verify);
    let apply = contract
        .call("probe_apply")
        .gas(Gas::from_tgas(attach))
        .transact()
        .await?;
    Ok((verify_gas, apply.is_success().then(|| tgas(apply))))
}

// TC-CT-004-01, TC-CT-004-02, TC-CT-004-03
#[tokio::test]
async fn gas_versus_edge_count() -> anyhow::Result<()> {
    // Dense: edges spread over at most MAX_MEMBERS accounts. Sparse: one ring, members = edges.
    for (shape, members, n) in [
        ("ring", 3, 3),
        ("ring", 6, 6),
        ("ring", 12, 12),
        ("ring", 30, 30),
        ("ring", 60, 60),
        ("ring", 120, 120),
        ("ring", 300, 300),
        ("ring", 600, 600),
        ("dense", 30, 30),
        ("dense", 30, 60),
        ("dense", 60, 120),
        ("dense", 60, 300),
        ("dense", 60, 600),
    ] {
        let (verify, apply) = measure(members, n, 1000).await?;
        println!("RESULT ct004 shape={shape} members={members} edges={n} verify_tgas={verify:.2} apply_tgas={}", apply.map_or("fail".into(), |g| format!("{g:.2}")));
    }
    Ok(())
}

// TC-CT-004-04
#[tokio::test]
async fn max_edges_per_tx_passes_with_margin() -> anyhow::Result<()> {
    let max_edges = MAX_EDGES_PER_TX as usize;
    assert!(max_edges.is_multiple_of(MAX_MEMBERS.min(60)));
    for (members, n) in [(60, max_edges), (max_edges, max_edges)] {
        let (verify, apply) = measure(members, n, 1000).await?;
        let apply = apply.expect("apply succeeds at MAX_EDGES_PER_TX");
        println!("RESULT ct004 max members={members} edges={n} verify_tgas={verify:.2} apply_tgas={apply:.2}");
        let worst = verify.max(apply);
        assert!(
            worst * 3.0 <= TX_GAS_LIMIT_TGAS,
            "need 3x margin below 1 PGas, worst = {worst:.1} Tgas"
        );
    }
    Ok(())
}

// Finding: the 16 KiB limit applies to the total log length of one call, not to each log.
#[tokio::test]
async fn log_limit_is_total_per_call() -> anyhow::Result<()> {
    let contract = fresh().await?;
    let ok = contract
        .call("probe_logs")
        .args_json(json!({"count": 1, "size": 16_000}))
        .transact()
        .await?;
    assert!(ok.is_success());
    let two = contract
        .call("probe_logs")
        .args_json(json!({"count": 2, "size": 8_200}))
        .transact()
        .await?;
    assert!(
        two.is_failure(),
        "two 8.2 KB logs (16.4 KB total) must exceed the limit"
    );
    println!("RESULT ct004 log_limit total_per_call=true 1x16000=ok 2x8200=fail");
    Ok(())
}
