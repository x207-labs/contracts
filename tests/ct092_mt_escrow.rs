//! CT-092 in near-sandbox (F8 class A): the escrow of an exact amount of one NEP-245 multi-token (`probes/mock_mt`). Needs `cargo near build
//! non-reproducible-wasm` first. Run with `-- --test-threads=1` on a loaded machine.

mod common;

use common::circle::{account_of, events, failure_text};
use common::mt_escrow::{deploy_mt, world, AMOUNT, PRICE, TOKEN};
use near_workspaces::types::{Gas, NearToken};
use serde_json::{json, Value};

// TC-CT-092-01..05: the grantor funds the escrow, the buyer pays and gets the multi-token
#[tokio::test]
async fn the_buyer_pays_and_gets_the_multi_token() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer(3_600_000_000_000).await?;
    let alice = w.alice.clone();
    let funded = w.deposit(&alice, AMOUNT, id).await?;
    assert!(funded.is_success(), "{:?}", failure_text(&funded));
    assert!(events(&funded).iter().any(|(n, d)| n == "mt_escrow_held" && d["mt_contract"] == w.mt.id().as_str() && d["token_id"] == TOKEN && d["amount"] == AMOUNT.to_string()));
    assert_eq!((w.in_circle().await?, w.of(&alice).await?), (AMOUNT, 9 * AMOUNT));
    let held = w.f.circle.view("get_mt_escrow", json!({"grant_id": id})).await?;
    assert_eq!((held["state"].as_str(), held["token_id"].as_str(), held["amount"].as_str(), held["buyer"].clone()), (Some("held"), Some(TOKEN), Some("1000"), Value::Null));
    assert_eq!((w.f.circle.view("get_escrow", json!({"grant_id": id})).await?, w.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?), (Value::Null, Value::Null));
    let strand = w.f.circle.call_as(&alice, "revoke_grant", json!({"grant_id": id, "reason": "grantor"})).await?;
    assert!(failure_text(&strand).expect("refused").contains("ERR_GRANT_REVOKE_DENIED"));

    let epoch = w.epoch().await?;
    let done = w.buy("bob", epoch, PRICE, id).await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert!(events(&done).iter().any(|(n, d)| n == "grant_activated" && d["grant_id"] == id && d["grantee"] == w.bob.id().as_str()));
    let bob = w.bob.clone();
    assert_eq!((w.of(&bob).await?, w.in_circle().await?), (AMOUNT, 0));
    assert_eq!(w.f.circle.view("get_mt_escrow", json!({"grant_id": id})).await?["state"], "delivered");
    assert_eq!(w.f.circle.view("obligation", json!({"epoch": epoch, "debtor": account_of("bob"), "creditor": account_of("alice")})).await?["obligation"], PRICE.to_string());
    Ok(())
}

