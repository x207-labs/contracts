//! BE-034, contract side (spec 18): `mt_on_transfer` settles a residual that arrives from `intents_id` as the multi-token
//! `nep141:<usdt_token_id>`. The multi-token contract is `probes/mock_intents` (it plays `intents.near` with `mt_transfer_call`). Needs
//! `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::flow::{finalize, s1_committed_on};
use common::funded::Funded;
use near_workspaces::result::ExecutionFinalResult;
use serde_json::{json, Value};

fn msg(epoch: u64) -> String {
    json!({"action": "settle", "epoch": epoch}).to_string()
}

async fn window() -> anyhow::Result<Funded> {
    // S1 payment window: scraper owes 35, gpu is owed 35, translator owes nothing
    s1_committed_on(Funded::deploy_with_intents(Some(Funded::params_short())).await?).await
}

async fn via_intents(
    f: &Funded,
    owner: &str,
    token: &str,
    amount: u128,
    message: &str,
) -> anyhow::Result<ExecutionFinalResult> {
    let intents = f.intents.as_ref().expect("mock intents");
    let caller = f.circle.worker.dev_create_account().await?;
    Ok(caller
        .call(intents.id(), "mt_transfer_call")
        .args_json(json!({"receiver_id": f.circle.contract.id(), "previous_owner_id": account_of(owner), "token_id": token, "amount": amount.to_string(), "msg": message}))
        .gas(near_workspaces::types::Gas::from_tgas(200))
        .transact()
        .await?)
}

fn token(f: &Funded) -> String {
    format!("nep141:{}", f.usdt.id())
}

async fn position(f: &Funded, name: &str) -> anyhow::Result<Value> {
    f.circle
        .view(
            "net_position",
            json!({"epoch": 1, "account_id": account_of(name)}),
        )
        .await
}

async fn snapshot(f: &Funded) -> anyhow::Result<(Value, Value)> {
    Ok((
        position(f, "scraper").await?,
        f.circle.view("get_pool", json!({})).await?,
    ))
}

// TC-BE-034-01..03: the debtor pays 35 and sends 40: it settles, 5 comes back as the return value, the books move like a settle by ft_transfer_call
#[tokio::test]
async fn a_settle_through_the_multi_token_pays_the_residual_and_hands_back_the_excess(
) -> anyhow::Result<()> {
    let f = window().await?;
    assert_eq!(position(&f, "scraper").await?["status"], "due");
    let before_pool = f.circle.view("get_pool", json!({})).await?;
    let paid = via_intents(&f, "scraper", &token(&f), 40 * CREDIT, &msg(1)).await?;
    assert!(paid.is_success(), "{:?}", failure_text(&paid));
    assert_eq!(
        paid.clone().json::<Vec<String>>()?,
        vec!["5000000".to_string()],
        "the excess is handed back per token"
    );
    let log = events(&paid);
    let residual = log
        .iter()
        .find(|(n, _)| n == "residual_paid")
        .expect("residual_paid");
    assert_eq!(
        residual.1,
        json!({"epoch": 1, "account_id": account_of("scraper"), "sender_id": account_of("scraper"), "amount": "35000000", "refunded": "5000000", "settled_history": "35000000"})
    );
    assert_eq!(position(&f, "scraper").await?["status"], "paid");
    assert_eq!(
        f.circle
            .view(
                "outstanding_debt",
                json!({"account_id": account_of("scraper")})
            )
            .await?,
        "0"
    );
    let after_pool = f.circle.view("get_pool", json!({})).await?;
    let grew = after_pool["epoch_pools"]
        .as_str()
        .unwrap()
        .parse::<u128>()?
        - before_pool["epoch_pools"]
            .as_str()
            .unwrap()
            .parse::<u128>()?;
    assert_eq!(grew, 35 * CREDIT);
    // the epoch can be finalized and the creditor is claimable as after any settle
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    assert_eq!(position(&f, "gpu").await?["status"], "claimable");
    Ok(())
}

