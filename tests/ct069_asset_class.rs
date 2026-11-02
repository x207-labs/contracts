//! CT-069 in near-sandbox (F8, spec 15): the D10 registry. Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{events, failure_text};
use common::funded::Funded;
use serde_json::{json, Value};

fn entry(asset_type: &str, class: &str) -> Value {
    json!({
        "asset_type": asset_type, "class": class, "delivery_methods": ["near_access_key"],
        "accepted_proofs": ["rpc_access_key", "zktls"], "terms_hash": "ab".repeat(32)
    })
}

// TC-CT-069-01..05
#[tokio::test]
async fn the_owner_registers_classes_and_the_views_return_them() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let dao = f.account("dao").await?;
    let other = f.account("scraper").await?;

    let set = f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": "near-rpc:access", "entry": entry("near-rpc:access", "B")})).await?;
    assert!(set.is_success(), "{:?}", failure_text(&set));
    assert!(events(&set).iter().any(|(n, d)| n == "asset_class_set" && d["asset_type"] == "near-rpc:access" && d["class"] == "B"));
    assert_eq!(f.circle.view("get_asset_class", json!({"asset_type": "near-rpc:access"})).await?, entry("near-rpc:access", "B"));
    assert_eq!(f.circle.view("get_asset_class", json!({"asset_type": "none:none"})).await?, Value::Null);

    // an update replaces the entry and does not add a second row
    assert!(f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": "near-rpc:access", "entry": entry("near-rpc:access", "C")})).await?.is_success());
    assert!(f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": "pay:nft", "entry": entry("pay:nft", "X")})).await?.is_success());
    let all = f.circle.view("list_asset_classes", json!({"from_index": 0, "limit": 10})).await?;
    assert_eq!(all, json!([entry("near-rpc:access", "C"), entry("pay:nft", "X")]));
    assert_eq!(f.circle.view("list_asset_classes", json!({"from_index": 1, "limit": 10})).await?, json!([entry("pay:nft", "X")]));
    assert_eq!(f.circle.view("list_asset_classes", json!({"from_index": 0, "limit": 0})).await?, json!([]));
    assert_eq!(f.circle.view("list_asset_classes", json!({"from_index": 5, "limit": 10})).await?, json!([]));

    // refusals leave nothing behind
    let denied = f.circle.call_as(&other, "set_asset_class", json!({"asset_type": "x:y", "entry": entry("x:y", "A")})).await?;
    assert!(failure_text(&denied).expect("refused").contains("ERR_NOT_OWNER"));
    for (kind, e) in [
        ("nocolon", entry("nocolon", "A")),
        ("a:b:c", entry("a:b:c", "A")),
        ("x:y", entry("x:z", "A")),
        ("x:y", json!({"asset_type": "x:y", "class": "A", "delivery_methods": [], "accepted_proofs": ["telepathy"], "terms_hash": "ab".repeat(32)})),
        ("x:y", json!({"asset_type": "x:y", "class": "A", "delivery_methods": [], "accepted_proofs": [], "terms_hash": "AB".repeat(32)})),
    ] {
        let refused = f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": kind, "entry": e})).await?;
        assert!(failure_text(&refused).expect("refused").contains("ERR_BAD_ASSET_TYPE"), "{kind}");
    }
    assert_eq!(f.circle.view("list_asset_classes", json!({"from_index": 0, "limit": 10})).await?.as_array().unwrap().len(), 2);
    Ok(())
}