// TC-CT-092-06..12: attacks on the deposit; the token gives everything back each time
#[tokio::test]
async fn a_wrong_multi_token_deposit_comes_back() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer(3_600_000_000_000).await?;
    let (alice, bob) = (w.alice.clone(), w.bob.clone());
    w.mt.call("mint").args_json(json!({"account_id": bob.id(), "token_id": TOKEN, "amount": (10 * AMOUNT).to_string()})).transact().await?.into_result()?;
    let start = w.of(&alice).await?;

    // another account puts the right amount against the grant of alice
    let _ = w.deposit(&bob, AMOUNT, id).await?;
    assert_eq!((w.of(&bob).await?, w.in_circle().await?), (10 * AMOUNT, 0), "bob has his tokens back");
    // too little and too much from the grantor
    let _ = w.deposit(&alice, AMOUNT - 1, id).await?;
    let _ = w.deposit(&alice, AMOUNT + 1, id).await?;
    assert_eq!((w.of(&alice).await?, w.in_circle().await?), (start, 0));
    // another token of the right contract, and the right token of another contract
    w.mt.call("mint").args_json(json!({"account_id": alice.id(), "token_id": "nep141:other.test.near", "amount": AMOUNT.to_string()})).transact().await?.into_result()?;
    let msg = json!({"action": "escrow", "grant_id": id}).to_string();
    let _ = w.deposit_with(&w.mt, &alice, "nep141:other.test.near", AMOUNT, &msg).await?;
    assert_eq!(w.balance(&w.mt, alice.id().as_str(), "nep141:other.test.near").await?, AMOUNT);
    let fake = deploy_mt(&w.f).await?;
    fake.call("mint").args_json(json!({"account_id": alice.id(), "token_id": TOKEN, "amount": AMOUNT.to_string()})).transact().await?.into_result()?;
    let _ = w.deposit_with(&fake, &alice, TOKEN, AMOUNT, &msg).await?;
    assert_eq!((w.balance(&fake, alice.id().as_str(), TOKEN).await?, w.balance(&fake, w.f.circle.contract.id().as_str(), TOKEN).await?), (AMOUNT, 0));
    // a batch of two tokens
    let batch = alice
        .call(w.mt.id(), "mt_batch_transfer_call")
        .args_json(json!({"receiver_id": w.f.circle.contract.id(), "token_ids": [TOKEN, "nep141:other.test.near"], "amounts": [AMOUNT.to_string(), AMOUNT.to_string()], "approvals": null, "memo": null, "msg": msg}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(200))
        .transact()
        .await?;
    let _ = batch;
    assert_eq!((w.of(&alice).await?, w.balance(&w.mt, alice.id().as_str(), "nep141:other.test.near").await?, w.in_circle().await?), (start, AMOUNT, 0), "the batch came back");
    // stake and settle through a multi-token contract that is not Intents, and a message with no grant
    for m in [json!({"action": "stake", "member_id": null}).to_string(), json!({"action": "settle", "epoch": 1, "member_id": null}).to_string(), "{\"action\":\"escrow\"}".to_string(), "hello".to_string()] {
        let _ = w.deposit_with(&w.mt, &alice, TOKEN, AMOUNT, &m).await?;
        assert_eq!((w.of(&alice).await?, w.in_circle().await?), (start, 0), "{m}");
    }
    assert_eq!(w.f.circle.view("get_mt_escrow", json!({"grant_id": id})).await?, Value::Null, "no refused deposit made an escrow");
    assert_eq!(w.f.member("alice").await?["stake"], (150 * common::circle::CREDIT).to_string(), "no stake came from the multi-token");

    // the right deposit works once; a second one comes back
    assert!(w.deposit(&alice, AMOUNT, id).await?.is_success());
    let _ = w.deposit(&alice, AMOUNT, id).await?;
    assert_eq!((w.of(&alice).await?, w.in_circle().await?), (start - AMOUNT, AMOUNT));
    Ok(())
}

