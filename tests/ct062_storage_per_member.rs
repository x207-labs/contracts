//! CT-062 (security review 3, I-3): what one member can make the contract store, measured in near-sandbox as the growth of `storage_usage` of the contract account. The
//! `join` deposit has to cover the worst case, because the contract pays for what a member posts (`post_intent`, `offer_grant`, the escrow of an NFT). Table:
//! `cargo test --test ct062_storage_per_member -- --nocapture | grep STORAGE`. Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::CREDIT;
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use serde_json::{json, Value};

/// Cost of one byte of storage on NEAR, in yoctoNEAR.
const BYTE_COST: u128 = 10_000_000_000_000_000_000;

async fn usage(f: &Funded) -> anyhow::Result<u64> {
    Ok(f.circle.contract.view_account().await?.storage_usage)
}

fn near(bytes: u64) -> String {
    format!("{:.4} NEAR", (bytes as u128 * BYTE_COST) as f64 / 1e24)
}

fn padded_scope(extra: Value) -> String {
    // a JSON object of exactly 1024 bytes, the most `offer_grant` takes
    let mut object = extra.as_object().cloned().unwrap_or_default();
    object.insert("pad".into(), json!(""));
    let base = serde_json::to_string(&object).unwrap().len();
    object.insert("pad".into(), json!("x".repeat(1024 - base)));
    let text = serde_json::to_string(&object).unwrap();
    assert_eq!(text.len(), 1024);
    text
}

// TC-CT-062-STORAGE-01: the worst case of one member, by part
#[tokio::test]
async fn what_one_member_can_make_the_contract_store() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let grantor = f.account("grantor").await?;
    let buyer = f.account("buyer").await?;
    let dao = f.account("dao").await?;
    for name in ["grantor", "buyer"] {
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
    }
    let after_join = usage(&f).await?;
    let before_join = after_join; // the part below is measured from the first join of the member
    println!("STORAGE two members joined: {after_join} bytes");
    assert!(f.stake(&grantor, CREDIT).await?.is_success());
    assert!(f.stake(&buyer, CREDIT).await?.is_success());
    let staked = usage(&f).await?;
    println!("STORAGE stake of 1 unit each (changes the member rows): {} bytes", staked as i64 - before_join as i64);

    // 64 intents
    let intents_from = usage(&f).await?;
    for n in 0..64u8 {
        let r = f.circle.call_as(&grantor, "post_intent", json!({"intent_hash": near_sdk::bs58::encode([n; 32]).into_string()})).await?;
        assert!(r.is_success());
    }
    let intents = usage(&f).await? - intents_from;
    println!("STORAGE 64 intents: {intents} bytes = {}", near(intents));

    // 16 open grants of class B with a scope of 1024 bytes, the longest asset_type, an operator, and all accepted by the other member (its index grows too)
    let long = format!("{}:{}", "p".repeat(64), "k".repeat(64));
    let entry = |t: &str, class: &str, method: &str| json!({"asset_type": t, "class": class, "delivery_methods": [method], "accepted_proofs": ["rpc_access_key"], "terms_hash": "ab".repeat(32)});
    assert!(f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": long, "entry": entry(&long, "B", "near_access_key")})).await?.is_success());
    assert!(f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": "nft:ticket", "entry": entry("nft:ticket", "A", "escrow_nft")})).await?.is_success());
    let expires = f.circle.now_ns().await? + 3_600_000_000_000;
    let operator = format!("{}.near", "o".repeat(59));
    let grants_from = usage(&f).await?;
    for _ in 0..8 {
        let input = json!({"input": {"asset_type": long, "delivery_method": "near_access_key",
            "limits": {"scope": padded_scope(json!({"receiver_id": "api.near", "method_names": ["get"], "allowance": "1"})), "quota": "340282366920938463463374607431768211455", "expires_at": expires.to_string()},
            "unit": "minute", "unit_price": "340282366920938463463374607431768211455", "operator": operator, "proof_hash": "ab".repeat(32), "proof_kind": "rpc_access_key"}});
        let r = f.circle.call_as(&grantor, "offer_grant", input).await?;
        assert!(r.is_success(), "{}", common::circle::failure_text(&r).map(|t| t.find("ERR_").map_or(t.chars().take(200).collect(), |i| t[i..].chars().take(160).collect::<String>())).unwrap_or_default());
    }
    let class_b = usage(&f).await? - grants_from;
    println!("STORAGE 8 open grants of class B (longest asset_type, scope 1024 bytes, operator, proof): {class_b} bytes = {}", near(class_b));
    for id in 1..=8u64 {
        let r = f.circle.call_as(&buyer, "accept_grant", json!({"grant_id": id, "delivery_pubkey": null})).await?;
        assert!(r.is_success(), "{}", common::circle::failure_text(&r).map(|t| t.find("ERR_").map_or(t.chars().take(200).collect(), |i| t[i..].chars().take(160).collect::<String>())).unwrap_or_default());
    }
    let accepted = usage(&f).await? - grants_from - class_b;
    println!("STORAGE the 8 grants accepted (index of the other member, pubkey none): {accepted} bytes");

    // 8 grants of class A, each with an NFT in the escrow (token id of 128 bytes)
    let nft = f.circle.worker.dev_deploy(&std::fs::read(common::probe_wasm("mock_nft"))?).await?;
    nft.call("new").transact().await?.into_result()?;
    let escrow_from = usage(&f).await?;
    for n in 0..8u64 {
        let token = format!("{n:0>128}");
        let input = json!({"input": {"asset_type": "nft:ticket", "delivery_method": "escrow_nft",
            "limits": {"scope": padded_scope(json!({"nft_contract": nft.id(), "token_id": token})), "quota": null, "expires_at": expires.to_string()},
            "unit": "once", "unit_price": "340282366920938463463374607431768211455", "operator": operator, "proof_hash": null, "proof_kind": null}});
        let r = f.circle.call_as(&grantor, "offer_grant", input).await?;
        assert!(r.is_success(), "{}", common::circle::failure_text(&r).map(|t| t.find("ERR_").map_or(t.chars().take(200).collect(), |i| t[i..].chars().take(160).collect::<String>())).unwrap_or_default());
        let id = r.json::<u64>()?;
        nft.call("nft_mint").args_json(json!({"token_id": token, "owner_id": grantor.id()})).transact().await?.into_result()?;
        let deposit = grantor
            .call(nft.id(), "nft_transfer_call")
            .args_json(json!({"receiver_id": f.circle.contract.id(), "token_id": token, "msg": json!({"grant_id": id}).to_string()}))
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(150))
            .transact()
            .await?;
        assert!(deposit.is_success());
    }
    let class_a = usage(&f).await? - escrow_from;
    println!("STORAGE 8 open grants of class A with an NFT in the escrow (token id 128 bytes, scope 1024 bytes): {class_a} bytes = {}", near(class_a));

    let per_grant_b = class_b / 8 + accepted / 8;
    let per_grant_a = class_a / 8;
    let worst_grants = 16 * per_grant_b.max(per_grant_a);
    let total = intents + worst_grants + 400; // 400: the member row with the longest account name and the grant counter
    println!("STORAGE per grant: class B {per_grant_b} bytes (accepted), class A with escrow {per_grant_a} bytes");
    println!("STORAGE worst case of one member: 64 intents {intents} + 16 grants {worst_grants} + member row 400 = {total} bytes = {}", near(total));
    Ok(())
}

