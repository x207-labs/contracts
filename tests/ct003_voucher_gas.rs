//! CT-003: verify voucher signatures and measure gas in near-sandbox. Bytes to sign are built here
//! independently of `canonical.rs` (spec 3.3), so a mismatch shows up as a failed verification.
//! Build first: `cargo near build non-reproducible-wasm --manifest-path probes/ct003_voucher_gas/Cargo.toml`.
//! Gas table: `cargo test --test ct003_voucher_gas -- --nocapture`.

use ed25519_dalek::{Signer, SigningKey};
use near_workspaces::types::Gas;
use near_workspaces::{Contract, Worker};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

mod common;

fn voucher(cumulative: &str) -> Value {
    json!({
        "circle": "x207-circle.near",
        "payer": "translator.x207-circle.near",
        "payee": "summarizer.x207-circle.near",
        "epoch": 42,
        "seq": 137,
        "cumulativeAmount": cumulative,
        "unit": "credit",
        "resource": "https://summarizer.example/api/summarize",
    })
}

/// Spec 3.3 written out byte by byte.
fn signing_hash(
    circle: &str,
    payer: &str,
    payee: &str,
    epoch: u64,
    seq: u64,
    cumulative: &str,
    resource: &str,
) -> [u8; 32] {
    let message = format!(
        r#"{{"circle":"{circle}","payer":"{payer}","payee":"{payee}","epoch":{epoch},"seq":{seq},"cumulativeAmount":"{cumulative}","unit":"credit","resource":"{resource}"}}"#
    );
    let nonce: [u8; 32] = Sha256::digest(format!("x207:{payer}:{payee}:{epoch}:{seq}")).into();
    let mut payload = Vec::new();
    payload.extend_from_slice(&2_147_484_061u32.to_le_bytes());
    payload.extend_from_slice(&(message.len() as u32).to_le_bytes());
    payload.extend_from_slice(message.as_bytes());
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&(circle.len() as u32).to_le_bytes());
    payload.extend_from_slice(circle.as_bytes());
    payload.push(0); // callback_url = None
    Sha256::digest(&payload).into()
}

fn sign(key: &SigningKey) -> [u8; 64] {
    let hash = signing_hash(
        "x207-circle.near",
        "translator.x207-circle.near",
        "summarizer.x207-circle.near",
        42,
        137,
        "2740000",
        "https://summarizer.example/api/summarize",
    );
    key.sign(&hash).to_bytes()
}

fn encode_sig(signature: &[u8]) -> String {
    format!(
        "ed25519:{}",
        near_sdk::bs58::encode(signature).into_string()
    )
}

fn encode_key(key: &SigningKey) -> String {
    format!(
        "ed25519:{}",
        near_sdk::bs58::encode(key.verifying_key().to_bytes()).into_string()
    )
}

async fn deploy() -> anyhow::Result<(Worker<near_workspaces::network::Sandbox>, Contract)> {
    let worker = near_workspaces::sandbox().await?;
    let wasm = std::fs::read(common::probe_wasm("ct003_voucher_gas"))?;
    let contract = worker.dev_deploy(&wasm).await?;
    Ok((worker, contract))
}

async fn verify(
    contract: &Contract,
    voucher: Value,
    signature: String,
    pubkey: String,
) -> anyhow::Result<bool> {
    let args = json!({"voucher": voucher, "signature": signature, "pubkey": pubkey});
    Ok(contract
        .call("probe_verify")
        .args_json(args)
        .gas(Gas::from_tgas(100))
        .transact()
        .await?
        .json()?)
}

// TC-CT-003-01..04
#[tokio::test]
async fn verification_accepts_valid_and_rejects_tampered() -> anyhow::Result<()> {
    let (_worker, contract) = deploy().await?;
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let signature = sign(&key);

    assert!(
        verify(
            &contract,
            voucher("2740000"),
            encode_sig(&signature),
            encode_key(&key)
        )
        .await?
    );
    assert!(
        !verify(
            &contract,
            voucher("2740001"),
            encode_sig(&signature),
            encode_key(&key)
        )
        .await?
    );

    let mut flipped = signature;
    flipped[0] ^= 1;
    assert!(
        !verify(
            &contract,
            voucher("2740000"),
            encode_sig(&flipped),
            encode_key(&key)
        )
        .await?
    );

    let access_key = SigningKey::from_bytes(&[9u8; 32]);
    assert!(
        !verify(
            &contract,
            voucher("2740000"),
            encode_sig(&signature),
            encode_key(&access_key)
        )
        .await?
    );
    Ok(())
}

async fn burnt_tgas(
    contract: &Contract,
    args: &Value,
    count: u32,
    stage: &str,
) -> anyhow::Result<f64> {
    let mut args = args.clone();
    args["count"] = json!(count);
    args["stage"] = json!(stage);
    let outcome = contract
        .call("probe_measure")
        .args_json(args)
        .gas(Gas::from_tgas(300))
        .transact()
        .await?;
    let burnt = outcome.total_gas_burnt.as_gas() as f64 / 1e12;
    let passed: u32 = outcome.json()?;
    assert_eq!(passed, count, "{stage} x{count}");
    Ok(burnt)
}

// TC-CT-003-06, TC-CT-003-07
#[tokio::test]
async fn gas_per_verification() -> anyhow::Result<()> {
    let (_worker, contract) = deploy().await?;
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let args = json!({
        "voucher": voucher("2740000"),
        "signature": encode_sig(&sign(&key)),
        "pubkey": encode_key(&key),
    });

    for stage in ["serialize", "serialize_hash", "full"] {
        let one = burnt_tgas(&contract, &args, 1, stage).await?;
        let ten = burnt_tgas(&contract, &args, 10, stage).await?;
        let fifty = burnt_tgas(&contract, &args, 50, stage).await?;
        let per_item = (fifty - ten) / 40.0;
        println!("RESULT ct003 stage={stage} n1={one:.3} n10={ten:.3} n50={fifty:.3} per_item_tgas={per_item:.4}");
        assert!(per_item > 0.0);
    }
    Ok(())
}
