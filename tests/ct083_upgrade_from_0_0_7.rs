//! CT-083 in near-sandbox: the real upgrade path from interface 0.0.7 to the code under test. The circle starts on the 0.0.7 wasm
//! (the wasm at `target/near/x207_circle.wasm`), gets F5 (bundle, intents), F8 (grants, class A escrow) and D2 state, and is then
//! upgraded through `propose_upgrade` and `apply_upgrade` (which runs `migrate`). Everything written before must read the same after.
//!
//! Skipped unless `X207_NEW_WASM` names the new wasm, because a plain `cargo test` has only one wasm. How to run it:
//!   1. build the 0.0.7 source (`git archive <commit before the 0.0.8 commit>`) and copy its wasm to `target/near/x207_circle.wasm`;
//!   2. `X207_NEW_WASM=<path of the new wasm> cargo test --test ct083_upgrade_from_0_0_7 -- --nocapture`.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const PRICE: u128 = 10 * CREDIT;
const CYCLE: [(&str, &str, u128); 3] = [("scraper", "translator", 6 * CREDIT), ("translator", "summarizer", 10 * CREDIT), ("summarizer", "scraper", 8 * CREDIT)];
const ALL: [&str; 3] = ["scraper", "translator", "summarizer"];

async fn snapshot(f: &Funded) -> anyhow::Result<Value> {
    let c = &f.circle;
    let h = |n: u8| near_sdk::bs58::encode([n; 32]).into_string();
    Ok(json!({
        "grant_1": c.view("get_grant", json!({"grant_id": 1})).await?,
        "grant_2": c.view("get_grant", json!({"grant_id": 2})).await?,
        "escrow_2": c.view("get_escrow", json!({"grant_id": 2})).await?,
        "bundle_1": c.view("get_bundle", json!({"bundle_id": 1})).await?,
        "intent_1": c.view("get_intent", json!({"intent_hash": h(1)})).await?,
        "intent_2": c.view("get_intent", json!({"intent_hash": h(2)})).await?,
        "classes": c.view("list_asset_classes", json!({"from_index": 0, "limit": 50})).await?,
        "headroom": [c.view("headroom", json!({"account_id": account_of("scraper")})).await?, c.view("headroom", json!({"account_id": account_of("translator")})).await?],
        "obligation": c.view("obligation", json!({"epoch": 1, "debtor": account_of("scraper"), "creditor": account_of("translator")})).await?,
        "params": c.view("get_params", json!({})).await?,
        "epoch": c.view("get_epoch", json!({})).await?,
        "owner": c.view("get_owner", json!({})).await?,
        "pool": c.view("get_pool", json!({})).await?,
    }))
}

