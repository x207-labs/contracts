//! CT-042: who may write what (I-15, I-18, ADR-06, ADR-20), in near-sandbox. One ordered story on a
//! real circle: after every call the state of D6, D7 and D2 is compared with the state before, and a
//! change is accepted only from the methods the spec names as writers of that store.
//! Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, events, failure_text, short_params, signing_hash, CREDIT};
use common::flow::edge;
use common::funded::Funded;
use ed25519_dalek::{Signer, SigningKey};
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::{Gas, NearToken};
use serde_json::{json, Value};

/// What a store shows from outside: pool, params, and the pair and debt entries of epoch 1.
async fn stores(f: &Funded) -> anyhow::Result<(Value, Value, Value)> {
    let c = &f.circle;
    let d6 = c.view("get_pool", json!({})).await?;
    let d7 = json!({"active": c.view("get_params", json!({})).await?, "pending": c.view("get_pending_params", json!({})).await?});
    let mut debts = serde_json::Map::new();
    for name in ["scraper", "gpu", "translator"] {
        debts.insert(name.into(), c.view("outstanding_debt", json!({"account_id": account_of(name)})).await?);
    }
    let pairs = c.view("list_obligations", json!({"epoch": 1, "from_index": 0, "limit": 100})).await?;
    Ok((d6, d7, json!({"pairs": pairs, "debts": debts})))
}

fn debts_of(d2: &Value) -> &Value {
    &d2["debts"]
}

/// Runs one call and checks the three stores against the writers of ADR-32 K-09.
struct Watch<'a> {
    f: &'a Funded,
}

impl Watch<'_> {
    async fn step(&self, label: &str, d6: bool, d7: bool, pairs: bool, debts: bool, call: impl std::future::Future<Output = anyhow::Result<ExecutionFinalResult>>) -> anyhow::Result<ExecutionFinalResult> {
        let (b6, b7, b2) = stores(self.f).await?;
        let result = call.await?;
        assert!(result.is_success(), "{label}: {:?}", failure_text(&result));
        let (a6, a7, a2) = stores(self.f).await?;
        if !d6 {
            assert_eq!(b6, a6, "{label} must not write D6");
        }
        if !d7 {
            assert_eq!(b7, a7, "{label} must not write D7");
        }
        if !pairs {
            assert_eq!(b2["pairs"], a2["pairs"], "{label} must not write the D2 pair entries");
        }
        if !debts {
            assert_eq!(debts_of(&b2), debts_of(&a2), "{label} must not write outstanding_debt");
        }
        Ok(result)
    }
}

fn voucher_json(circle: &str, payer: &str, payee: &str, epoch: u64, seq: u64, cumulative: u128) -> Value {
    json!({"circle": circle, "payer": account_of(payer), "payee": account_of(payee), "epoch": epoch, "seq": seq,
           "cumulativeAmount": cumulative.to_string(), "unit": "credit", "resource": "https://example.test/api"})
}

fn sign(key: &SigningKey, v: &Value) -> String {
    let hash = signing_hash(
        v["circle"].as_str().unwrap(),
        v["payer"].as_str().unwrap(),
        v["payee"].as_str().unwrap(),
        v["epoch"].as_u64().unwrap(),
        v["seq"].as_u64().unwrap(),
        v["cumulativeAmount"].as_str().unwrap(),
        v["resource"].as_str().unwrap(),
    );
    format!("ed25519:{}", near_sdk::bs58::encode(key.sign(&hash).to_bytes()).into_string())
}

async fn call(f: &Funded, who: &str, method: &str, args: Value, deposit: u128) -> anyhow::Result<ExecutionFinalResult> {
    Ok(f.accounts[who]
        .call(f.circle.contract.id(), method)
        .args_json(args)
        .deposit(NearToken::from_yoctonear(deposit))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?)
}

