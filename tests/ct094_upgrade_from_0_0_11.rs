//! CT-094 in near-sandbox: the upgrade from interface 0.0.11 to 0.0.12. The circle starts on the 0.0.11 wasm (the wasm at
//! `target/near/x207_circle.wasm`), gets a member set, a committed bundle and a pending params change, and is upgraded through
//! `propose_upgrade` and `apply_upgrade`. The Params stored inside each Epoch record are Borsh and do not change; the two rules of the
//! bond (`bond_threshold`, `bond_bps`) read their defaults until the owner sets them.
//!
//! Skipped unless `X207_NEW_WASM` names the new wasm. How to run it: build the 0.0.11 source (`git archive 3d148f9`), copy its wasm to
//! `target/near/x207_circle.wasm`, then `X207_NEW_WASM=<new wasm> cargo test --test ct094_upgrade_from_0_0_11 -- --nocapture`.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::funded::Funded;
use near_workspaces::types::Gas;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const ALL: [&str; 2] = ["scraper", "translator"];

// TC-CT-094-UPGRADE-01..03
#[tokio::test]
async fn state_written_by_0_0_11_reads_the_same_and_the_bond_rules_start_from_their_defaults() -> anyhow::Result<()> {
    let Ok(new_path) = std::env::var("X207_NEW_WASM") else {
        println!("skipped: X207_NEW_WASM is not set");
        return Ok(());
    };
    let new_wasm = std::fs::read(new_path)?;
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    assert_eq!(f.circle.view("interface_version", json!({})).await?, "0.0.11", "target/near/x207_circle.wasm must be the 0.0.11 build");
    for name in ALL {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 800 * CREDIT).await?.is_success());
    }
    let dao = f.account("dao").await?;
    let first = f.circle.commit_bundle(1, &[("scraper", "translator", 5 * CREDIT), ("translator", "scraper", 4 * CREDIT)], &ALL).await?;
    assert!(first.is_success(), "{:?}", failure_text(&first));
    let params_before = f.circle.view("get_params", json!({})).await?;
    let before = json!({
        "params": params_before,
        "epoch": f.circle.view("get_epoch", json!({})).await?,
        "bundle_1": f.circle.view("get_bundle", json!({"bundle_id": 1})).await?,
        "headroom": [f.circle.view("headroom", json!({"account_id": account_of("scraper")})).await?, f.circle.view("headroom", json!({"account_id": account_of("translator")})).await?],
        "pool": f.circle.view("get_pool", json!({})).await?,
        "owner": f.circle.view("get_owner", json!({})).await?,
    });

    let hash = near_sdk::bs58::encode(Sha256::digest(&new_wasm)).into_string();
    assert!(f.circle.call_as(&dao, "propose_upgrade", json!({"code_hash": hash})).await?.is_success());
    let mut stored = Sha256::digest(&new_wasm).to_vec();
    stored.extend_from_slice(&1u64.to_le_bytes());
    f.circle.worker.patch_state(f.circle.contract.id(), b"x207_upgrade", &stored).await?;
    f.circle.worker.fast_forward(2).await?;
    let applied = dao.call(f.circle.contract.id(), "apply_upgrade").args(new_wasm).gas(Gas::from_tgas(300)).transact().await?;
    assert!(applied.is_success(), "{:?}", failure_text(&applied));
    assert!(events(&applied).iter().any(|(n, _)| n == "upgrade_applied"));
    assert_eq!(f.circle.view("interface_version", json!({})).await?, x207_circle::INTERFACE_VERSION);

    // 1. everything that was written reads the same, except that get_params has the two new fields with their defaults
    let after_params = f.circle.view("get_params", json!({})).await?;
    let mut expected = before["params"].clone();
    expected["bond_threshold"] = json!("100000000");
    expected["bond_bps"] = json!(2000);
    assert_eq!(after_params, expected);
    for key in ["epoch", "bundle_1", "headroom", "pool", "owner"] {
        let now = match key {
            "epoch" => f.circle.view("get_epoch", json!({})).await?,
            "bundle_1" => f.circle.view("get_bundle", json!({"bundle_id": 1})).await?,
            "headroom" => json!([f.circle.view("headroom", json!({"account_id": account_of("scraper")})).await?, f.circle.view("headroom", json!({"account_id": account_of("translator")})).await?]),
            "pool" => f.circle.view("get_pool", json!({})).await?,
            _ => f.circle.view("get_owner", json!({})).await?,
        };
        // the params that an epoch record carries show the two new fields with their defaults
        let (mut now, mut was) = (now, before[key].clone());
        for v in [&mut now, &mut was] {
            if let Some(p) = v.get_mut("params").and_then(Value::as_object_mut) {
                p.remove("bond_threshold");
                p.remove("bond_bps");
            }
        }
        assert_eq!(now, was, "{key} reads the same after the upgrade");
    }
    // 2. no deal exists for the old bundle, no bond is locked, nobody is frozen
    assert_eq!(f.circle.view("get_deal", json!({"bundle_id": 1})).await?, Value::Null);
    assert_eq!(f.circle.view("bond_locked", json!({"account_id": account_of("scraper")})).await?, "0");
    assert_eq!(f.circle.view("is_frozen", json!({"account_id": account_of("scraper")})).await?, false);
    // 3. a bundle without an arbiter still commits on the new wasm, and the owner can set the rules of the bond
    let second = f.circle.commit_bundle(1, &[("scraper", "translator", CREDIT), ("translator", "scraper", CREDIT)], &ALL).await?;
    assert!(second.is_success(), "{:?}", failure_text(&second));
    let mut set = after_params.clone();
    set["bond_threshold"] = json!("50000000");
    set["bond_bps"] = json!(1500);
    let r = f.circle.call_as(&dao, "set_params", json!({"params": set})).await?;
    assert!(r.is_success(), "{:?}", failure_text(&r));
    let pending = f.circle.view("get_pending_params", json!({})).await?;
    assert_eq!(pending["params"]["bond_threshold"], "50000000");
    assert_eq!(pending["params"]["bond_bps"], 1500);
    f.assert_d6().await
}