// TC-CT-092-13..14: the multi-token refuses the buyer (blocked, frozen): nothing is paid, the escrow stays for sale, and after the time the grantor gets the tokens back
#[tokio::test]
async fn a_buyer_the_token_refuses_pays_nothing_and_the_grantor_can_take_the_tokens_back() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer(90_000_000_000).await?;
    let alice = w.alice.clone();
    let start = w.of(&alice).await?;
    assert!(w.deposit(&alice, AMOUNT, id).await?.is_success());
    let epoch = w.epoch().await?;
    let before = w.f.circle.view("get_epoch", json!({})).await?;
    w.mt.call("block_receiver").args_json(json!({"account_id": w.f.accounts["bob"].id(), "blocked": true})).transact().await?.into_result()?;
    w.mt.call("set_frozen").args_json(json!({"account_id": w.f.accounts["carol"].id(), "frozen": true})).transact().await?.into_result()?;
    for buyer in ["bob", "carol"] {
        let tried = w.buy(buyer, epoch, PRICE, id).await?;
        assert!(tried.is_success(), "{buyer}: {:?}", failure_text(&tried));
        let failed = events(&tried).into_iter().find(|(n, _)| n == "escrow_failed").unwrap_or_else(|| panic!("{buyer}: no escrow_failed")).1;
        assert_eq!((failed["step"].as_str(), failed["reverted"].clone(), failed["delta"].as_str()), (Some("complete"), json!(true), Some("10000000")), "{buyer}");
        assert!(!events(&tried).iter().any(|(n, _)| n == "grant_activated"), "{buyer}");
        assert_eq!(w.in_circle().await?, AMOUNT, "{buyer}: the tokens did not move");
        assert_eq!(w.f.circle.view("obligation", json!({"epoch": epoch, "debtor": account_of(buyer), "creditor": account_of("alice")})).await?, Value::Null, "{buyer}");
        assert_eq!(w.f.circle.view("outstanding_debt", json!({"account_id": account_of(buyer)})).await?, "0", "{buyer}");
        assert_eq!(w.f.circle.view("get_mt_escrow", json!({"grant_id": id})).await?["state"], "held", "{buyer}");
        assert_eq!(w.f.circle.view("get_grant", json!({"grant_id": id})).await?["status"], "offered", "{buyer}");
        let after = w.f.circle.view("get_epoch", json!({})).await?;
        for field in ["edge_count", "participant_count", "total_obligation"] {
            assert_eq!(before[field], after[field], "{buyer}: {field}");
        }
    }
    // after the time of the grant the grantor takes the tokens back, although two deliveries failed
    let expires = w.f.circle.view("get_grant", json!({"grant_id": id})).await?["limits"]["expires_at"].as_str().unwrap().parse::<u64>()?;
    w.f.circle.advance_to(expires + 2_000_000_000).await?;
    let done = w.f.circle.call_as(&alice, "cancel_escrow", json!({"grant_id": id})).await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert_eq!((w.of(&alice).await?, w.in_circle().await?), (start, 0));
    assert_eq!(w.f.circle.view("get_mt_escrow", json!({"grant_id": id})).await?["state"], "cancelled");
    assert_eq!(w.f.circle.view("get_grant", json!({"grant_id": id})).await?["status"], "revoked");
    Ok(())
}

// TC-CT-092-15: a contract that is not the multi-token of the scope calls mt_on_transfer itself (a fake contract, or any account): the answer hands everything back
#[tokio::test]
async fn a_fake_contract_calling_the_receiver_directly_gets_everything_back() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer(3_600_000_000_000).await?;
    let fake = w.f.account("mallory").await?;
    let msg = json!({"action": "escrow", "grant_id": id}).to_string();
    // the owner it names is the grantor, the token is the right one, the amount is right: only the caller is wrong
    let called = fake
        .call(w.f.circle.contract.id(), "mt_on_transfer")
        .args_json(json!({"sender_id": w.alice.id(), "previous_owner_ids": [w.alice.id()], "token_ids": [TOKEN], "amounts": [AMOUNT.to_string()], "msg": msg}))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert!(called.is_success(), "a refusal is an answer, not a failure: {:?}", failure_text(&called));
    assert_eq!(called.clone().json::<Vec<String>>()?, vec![AMOUNT.to_string()], "the whole amount is handed back");
    assert!(called.logs().iter().any(|l| l.contains("ERR_ESCROW_MT_MISMATCH")), "{:?}", called.logs());
    assert_eq!(w.f.circle.view("get_mt_escrow", json!({"grant_id": id})).await?, Value::Null);
    // a second fake multi-token contract that names the grantor as owner but is not the contract of the scope
    let other = deploy_mt(&w.f).await?;
    other.call("mint").args_json(json!({"account_id": w.alice.id(), "token_id": TOKEN, "amount": AMOUNT.to_string()})).transact().await?.into_result()?;
    let alice = w.alice.clone();
    let _ = w.deposit_with(&other, &alice, TOKEN, AMOUNT, &msg).await?;
    assert_eq!(w.balance(&other, alice.id().as_str(), TOKEN).await?, AMOUNT, "the tokens came back through the resolve of the fake contract");
    assert_eq!(w.f.circle.view("get_mt_escrow", json!({"grant_id": id})).await?, Value::Null);
    Ok(())
}
