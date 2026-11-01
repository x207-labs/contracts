//! CT-025: registration rules in near-sandbox, through both `register_cumulative` and
//! `register_voucher`. Members are seeded with `patch_state` (their D1 records written under the
//! `m` prefix), because `join` and stake are not built yet; a real F1 fixture replaces this later.
//! Needs `cargo near build non-reproducible-wasm` first.

use std::collections::HashMap;

use ed25519_dalek::{Signer, SigningKey};
use near_sdk::borsh;
use near_sdk::{CurveType, PublicKey};
use near_workspaces::network::Sandbox;
use near_workspaces::{Account, Contract, Worker};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use x207_circle::clearing::Epoch;
use x207_circle::member::{Member, MemberStatus};

const WASM: &str = "target/near/x207_circle.wasm";
const CREDIT: u128 = 1_000_000;

struct Ctx {
    worker: Worker<Sandbox>,
    contract: Contract,
    facilitator: Account,
}

fn key_of(name: &str) -> SigningKey {
    let mut seed = [3u8; 32];
    seed[..name.len()].copy_from_slice(name.as_bytes());
    SigningKey::from_bytes(&seed)
}

fn member_id(name: &str) -> String {
    format!("{name}.test.near")
}

fn member_record(name: &str, status: MemberStatus, limit: u128) -> Member {
    let id = member_id(name).parse().unwrap();
    Member {
        account_id: id,
        pubkey_voucher: PublicKey::from_parts(CurveType::ED25519, key_of(name).verifying_key().to_bytes().to_vec()).unwrap(),
        terms_hash: "a".repeat(64),
        stake: 100 * CREDIT,
        limit,
        status,
        admitted: false,
        joined_at: 0,
        settled_history: 0,
        last_active_epoch: 0,
        exit_requested_epoch: None,
        withdraw_pending: false,
        reactivate_epoch: None,
    }
}

fn epoch_key(epoch: u64) -> Vec<u8> {
    let mut key = vec![b'c'];
    key.extend(epoch.to_le_bytes());
    key
}

async fn setup() -> anyhow::Result<Ctx> {
    let worker = near_workspaces::sandbox().await?;
    let contract = worker.dev_deploy(&std::fs::read(WASM)?).await?;
    contract
        .call("new")
        .args_json(json!({
            "owner_id": "dao.test.near", "circle_id": "agents-main",
            "usdt_token_id": "usdt.test.near", "intents_id": "intents.test.near", "params": null
        }))
        .transact()
        .await?
        .into_result()?;
    for (name, status, limit) in [
        ("translator", MemberStatus::Active, 80 * CREDIT),
        ("scraper", MemberStatus::Active, 80 * CREDIT),
        ("gpu", MemberStatus::Active, 80 * CREDIT),
        ("pending", MemberStatus::Pending, 0),
    ] {
        let mut key = vec![b'm'];
        key.extend(borsh::to_vec(&member_id(name))?);
        worker.patch_state(contract.id(), &key, &borsh::to_vec(&member_record(name, status, limit))?).await?;
    }
    let facilitator = worker.dev_create_account().await?;
    Ok(Ctx { worker, contract, facilitator })
}

fn voucher(ctx: &Ctx, payer: &str, payee: &str, seq: u64, cumulative: u128) -> Value {
    json!({
        "circle": ctx.contract.id().as_str(), "payer": member_id(payer), "payee": member_id(payee),
        "epoch": 1, "seq": seq, "cumulativeAmount": cumulative.to_string(), "unit": "credit",
        "resource": "https://scraper.example/api",
    })
}

