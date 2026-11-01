//! CT-035 in near-sandbox: `claim` and `on_claim_resolved` on a real circle with a NEP-141 mock as
//! USDT and a `mock_intents` receiver. Flow: join, stake, vouchers, clearing, commit, settle or
//! default, finalize, claim. Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::flow::{finalize, s1_committed, s1_committed_on};
use common::funded::Funded;
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::Account;
use serde_json::{json, Value};

const GPU_START: u128 = 900 * CREDIT; // 1000 minted, 100 staked

async fn claim(f: &Funded, who: &Account, destination: &str) -> anyhow::Result<ExecutionFinalResult> {
    Ok(who
        .call(f.circle.contract.id(), "claim")
        .args_json(json!({"epoch": 1, "destination": destination}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?)
}

async fn position(f: &Funded, name: &str) -> anyhow::Result<Value> {
    f.circle.view("net_position", json!({"epoch": 1, "account_id": account_of(name)})).await
}

async fn pool_state(f: &Funded) -> anyhow::Result<(String, String)> {
    let pool = f.circle.view("get_pool", json!({})).await?;
    Ok((pool["epoch_pools"].as_str().unwrap().into(), pool["in_flight"].as_str().unwrap().into()))
}

/// S1 where the debtor pays: scraper settles 35 inside the payment window, then `finalize_epoch`.
async fn s1_fin_paid(f: Funded) -> anyhow::Result<Funded> {
    let f = s1_committed_on(f).await?;
    let scraper = f.accounts["scraper"].clone();
    let pay = f.transfer_call(&f.usdt, &scraper, 35 * CREDIT, r#"{"action":"settle","epoch":1}"#).await?;
    assert!(pay.is_success(), "{:?}", failure_text(&pay));
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    Ok(f)
}

// TC-CT-035-01, TC-CT-035-13, TC-CT-035-14, TC-CT-035-17
#[tokio::test]
async fn claim_to_near_pays_once_and_the_callback_is_private() -> anyhow::Result<()> {
    let f = s1_fin_paid(Funded::deploy(Some(Funded::params_short())).await?).await?;
    let gpu = f.accounts["gpu"].clone();
    assert_eq!(f.balance(&f.usdt, gpu.id()).await?, GPU_START);

    let outsider = f.circle.worker.dev_create_account().await?;
    let direct = f
        .circle
        .call_as(&outsider, "on_claim_resolved", json!({"epoch": 1, "account_id": gpu.id(), "amount": "35000000", "destination": "near"}))
        .await?;
    assert!(failure_text(&direct).expect("private callback").contains("Method on_claim_resolved is private"));

    // two claims in the same block: one wins, one is refused (I-14)
    let (a, b) = tokio::join!(claim(&f, &gpu, "near"), claim(&f, &gpu, "near"));
    let (a, b) = (a?, b?);
    let (won, lost) = if a.is_success() { (a, b) } else { (b, a) };
    assert!(won.is_success(), "{:?}", failure_text(&won));
    assert!(failure_text(&lost).expect("second claim").contains("ERR_NOT_CLAIMABLE"));
    assert!(won.failures().is_empty(), "the callback receipt must succeed: {:?}", won.failures());
    assert_eq!(won.clone().json::<String>()?, "35000000", "on_claim_resolved returns the delivered amount");

    let log = events(&won);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].0, "residual_claimed");
    assert_eq!(log[0].1, json!({"epoch": 1, "account_id": account_of("gpu"), "amount": "35000000", "destination": "near", "status": "claimed"}));
    assert_eq!(f.balance(&f.usdt, gpu.id()).await?, GPU_START + 35 * CREDIT);
    assert_eq!(position(&f, "gpu").await?["status"], "claimed");
    assert_eq!(pool_state(&f).await?, ("0".to_string(), "0".to_string()));
    f.assert_d6().await
}

// TC-CT-035-02: the debtor defaulted, the creditor is still paid in full
#[tokio::test]
async fn creditor_is_paid_in_full_when_the_debtor_defaults() -> anyhow::Result<()> {
    let f = s1_committed().await?;
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    assert_eq!(position(&f, "scraper").await?["status"], "defaulted");

    let gpu = f.accounts["gpu"].clone();
    let result = claim(&f, &gpu, "near").await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    assert_eq!(f.balance(&f.usdt, gpu.id()).await?, GPU_START + 35 * CREDIT);
    assert_eq!(pool_state(&f).await?, ("0".to_string(), "0".to_string()));
    f.assert_d6().await
}

// TC-CT-035-03: Intents receives the amount with msg = the creditor account
#[tokio::test]
async fn claim_to_intents_sends_the_creditor_as_msg() -> anyhow::Result<()> {
    let f = s1_fin_paid(Funded::deploy_with_intents(Some(Funded::params_short())).await?).await?;
    let gpu = f.accounts["gpu"].clone();
    let result = claim(&f, &gpu, "intents").await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    assert_eq!(result.clone().json::<String>()?, "35000000");

    let intents = f.intents.as_ref().expect("mock intents");
    let msg: Option<String> = intents.view("last_msg").await?.json()?;
    assert_eq!(msg.as_deref(), Some(account_of("gpu").as_str()));
    let received: String = intents.view("received").await?.json()?;
    assert_eq!(received, "35000000");
    assert_eq!(f.balance(&f.usdt, intents.id()).await?, 35 * CREDIT);
    assert_eq!(f.balance(&f.usdt, gpu.id()).await?, GPU_START, "the funds went to Intents, not to the wallet");

    let log = events(&result);
    assert_eq!((log[0].0.as_str(), log[0].1["destination"].as_str(), log[0].1["status"].as_str()), ("residual_claimed", Some("intents"), Some("claimed")));
    f.assert_d6().await
}

// TC-CT-035-10: the transfer fails, the claim comes back, a second try after storage_deposit works
#[tokio::test]
async fn failed_transfer_restores_the_claim() -> anyhow::Result<()> {
    let f = s1_fin_paid(Funded::deploy(Some(Funded::params_short())).await?).await?;
    let gpu = f.accounts["gpu"].clone();
    gpu.call(f.usdt.id(), "storage_unregister").args_json(json!({"force": true})).deposit(NearToken::from_yoctonear(1)).transact().await?.into_result()?;

    let result = claim(&f, &gpu, "near").await?;
    assert!(result.failures().is_empty() || result.is_success(), "callback must not fail the claim: {:?}", result.failures());
    assert_eq!(result.clone().json::<String>()?, "0");
    let log = events(&result);
    assert_eq!(log.len(), 1);
    assert_eq!((log[0].0.as_str(), log[0].1["amount"].as_str(), log[0].1["status"].as_str()), ("residual_claim_failed", Some("35000000"), Some("claimable")));
    let p = position(&f, "gpu").await?;
    assert_eq!((p["status"].as_str(), p["amount"].as_str()), (Some("claimable"), Some("35000000")));
    assert_eq!(pool_state(&f).await?, ("35000000".to_string(), "0".to_string()));
    f.assert_d6().await?;

    gpu.call(f.usdt.id(), "storage_deposit").args_json(json!({"account_id": gpu.id()})).deposit(NearToken::from_millinear(10)).transact().await?.into_result()?;
    let again = claim(&f, &gpu, "near").await?;
    assert!(again.is_success(), "{:?}", failure_text(&again));
    assert_eq!(f.balance(&f.usdt, gpu.id()).await?, GPU_START + 35 * CREDIT);
    f.assert_d6().await
}

// TC-CT-035-11: Intents keeps 25 and returns 10
#[tokio::test]
async fn intents_returns_part_of_the_amount() -> anyhow::Result<()> {
    let f = s1_fin_paid(Funded::deploy_with_intents(Some(Funded::params_short())).await?).await?;
    let intents = f.intents.clone().unwrap();
    intents.call("set_mode").args_json(json!({"mode": "refund", "refund": (10 * CREDIT).to_string()})).transact().await?.into_result()?;

    let gpu = f.accounts["gpu"].clone();
    let result = claim(&f, &gpu, "intents").await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    assert_eq!(result.clone().json::<String>()?, "25000000");
    let log = events(&result);
    assert_eq!(log.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["residual_claimed", "residual_claim_failed"]);
    assert_eq!((log[0].1["amount"].as_str(), log[1].1["amount"].as_str()), (Some("25000000"), Some("10000000")));
    let p = position(&f, "gpu").await?;
    assert_eq!((p["status"].as_str(), p["amount"].as_str()), (Some("claimable"), Some("10000000")));
    assert_eq!(pool_state(&f).await?, ("10000000".to_string(), "0".to_string()));
    assert_eq!(f.balance(&f.usdt, intents.id()).await?, 25 * CREDIT);
    f.assert_d6().await
}

// TC-CT-035-12, TC-CT-035-14: Intents panics, or hands everything back; both leave the claim intact
#[tokio::test]
async fn intents_failure_or_full_refund_keeps_the_claim() -> anyhow::Result<()> {
    let f = s1_fin_paid(Funded::deploy_with_intents(Some(Funded::params_short())).await?).await?;
    let intents = f.intents.clone().unwrap();
    let gpu = f.accounts["gpu"].clone();

    intents.call("set_mode").args_json(json!({"mode": "panic"})).transact().await?.into_result()?;
    let result = claim(&f, &gpu, "intents").await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    assert_eq!(result.clone().json::<String>()?, "0");
    assert_eq!(events(&result)[0].0, "residual_claim_failed");
    let p = position(&f, "gpu").await?;
    assert_eq!((p["status"].as_str(), p["amount"].as_str()), (Some("claimable"), Some("35000000")));
    assert_eq!(pool_state(&f).await?, ("35000000".to_string(), "0".to_string()));
    f.assert_d6().await?;

    intents.call("set_mode").args_json(json!({"mode": "refund", "refund": (35 * CREDIT).to_string()})).transact().await?.into_result()?;
    let result = claim(&f, &gpu, "intents").await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    assert_eq!(events(&result)[0].0, "residual_claim_failed");
    let p = position(&f, "gpu").await?;
    assert_eq!((p["status"].as_str(), p["amount"].as_str()), (Some("claimable"), Some("35000000")));
    assert_eq!(pool_state(&f).await?, ("35000000".to_string(), "0".to_string()));
    assert_eq!(f.balance(&f.usdt, gpu.id()).await?, GPU_START);
    f.assert_d6().await
}
