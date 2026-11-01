//! CT-039: the whole life of an epoch without any contract at `intents_id` (I-17): the circle only
//! talks to the USDT token. Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{failure_text, CREDIT};
use common::flow::edge;
use common::funded::Funded;
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::{Gas, NearToken};
use serde_json::json;

/// Accounts that executed a receipt of the call, in order.
fn executors(result: &ExecutionFinalResult) -> Vec<String> {
    result.receipt_outcomes().iter().map(|o| o.executor_id.to_string()).collect()
}

// TC-CT-039-01 .. TC-CT-039-05
#[tokio::test]
async fn full_epoch_without_an_intents_contract() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let circle = f.circle.contract.id().to_string();
    assert!(f.circle.worker.view_account(&"intents.test.near".parse()?).await.is_err(), "no contract or account at intents_id");

    for name in ["scraper", "gpu", "translator"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    for (payer, payee, amount) in [("scraper", "gpu", 60), ("gpu", "translator", 25), ("translator", "scraper", 25)] {
        assert!(f.circle.register(payer, payee, 1, amount * CREDIT).await?.is_success());
    }
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

    let scraper = f.accounts["scraper"].clone();
    let pay = f.transfer_call(&f.usdt, &scraper, 35 * CREDIT, r#"{"action":"settle","epoch":1}"#).await?;
    assert!(pay.is_success(), "{:?}", failure_text(&pay));

    let c = &f.circle;
    c.advance_to(c.epoch_ts(1, "payment_deadline").await?).await?;
    let fin = c.call_keeper("finalize_epoch", json!({"epoch": 1})).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));

    // -02, -03: these methods leave no receipt for anybody else
    for (name, result) in [("close_epoch", &close), ("submit_clearing", &submit), ("commit_epoch", &commit), ("finalize_epoch", &fin)] {
        assert_eq!(executors(result), [circle.clone()], "{name}: one receipt, no promise out");
    }

    // -04: claim to a NEAR account: ft_transfer to usdt, then the callback to itself
    let gpu = f.accounts["gpu"].clone();
    let claim = gpu
        .call(f.circle.contract.id(), "claim")
        .args_json(json!({"epoch": 1, "destination": "near"}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?;
    assert!(claim.is_success(), "{:?}", failure_text(&claim));
    // the gpu receipts are refunds of the 1 yocto deposit to the signer, not calls by the circle
    let seen: Vec<String> = executors(&claim).into_iter().filter(|e| *e != gpu.id().as_str()).collect();
    let usdt = f.usdt.id().to_string();
    assert_eq!(seen, [circle.clone(), usdt, circle.clone()], "claim: the circle, the token, the callback");
    f.assert_d6().await
}

// TC-CT-039-05: a claim to Intents still goes through the token; the circle never calls `intents_id`
#[tokio::test]
async fn claim_to_intents_goes_through_the_token() -> anyhow::Result<()> {
    let f = common::flow::s1_committed_on(Funded::deploy_with_intents(Some(Funded::params_short())).await?).await?;
    let fin = common::flow::finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    let gpu = f.accounts["gpu"].clone();
    let claim = gpu
        .call(f.circle.contract.id(), "claim")
        .args_json(json!({"epoch": 1, "destination": "intents"}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?;
    assert!(claim.is_success(), "{:?}", failure_text(&claim));
    let (circle, usdt, intents) = (f.circle.contract.id().to_string(), f.usdt.id().to_string(), f.intents.as_ref().unwrap().id().to_string());
    let seen: Vec<String> = executors(&claim).into_iter().filter(|e| *e != gpu.id().as_str()).collect();
    // circle.claim -> usdt.ft_transfer_call -> intents.ft_on_transfer -> usdt.ft_resolve_transfer -> circle.on_claim_resolved
    assert_eq!(seen, [circle.clone(), usdt.clone(), intents, usdt, circle]);
    Ok(())
}