/// Spec 3.3 written out byte by byte, from the fields of `v` (so a tampered `v` signs differently).
fn sign(key_name: &str, v: &Value) -> String {
    let s = |f: &str| v[f].as_str().unwrap().to_string();
    let message = format!(
        r#"{{"circle":"{}","payer":"{}","payee":"{}","epoch":{},"seq":{},"cumulativeAmount":"{}","unit":"{}","resource":"{}"}}"#,
        s("circle"), s("payer"), s("payee"), v["epoch"], v["seq"], s("cumulativeAmount"), s("unit"), s("resource")
    );
    let nonce: [u8; 32] = Sha256::digest(format!("x207:{}:{}:{}:{}", s("payer"), s("payee"), v["epoch"], v["seq"])).into();
    let mut payload = 2_147_484_061u32.to_le_bytes().to_vec();
    payload.extend((message.len() as u32).to_le_bytes());
    payload.extend(message.as_bytes());
    payload.extend(nonce);
    payload.extend((s("circle").len() as u32).to_le_bytes());
    payload.extend(s("circle").as_bytes());
    payload.push(0);
    let signature = key_of(key_name).sign(&Sha256::digest(&payload)).to_bytes();
    format!("ed25519:{}", near_sdk::bs58::encode(signature).into_string())
}

async fn snapshot(ctx: &Ctx) -> anyhow::Result<HashMap<Vec<u8>, Vec<u8>>> {
    Ok(ctx.contract.view_state().await?)
}

async fn try_register(ctx: &Ctx, method: &str, v: &Value, signature: &str) -> anyhow::Result<Result<near_workspaces::result::ExecutionSuccess, String>> {
    let r = ctx
        .facilitator
        .call(ctx.contract.id(), method)
        .args_json(json!({"voucher": v, "signature": signature}))
        .max_gas()
        .transact()
        .await?;
    Ok(r.into_result().map_err(|e| format!("{e:?}")))
}

/// A negative case: the call fails with `code` and the contract state is byte-identical (I-06).
async fn rejected(ctx: &Ctx, method: &str, v: &Value, signature: &str, code: &str) -> anyhow::Result<()> {
    let before = snapshot(ctx).await?;
    let err = try_register(ctx, method, v, signature).await?.expect_err(&format!("{method} should reject with {code}"));
    assert!(err.contains(code), "{method}: expected {code}, got {err}");
    assert_eq!(before, snapshot(ctx).await?, "{method}/{code}: state changed");
    Ok(())
}

async fn accepted(ctx: &Ctx, method: &str, v: &Value, signer: &str) -> anyhow::Result<()> {
    let r = try_register(ctx, method, v, &sign(signer, v)).await?;
    r.map_err(|e| anyhow::anyhow!("{method} rejected a valid voucher: {e}"))?;
    Ok(())
}

