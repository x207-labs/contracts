//! CT-091 in near-sandbox (F8 class A): the escrow of a real-world-asset token that restricts transfers (`probes/mock_rwa`: whitelist, frozen accounts,
//! a transfer fee). Every refusal must leave funds, voucher and escrow state as they were. Needs `cargo near build non-reproducible-wasm` first.
//! Run with `-- --test-threads=1` on a loaded machine.

mod common;

use common::circle::{events, failure_text};
use common::ft_escrow::world;
use common::ft_escrow::{AMOUNT, PRICE};
use near_workspaces::types::NearToken;
use serde_json::{json, Value};

// TC-CT-091-01..04: a buyer the token refuses (not whitelisted, frozen, not registered) pays nothing and the escrow stays for sale
#[tokio::test]
async fn a_buyer_the_token_refuses_leaves_no_obligation() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer(3_600_000_000_000).await?;
    let alice = w.alice.clone();
    w.whitelist(&[
        w.f.circle.contract.id().as_str(),
        alice.id().as_str(),
        w.f.accounts["carol"].id().as_str(),
    ])
    .await?;
    assert!(w.deposit(&alice, AMOUNT, id).await?.is_success());
    let epoch = w.epoch().await?;
    let before = w.f.circle.view("get_epoch", json!({})).await?;

    for (why, setup) in [
        ("not whitelisted", None),
        (
            "frozen",
            Some((
                "set_frozen",
                json!({"account_id": w.f.accounts["carol"].id(), "frozen": true}),
            )),
        ),
        (
            "not registered",
            Some(("storage_unregister", json!({"force": null}))),
        ),
    ] {
        let buyer = if why == "not whitelisted" {
            "bob"
        } else {
            "carol"
        };
        if let Some((method, args)) = setup {
            if method == "storage_unregister" {
                w.f.accounts[buyer]
                    .call(w.rwa.id(), method)
                    .args_json(args)
                    .deposit(NearToken::from_yoctonear(1))
                    .transact()
                    .await?
                    .into_result()?;
            } else {
                w.rwa
                    .call(method)
                    .args_json(args)
                    .transact()
                    .await?
                    .into_result()?;
            }
        }
        let tried = w.buy(buyer, epoch, PRICE, id).await?;
        assert!(
            tried.is_success(),
            "{why}: the callback runs: {:?}",
            failure_text(&tried)
        );
        let failed = events(&tried)
            .into_iter()
            .find(|(n, _)| n == "escrow_failed")
            .unwrap_or_else(|| panic!("{why}: no escrow_failed"))
            .1;
        assert_eq!(
            (
                failed["step"].as_str(),
                failed["reverted"].clone(),
                failed["delta"].as_str()
            ),
            (Some("complete"), json!(true), Some("10000000")),
            "{why}"
        );
        assert!(
            !events(&tried).iter().any(|(n, _)| n == "grant_activated"),
            "{why}"
        );
        assert_eq!(
            (
                w.rwa_in_circle().await?,
                w.rwa_of(&w.f.accounts[buyer].clone()).await?
            ),
            (AMOUNT, 0),
            "{why}: the tokens did not move"
        );
        w.assert_clean_after_failure(id, epoch, buyer).await?;
        let after = w.f.circle.view("get_epoch", json!({})).await?;
        for field in ["edge_count", "participant_count", "total_obligation"] {
            assert_eq!(before[field], after[field], "{why}: {field}");
        }
    }
    // the pair is free again: an allowed buyer pays and gets the tokens
    w.rwa
        .call("set_frozen")
        .args_json(json!({"account_id": w.f.accounts["carol"].id(), "frozen": false}))
        .transact()
        .await?
        .into_result()?;
    w.f.accounts["carol"]
        .call(w.rwa.id(), "storage_deposit")
        .args_json(json!({"account_id": null, "registration_only": true}))
        .transact()
        .await?
        .into_result()?;
    let ok = w.buy("carol", epoch, PRICE, id).await?;
    assert!(ok.is_success(), "{:?}", failure_text(&ok));
    assert_eq!(w.rwa_of(&w.f.accounts["carol"].clone()).await?, AMOUNT);
    assert_eq!(
        w.f.circle
            .view("get_grant", json!({"grant_id": id}))
            .await?["status"],
        "active"
    );
    Ok(())
}

