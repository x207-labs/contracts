//! CT-099 in near-sandbox: the logs of one receipt may not exceed 16,384 bytes in total (`HostError::TotalLogLengthExceeded`), so every call whose events grow with the
//! number of edges, members or legs must stay under it at the largest size that the contract accepts, with account names of the longest size (64 characters).
//! Principle: a proposal that `submit_clearing` accepted can always be committed. Run one test at a time (each starts a sandbox with up to 64 members):
//!   `cargo test --test ct099_commit_log_size <name> -- --nocapture --test-threads=1`
//! Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, failure_text, CREDIT};
use common::flow::edge;
use common::funded::Funded;
use serde_json::{json, Value};

/// Account names of 64 characters in all (the longest NEAR allows): `m<nn>` padded, then `.test.near`.
fn long(i: usize) -> String {
    let head = format!("m{i:02}");
    format!("{head}{}", "x".repeat(64 - ".test.near".len() - head.len()))
}

async fn members(f: &mut Funded, n: usize) -> anyhow::Result<()> {
    for i in 0..n {
        let name = long(i);
        let account = f.account(&name).await?;
        assert!(f.join(&name, Funded::join_deposit()).await?.is_success(), "join {i}");
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success(), "stake {i}");
    }
    assert_eq!(account_of(&long(0)).len(), 64);
    Ok(())
}

/// Every member owes the next `offsets` members one credit: a balanced graph of `n * offsets` edges that clears entirely.
async fn register_ring(f: &mut Funded, n: usize, offsets: usize) -> anyhow::Result<Vec<Value>> {
    let mut edges = vec![];
    for k in 1..=offsets {
        for i in 0..n {
            let (payer, payee) = (long(i), long((i + k) % n));
            let r = f.circle.register(&payer, &payee, 1, CREDIT).await?;
            assert!(r.is_success(), "register {i}->{}: {:?}", (i + k) % n, failure_text(&r));
            edges.push(edge(&payer, &payee, CREDIT));
        }
    }
    Ok(edges)
}

async fn commit_with_proposal(f: &mut Funded, edges: Vec<Value>) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
    let solver = f.circle.worker.dev_create_account().await?;
    f.circle.advance_to(f.circle.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(f.circle.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
    let submit = f.circle.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": edges})).await?;
    assert!(submit.is_success(), "the proposal is accepted by the verifier: {:?}", failure_text(&submit));
    f.circle.advance_to(f.circle.epoch_ts(1, "proposal_deadline").await?).await?;
    f.circle.call_keeper("commit_epoch", json!({"epoch": 1})).await
}

/// The total length of the logs of one transaction: NEAR refuses a receipt above 16,384 bytes; the contract keeps a margin of 10 percent.
fn log_bytes(r: &near_workspaces::result::ExecutionFinalResult) -> usize {
    r.logs().iter().map(|l| l.len()).sum()
}

const LOG_BUDGET: usize = 16 * 1024 / 10 * 9;

fn fees_params() -> Value {
    let mut params = Funded::params_short();
    params["fees"] = json!({"clearing_fee_bps": 10, "solver_fee_bps": 5});
    params["proposal_window"] = json!(600);
    params
}

// TC-CT-099-01: the largest proposal in edges (300, MAX_EDGES_PER_TX) on 30 members with names of 64 characters
#[tokio::test]
async fn a_proposal_of_300_edges_can_be_committed() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(fees_params())).await?;
    members(&mut f, 30).await?;
    let edges = register_ring(&mut f, 30, 10).await?;
    assert_eq!(edges.len(), 300);
    let commit = commit_with_proposal(&mut f, edges).await?;
    assert!(commit.is_success(), "commit_epoch of an accepted proposal of 300 edges: {:?}", failure_text(&commit));
    assert!(log_bytes(&commit) <= LOG_BUDGET, "commit_epoch logs {} bytes, the budget is {LOG_BUDGET}", log_bytes(&commit));
    println!("CT099 commit_epoch 300 edges, 30 members, names of 64 characters: {:.1} Tgas, logs {} bytes", commit.total_gas_burnt.as_gas() as f64 / 1e12, log_bytes(&commit));
    Ok(())
}

// TC-CT-099-02: the largest proposal in members: 64 members (MAX_MEMBERS) with names of 64 characters and 256 edges, so that edges and positions are both large
#[tokio::test]
async fn a_proposal_on_64_members_can_be_committed() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(fees_params())).await?;
    members(&mut f, 64).await?;
    let edges = register_ring(&mut f, 64, 4).await?;
    assert_eq!(edges.len(), 256);
    let commit = commit_with_proposal(&mut f, edges).await?;
    assert!(commit.is_success(), "commit_epoch on 64 members and 256 edges: {:?}", failure_text(&commit));
    assert!(log_bytes(&commit) <= LOG_BUDGET, "commit_epoch logs {} bytes, the budget is {LOG_BUDGET}", log_bytes(&commit));
    println!("CT099 commit_epoch 256 edges, 64 members, names of 64 characters: {:.1} Tgas, logs {} bytes", commit.total_gas_burnt.as_gas() as f64 / 1e12, log_bytes(&commit));
    Ok(())
}