async fn rules(method: &str) -> anyhow::Result<()> {
    let demo = method == "register_voucher";
    let ctx = setup().await?;
    // Three valid registrations, 10 / 20 / 30 credit (TC-CT-025-03 uses seq 3, cumulative 30).
    for (seq, cumulative) in [(1, 10), (2, 20), (3, 30)] {
        accepted(&ctx, method, &voucher(&ctx, "translator", "scraper", seq, cumulative * CREDIT), "translator").await?;
    }

    // TC-CT-025-01: signed with another key
    let v = voucher(&ctx, "translator", "scraper", 4, 31 * CREDIT);
    rejected(&ctx, method, &v, &sign("gpu", &v), "ERR_BAD_SIGNATURE").await?;

    // TC-CT-025-02: fields changed after signing. In demo mode a new `payee` pair starts at seq 0 and a
    // changed `seq` skips ahead, so both meet the cheaper gap check (check 9) before the signature (12).
    let signature = sign("translator", &v);
    let mut tampered = vec![
        ("cumulativeAmount", json!((32 * CREDIT).to_string()), "ERR_BAD_SIGNATURE"),
        ("payee", json!(member_id("gpu")), if demo { "ERR_SEQ_GAP" } else { "ERR_BAD_SIGNATURE" }),
        ("resource", json!("https://evil.example/api"), "ERR_BAD_SIGNATURE"),
    ];
    tampered.push(("seq", json!(5), if demo { "ERR_SEQ_GAP" } else { "ERR_BAD_SIGNATURE" }));
    for (field, value, code) in tampered {
        let mut t = v.clone();
        t[field] = value;
        rejected(&ctx, method, &t, &signature, code).await?;
    }

    // TC-CT-025-03: same seq again with a bigger total; TC-CT-025-04: smaller seq
    let same_seq = voucher(&ctx, "translator", "scraper", 3, 40 * CREDIT);
    rejected(&ctx, method, &same_seq, &sign("translator", &same_seq), "ERR_SEQ_NOT_INCREASING").await?;
    let lower = voucher(&ctx, "translator", "scraper", 2, 40 * CREDIT);
    rejected(&ctx, method, &lower, &sign("translator", &lower), "ERR_SEQ_NOT_INCREASING").await?;
    let obligation: Value = ctx.contract.view("obligation").args_json(json!({"epoch": 1, "debtor": member_id("translator"), "creditor": member_id("scraper")})).await?.json()?;
    assert_eq!(obligation["obligation"], (30 * CREDIT).to_string());

    // TC-CT-025-05: cumulative goes down
    let down = voucher(&ctx, "translator", "scraper", 4, 29 * CREDIT);
    rejected(&ctx, method, &down, &sign("translator", &down), "ERR_CUMULATIVE_DECREASED").await?;

    // TC-CT-025-06: headroom is 80 - 30 = 50, so +51 fails
    let over = voucher(&ctx, "translator", "scraper", 4, 81 * CREDIT);
    rejected(&ctx, method, &over, &sign("translator", &over), "ERR_INSUFFICIENT_CREDIT").await?;

    // TC-CT-025-08: pending payer, pending payee
    let pending_payer = voucher(&ctx, "pending", "scraper", 1, CREDIT);
    rejected(&ctx, method, &pending_payer, &sign("pending", &pending_payer), "ERR_PAYER_NOT_ACTIVE").await?;
    let pending_payee = voucher(&ctx, "translator", "pending", 1, CREDIT);
    rejected(&ctx, method, &pending_payee, &sign("translator", &pending_payee), "ERR_PAYEE_NOT_ACTIVE").await?;

    // TC-CT-025-11: shape
    let mut wrong_circle = voucher(&ctx, "translator", "scraper", 4, 31 * CREDIT);
    wrong_circle["circle"] = json!("other.test.near");
    rejected(&ctx, method, &wrong_circle, &sign("translator", &wrong_circle), "ERR_WRONG_CIRCLE").await?;
    let mut bad_unit = voucher(&ctx, "translator", "scraper", 4, 31 * CREDIT);
    bad_unit["unit"] = json!("usd");
    rejected(&ctx, method, &bad_unit, &sign("translator", &bad_unit), "ERR_BAD_UNIT").await?;
    let mut non_ascii = voucher(&ctx, "translator", "scraper", 4, 31 * CREDIT);
    non_ascii["resource"] = json!("https://x.example/caf\u{e9}");
    rejected(&ctx, method, &non_ascii, &sign("translator", &non_ascii), "ERR_RESOURCE_NOT_ASCII").await?;
    let self_pay = voucher(&ctx, "translator", "translator", 1, CREDIT);
    rejected(&ctx, method, &self_pay, &sign("translator", &self_pay), "ERR_SELF_PAYMENT").await?;

    // TC-CT-025-09: one entry for the pair, `last_seq` is the largest valid seq
    let list: Vec<Value> = ctx.contract.view("list_obligations").args_json(json!({"epoch": 1, "from_index": 0, "limit": 10})).await?.json()?;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["last_seq"], 3);

    // TC-CT-025-12: a jump in `seq` is allowed in cumulative mode and rejected in demo mode
    let jump = voucher(&ctx, "translator", "scraper", 9, 30 * CREDIT);
    if demo {
        rejected(&ctx, method, &jump, &sign("translator", &jump), "ERR_SEQ_GAP").await?;
    } else {
        accepted(&ctx, method, &jump, "translator").await?;
        let seq: u64 = ctx.contract.view("last_seq").args_json(json!({"payer": member_id("translator"), "payee": member_id("scraper"), "epoch": 1})).await?.json()?;
        assert_eq!(seq, 9);
        let list: Vec<Value> = ctx.contract.view("list_obligations").args_json(json!({"epoch": 1, "from_index": 0, "limit": 10})).await?.json()?;
        assert_eq!(list.len(), 1);
    }
    Ok(())
}

