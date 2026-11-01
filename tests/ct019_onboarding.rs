//! CT-019: F1 onboarding in near-sandbox with a real NEP-141 mock as USDT: join, stake through
//! `ft_transfer_call`, refunds on every rejection. Needs `cargo near build non-reproducible-wasm`.

mod common;

use common::circle::{events, failure_text, receipt_failures, CREDIT};
use common::funded::Funded;
use near_workspaces::types::NearToken;
use serde_json::{json, Value};

/// The final value of `ft_transfer_call`: the amount the receiver used.
fn used(result: &near_workspaces::result::ExecutionFinalResult) -> u128 {
    result.clone().json::<String>().expect("ft_transfer_call returns U128").parse().unwrap()
}

// TC-CT-019-01
#[tokio::test]
async fn join_then_stake_activates_the_member() -> anyhow::Result<()> {
    let mut f = Funded::deploy(None).await?;
    let translator = f.account("translator").await?;
    let join = f.join("translator", Funded::join_deposit()).await?;
    assert!(join.is_success(), "{:?}", failure_text(&join));

    let before_contract = f.balance(&f.usdt, f.circle.contract.id()).await?;
    let stake = f.stake(&translator, 100 * CREDIT).await?;
    assert!(stake.is_success(), "{:?}", failure_text(&stake));
    assert_eq!(used(&stake), 100 * CREDIT, "nothing is refunded");

    let member = f.member("translator").await?;
    assert_eq!((member["status"].as_str(), member["stake"].as_str(), member["limit"].as_str()), (Some("active"), Some("100000000"), Some("80000000")));

    let mut names: Vec<String> = events(&join).into_iter().map(|(n, _)| n).collect();
    names.extend(events(&stake).into_iter().map(|(n, _)| n));
    assert_eq!(names, ["member_joined", "stake_added"]);

    assert_eq!(f.balance(&f.usdt, f.circle.contract.id()).await?, before_contract + 100 * CREDIT);
    assert_eq!(f.balance(&f.usdt, translator.id()).await?, 900 * CREDIT);
    f.assert_d6().await?;
    Ok(())
}

/// A pending member `translator` who sends 100 through `token` with `msg`, and the state around it.
async fn refused(token_is_wrong: bool, msg: &str) -> anyhow::Result<(Funded, near_workspaces::result::ExecutionFinalResult)> {
    let mut f = Funded::deploy(None).await?;
    let translator = f.account("translator").await?;
    assert!(f.join("translator", Funded::join_deposit()).await?.is_success());
    let token = if token_is_wrong { f.palsu.clone() } else { f.usdt.clone() };
    let result = f.transfer_call(&token, &translator, 100 * CREDIT, msg).await?;
    Ok((f, result))
}

async fn assert_refunded_and_pending(f: &Funded, token_is_wrong: bool) -> anyhow::Result<()> {
    let token = if token_is_wrong { &f.palsu } else { &f.usdt };
    let translator = f.accounts["translator"].id().clone();
    assert_eq!(f.balance(token, &translator).await?, 1000 * CREDIT, "full refund");
    assert_eq!(f.balance(token, f.circle.contract.id()).await?, 0);
    let member = f.member("translator").await?;
    assert_eq!((member["status"].as_str(), member["stake"].as_str()), (Some("pending"), Some("0")));
    Ok(())
}

// TC-CT-019-02
#[tokio::test]
async fn wrong_token_is_refunded() -> anyhow::Result<()> {
    let (f, result) = refused(true, r#"{"action":"stake"}"#).await?;
    assert!(receipt_failures(&result).contains("ERR_WRONG_TOKEN"), "{}", receipt_failures(&result));
    assert_eq!(used(&result), 0);
    assert_refunded_and_pending(&f, true).await?;
    f.assert_d6().await
}

// TC-CT-019-03
#[tokio::test]
async fn unknown_msg_is_refunded() -> anyhow::Result<()> {
    let (f, result) = refused(false, r#"{"action":"burn"}"#).await?;
    assert!(receipt_failures(&result).contains("ERR_BAD_MSG"), "{}", receipt_failures(&result));
    assert_eq!(used(&result), 0);
    assert_refunded_and_pending(&f, false).await?;
    f.assert_d6().await
}

// TC-CT-019-04
#[tokio::test]
async fn second_join_fails_and_the_first_entry_stays() -> anyhow::Result<()> {
    let mut f = Funded::deploy(None).await?;
    assert!(f.join("translator", Funded::join_deposit()).await?.is_success());
    let first = f.member("translator").await?;
    let second = f.join("translator", Funded::join_deposit()).await?;
    assert!(failure_text(&second).expect("second join fails").contains("ERR_ALREADY_MEMBER"));
    assert_eq!(f.member("translator").await?, first);
    Ok(())
}

// TC-CT-019-05
#[tokio::test]
async fn stake_before_join_is_refunded() -> anyhow::Result<()> {
    let mut f = Funded::deploy(None).await?;
    let outsider = f.account("outsider").await?;
    let result = f.stake(&outsider, 100 * CREDIT).await?;
    assert!(receipt_failures(&result).contains("ERR_NOT_MEMBER"), "{}", receipt_failures(&result));
    assert_eq!(used(&result), 0);
    assert_eq!(f.balance(&f.usdt, outsider.id()).await?, 1000 * CREDIT);
    assert_eq!(f.member("outsider").await?, Value::Null);
    f.assert_d6().await
}

// TC-CT-019-06
#[tokio::test]
async fn join_without_deposit_fails() -> anyhow::Result<()> {
    let mut f = Funded::deploy(None).await?;
    let result = f.join("translator", NearToken::from_yoctonear(0)).await?;
    assert!(failure_text(&result).expect("join without deposit fails").contains("ERR_INSUFFICIENT_STORAGE_DEPOSIT"));
    assert_eq!(f.member("translator").await?, Value::Null);
    Ok(())
}

// TC-CT-019-07
#[tokio::test]
async fn fixture_with_six_active_members() -> anyhow::Result<()> {
    let mut f = Funded::deploy(None).await?;
    for name in ["translator", "scraper", "gpu", "summarizer", "analyst", "coder"] {
        let account = f.account(name).await?;
        let join = f.join(name, Funded::join_deposit()).await?;
        assert!(join.is_success(), "{name}: {:?}", failure_text(&join));
        let stake = f.stake(&account, 100 * CREDIT).await?;
        assert_eq!(used(&stake), 100 * CREDIT, "{name}");
    }
    let members = f.circle.view("list_members", json!({"from_index": 0, "limit": 100})).await?;
    let members = members.as_array().unwrap();
    assert_eq!(members.len(), 6);
    assert!(members.iter().all(|m| m["status"] == "active" && m["limit"] == "80000000"));
    let pool = f.circle.view("get_pool", json!({})).await?;
    assert_eq!(pool["total_stake"], "600000000");
    f.assert_d6().await
}