// TC-CT-042-02, -04, -05, -03, -06, -07, -08, -01 in one story
#[tokio::test]
async fn writers_of_each_store_and_the_powers_nobody_has() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(short_params())).await?;
    for name in ["dao", "facilitator", "outsider"] {
        f.account(name).await?;
    }
    let circle_id = f.circle.contract.id().to_string();
    for name in ["scraper", "gpu", "translator"] {
        f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
    }
    let watch = Watch { f: &f };

    // members: stake is a D6 write
    for name in ["scraper", "gpu", "translator"] {
        let account = f.accounts[name].clone();
        let stake = watch.step("stake", true, false, false, false, f.stake(&account, 100 * CREDIT)).await?;
        assert!(events(&stake).iter().any(|(n, _)| n == "stake_added"));
    }

    // -06 the facilitator cannot forge a voucher, nor change the amount of a valid one
    let facilitator_key = SigningKey::from_bytes(&[7u8; 32]);
    let forged = voucher_json(&circle_id, "translator", "gpu", 1, 1, 5 * CREDIT);
    let r = call(&f, "facilitator", "register_cumulative", json!({"voucher": forged, "signature": sign(&facilitator_key, &forged)}), 0).await?;
    assert!(failure_text(&r).expect("forged").contains("ERR_BAD_SIGNATURE"));
    let honest = voucher_json(&circle_id, "translator", "gpu", 1, 1, 5 * CREDIT);
    let honest_sig = sign(&f.circle.keys["translator"], &honest);
    let mut inflated = honest.clone();
    inflated["cumulativeAmount"] = json!((6 * CREDIT).to_string());
    let r = call(&f, "facilitator", "register_cumulative", json!({"voucher": inflated, "signature": honest_sig}), 0).await?;
    assert!(failure_text(&r).expect("tampered").contains("ERR_BAD_SIGNATURE"));
    let (_, _, d2) = stores(&f).await?;
    assert_eq!(d2["pairs"], json!([]), "refused vouchers leave no trace");

    // registration writes D2 pairs and debts, nothing else (-05)
    for (payer, payee, amount) in [("scraper", "gpu", 60), ("gpu", "translator", 25), ("translator", "scraper", 25)] {
        let key = f.circle.keys[payer].clone();
        let voucher = voucher_json(&circle_id, payer, payee, 1, 1, amount * CREDIT);
        let signature = sign(&key, &voucher);
        // the facilitator submits what the payer signed
        watch
            .step("register_cumulative", false, false, true, true, call(&f, "facilitator", "register_cumulative", json!({"voucher": voucher, "signature": signature}), 0))
            .await?;
    }

    // -04 set_params by the owner writes only the pending D7 value; an outsider cannot (-02)
    let mut next = short_params();
    next["limit_growth"] = json!(1500);
    let denied = call(&f, "outsider", "set_params", json!({"params": next}), 0).await?;
    assert!(failure_text(&denied).expect("outsider").contains("ERR_NOT_OWNER"));
    watch.step("set_params", false, true, false, false, call(&f, "dao", "set_params", json!({"params": next}), 0)).await?;
    let (_, d7, _) = stores(&f).await?;
    assert_eq!(d7["active"]["limit_growth"], 1000, "the active params stay until close_epoch");
    assert_eq!(d7["pending"]["params"]["limit_growth"], 1500);

    let c = &f.circle;
    let solver = c.worker.dev_create_account().await?;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    let close = watch.step("close_epoch", false, true, false, false, c.call_keeper("close_epoch", json!({"epoch": 1}))).await?;
    assert!(events(&close).iter().any(|(n, _)| n == "epoch_cutoff"));
    assert_eq!(c.view("get_params", json!({})).await?["limit_growth"], 1500, "close_epoch moved pending to active");

    let edges = json!([edge("scraper", "gpu", 25 * CREDIT), edge("gpu", "translator", 25 * CREDIT), edge("translator", "scraper", 25 * CREDIT)]);
    let submitted = c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": edges})).await?;
    assert!(submitted.is_success(), "{:?}", failure_text(&submitted));
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?).await?;
    watch.step("commit_epoch", false, false, true, true, c.call_keeper("commit_epoch", json!({"epoch": 1}))).await?;

    // -01 the owner calls every public method in the middle of the flow: no USDT moves to it, the pool does not shrink
    let dao_before = f.balance(&f.usdt, f.accounts["dao"].id()).await?;
    let pool_before: u128 = c.view("get_pool", json!({})).await?["accounted"].as_str().unwrap().parse()?;
    let abi: Value = serde_json::from_str(&std::fs::read_to_string("abi.json")?)?;
    for function in abi["body"]["functions"].as_array().unwrap().iter().filter(|m| m["kind"] == "call") {
        let name = function["name"].as_str().unwrap();
        if name == "new" {
            continue; // a second init is refused by near-sdk
        }
        let args = match name {
            "claim" => json!({"epoch": 1, "destination": "near"}),
            "close_epoch" | "commit_epoch" | "finalize_epoch" => json!({"epoch": 1}),
            "submit_clearing" => json!({"epoch": 1, "edges": [edge("scraper", "gpu", 1)]}),
            "ft_on_transfer" => json!({"sender_id": account_of("dao"), "amount": "1", "msg": "{\"action\":\"stake\"}"}),
            "join" => json!({"circle_id": "agents-main", "pubkey_voucher": "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp", "terms_hash": "a".repeat(64)}),
            "register_cumulative" | "register_voucher" => json!({"voucher": voucher_json(&circle_id, "dao", "gpu", 1, 1, 1), "signature": "ed25519:1"}),
            "on_claim_resolved" => json!({"epoch": 1, "account_id": account_of("gpu"), "amount": "1", "destination": "near"}),
            "on_withdraw_resolved" => json!({"account_id": account_of("gpu"), "amount": "1"}),
            "set_params" => json!({"params": short_params()}),
            _ => json!({}),
        };
        let _ = call(&f, "dao", name, args, 1).await?;
    }
    assert_eq!(f.balance(&f.usdt, f.accounts["dao"].id()).await?, dao_before, "the owner receives no USDT");
    let pool_after: u128 = c.view("get_pool", json!({})).await?["accounted"].as_str().unwrap().parse()?;
    assert!(pool_after >= pool_before, "the pool does not shrink by an owner call");
    f.assert_d6().await?;

    // settle is a D6 and debt write, nothing else
    let scraper = f.accounts["scraper"].clone();
    watch.step("settle", true, false, false, true, f.transfer_call(&f.usdt, &scraper, 35 * CREDIT, r#"{"action":"settle","epoch":1}"#)).await?;

    c.advance_to(c.epoch_ts(1, "payment_deadline").await?).await?;
    watch.step("finalize_epoch", true, false, false, true, c.call_keeper("finalize_epoch", json!({"epoch": 1}))).await?;

    // -08 the owner cannot claim for someone else, -07 neither can the facilitator
    let gpu_before = f.balance(&f.usdt, f.accounts["gpu"].id()).await?;
    for who in ["dao", "facilitator"] {
        let r = call(&f, who, "claim", json!({"epoch": 1, "destination": "near"}), 1).await?;
        assert!(failure_text(&r).expect("no position").contains("ERR_NO_POSITION"), "{who}");
    }
    let r = call(&f, "facilitator", "withdraw_stake", json!({}), 1).await?;
    assert!(failure_text(&r).expect("not a member").contains("ERR_NOT_MEMBER"));
    let r = call(&f, "facilitator", "on_claim_resolved", json!({"epoch": 1, "account_id": account_of("gpu"), "amount": "35000000", "destination": "near"}), 0).await?;
    assert!(failure_text(&r).expect("private").contains("Method on_claim_resolved is private"));
    assert_eq!(f.balance(&f.usdt, f.accounts["gpu"].id()).await?, gpu_before);

    // claim writes D6 only
    watch.step("claim", true, false, false, false, call(&f, "gpu", "claim", json!({"epoch": 1, "destination": "near"}), 1)).await?;
    assert_eq!(f.balance(&f.usdt, f.accounts["gpu"].id()).await?, gpu_before + 35 * CREDIT);
    f.assert_d6().await
}