// TC-CT-094-STORAGE-01 (0.0.12): what the bonds and a freeze add to the worst case of one member. Eight open deals (the limit), each between accounts and an arbiter with names of
// 64 characters, against eight bundles of the same legs without a deal (the bundle record was in the contract before); and one freeze record with a reason hash.
#[tokio::test]
async fn what_bonds_and_a_freeze_add_to_the_worst_case_of_one_member() -> anyhow::Result<()> {
    const HOUR: u64 = 3600 * 1_000_000_000;
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    // names of 64 characters: 50 + ".test.near" + padding to 64 in total
    let long = |tag: &str| format!("{}{}", tag, "x".repeat(64 - ".test.near".len() - tag.len()));
    let (pa, pb, parb) = (long("a"), long("b"), long("c"));
    assert_eq!(format!("{pa}.test.near").len(), 64);
    let a = f.account(&pa).await?;
    let b = f.account(&pb).await?;
    let arb = f.account(&parb).await?;
    for (name, account) in [(&pa, &a), (&pb, &b)] {
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(account, 800 * CREDIT).await?.is_success());
    }
    let dao = f.account("dao").await?;
    // eight bundles of the same legs, each with a different amount so that the terms differ; first without a deal
    let legs = |n: u128| [(pa.as_str(), pb.as_str(), (10 + n) * CREDIT), (pb.as_str(), pa.as_str(), (9 + n) * CREDIT)];
    let plain_from = usage(&f).await?;
    for n in 0..8u128 {
        let r = f.circle.commit_bundle(1, &legs(n), &[pa.as_str(), pb.as_str()]).await?;
        assert!(r.is_success(), "{:?}", r.failures().iter().map(|f| format!("{f:?}")).collect::<Vec<_>>());
    }
    let plain = usage(&f).await? - plain_from;
    let now = f.circle.now_ns().await?;
    let deal_from = usage(&f).await?;
    for n in 8..16u128 {
        let hash = "ab".repeat(32);
        let r = f.circle.commit_bundle_with_deal(1, &legs(n), (arb.id().as_str(), Some(hash.as_str()), now + 24 * HOUR), &[pa.as_str(), pb.as_str()]).await?;
        assert!(r.is_success(), "{:?}", r.failures().iter().map(|f| format!("{f:?}")).collect::<Vec<_>>());
    }
    let with_deal = usage(&f).await? - deal_from;
    let per_deal = (with_deal.saturating_sub(plain)) / 8;
    println!("STORAGE 8 bundles without a deal: {plain} bytes; with a deal (arbiter and parties of 64 characters, agreement hash): {with_deal} bytes; extra per open deal: {per_deal} bytes");

    let freeze_from = usage(&f).await?;
    let r = f.circle.call_as(&dao, "propose_freeze", json!({"account_id": format!("{pa}.test.near"), "reason_hash": "cd".repeat(32)})).await?;
    assert!(r.is_success());
    let freeze = usage(&f).await? - freeze_from;
    println!("STORAGE one freeze record of an account with a 64 character name and a reason hash: {freeze} bytes");
    let extra = 8 * per_deal + freeze;
    println!("STORAGE extra of 0.0.12 for one member: 8 deals {} + freeze {freeze} = {extra} bytes = {}", 8 * per_deal, near(extra));
    println!("STORAGE worst case with 0.0.12: 31282 + {extra} = {} bytes = {}", 31_282 + extra, near(31_282 + extra));
    assert!((31_282 + extra) as u128 * BYTE_COST <= x207_circle::member::JOIN_STORAGE_DEPOSIT.as_yoctonear(), "the join deposit does not cover the worst case of 0.0.12");
    assert!(extra < 6_000, "the extra storage of 0.0.12 is larger than the estimate of docs/rules-0.0.12.md by a wide margin: {extra} bytes");
    Ok(())
}
