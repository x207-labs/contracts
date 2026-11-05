//! CT-072 in near-sandbox (F8, spec 15.2): the proof of a grant. Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{failure_text, CREDIT};
use common::funded::Funded;
use serde_json::{json, Value};

fn input(proof_hash: Value, proof_kind: Value, expires_at: u64) -> Value {
    json!({
        "asset_type": "near-rpc:access", "delivery_method": "near_access_key",
        "limits": {"scope": "{}", "quota": null, "expires_at": expires_at.to_string()},
        "unit": "once", "unit_price": "1", "operator": null, "proof_hash": proof_hash, "proof_kind": proof_kind
    })
}

// TC-CT-072-01..05
#[tokio::test]
async fn a_proof_needs_an_accepted_kind_and_a_sha256_hash() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let scraper = f.account("scraper").await?;
    assert!(f
        .join("scraper", Funded::join_deposit())
        .await?
        .is_success());
    assert!(f.stake(&scraper, 100 * CREDIT).await?.is_success());
    let dao = f.account("dao").await?;
    // this asset type accepts rpc_access_key only
    let entry = json!({"asset_type": "near-rpc:access", "class": "B", "delivery_methods": ["near_access_key"], "accepted_proofs": ["rpc_access_key"], "terms_hash": "ef".repeat(32)});
    assert!(f
        .circle
        .call_as(
            &dao,
            "set_asset_class",
            json!({"asset_type": "near-rpc:access", "entry": entry})
        )
        .await?
        .is_success());
    let expires = f.circle.now_ns().await? + 3_600_000_000_000;
    let hash = "0a".repeat(32);

    for (case, code) in [
        (
            input(json!(hash), json!("zktls"), expires),
            "ERR_PROOF_KIND_NOT_ACCEPTED",
        ),
        (
            input(json!(hash), json!("telepathy"), expires),
            "ERR_PROOF_KIND_NOT_ACCEPTED",
        ),
        (
            input(json!("0A".repeat(32)), json!("rpc_access_key"), expires),
            "ERR_BAD_GRANT",
        ),
        (
            input(json!("0a"), json!("rpc_access_key"), expires),
            "ERR_BAD_GRANT",
        ),
        (input(json!(hash), Value::Null, expires), "ERR_BAD_GRANT"),
        (
            input(Value::Null, json!("rpc_access_key"), expires),
            "ERR_BAD_GRANT",
        ),
    ] {
        let refused = f
            .circle
            .call_as(&scraper, "offer_grant", json!({"input": case.clone()}))
            .await?;
        assert!(
            failure_text(&refused).expect("refused").contains(code),
            "{code}: {case}"
        );
    }
    assert_eq!(
        f.circle.view("get_grant", json!({"grant_id": 1})).await?,
        Value::Null
    );

    let ok = f
        .circle
        .call_as(
            &scraper,
            "offer_grant",
            json!({"input": input(json!(hash), json!("rpc_access_key"), expires)}),
        )
        .await?;
    assert!(ok.is_success(), "{:?}", failure_text(&ok));
    let grant = f.circle.view("get_grant", json!({"grant_id": 1})).await?;
    assert_eq!(
        (grant["proof_hash"].as_str(), grant["proof_kind"].as_str()),
        (Some(hash.as_str()), Some("rpc_access_key"))
    );
    // no proof at all is fine
    assert!(f
        .circle
        .call_as(
            &scraper,
            "offer_grant",
            json!({"input": input(Value::Null, Value::Null, expires)})
        )
        .await?
        .is_success());
    Ok(())
}