// TC-CT-099-03: 63 debtors default at once (names of 64 characters): `finalize_epoch` must go through and claims must stay possible
#[tokio::test]
async fn finalize_with_63_defaults_goes_through() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(fees_params())).await?;
    members(&mut f, 64).await?;
    for i in 1..64 {
        let r = f.circle.register(&long(i), &long(0), 1, CREDIT).await?;
        assert!(r.is_success(), "register {i}->0: {:?}", failure_text(&r));
    }
    f.circle.advance_to(f.circle.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(f.circle.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
    f.circle.advance_to(f.circle.epoch_ts(1, "proposal_deadline").await?).await?;
    let commit = f.circle.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "commit_epoch with no proposal on 64 members: {:?}", failure_text(&commit));
    let fin = common::flow::finalize(&f.circle).await?;
    assert!(fin.is_success(), "finalize_epoch with 63 defaults: {:?}", failure_text(&fin));
    let pool = f.circle.view("get_epoch", json!({"epoch": 1})).await?;
    assert_eq!(pool["phase"], "finalized");
    println!("CT099 finalize_epoch 63 defaults, 64 members, names of 64 characters: logs {} bytes", log_bytes(&fin));
    assert!(log_bytes(&fin) <= LOG_BUDGET, "finalize_epoch logs {} bytes, the budget is {LOG_BUDGET}", log_bytes(&fin));
    Ok(())
}

// TC-CT-099-04: a bundle of 64 legs (the most) among 64 members with names of 64 characters
#[tokio::test]
async fn a_bundle_of_64_legs_with_long_names_can_be_committed() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    members(&mut f, 64).await?;
    let names: Vec<String> = (0..64).map(long).collect();
    let spec: Vec<(&str, &str, u128)> = (0..64).map(|i| (names[i].as_str(), names[(i + 1) % 64].as_str(), 1u128)).collect();
    let signers: Vec<&str> = names.iter().map(String::as_str).collect();
    let commit = f.circle.commit_bundle(1, &spec, &signers).await?;
    assert!(commit.is_success(), "commit_bundle of 64 legs: {:?}", failure_text(&commit));
    println!("CT099 commit_bundle 64 legs, names of 64 characters: logs {} bytes", log_bytes(&commit));
    assert!(log_bytes(&commit) <= LOG_BUDGET, "commit_bundle logs {} bytes, the budget is {LOG_BUDGET}", log_bytes(&commit));
    Ok(())
}

// TC-CT-099-05: the proposal is readable before the commit through `get_proposal`, the commit log carries its count and hash, and `list_obligations` has `cleared` afterwards
#[tokio::test]
async fn the_edges_are_read_from_the_views_and_match_the_hash_of_the_log() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(fees_params())).await?;
    members(&mut f, 12).await?;
    let edges = register_ring(&mut f, 12, 5).await?;
    let solver = f.circle.worker.dev_create_account().await?;
    f.circle.advance_to(f.circle.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(f.circle.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
    let submit = f.circle.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": edges})).await?;
    assert!(submit.is_success(), "{:?}", failure_text(&submit));
    // before the commit: a page of the stored edges
    let page = f.circle.view("get_proposal", json!({"epoch": 1, "from_index": 40, "limit": 100})).await?;
    assert_eq!((page["edge_count"].as_u64(), page["edges"].as_array().map(Vec::len), page["committed"].as_bool()), (Some(60), Some(20), Some(false)));
    let all = f.circle.view("get_proposal", json!({"epoch": 1, "from_index": 0, "limit": 300})).await?;
    f.circle.advance_to(f.circle.epoch_ts(1, "proposal_deadline").await?).await?;
    let commit = f.circle.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    let applied = common::circle::events(&commit).into_iter().find(|(n, _)| n == "clearing_applied").expect("clearing_applied").1;
    assert_eq!(applied["edge_count"], 60);
    assert!(applied.get("edges").is_none());
    // the hash of the log is the hash of the edges that the view gave, in the canonical form of `scripts/clearing_hashes.py`
    let mut sorted: Vec<(String, String, String)> = all["edges"].as_array().unwrap().iter().map(|e| (e["debtor"].as_str().unwrap().to_string(), e["creditor"].as_str().unwrap().to_string(), e["cleared"].as_str().unwrap().to_string())).collect();
    sorted.sort();
    let canonical = format!("[{}]", sorted.iter().map(|(d, c, v)| format!(r#"{{"debtor":"{d}","creditor":"{c}","cleared":"{v}"}}"#)).collect::<Vec<_>>().join(","));
    use sha2::{Digest, Sha256};
    let hash: String = Sha256::digest(canonical.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(applied["edges_hash"], hash);
    // B8-F3: the view gave that hash on every page before the commit, and keeps it after
    assert_eq!(all["proposal_hash"], applied["edges_hash"]);
    assert_eq!(page["proposal_hash"], all["proposal_hash"]);
    // after the commit: the proposal says committed and `list_obligations` has the cleared amounts
    let after = f.circle.view("get_proposal", json!({"epoch": 1, "from_index": 0, "limit": 300})).await?;
    assert_eq!((after["committed"].as_bool(), after["edges"].as_array().map(Vec::len)), (Some(true), Some(0)));
    // B8-F2: the count of the committed edges is still there
    assert_eq!(after["edge_count"], 60);
    assert_eq!(after["proposal_hash"], applied["edges_hash"]);
    let obligations = f.circle.view("list_obligations", json!({"epoch": 1, "from_index": 0, "limit": 100})).await?;
    assert_eq!(obligations.as_array().unwrap().iter().filter(|o| o["cleared"] != "0").count(), 60);
    let committed = common::circle::events(&commit).into_iter().find(|(n, _)| n == "epoch_committed").expect("epoch_committed").1;
    assert_eq!((committed["position_count"].as_u64(), committed["positions"].as_array().map(Vec::len)), (Some(12), Some(12)));
    assert_eq!(committed["positions_hash"].as_str().map(str::len), Some(64));
    Ok(())
}
