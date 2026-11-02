//! BE-033 on near-sandbox, without mainnet: after clearing only members with a non-zero net position move USDT,
//! and the number of those members equals `residualTransfers` of the metrics endpoint (TC-BE-033-01..03 adapted:
//! the S1 circle of three members at epoch 1 stands in for FX-LUNAS and FX-E42; the cross-chain withdrawal of the
//! backlog item needs mainnet and is not part of this test).
//! Skipped when the backend is not built (`npm run build` there). Needs `cargo near build non-reproducible-wasm`.

mod common;

use std::path::Path;

use common::circle::{account_of, failure_text, CREDIT};
use common::flow::{finalize, s1_committed_on};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::AccountId;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

const BACKEND: &str = "../backend/dist/index.js";

fn spawn_indexer(rpc: &str, contract: &str, start: u64, db: &Path, accounts: &[String]) -> anyhow::Result<tokio::process::Child> {
    Ok(tokio::process::Command::new("node")
        .arg("tests/e2e/scenario_chain.mjs")
        .args([rpc.to_string(), contract.to_string(), start.to_string(), "agents-main".into()])
        .args(accounts)
        .env("X207_DB", db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?)
}

async fn stop(mut child: tokio::process::Child) -> anyhow::Result<Value> {
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    // a halted indexer has already left: the broken pipe is then not the interesting error
    let _ = child.stdin.take().expect("stdin").write_all(b"stop\n").await;
    let output = child.wait_with_output().await?;
    assert!(output.status.success(), "indexer failed: {}\n{}", String::from_utf8_lossy(&output.stderr), String::from_utf8_lossy(&output.stdout));
    let out: Value = serde_json::from_slice(&output.stdout)?;
    assert!(out["haltError"].is_null(), "the indexer halted: {}", out["haltError"]);
    Ok(out)
}

fn items<'a>(v: &'a Value) -> &'a Vec<Value> {
    v["body"]["items"].as_array().unwrap_or_else(|| panic!("no items in {v}"))
}


async fn usdt(f: &Funded, name: &str) -> anyhow::Result<u128> {
    let id: AccountId = account_of(name).parse()?;
    f.balance(&f.usdt, &id).await
}

/// S1 under an indexer; `pays` decides whether scraper settles its 35 before `finalize_epoch`.
async fn run(pays: bool) -> anyhow::Result<(Value, Vec<&'static str>)> {
    if !Path::new(BACKEND).exists() {
        eprintln!("skipped: {BACKEND} not found (build the backend with `npm run build`)");
        return Ok((Value::Null, vec![]));
    }
    let names = ["scraper", "gpu", "translator"];
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    f.circle.max_jump = 25;
    let start_block = f.circle.worker.view_block().await?.height();
    let rpc = format!("http://{}", f.circle.worker.rpc_addr().trim_start_matches("http://"));
    let accounts: Vec<String> = names.iter().map(|n| account_of(n)).collect();
    let db_dir = std::env::temp_dir().join(format!("x207-be033-{}-{pays}", std::process::id()));
    std::fs::create_dir_all(&db_dir)?;
    let indexer = spawn_indexer(&rpc, f.circle.contract.id().as_str(), start_block, &db_dir.join("d9.sqlite"), &accounts)?;

    let mut f = s1_committed_on(f).await?;
    let mut before = Vec::new();
    for n in names {
        before.push(usdt(&f, n).await?);
    }
    if pays {
        let scraper = f.accounts["scraper"].clone();
        let pay = f.transfer_call(&f.usdt, &scraper, 35 * CREDIT, r#"{"action":"settle","epoch":1}"#).await?;
        assert!(pay.is_success(), "{:?}", failure_text(&pay));
    }
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    let claim = f.accounts["gpu"]
        .call(f.circle.contract.id(), "claim")
        .args_json(json!({"epoch": 1, "destination": "near"}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?;
    assert!(claim.is_success(), "{:?}", failure_text(&claim));
    f.circle.worker.fast_forward(30).await?;
    let mut moved = Vec::new();
    for (i, n) in names.iter().enumerate() {
        if usdt(&f, n).await? != before[i] {
            moved.push(*n);
        }
    }
    let out = stop(indexer).await?;
    let _ = std::fs::remove_dir_all(&db_dir);
    Ok((out, moved))
}

// TC-BE-033-01, TC-BE-033-02: everyone paid; the debtor and the creditor move USDT, the member at zero does not
#[tokio::test]
async fn only_nonzero_net_positions_move_usdt() -> anyhow::Result<()> {
    let (out, moved) = run(true).await?;
    if out.is_null() {
        return Ok(());
    }
    assert_eq!(moved, vec!["scraper", "gpu"], "translator has a zero net position and moves nothing");
    assert_eq!(out["metrics"]["body"]["residualTransfers"], moved.len(), "{}", out["metrics"]);
    let rows = out["report"]["1"]["body"]["members"].as_array().unwrap();
    for m in rows {
        assert_eq!(m["status"], 200, "{m}");
    }
    Ok(())
}

// TC-BE-033-03: the debtor defaults; only the creditor moves USDT, paid out of the debtor's stake
#[tokio::test]
async fn default_moves_only_the_claim() -> anyhow::Result<()> {
    let (out, moved) = run(false).await?;
    if out.is_null() {
        return Ok(());
    }
    assert_eq!(moved, vec!["gpu"]);
    let rows = out["report"]["1"]["body"]["members"].as_array().unwrap();
    let scraper = rows.iter().find(|m| m["account"] == account_of("scraper").as_str()).unwrap();
    assert_eq!(scraper["status"], 409);
    let gpu = rows.iter().find(|m| m["account"] == account_of("gpu").as_str()).unwrap();
    assert_eq!((gpu["status"].as_u64(), gpu["settlement"].as_str()), (Some(200), Some("claimed")));
    Ok(())
}
