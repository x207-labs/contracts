//! CT-090 in near-sandbox (F8 class A): the escrow of an exact amount of a NEP-141 token. The token is `probes/mock_rwa` used as a plain token.
//! Needs `cargo near build non-reproducible-wasm` first. Run with `-- --test-threads=1` on a loaded machine.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::ft_escrow::{deploy_rwa, offer, world, AMOUNT, PRICE};
use near_workspaces::types::{Gas, NearToken};
use serde_json::{json, Value};

// TC-CT-090-01..05: the grantor funds the escrow, the buyer pays and gets the tokens
#[tokio::test]
async fn the_buyer_pays_and_gets_the_tokens() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer(3_600_000_000_000).await?;
    let alice = w.alice.clone();
    let funded = w.deposit(&alice, AMOUNT, id).await?;
    assert!(funded.is_success(), "{:?}", failure_text(&funded));
    assert!(events(&funded).iter().any(|(n, d)| n == "ft_escrow_held" && d["ft_contract"] == w.rwa.id().as_str() && d["amount"] == AMOUNT.to_string()));
    assert_eq!((w.rwa_in_circle().await?, w.rwa_of(&alice).await?), (AMOUNT, 9 * AMOUNT));
    let held = w.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?;
    assert_eq!((held["state"].as_str(), held["amount"].as_str(), held["buyer"].clone()), (Some("held"), Some("1000"), Value::Null));
    assert_eq!(w.f.circle.view("get_escrow", json!({"grant_id": id})).await?, Value::Null, "the NFT view does not show it");
    let strand = w.f.circle.call_as(&alice, "revoke_grant", json!({"grant_id": id, "reason": "grantor"})).await?;
    assert!(failure_text(&strand).expect("refused").contains("ERR_GRANT_REVOKE_DENIED"));

    let epoch = w.epoch().await?;
    let done = w.buy("bob", epoch, PRICE, id).await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert!(events(&done).iter().any(|(n, d)| n == "grant_activated" && d["grant_id"] == id && d["grantee"] == w.bob.id().as_str()));
    let bob = w.bob.clone();
    assert_eq!((w.rwa_of(&bob).await?, w.rwa_in_circle().await?), (AMOUNT, 0));
    assert_eq!(w.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?["state"], "delivered");
    assert_eq!(w.f.circle.view("obligation", json!({"epoch": epoch, "debtor": account_of("bob"), "creditor": account_of("alice")})).await?["obligation"], PRICE.to_string());
    // the USDT actions of ft_on_transfer still work next to the escrow action (world() staked with them)
    assert_eq!(w.f.member("alice").await?["status"], "active");
    Ok(())
}

// TC-CT-090-06..12: attacks on the deposit; the token gives everything back each time
#[tokio::test]
async fn nobody_fills_the_escrow_of_somebody_else_and_a_wrong_deposit_comes_back() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer(3_600_000_000_000).await?;
    let (alice, bob) = (w.alice.clone(), w.bob.clone());
    w.rwa.call("mint").args_json(json!({"account_id": bob.id(), "amount": (10 * AMOUNT).to_string()})).transact().await?.into_result()?;
    let alice_start = w.rwa_of(&alice).await?;

    // another account puts the right amount against the grant of alice
    let _ = w.deposit(&bob, AMOUNT, id).await?;
    assert_eq!((w.rwa_of(&bob).await?, w.rwa_in_circle().await?), (10 * AMOUNT, 0), "bob has his tokens back");
    // too little and too much from the grantor
    let _ = w.deposit(&alice, AMOUNT - 1, id).await?;
    let _ = w.deposit(&alice, AMOUNT + 1, id).await?;
    assert_eq!((w.rwa_of(&alice).await?, w.rwa_in_circle().await?), (alice_start, 0));
    // a token that is not the one in the scope
    let fake = deploy_rwa(&w.f).await?;
    fake.call("mint").args_json(json!({"account_id": alice.id(), "amount": AMOUNT.to_string()})).transact().await?.into_result()?;
    let _ = w.deposit_with(&fake, &alice, AMOUNT, &json!({"action": "escrow", "grant_id": id}).to_string()).await?;
    assert_eq!((w.balance(&fake, alice.id().as_str()).await?, w.balance(&fake, w.f.circle.contract.id().as_str()).await?), (AMOUNT, 0));
    // other actions through this token, and a message with no grant
    for msg in [json!({"action": "stake", "member_id": null}).to_string(), json!({"action": "settle", "epoch": 1, "member_id": null}).to_string(), "{\"action\":\"escrow\"}".to_string(), "hello".to_string()] {
        let _ = w.deposit_with(&w.rwa, &alice, AMOUNT, &msg).await?;
        assert_eq!((w.rwa_of(&alice).await?, w.rwa_in_circle().await?), (alice_start, 0), "{msg}");
    }
    assert_eq!(w.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?, Value::Null, "no refused deposit made an escrow");
    assert_eq!(w.f.member("alice").await?["stake"], (150 * CREDIT).to_string(), "no stake came from the RWA token");

    // the right deposit works once; a second one comes back
    assert!(w.deposit(&alice, AMOUNT, id).await?.is_success());
    let _ = w.deposit(&alice, AMOUNT, id).await?;
    assert_eq!((w.rwa_of(&alice).await?, w.rwa_in_circle().await?), (alice_start - AMOUNT, AMOUNT));
    Ok(())
}

