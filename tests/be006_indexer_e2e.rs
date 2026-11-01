//! BE-006 / BE-020 / BE-039 end to end: the backend indexer (`../backend/dist`, built with `npm run build`)
//! reads a real near-sandbox chain through the JSON-RPC block source, and its read API must agree with
//! the contract's own views. Flow: three staked members, vouchers, clearing, commit, the debtor defaults,
//! finalize, the creditor claims, one member requests exit and withdraws.
//! Skipped when the backend is not built next to this repo. Needs `cargo near build non-reproducible-wasm`.

mod common;

use std::path::Path;

use common::circle::{account_of, failure_text, CREDIT};
use common::flow::{finalize, request_exit, s1_committed_exit, withdraw_stake};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use serde_json::{json, Value};

const BACKEND: &str = "../backend/dist/index.js";

#[tokio::test]
async fn indexer_agrees_with_the_contract() -> anyhow::Result<()> {
    if !Path::new(BACKEND).exists() {
        eprintln!("skipped: {BACKEND} not found (build the backend with `npm run build`)");
        return Ok(());
    }
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    // short jumps: the sandbox forgets old outcomes after a few epochs of height, so the indexer follows live
    f.circle.max_jump = 25;
    let start_block = f.circle.worker.view_block().await?.height();
    let rpc = format!("http://{}", f.circle.worker.rpc_addr().trim_start_matches("http://"));
    let accounts: Vec<String> = ["scraper", "gpu", "translator"].iter().map(|n| account_of(n)).collect();
    let mut indexer = tokio::process::Command::new("node")
        .arg("tests/e2e/index_chain.mjs")
        .args([rpc, f.circle.contract.id().to_string(), start_block.to_string(), "agents-main".into()])
        .args(&accounts)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // translator asks to leave in epoch 1; scraper never pays
    let f = s1_committed_exit(f, &["translator"]).await?;
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
    let exit = withdraw_stake(&f, "translator").await?;
    assert!(exit.is_success(), "{:?}", failure_text(&exit));
    f.circle.worker.fast_forward(30).await?;

    // let the indexer reach the head, then ask it to finish
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    use tokio::io::AsyncWriteExt;
    // a halted indexer has already left: the broken pipe is then not the interesting error
    let _ = indexer.stdin.take().expect("stdin").write_all(b"stop\n").await;
    let output = indexer.wait_with_output().await?;
    assert!(output.status.success(), "indexer failed: {}\n{}", String::from_utf8_lossy(&output.stderr), String::from_utf8_lossy(&output.stdout));
    let out: Value = serde_json::from_slice(&output.stdout)?;

    let list = |v: &Value| v["events"].as_array().unwrap().iter().map(|e| format!("{}:{}", e["event"], e["handled"])).collect::<Vec<_>>().join(" ");
    assert!(out["haltError"].is_null(), "the indexer halted: {} after {} blocks; debug {}; events: {}", out["haltError"], out["processed"], out["debug"], list(&out));
    // every event the contract wrote was understood: nothing pending, nothing unknown
    let events = out["events"].as_array().unwrap();
    assert!(events.len() >= 15, "{} events: {}; debug {}", events.len(), list(&out), out["debug"]);
    for e in events {
        assert_eq!(e["handled"], 1, "event {} was not handled", e["event"]);
    }
    let names: Vec<&str> = events.iter().map(|e| e["event"].as_str().unwrap()).collect();
    for expected in ["member_joined", "stake_added", "voucher_registered", "epoch_cutoff", "clearing_submitted", "clearing_applied", "epoch_committed", "member_defaulted", "limits_updated", "epoch_finalized", "residual_claimed", "exit_requested", "stake_withdrawn"] {
        assert!(names.contains(&expected), "{expected} missing from {names:?}");
    }

    // TC-BE-020-15: the 207 report equals net_position of the contract
    let report = &out["report"]["1"];
    assert_eq!(report["status"], 207);
    let body = &report["body"];
    assert_eq!((body["phase"].as_str(), body["committed"].as_bool(), body["finalized"].as_bool()), (Some("finalized"), Some(true), Some(true)));
    assert_eq!((body["totalObligation"].as_str(), body["cleared"].as_str()), (Some("110000000"), Some("75000000")));
    for name in ["scraper", "gpu", "translator"] {
        let id = account_of(name);
        let on_chain = f.circle.view("net_position", json!({"epoch": 1, "account_id": id})).await?;
        let entry = body["members"].as_array().unwrap().iter().find(|m| m["account"] == id.as_str()).unwrap_or_else(|| panic!("{name} missing from the report"));
        let expected_net = if on_chain.is_null() { "0".to_string() } else {
            let net = on_chain["net"].as_str().unwrap();
            if net.starts_with('-') || net == "0" { net.to_string() } else { format!("+{net}") }
        };
        assert_eq!(entry["net"], expected_net.as_str(), "{name}: net");
        let expected_status = match on_chain["status"].as_str() {
            None => 200,
            Some("paid") | Some("claimed") => 200,
            Some("due") => 402,
            Some("claimable") => 202,
            Some("defaulted") => 409,
            Some(other) => panic!("status {other}"),
        };
        assert_eq!(entry["status"], expected_status, "{name}: status");
    }
    let scraper = body["members"].as_array().unwrap().iter().find(|m| m["account"] == account_of("scraper").as_str()).unwrap();
    assert_eq!((scraper["status"].as_u64(), scraper["stakeSlashed"].as_str(), scraper["amount"].as_str()), (Some(409), Some("35000000"), Some("35000000")));
    let gpu = body["members"].as_array().unwrap().iter().find(|m| m["account"] == account_of("gpu").as_str()).unwrap();
    assert_eq!((gpu["status"].as_u64(), gpu["settlement"].as_str()), (Some(200), Some("claimed")));

    // TC-BE-039-12: members in D9 equal the contract view
    for name in ["scraper", "gpu", "translator"] {
        let on_chain = f.member(name).await?;
        let indexed = &out["members"][account_of(name)]["body"];
        for (chain, api) in [("stake", "stake"), ("limit", "limit"), ("status", "status")] {
            assert_eq!(indexed[api], on_chain[chain], "{name}: {api}");
        }
    }
    let translator = &out["members"][account_of("translator")]["body"];
    assert_eq!((translator["status"].as_str(), translator["exitRequestedEpoch"].as_u64()), (Some("exited"), Some(1)));
    let _ = (request_exit, CREDIT);
    Ok(())
}