// TC-CT-091-05..07: a deposit the token refuses makes no escrow and costs the grantor nothing
#[tokio::test]
async fn a_deposit_the_token_refuses_makes_no_escrow() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer(3_600_000_000_000).await?;
    let alice = w.alice.clone();
    let start = w.rwa_of(&alice).await?;
    // the circle is not whitelisted
    w.whitelist(&[alice.id().as_str()]).await?;
    let _ = w.deposit(&alice, AMOUNT, id).await?;
    assert_eq!(
        (w.rwa_of(&alice).await?, w.rwa_in_circle().await?),
        (start, 0),
        "not whitelisted circle"
    );
    // the grantor is frozen
    w.whitelist(&[w.f.circle.contract.id().as_str()]).await?;
    w.rwa
        .call("set_frozen")
        .args_json(json!({"account_id": alice.id(), "frozen": true}))
        .transact()
        .await?
        .into_result()?;
    let _ = w.deposit(&alice, AMOUNT, id).await?;
    assert_eq!(
        (w.rwa_of(&alice).await?, w.rwa_in_circle().await?),
        (start, 0),
        "frozen grantor"
    );
    assert_eq!(
        w.f.circle
            .view("get_ft_escrow", json!({"grant_id": id}))
            .await?,
        Value::Null
    );
    assert_eq!(
        w.f.circle
            .view("get_grant", json!({"grant_id": id}))
            .await?["status"],
        "offered"
    );
    Ok(())
}

// TC-CT-091-08..09: the token freezes the grantor after the deposit, or takes a fee: the return and the sale fail, nothing is lost or paid for nothing
#[tokio::test]
async fn a_token_that_cannot_send_what_it_took_in_leaves_the_sale_unpaid() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer(90_000_000_000).await?;
    let alice = w.alice.clone();
    // a flat fee of 1: the circle receives AMOUNT - 1 and cannot send AMOUNT
    w.rwa
        .call("set_fee")
        .args_json(json!({"fee": "1"}))
        .transact()
        .await?
        .into_result()?;
    assert!(w.deposit(&alice, AMOUNT, id).await?.is_success());
    assert_eq!(w.rwa_in_circle().await?, AMOUNT - 1);
    let epoch = w.epoch().await?;
    let tried = w.buy("bob", epoch, PRICE, id).await?;
    assert!(tried.is_success(), "{:?}", failure_text(&tried));
    assert!(events(&tried)
        .iter()
        .any(|(n, d)| n == "escrow_failed" && d["reverted"] == true));
    w.assert_clean_after_failure(id, epoch, "bob").await?;
    // the grantor cannot get it back either, and the escrow stays held (not lost): the shortfall is the fee
    let expires =
        w.f.circle
            .view("get_grant", json!({"grant_id": id}))
            .await?["limits"]["expires_at"]
            .as_str()
            .unwrap()
            .parse::<u64>()?;
    w.f.circle.advance_to(expires + 2_000_000_000).await?;
    let back =
        w.f.circle
            .call_as(&alice, "cancel_escrow", json!({"grant_id": id}))
            .await?;
    assert!(back.is_success(), "{:?}", failure_text(&back));
    assert!(events(&back)
        .iter()
        .any(|(n, d)| n == "escrow_failed" && d["step"] == "cancel"));
    assert_eq!(
        w.f.circle
            .view("get_ft_escrow", json!({"grant_id": id}))
            .await?["state"],
        "held"
    );
    assert_eq!(w.rwa_in_circle().await?, AMOUNT - 1);
    Ok(())
}

// TC-CT-091-10: after one or more failed deliveries the grantor still gets the tokens back once the time of the grant has passed
#[tokio::test]
async fn the_grantor_gets_the_tokens_back_after_failed_deliveries() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer(90_000_000_000).await?;
    let alice = w.alice.clone();
    let start = w.rwa_of(&alice).await?;
    w.whitelist(&[w.f.circle.contract.id().as_str(), alice.id().as_str()])
        .await?;
    assert!(w.deposit(&alice, AMOUNT, id).await?.is_success());
    let epoch = w.epoch().await?;
    // bob and carol are not whitelisted: both deliveries fail and are taken back
    for buyer in ["bob", "carol"] {
        let tried = w.buy(buyer, epoch, PRICE, id).await?;
        assert!(
            events(&tried)
                .iter()
                .any(|(n, d)| n == "escrow_failed" && d["reverted"] == true),
            "{buyer}"
        );
        w.assert_clean_after_failure(id, epoch, buyer).await?;
    }
    assert_eq!(w.rwa_in_circle().await?, AMOUNT);
    let expires =
        w.f.circle
            .view("get_grant", json!({"grant_id": id}))
            .await?["limits"]["expires_at"]
            .as_str()
            .unwrap()
            .parse::<u64>()?;
    w.f.circle.advance_to(expires + 2_000_000_000).await?;
    let done =
        w.f.circle
            .call_as(&alice, "cancel_escrow", json!({"grant_id": id}))
            .await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert_eq!(
        (w.rwa_of(&alice).await?, w.rwa_in_circle().await?),
        (start, 0),
        "the tokens are back with the grantor"
    );
    assert_eq!(
        w.f.circle
            .view("get_ft_escrow", json!({"grant_id": id}))
            .await?["state"],
        "cancelled"
    );
    assert_eq!(
        w.f.circle
            .view("get_grant", json!({"grant_id": id}))
            .await?["status"],
        "revoked"
    );
    Ok(())
}