// TC-BE-034-04: `member_id` pays for another member, as in ft_on_transfer
#[tokio::test]
async fn member_id_pays_for_somebody_else() -> anyhow::Result<()> {
    let f = window().await?;
    let message =
        json!({"action": "settle", "epoch": 1, "member_id": account_of("scraper")}).to_string();
    let paid = via_intents(&f, "translator", &token(&f), 35 * CREDIT, &message).await?;
    assert!(paid.is_success(), "{:?}", failure_text(&paid));
    assert_eq!(paid.clone().json::<Vec<String>>()?, vec!["0".to_string()]);
    assert_eq!(position(&f, "scraper").await?["status"], "paid");
    Ok(())
}

// TC-BE-034-05..09: refusals change nothing
#[tokio::test]
async fn refusals_change_nothing() -> anyhow::Result<()> {
    let f = window().await?;
    let before = snapshot(&f).await?;
    let refused = |result: ExecutionFinalResult, code: &str| {
        // the mock plays NEP-245: a refused call comes back to the caller as a full refund, and the failed receipt is in `failures()`
        let text = format!("{:?}", result.failures());
        assert!(
            text.contains(code),
            "expected {code}: {}",
            text.chars().take(300).collect::<String>()
        );
    };

    // another predecessor than intents_id, calling the receiver directly
    let outsider = f.circle.worker.dev_create_account().await?;
    let direct = outsider
        .call(f.circle.contract.id(), "mt_on_transfer")
        .args_json(json!({"sender_id": account_of("scraper"), "previous_owner_ids": [account_of("scraper")], "token_ids": [token(&f)], "amounts": [(35 * CREDIT).to_string()], "msg": msg(1)}))
        .gas(near_workspaces::types::Gas::from_tgas(100))
        .transact()
        .await?;
    refused(direct, "ERR_WRONG_TOKEN");
    // another token, the wrong-token mock, the bare USDT id without the prefix, and two tokens at once
    for other in [
        format!("nep141:{}", f.palsu.id()),
        f.usdt.id().to_string(),
        "nep245:x".to_string(),
    ] {
        refused(
            via_intents(&f, "scraper", &other, 35 * CREDIT, &msg(1)).await?,
            "ERR_WRONG_TOKEN",
        );
    }
    // a message that is not a settle: stake does not come through the multi-token
    refused(
        via_intents(
            &f,
            "scraper",
            &token(&f),
            35 * CREDIT,
            r#"{"action":"stake"}"#,
        )
        .await?,
        "ERR_BAD_MSG",
    );
    refused(
        via_intents(&f, "scraper", &token(&f), 35 * CREDIT, "not json").await?,
        "ERR_BAD_MSG",
    );
    refused(
        via_intents(&f, "scraper", &token(&f), 0, &msg(1)).await?,
        "ERR_ZERO_AMOUNT",
    );
    // the checks of settle_transfer: too little, a debtor with no position, an epoch that is not in the payment window
    refused(
        via_intents(&f, "scraper", &token(&f), 34 * CREDIT, &msg(1)).await?,
        "ERR_UNDERPAYMENT",
    );
    refused(
        via_intents(&f, "translator", &token(&f), 35 * CREDIT, &msg(1)).await?,
        "ERR_NO_POSITION",
    );
    refused(
        via_intents(&f, "scraper", &token(&f), 35 * CREDIT, &msg(7)).await?,
        "ERR_UNKNOWN_EPOCH",
    );
    assert_eq!(snapshot(&f).await?, before, "no refusal moved a thing");
    // and the second payment of the same debtor is not due any more
    assert!(via_intents(&f, "scraper", &token(&f), 35 * CREDIT, &msg(1))
        .await?
        .is_success());
    refused(
        via_intents(&f, "scraper", &token(&f), 35 * CREDIT, &msg(1)).await?,
        "ERR_NOT_DUE",
    );
    Ok(())
}
