//! CT-096 in near-sandbox: the official contracts of an asset type (the registry D10 side list, set by the owner = the DAO account of the fixture). A token from a contract
//! that is not on the list is refused at `offer_grant` and at the deposit, an empty list is the old behaviour, and a held escrow is not touched by a change of the list.
//! Needs `cargo near build non-reproducible-wasm` first. Run one test at a time on a loaded machine.

mod common;

use common::circle::{events, failure_text};
use common::ft_escrow::{deploy_rwa, offer, world, AMOUNT, PRICE};
use serde_json::{json, Value};

const TYPE: &str = "rwa:bond";

// TC-CT-096-01: who may set it, what it may be set for, the view and the event
#[tokio::test]
async fn only_the_owner_sets_a_list_for_a_known_class_a_type() -> anyhow::Result<()> {
    let mut w = world().await?;
    let dao = w.f.account("dao").await?;
    let bob = w.bob.clone();
    let rwa = w.rwa.id().to_string();
    let not_owner = w.f.circle.call_as(&bob, "set_asset_contracts", json!({"asset_type": TYPE, "contracts": [rwa]})).await?;
    assert!(failure_text(&not_owner).expect("refused").contains("ERR_NOT_OWNER"));
    let unknown = w.f.circle.call_as(&dao, "set_asset_contracts", json!({"asset_type": "nope:type", "contracts": [rwa]})).await?;
    assert!(failure_text(&unknown).expect("refused").contains("ERR_ASSET_TYPE_UNKNOWN"));
    assert_eq!(w.f.circle.view("get_asset_contracts", json!({"asset_type": TYPE})).await?, json!([]));
    let set = w.f.circle.call_as(&dao, "set_asset_contracts", json!({"asset_type": TYPE, "contracts": [rwa]})).await?;
    assert!(set.is_success(), "{:?}", failure_text(&set));
    assert!(events(&set).iter().any(|(n, d)| n == "asset_contracts_set" && d["asset_type"] == TYPE && d["contracts"] == json!([rwa])));
    assert_eq!(w.f.circle.view("get_asset_contracts", json!({"asset_type": TYPE})).await?, json!([rwa]));
    Ok(())
}

// TC-CT-096-02..06: a fake token with the right name
#[tokio::test]
async fn a_token_from_a_contract_off_the_list_is_refused_at_the_offer_and_at_the_deposit() -> anyhow::Result<()> {
    let mut w = world().await?;
    let dao = w.f.account("dao").await?;
    let alice = w.alice.clone();
    let official = w.rwa.id().to_string();
    let fake = deploy_rwa(&w.f).await?;
    fake.call("mint").args_json(json!({"account_id": alice.id(), "amount": (10 * AMOUNT).to_string()})).transact().await?.into_result()?;
    let expires = w.f.circle.now_ns().await? + 3_600_000_000_000;

    // a grant offered BEFORE the list exists, that names the fake token
    let early = w.f.circle.call_as(&alice, "offer_grant", offer(expires, fake.id().as_str(), AMOUNT)).await?.json::<u64>()?;
    assert!(w.f.circle.call_as(&dao, "set_asset_contracts", json!({"asset_type": TYPE, "contracts": [official]})).await?.is_success());

    // the offer that names the fake token is refused now, before anything is put in escrow
    let refused = w.f.circle.call_as(&alice, "offer_grant", offer(expires, fake.id().as_str(), AMOUNT)).await?;
    assert!(failure_text(&refused).expect("refused").contains("ERR_CONTRACT_NOT_OFFICIAL"));
    // the early grant is checked again at the deposit: the fake token comes back
    let msg = json!({"action": "escrow", "grant_id": early}).to_string();
    let before = w.balance(&fake, alice.id().as_str()).await?;
    let _ = w.deposit_with(&fake, &alice, AMOUNT, &msg).await?;
    assert_eq!((w.balance(&fake, alice.id().as_str()).await?, w.balance(&fake, w.f.circle.contract.id().as_str()).await?), (before, 0));
    assert_eq!(w.f.circle.view("get_ft_escrow", json!({"grant_id": early})).await?, Value::Null);

    // the official token goes through
    let id = w.offer(3_600_000_000_000).await?;
    assert!(w.deposit(&alice, AMOUNT, id).await?.is_success());
    assert_eq!(w.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?["state"], "held");
    assert_eq!(w.in_circle_of(&w.rwa).await?, AMOUNT);

    // a change of the list does not reach into the escrow that is held
    assert!(w.f.circle.call_as(&dao, "set_asset_contracts", json!({"asset_type": TYPE, "contracts": [fake.id()]})).await?.is_success());
    assert_eq!(w.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?["state"], "held");
    Ok(())
}

