//! BE-022 (TC-BE-022-01..08): the demo scenario on near-sandbox with six staked members; the backend
//! indexer (`../backend/dist`) follows the chain over JSON-RPC, is stopped after `commit_epoch` and started
//! again on the same database file, and the read API must agree with the contract in every flow F1-F4, F6, F7.
//! Skipped when the backend is not built (`npm run build` there). Needs `cargo near build non-reproducible-wasm`.
//! Not covered: a claim that fails (TC-BE-022-04, third part).

mod common;

use std::path::Path;

use common::circle::{account_of, failure_text, CREDIT};
use common::flow::{edge, finalize};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
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

#[tokio::test]
async fn demo_scenario_is_indexed_across_a_restart() -> anyhow::Result<()> {
    if !Path::new(BACKEND).exists() {
        eprintln!("skipped: {BACKEND} not found (build the backend with `npm run build`)");
        return Ok(());
    }
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    // short jumps: the sandbox forgets old outcomes after a few epochs of height, so the indexer follows live
    f.circle.max_jump = 25;
    let start_block = f.circle.worker.view_block().await?.height();
    let rpc = format!("http://{}", f.circle.worker.rpc_addr().trim_start_matches("http://"));
    let names = ["scraper", "gpu", "translator", "fx1", "fx2", "fx3"];
    let accounts: Vec<String> = names.iter().map(|n| account_of(n)).collect();
    let db_dir = std::env::temp_dir().join(format!("x207-be022-{}", std::process::id()));
    std::fs::create_dir_all(&db_dir)?;
    let db = db_dir.join("d9.sqlite");
    let contract = f.circle.contract.id().to_string();
    let first = spawn_indexer(&rpc, &contract, start_block, &db, &accounts)?;

    // F1: six members join and stake
    for name in names {
        let account = f.account(name).await?;
        let join = f.join(name, Funded::join_deposit()).await?;
        assert!(join.is_success(), "join {name}: {:?}", failure_text(&join));
        let stake = f.stake(&account, 100 * CREDIT).await?;
        assert!(stake.is_success(), "stake {name}: {:?}", failure_text(&stake));
    }
    // F2: vouchers, the S1 cycle and one plain debt fx1 -> fx2
    for (payer, payee, amount) in [("scraper", "gpu", 60), ("gpu", "translator", 25), ("translator", "scraper", 25), ("fx1", "fx2", 20)] {
        let result = f.circle.register(payer, payee, 1, amount * CREDIT).await?;
        assert!(result.is_success(), "register {payer}->{payee}: {:?}", failure_text(&result));
    }
    // F7: the owner changes a parameter, effective from the next epoch
    let dao = f.account("dao").await?;
    let mut params = Funded::params_short();
    params["limit_decay"] = json!(100);
    let set = f.circle.call_as(&dao, "set_params", json!({"params": params})).await?;
    assert!(set.is_success(), "set_params: {:?}", failure_text(&set));

    // F3: clearing
    let c = &f.circle;
    let solver = c.worker.dev_create_account().await?;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    let close = c.call_keeper("close_epoch", json!({"epoch": 1})).await?;
    assert!(close.is_success(), "{:?}", failure_text(&close));
    let edges = json!([edge("scraper", "gpu", 25 * CREDIT), edge("gpu", "translator", 25 * CREDIT), edge("translator", "scraper", 25 * CREDIT)]);
    let submit = c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": edges})).await?;
    assert!(submit.is_success(), "{:?}", failure_text(&submit));
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?).await?;
    let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));

    // TC-BE-022-07: stop the indexer after commit_epoch and start it again on the same file
    c.worker.fast_forward(5).await?;
    let before_restart = stop(first).await?;
    let blocks_before = before_restart["processed"].as_u64().unwrap();
    let second = spawn_indexer(&rpc, &contract, start_block, &db, &accounts)?;

    // F4: fx1 pays, scraper does not; finalize; gpu claims
    let fx1 = f.accounts["fx1"].clone();
    let pay = f.transfer_call(&f.usdt, &fx1, 20 * CREDIT, r#"{"action":"settle","epoch":1}"#).await?;
    assert!(pay.is_success(), "{:?}", failure_text(&pay));
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
    let out = stop(second).await?;
    assert!(blocks_before > 0, "the first run read no block");

    // TC-BE-022-08: every P0 event is in events_raw, handled
    let names_seen: Vec<&str> = out["eventNames"].as_array().unwrap().iter().map(|e| e.as_str().unwrap()).collect();
    for expected in ["member_joined", "stake_added", "voucher_registered", "epoch_cutoff", "clearing_submitted", "clearing_applied", "epoch_committed", "residual_paid", "member_defaulted", "limits_updated", "epoch_finalized", "residual_claimed", "params_updated"] {
        assert!(names_seen.contains(&expected), "{expected} missing from {names_seen:?}");
    }
    for e in out["events"].as_array().unwrap() {
        assert_eq!(e["handled"], 1, "event {} was not handled", e["event"]);
    }
    // TC-BE-022-07: no duplicate rows after the restart
    assert_eq!(out["duplicates"], 0, "events_raw holds a block/log pair twice");

    // TC-BE-022-01
    assert_eq!(out["counts"]["members"], 6);
    let list = items(&out["memberList"]);
    assert_eq!(list.len(), 6);
    for name in ["gpu", "translator", "fx1", "fx2", "fx3"] {
        let on_chain = f.member(name).await?;
        let indexed = &out["members"][account_of(name)]["body"];
        assert_eq!((indexed["status"].as_str(), indexed["stake"].as_str()), (on_chain["status"].as_str(), on_chain["stake"].as_str()), "{name}");
    }
    // TC-BE-022-02: obligations of the epoch, live stage
    let live = &out["live"]["body"];
    assert_eq!(live["totalObligation"], "130000000", "live obligations: {live}");
    // TC-BE-022-03: clearing is on record and the 207 report answers
    assert_eq!(out["report"]["1"]["status"], 207);
    assert_eq!(out["report"]["1"]["body"]["cleared"], "75000000");
    // TC-BE-022-04: settlements of the epoch: fx1 paid, gpu claimed, scraper defaulted
    let settlements = items(&out["settlements"]);
    let status_of = |name: &str| settlements.iter().find(|s| s["account"] == account_of(name).as_str()).map(|s| s["status"].clone());
    assert_eq!(status_of("fx1"), Some(json!("paid")), "{settlements:?}");
    assert_eq!(status_of("gpu"), Some(json!("claimed")), "{settlements:?}");
    // TC-BE-022-05: scraper never paid
    let scraper = &out["members"][account_of("scraper")]["body"];
    assert_eq!((scraper["status"].as_str(), scraper["limit"].as_str()), (Some("suspended"), Some("0")));
    let rows = out["report"]["1"]["body"]["members"].as_array().unwrap();
    let report = rows.iter().find(|m| m["account"] == account_of("scraper").as_str()).unwrap();
    assert_eq!(report["status"], 409);
    // TC-BE-022-06: set_params is in the history and the next epoch carries it
    let history = items(&out["paramsHistory"]);
    assert!(history.iter().any(|h| h["field"] == "limit_decay" && h["newValue"] == 100), "{history:?}");
    assert!(out["params"]["body"]["nextEpoch"].get("limit_decay").is_some() || out["params"]["body"]["active"]["limit_decay"] == 100, "{}", out["params"]);
    let _ = std::fs::remove_dir_all(&db_dir);
    Ok(())
}
