//! CT-092 in near-sandbox: an escrow that arrives from `intents_id` (a token held at Intents) must not disturb the settle action. One circle, `mock_intents` as
//! `intents_id`: a settle, an escrow of another token, a refused escrow and a second settle, in this order, are all right. Needs
//! `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::flow::s1_committed_on;
use common::funded::Funded;
use near_workspaces::result::ExecutionFinalResult;
use serde_json::json;

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

// TC-CT-092-16: settle, escrow, refused escrow, escrow of the USDT, settle: each does what it should and none disturbs the others
#[tokio::test]
async fn escrow_and_settle_through_intents_in_turn() -> anyhow::Result<()> {
    let mut f =
        s1_committed_on(Funded::deploy_with_intents(Some(Funded::params_short())).await?).await?;
    let usdt_token = format!("nep141:{}", f.usdt.id());
    let other = "nep141:usdc.test.near";
    let intents = f.intents.as_ref().expect("mock intents").id().to_string();
    // a class A grant of gpu that takes 1000 of `other` held at Intents
    let dao = f.account("dao").await?;
    let entry = json!({"asset_type": "mt:usdc", "class": "A", "delivery_methods": ["escrow_mt"], "accepted_proofs": [], "terms_hash": "5a".repeat(32)});
    assert!(f
        .circle
        .call_as(
            &dao,
            "set_asset_class",
            json!({"asset_type": "mt:usdc", "entry": entry})
        )
        .await?
        .is_success());
    let expires = f.circle.now_ns().await? + 3_600_000_000_000;
    let gpu = f.accounts["gpu"].clone();
    let offered = json!({"input": {
        "asset_type": "mt:usdc", "delivery_method": "escrow_mt",
        "limits": {"scope": json!({"mt_contract": intents, "token_id": other, "amount": "1000"}).to_string(), "quota": null, "expires_at": expires.to_string()},
        "unit": "once", "unit_price": (10 * CREDIT).to_string(), "operator": null, "proof_hash": null, "proof_kind": null
    }});
    let id = f
        .circle
        .call_as(&gpu, "offer_grant", offered)
        .await?
        .json::<u64>()?;
    let escrow_msg = json!({"action": "escrow", "grant_id": id}).to_string();
    let settle = json!({"action": "settle", "epoch": 1}).to_string();

    // 1. a settle: scraper pays 35 and sends 40, 5 comes back
    let paid = via_intents(&f, "scraper", &usdt_token, 40 * CREDIT, &settle).await?;
    assert!(paid.is_success(), "{:?}", failure_text(&paid));
    assert_eq!(
        paid.clone().json::<Vec<String>>()?,
        vec!["5000000".to_string()]
    );
    assert!(events(&paid)
        .iter()
        .any(|(n, d)| n == "residual_paid" && d["amount"] == "35000000"));
    // 2. an escrow of gpu for the grant: kept
    let held = via_intents(&f, "gpu", other, 1000, &escrow_msg).await?;
    assert!(held.is_success(), "{:?}", failure_text(&held));
    assert_eq!(held.clone().json::<Vec<String>>()?, vec!["0".to_string()]);
    assert!(events(&held).iter().any(|(n, d)| n == "mt_escrow_held"
        && d["mt_contract"] == intents.as_str()
        && d["token_id"] == other));
    assert_eq!(
        f.circle
            .view("get_mt_escrow", json!({"grant_id": id}))
            .await?["state"],
        "held"
    );
    // 3. a refused escrow (somebody else, wrong amount, the USDT of the circle) is handed back by the answer and changes nothing
    let pool = f.circle.view("get_pool", json!({})).await?;
    for (owner, token, amount, code) in [
        ("translator", other, 1000u128, "ERR_ESCROW_DEPOSITOR"),
        ("gpu", other, 999, "ERR_ESCROW_MT_AMOUNT"),
        ("gpu", other, 1000, "ERR_ESCROW_EXISTS"),
        ("gpu", usdt_token.as_str(), 1000, "ERR_ESCROW_USDT"),
    ] {
        let refused = via_intents(&f, owner, token, amount, &escrow_msg).await?;
        assert!(refused.is_success(), "{code}: {:?}", failure_text(&refused));
        assert_eq!(
            refused.clone().json::<Vec<String>>()?,
            vec![amount.to_string()],
            "{code}: handed back"
        );
        assert!(
            refused.logs().iter().any(|l| l.contains(code)),
            "{code}: {:?}",
            refused.logs()
        );
    }
    assert_eq!(
        f.circle.view("get_pool", json!({})).await?,
        pool,
        "no refused escrow touched the pool"
    );
    assert_eq!(
        f.circle.view("intents_balance", json!({})).await?,
        "35000000",
        "only the settle booked a balance at Intents"
    );
    // 4. a second settle of the same debt after the escrow is refused as before (the debtor is paid already): the mock plays the refund of a failed receipt
    let again = via_intents(&f, "scraper", &usdt_token, 35 * CREDIT, &settle).await?;
    assert_eq!(
        again.json::<Vec<String>>()?,
        vec!["35000000".to_string()],
        "the repeat comes back in full"
    );
    assert_eq!(
        f.circle
            .view(
                "net_position",
                json!({"epoch": 1, "account_id": account_of("scraper")})
            )
            .await?["status"],
        "paid"
    );
    assert_eq!(
        f.circle
            .view("get_mt_escrow", json!({"grant_id": id}))
            .await?["state"],
        "held",
        "the settle left the escrow alone"
    );
    Ok(())
}
