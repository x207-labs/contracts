//! CT-070 in near-sandbox (F8, spec 15): `offer_grant`, `accept_grant`, `get_grant`, `grants_of`. Needs
//! `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{events, failure_text};
use common::funded::Funded;
use common::circle::CREDIT;
use serde_json::{json, Value};

fn entry(asset_type: &str, class: &str) -> Value {
    json!({
        "asset_type": asset_type, "class": class, "delivery_methods": ["near_access_key"],
        "accepted_proofs": ["rpc_access_key", "zktls"], "terms_hash": "ab".repeat(32)
    })
}

fn input(asset_type: &str, expires_at: u64) -> Value {
    json!({
        "asset_type": asset_type, "delivery_method": "near_access_key",
        "limits": {"scope": "{\"receiver_id\":\"api.near\",\"method_names\":[\"get\"],\"allowance\":\"1\"}", "quota": "100", "expires_at": expires_at.to_string()},
        "unit": "call", "unit_price": "10000", "operator": null, "proof_hash": null, "proof_kind": null
    })
}

async fn three() -> anyhow::Result<Funded> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in ["scraper", "translator", "summarizer"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    let dao = f.account("dao").await?;
    for (t, c) in [("near-rpc:access", "B"), ("pay:nft", "X")] {
        assert!(f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": t, "entry": entry(t, c)})).await?.is_success());
    }
    Ok(f)
}

// TC-CT-070-01..04: a valid offer, the class comes from D10, the views
#[tokio::test]
async fn a_member_offers_and_another_accepts() -> anyhow::Result<()> {
    let mut f = three().await?;
    let (scraper, translator, summarizer) = (f.accounts["scraper"].clone(), f.account("translator").await?, f.account("summarizer").await?);
    let expires = f.circle.now_ns().await? + 3_600_000_000_000;
    let offer = f.circle.call_as(&scraper, "offer_grant", json!({"input": input("near-rpc:access", expires)})).await?;
    assert!(offer.is_success(), "{:?}", failure_text(&offer));
    assert!(events(&offer).iter().any(|(n, d)| n == "grant_offered" && d["grant_id"] == 1 && d["class"] == "B" && d["unit_price"] == "10000" && d["unit"] == "call"));
    assert_eq!(offer.json::<u64>()?, 1);
    let grant = f.circle.view("get_grant", json!({"grant_id": 1})).await?;
    assert_eq!(grant["status"], "offered");
    assert_eq!(grant["grantee"], Value::Null);
    assert_eq!(grant["class"], "B");
    assert_eq!(grant["grantor"], scraper.id().as_str());
    assert_eq!(f.circle.view("get_grant", json!({"grant_id": 9})).await?, Value::Null);

    // refusals before the accept
    let own = f.circle.call_as(&scraper, "accept_grant", json!({"grant_id": 1, "delivery_pubkey": null})).await?;
    assert!(failure_text(&own).expect("refused").contains("ERR_GRANT_SELF"));
    let unknown = f.circle.call_as(&translator, "accept_grant", json!({"grant_id": 7, "delivery_pubkey": null})).await?;
    assert!(failure_text(&unknown).expect("refused").contains("ERR_UNKNOWN_GRANT"));

    let pubkey = f.circle.key_for("translator");
    let accepted = f.circle.call_as(&translator, "accept_grant", json!({"grant_id": 1, "delivery_pubkey": pubkey})).await?;
    assert!(accepted.is_success(), "{:?}", failure_text(&accepted));
    assert!(events(&accepted).iter().any(|(n, d)| n == "grant_activated" && d["grant_id"] == 1 && d["grantee"] == translator.id().as_str()));
    let grant = f.circle.view("get_grant", json!({"grant_id": 1})).await?;
    assert_eq!((grant["status"].as_str(), grant["grantee"].as_str()), (Some("active"), Some(translator.id().as_str())));
    assert_eq!(grant["delivery_pubkey"], pubkey);

    let again = f.circle.call_as(&summarizer, "accept_grant", json!({"grant_id": 1, "delivery_pubkey": null})).await?;
    assert!(failure_text(&again).expect("refused").contains("ERR_GRANT_NOT_OFFERED"));

    let by_grantor = f.circle.view("grants_of", json!({"account_id": scraper.id(), "role": "grantor"})).await?;
    let by_grantee = f.circle.view("grants_of", json!({"account_id": translator.id(), "role": "grantee"})).await?;
    assert_eq!((by_grantor.as_array().unwrap().len(), by_grantee.as_array().unwrap().len()), (1, 1));
    assert_eq!(f.circle.view("grants_of", json!({"account_id": scraper.id(), "role": "grantee"})).await?, json!([]));
    Ok(())
}

// TC-CT-070-05..09: every refusal changes nothing (the next grant is still number 1)
#[tokio::test]
async fn refused_offers_change_nothing() -> anyhow::Result<()> {
    let mut f = three().await?;
    let scraper = f.accounts["scraper"].clone();
    let expires = f.circle.now_ns().await? + 3_600_000_000_000;
    let with = |edit: &dyn Fn(&mut Value)| {
        let mut v = input("near-rpc:access", expires);
        edit(&mut v);
        v
    };
    let cases: Vec<(&str, Value)> = vec![
        ("ERR_ASSET_TYPE_UNKNOWN", input("none:none", expires)),
        ("ERR_ASSET_CLASS_FORBIDDEN", input("pay:nft", expires)),
        ("ERR_DELIVERY_NOT_ALLOWED", with(&|v| v["delivery_method"] = json!("carrier_pigeon"))),
        ("ERR_BAD_GRANT", with(&|v| v["unit"] = json!("hour"))),
        ("ERR_BAD_GRANT", with(&|v| v["unit_price"] = json!("0"))),
        ("ERR_BAD_GRANT", with(&|v| v["limits"]["expires_at"] = json!("1"))),
        ("ERR_BAD_GRANT", with(&|v| v["limits"]["scope"] = json!("not json"))),
        ("ERR_BAD_GRANT", with(&|v| v["limits"]["quota"] = json!("0"))),
    ];
    for (code, case) in cases {
        let refused = f.circle.call_as(&scraper, "offer_grant", json!({"input": case.clone()})).await?;
        assert!(failure_text(&refused).expect("refused").contains(code), "{code}: {case}");
    }
    // not a member, and a member that has not staked yet
    let stranger = f.account("stranger").await?;
    let no = f.circle.call_as(&stranger, "offer_grant", json!({"input": input("near-rpc:access", expires)})).await?;
    assert!(failure_text(&no).expect("refused").contains("ERR_NOT_MEMBER"));
    let newbie = f.account("newbie").await?;
    assert!(f.join("newbie", Funded::join_deposit()).await?.is_success());
    let pending = f.circle.call_as(&newbie, "offer_grant", json!({"input": input("near-rpc:access", expires)})).await?;
    assert!(failure_text(&pending).expect("refused").contains("ERR_GRANT_NOT_ACTIVE_MEMBER"));

    assert_eq!(f.circle.view("get_grant", json!({"grant_id": 1})).await?, Value::Null);
    let first = f.circle.call_as(&scraper, "offer_grant", json!({"input": input("near-rpc:access", expires)})).await?;
    assert_eq!(first.json::<u64>()?, 1);

    // a member that is not active cannot accept
    let late = f.circle.call_as(&newbie, "accept_grant", json!({"grant_id": 1, "delivery_pubkey": null})).await?;
    assert!(failure_text(&late).expect("refused").contains("ERR_GRANT_NOT_ACTIVE_MEMBER"));
    Ok(())
}

// TC-CT-070-10: at most 16 open grants per grantor
#[tokio::test]
async fn a_grantor_holds_at_most_16_open_grants() -> anyhow::Result<()> {
    let mut f = three().await?;
    let scraper = f.accounts["scraper"].clone();
    let expires = f.circle.now_ns().await? + 3_600_000_000_000;
    for n in 1..=16u64 {
        let ok = f.circle.call_as(&scraper, "offer_grant", json!({"input": input("near-rpc:access", expires)})).await?;
        assert_eq!(ok.json::<u64>()?, n);
    }
    let over = f.circle.call_as(&scraper, "offer_grant", json!({"input": input("near-rpc:access", expires)})).await?;
    assert!(failure_text(&over).expect("refused").contains("ERR_BAD_GRANT"));
    // another grantor is not affected
    let translator = f.account("translator").await?;
    assert!(f.circle.call_as(&translator, "offer_grant", json!({"input": input("near-rpc:access", expires)})).await?.is_success());
    Ok(())
}

// TC-CT-070-11: an offer that ran out cannot be accepted
#[tokio::test]
async fn an_expired_offer_cannot_be_accepted() -> anyhow::Result<()> {
    let mut f = three().await?;
    let (scraper, translator) = (f.accounts["scraper"].clone(), f.account("translator").await?);
    let expires = f.circle.now_ns().await? + 60_000_000_000;
    assert!(f.circle.call_as(&scraper, "offer_grant", json!({"input": input("near-rpc:access", expires)})).await?.is_success());
    f.circle.advance_to(expires + 1_000_000_000).await?;
    let late = f.circle.call_as(&translator, "accept_grant", json!({"grant_id": 1, "delivery_pubkey": null})).await?;
    assert!(failure_text(&late).expect("refused").contains("ERR_GRANT_EXPIRED"));
    assert_eq!(f.circle.view("get_grant", json!({"grant_id": 1})).await?["status"], "offered");
    Ok(())
}