// TC-CT-096-07: an empty list is the old behaviour
#[tokio::test]
async fn an_empty_list_takes_any_contract_again() -> anyhow::Result<()> {
    let mut w = world().await?;
    let dao = w.f.account("dao").await?;
    let alice = w.alice.clone();
    let fake = deploy_rwa(&w.f).await?;
    fake.call("mint").args_json(json!({"account_id": alice.id(), "amount": (10 * AMOUNT).to_string()})).transact().await?.into_result()?;
    let expires = w.f.circle.now_ns().await? + 3_600_000_000_000;
    assert!(w.f.circle.call_as(&dao, "set_asset_contracts", json!({"asset_type": TYPE, "contracts": [w.rwa.id()]})).await?.is_success());
    assert!(failure_text(&w.f.circle.call_as(&alice, "offer_grant", offer(expires, fake.id().as_str(), AMOUNT)).await?).is_some());
    assert!(w.f.circle.call_as(&dao, "set_asset_contracts", json!({"asset_type": TYPE, "contracts": []})).await?.is_success());
    assert_eq!(w.f.circle.view("get_asset_contracts", json!({"asset_type": TYPE})).await?, json!([]));
    let id = w.f.circle.call_as(&alice, "offer_grant", offer(expires, fake.id().as_str(), AMOUNT)).await?.json::<u64>()?;
    let msg = json!({"action": "escrow", "grant_id": id}).to_string();
    assert!(w.deposit_with(&fake, &alice, AMOUNT, &msg).await?.is_success());
    assert_eq!(w.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?["state"], "held");
    Ok(())
}

// TC-CT-096-08 (review C1): a token put in escrow before the list is set cannot be bought after it; the grantor still takes it back
#[tokio::test]
async fn a_fake_token_in_escrow_before_the_list_cannot_be_bought_but_can_be_taken_back() -> anyhow::Result<()> {
    let mut w = world().await?;
    let dao = w.f.account("dao").await?;
    let alice = w.alice.clone();
    let official = w.rwa.id().to_string();
    let fake = deploy_rwa(&w.f).await?;
    fake.call("mint").args_json(json!({"account_id": alice.id(), "amount": (10 * AMOUNT).to_string()})).transact().await?.into_result()?;
    let expires = w.f.circle.now_ns().await? + 90_000_000_000;
    let id = w.f.circle.call_as(&alice, "offer_grant", offer(expires, fake.id().as_str(), AMOUNT)).await?.json::<u64>()?;
    let msg = json!({"action": "escrow", "grant_id": id}).to_string();
    assert!(w.deposit_with(&fake, &alice, AMOUNT, &msg).await?.is_success());
    assert_eq!(w.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?["state"], "held");

    assert!(w.f.circle.call_as(&dao, "set_asset_contracts", json!({"asset_type": TYPE, "contracts": [official]})).await?.is_success());
    let epoch = w.epoch().await?;
    let buy = w.buy("bob", epoch, PRICE, id).await?;
    assert!(failure_text(&buy).expect("refused").contains("ERR_CONTRACT_NOT_OFFICIAL"), "{:?}", failure_text(&buy));
    assert_eq!(w.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?["state"], "held");

    w.f.circle.advance_to(expires + 2_000_000_000).await?;
    let back = w.f.circle.call_as(&alice, "cancel_escrow", json!({"grant_id": id})).await?;
    assert!(back.is_success(), "{:?}", failure_text(&back));
    assert_eq!(w.balance(&fake, alice.id().as_str()).await?, 10 * AMOUNT);
    Ok(())
}