// TC-CT-025-01..06, 08, 09, 11, 13
#[tokio::test]
async fn rules_through_register_cumulative() -> anyhow::Result<()> {
    rules("register_cumulative").await
}

// TC-CT-025-12
#[tokio::test]
async fn rules_through_register_voucher() -> anyhow::Result<()> {
    rules("register_voucher").await
}

/// Rewrites `cutoff_at` of epoch 1 in the contract state, keeping the rest of the record.
async fn set_cutoff(ctx: &Ctx, cutoff_ns: u64) -> anyhow::Result<()> {
    let state = snapshot(ctx).await?;
    let raw = state.get(&epoch_key(1)).expect("epoch 1 record");
    let mut epoch: Epoch = borsh::from_slice(raw)?;
    epoch.cutoff_at = cutoff_ns;
    ctx.worker.patch_state(ctx.contract.id(), &epoch_key(1), &borsh::to_vec(&epoch)?).await?;
    Ok(())
}

// TC-CT-025-07: past the cutoff before `close_epoch`, then after it
#[tokio::test]
async fn cutoff_then_close() -> anyhow::Result<()> {
    let ctx = setup().await?;
    set_cutoff(&ctx, 2).await?;
    let v = voucher(&ctx, "translator", "scraper", 1, CREDIT);
    rejected(&ctx, "register_cumulative", &v, &sign("translator", &v), "ERR_PAST_CUTOFF").await?;
    ctx.facilitator.call(ctx.contract.id(), "close_epoch").args_json(json!({"epoch": 1})).max_gas().transact().await?.into_result()?;
    rejected(&ctx, "register_cumulative", &v, &sign("translator", &v), "ERR_EPOCH_MISMATCH").await?;
    let epoch: Value = ctx.contract.view("get_epoch").args_json(json!({"epoch": 1})).await?.json()?;
    assert_eq!(epoch["phase"], "clearing");
    Ok(())
}

// TC-CT-025-10: a voucher registered before the cutoff is in that epoch's snapshot and in `epoch_cutoff`
#[tokio::test]
async fn voucher_before_cutoff_counts_in_snapshot() -> anyhow::Result<()> {
    let ctx = setup().await?;
    accepted(&ctx, "register_cumulative", &voucher(&ctx, "translator", "scraper", 1, 10 * CREDIT), "translator").await?;
    set_cutoff(&ctx, 2).await?;
    let closed = ctx.facilitator.call(ctx.contract.id(), "close_epoch").args_json(json!({"epoch": 1})).max_gas().transact().await?.into_result()?;
    let log = closed.logs().into_iter().find(|l| l.contains("epoch_cutoff")).expect("epoch_cutoff logged").to_string();
    let event: Value = serde_json::from_str(log.trim_start_matches("EVENT_JSON:"))?;
    assert_eq!(event["data"][0]["total_obligation"], (10 * CREDIT).to_string());
    assert_eq!(event["data"][0]["edge_count"], 1);
    let obligation: Value = ctx.contract.view("obligation").args_json(json!({"epoch": 1, "debtor": member_id("translator"), "creditor": member_id("scraper")})).await?.json()?;
    assert_eq!(obligation["obligation"], (10 * CREDIT).to_string());
    Ok(())
}