// TC-CT-083-01..06
#[tokio::test]
async fn state_written_by_0_0_7_reads_the_same_after_the_upgrade() -> anyhow::Result<()> {
    let Ok(new_path) = std::env::var("X207_NEW_WASM") else {
        println!("skipped: X207_NEW_WASM is not set");
        return Ok(());
    };
    let new_wasm = std::fs::read(new_path)?;
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    assert_eq!(f.circle.view("interface_version", json!({})).await?, "0.0.7", "target/near/x207_circle.wasm must be the 0.0.7 build");
    for name in ALL {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 150 * CREDIT).await?.is_success());
    }
    let (dao, scraper, translator, summarizer) = (f.account("dao").await?, f.accounts["scraper"].clone(), f.accounts["translator"].clone(), f.accounts["summarizer"].clone());
    let entry = |t: &str, class: &str, method: &str| json!({"asset_type": t, "class": class, "delivery_methods": [method], "accepted_proofs": [], "terms_hash": "5a".repeat(32)});
    for (t, class, method) in [("near-rpc:access", "B", "near_access_key"), ("nft:ticket", "A", "escrow_nft")] {
        assert!(f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": t, "entry": entry(t, class, method)})).await?.is_success());
    }
    let expires = f.circle.now_ns().await? + 3_600_000_000_000;
    let offer = |t: &str, method: &str, scope: &str| json!({"input": {"asset_type": t, "delivery_method": method,
        "limits": {"scope": scope, "quota": null, "expires_at": expires.to_string()}, "unit": "once", "unit_price": PRICE.to_string(), "operator": null, "proof_hash": null, "proof_kind": null}});
    // F8: a class B grant that is accepted, and a class A grant with its NFT in the escrow (0.0.7 took any scope)
    let b = f.circle.call_as(&scraper, "offer_grant", offer("near-rpc:access", "near_access_key", "{\"receiver_id\":\"api.near\",\"method_names\":[\"get\"],\"allowance\":\"1\"}")).await?;
    assert!(b.is_success(), "{:?}", failure_text(&b));
    assert!(f.circle.call_as(&translator, "accept_grant", json!({"grant_id": 1, "delivery_pubkey": null})).await?.is_success());
    let a = f.circle.call_as(&scraper, "offer_grant", offer("nft:ticket", "escrow_nft", "{}")).await?;
    assert!(a.is_success(), "{:?}", failure_text(&a));
    let nft = f.circle.worker.dev_deploy(&std::fs::read(common::probe_wasm("mock_nft"))?).await?;
    nft.call("new").transact().await?.into_result()?;
    nft.call("nft_mint").args_json(json!({"token_id": "t1", "owner_id": scraper.id()})).transact().await?.into_result()?;
    let deposit = scraper
        .call(nft.id(), "nft_transfer_call")
        .args_json(json!({"receiver_id": f.circle.contract.id(), "token_id": "t1", "msg": json!({"grant_id": 2}).to_string()}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(150))
        .transact()
        .await?;
    assert!(deposit.is_success(), "{:?}", failure_text(&deposit));
    assert_eq!(f.circle.view("get_escrow", json!({"grant_id": 2})).await?["state"], "held");
    // F5: two intents, one bundle with its reservations; D2: a registered voucher
    for n in [1u8, 2] {
        let hash = near_sdk::bs58::encode([n; 32]).into_string();
        assert!(f.circle.call_as(&summarizer, "post_intent", json!({"intent_hash": hash})).await?.is_success());
    }
    let commit = f.circle.commit_bundle(1, &CYCLE, &ALL).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    assert!(f.circle.register("scraper", "translator", 1, 4 * CREDIT).await?.is_success());
    let before = snapshot(&f).await?;

    // the upgrade: propose, move the timelock into the past as ct055 does, apply with the new wasm
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

    // 1. everything that was written reads the same
    assert_eq!(snapshot(&f).await?, before);
    // 2. the paged grants_of lists the old index (the ended-grant cleanup only starts with new endings)
    let page = |who: &near_workspaces::Account, role: &str| json!({"account_id": who.id(), "role": role, "from_index": 0, "limit": 10});
    let ids = |v: Value| v.as_array().unwrap().iter().map(|g| g["grant_id"].as_u64().unwrap()).collect::<Vec<_>>();
    assert_eq!(ids(f.circle.view("grants_of", page(&scraper, "grantor")).await?), vec![1, 2]);
    assert_eq!(ids(f.circle.view("grants_of", page(&translator, "grantee")).await?), vec![1]);
    // 3. an intent posted before the upgrade has no counter: it can still be removed by its owner, and a new one is accepted
    let h1 = near_sdk::bs58::encode([1u8; 32]).into_string();
    assert!(failure_text(&f.circle.call_as(&scraper, "remove_intent", json!({"intent_hash": h1})).await?).expect("refused").contains("ERR_NOT_INTENT_OWNER"));
    assert!(f.circle.call_as(&summarizer, "remove_intent", json!({"intent_hash": h1})).await?.is_success());
    assert_eq!(f.circle.view("get_intent", json!({"intent_hash": h1})).await?, Value::Null);
    let h3 = near_sdk::bs58::encode([3u8; 32]).into_string();
    assert!(f.circle.call_as(&summarizer, "post_intent", json!({"intent_hash": h3})).await?.is_success());
    // 4. the old bundle keeps its reservation, a new bundle works on top of it, and the counters start from zero
    let second = f.circle.commit_bundle(1, &[("scraper", "translator", CREDIT), ("translator", "summarizer", CREDIT), ("summarizer", "scraper", CREDIT)], &ALL).await?;
    assert!(second.is_success(), "{:?}", failure_text(&second));
    assert_eq!(second.json::<u64>()?, 2);
    // 5. class A: a buyer completes the escrow that was filled under 0.0.7 (its scope is "{}": only a new deposit checks the scope)
    let epoch = f.circle.view("get_epoch", json!({})).await?["epoch"].as_u64().unwrap();
    let (voucher, signature) = f.circle.next_voucher("translator", "scraper", epoch, PRICE);
    let buy = f.circle.call_as(&translator, "complete_escrow", json!({"grant_id": 2, "voucher": voucher, "signature": signature})).await?;
    assert!(buy.is_success(), "{:?}", failure_text(&buy));
    let owner: Value = nft.view("nft_token").args_json(json!({"token_id": "t1"})).await?.json()?;
    assert_eq!(owner["owner_id"], translator.id().as_str());
    // 6. a revoke after the upgrade takes the grant out of the index
    let revoke = f.circle.call_as(&scraper, "revoke_grant", json!({"grant_id": 1, "reason": "grantor"})).await?;
    assert!(revoke.is_success(), "{:?}", failure_text(&revoke));
    assert_eq!(ids(f.circle.view("grants_of", page(&translator, "grantee")).await?), vec![2], "grant 1 is gone, grant 2 became active for the buyer");
    assert_eq!(ids(f.circle.view("grants_of", page(&scraper, "grantor")).await?), vec![2]);
    f.assert_d6().await
}