// TC-CT-090-13: the USDT of the circle is not an escrow asset
#[tokio::test]
async fn the_usdt_of_the_circle_is_not_an_escrow_asset() -> anyhow::Result<()> {
    let w = world().await?;
    let expires = w.f.circle.now_ns().await? + 3_600_000_000_000;
    let alice = w.alice.clone();
    let usdt = w.f.usdt.id().to_string();
    let id = w.f.circle.call_as(&alice, "offer_grant", offer(expires, &usdt, AMOUNT)).await?.json::<u64>()?;
    let before = w.f.balance(&w.f.usdt, alice.id()).await?;
    let r = alice.call(w.f.usdt.id(), "ft_transfer_call")
        .args_json(json!({"receiver_id": w.f.circle.contract.id(), "amount": AMOUNT.to_string(), "memo": null, "msg": json!({"action": "escrow", "grant_id": id}).to_string()}))
        .deposit(NearToken::from_yoctonear(1)).gas(Gas::from_tgas(150)).transact().await?;
    let _ = r;
    assert_eq!(w.f.balance(&w.f.usdt, alice.id()).await?, before, "the USDT came back");
    assert_eq!(w.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?, Value::Null);
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-090-14: nobody buys, the grantor takes the tokens back after the time of the grant
#[tokio::test]
async fn the_grantor_takes_the_tokens_back_after_the_time() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer(90_000_000_000).await?;
    let (alice, bob) = (w.alice.clone(), w.bob.clone());
    assert!(w.deposit(&alice, AMOUNT, id).await?.is_success());
    let early = w.f.circle.call_as(&alice, "cancel_escrow", json!({"grant_id": id})).await?;
    assert!(failure_text(&early).expect("refused").contains("ERR_ESCROW_NOT_EXPIRED"));
    let expires = w.f.circle.view("get_grant", json!({"grant_id": id})).await?["limits"]["expires_at"].as_str().unwrap().parse::<u64>()?;
    w.f.circle.advance_to(expires + 2_000_000_000).await?;
    let stranger = w.f.circle.call_as(&bob, "cancel_escrow", json!({"grant_id": id})).await?;
    assert!(failure_text(&stranger).expect("refused").contains("ERR_ESCROW_NOT_GRANTOR"));
    let late = w.buy("bob", 1, PRICE, id).await?;
    assert!(failure_text(&late).expect("refused").contains("ERR_GRANT_EXPIRED"));
    let done = w.f.circle.call_as(&alice, "cancel_escrow", json!({"grant_id": id})).await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert_eq!((w.rwa_of(&alice).await?, w.rwa_in_circle().await?), (10 * AMOUNT, 0));
    assert_eq!(w.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?["state"], "cancelled");
    assert_eq!(w.f.circle.view("get_grant", json!({"grant_id": id})).await?["status"], "revoked");
    Ok(())
}

