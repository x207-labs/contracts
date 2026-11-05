//! CT-108: opt-in settlement from the stake, on the real contract in near-sandbox.
//! 1. An EVM member (only a `0x` key) opts in with a signed Action, owes a voucher, does not pay; `finalize_epoch` pays the due
//!    from its stake, the member stays `active`, the creditor claims in full.
//! 2. Gas of `finalize_epoch` with 64 members, 63 of them opted in and unpaid.
//!
//! Needs `cargo near build non-reproducible-wasm` first. The EIP-712 hashes are built here with keccak (`sha3`) and signed
//! with `secp256k1`, the same bytes as `x207-circle/src/evm.rs` (that code is checked against viem in `tests/fixtures`).

mod common;

use common::circle::{failure_text, CREDIT};
use common::flow::finalize;
use common::funded::Funded;
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::{Gas, NearToken};
use secp256k1::{Message, Secp256k1, SecretKey};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};

fn k(bytes: &[u8]) -> [u8; 32] {
    Keccak256::digest(bytes).into()
}

fn ws(s: &str) -> [u8; 32] {
    k(s.as_bytes())
}

fn w64(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

fn w128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

fn digest(contract: &str, struct_hash: [u8; 32]) -> [u8; 32] {
    let network = if contract.ends_with(".testnet") {
        "testnet"
    } else {
        "mainnet"
    };
    let salt = k(format!("near:{network}:{contract}").as_bytes());
    let domain = k(&[
        k(b"EIP712Domain(string name,string version,bytes32 salt)"),
        ws("x207"),
        ws("1"),
        salt,
    ]
    .concat());
    k(&[&[0x19u8, 0x01][..], &domain, &struct_hash].concat())
}

fn sign(secret: &SecretKey, digest: [u8; 32]) -> String {
    let (id, compact) = Secp256k1::new()
        .sign_ecdsa_recoverable(&Message::from_slice(&digest).unwrap(), secret)
        .serialize_compact();
    let mut bytes = compact.to_vec();
    bytes.push(27 + id.to_i32() as u8);
    format!(
        "eip712:0x{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

fn address(secret: &SecretKey) -> String {
    let public =
        secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), secret).serialize_uncompressed();
    let hash = k(&public[1..]);
    format!(
        "0x{}",
        hash[12..]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

#[allow(clippy::too_many_arguments)]
fn action_sig(
    secret: &SecretKey,
    contract: &str,
    member: &str,
    action: &str,
    epoch: u64,
    receiver: &str,
    terms: &str,
    nonce: u64,
    deadline: u64,
) -> String {
    let type_hash = ws("Action(string circle,string member,string action,uint64 epoch,string receiver,string termsHash,uint64 nonce,uint64 deadline,uint256 maxFee)");
    let hash = k(&[
        type_hash,
        ws(contract),
        ws(member),
        ws(action),
        w64(epoch),
        ws(receiver),
        ws(terms),
        w64(nonce),
        w64(deadline),
        w64(0), // maxFee
    ]
    .concat());
    sign(secret, digest(contract, hash))
}

#[allow(clippy::too_many_arguments)]
fn voucher_sig(
    secret: &SecretKey,
    contract: &str,
    payer: &str,
    payee: &str,
    epoch: u64,
    seq: u64,
    cumulative: u128,
) -> String {
    let type_hash = ws("Voucher(string circle,string payer,string payee,uint64 epoch,uint64 seq,uint256 cumulativeAmount,string unit,string resource)");
    let hash = k(&[
        type_hash,
        ws(contract),
        ws(payer),
        ws(payee),
        w64(epoch),
        w64(seq),
        w128(cumulative),
        ws("credit"),
        ws("/api/x"),
    ]
    .concat());
    sign(secret, digest(contract, hash))
}

fn tgas(result: &ExecutionFinalResult) -> f64 {
    result.total_gas_burnt.as_gas() as f64 / 1e12
}

// TC-CT-108-06: a signed opt-in, an unpaid due, finalize
#[tokio::test]
async fn an_opted_in_evm_member_who_does_not_pay_stays_active() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let contract = f.circle.contract.id().to_string();
    let secret = SecretKey::from_slice(&[7u8; 32])?;
    let member = address(&secret);
    let deadline = f.circle.now_ns().await? / 1_000_000 + 20 * 3_600_000; // ms: 20 hours after the sandbox clock (the contract allows 24)
    let relayer = f.account("relayer").await?;
    let gpu = f.account("gpu").await?;
    assert!(f.join("gpu", Funded::join_deposit()).await?.is_success());
    assert!(f.stake(&gpu, 100 * CREDIT).await?.is_success());

    // join_evm, stake for the 0x id by the relayer (the token comes from the relayer, the member is the 0x id)
    let terms = "a".repeat(64);
    let join_sig = action_sig(
        &secret, &contract, &member, "join", 0, "", &terms, 0, deadline,
    );
    let join = relayer
        .call(f.circle.contract.id(), "join_evm")
        .args_json(json!({"address": member, "terms_hash": terms, "deadline": deadline, "signature": join_sig}))
        .deposit(NearToken::from_millinear(350))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert!(join.is_success(), "join_evm: {:?}", failure_text(&join));
    let stake = f
        .transfer_call(
            &f.usdt,
            &relayer,
            100 * CREDIT,
            &json!({"action": "stake", "member_id": member}).to_string(),
        )
        .await?;
    assert!(stake.is_success(), "stake: {:?}", failure_text(&stake));

    // opt in: Action `auto_settle`, nonce 0 (first action after the join)
    let opt = relayer
        .call(f.circle.contract.id(), "set_auto_settle_evm")
        .args_json(json!({"member": member, "enabled": true, "nonce": 0, "deadline": deadline, "signature": action_sig(&secret, &contract, &member, "auto_settle", 0, "", "", 0, deadline)}))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert!(
        opt.is_success(),
        "set_auto_settle_evm: {:?}",
        failure_text(&opt)
    );
    assert_eq!(
        f.circle
            .view("is_auto_settle", json!({"account_id": member}))
            .await?,
        json!(true)
    );
    // the same signature cannot be used twice
    let again = relayer
        .call(f.circle.contract.id(), "set_auto_settle_evm")
        .args_json(json!({"member": member, "enabled": true, "nonce": 0, "deadline": deadline, "signature": action_sig(&secret, &contract, &member, "auto_settle", 0, "", "", 0, deadline)}))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert!(
        again.is_failure()
            && failure_text(&again)
                .unwrap_or_default()
                .contains("ERR_EVM_NONCE"),
        "{:?}",
        failure_text(&again)
    );

    // the EVM member owes gpu 20 credit
    let amount = 20 * CREDIT;
    let voucher = json!({"circle": contract, "payer": member, "payee": gpu.id(), "epoch": 1, "seq": 1, "cumulativeAmount": amount.to_string(), "unit": "credit", "resource": "/api/x"});
    let signature = voucher_sig(&secret, &contract, &member, gpu.id().as_str(), 1, 1, amount);
    let reg = relayer
        .call(f.circle.contract.id(), "register_cumulative")
        .args_json(json!({"voucher": voucher, "signature": signature}))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert!(
        reg.is_success(),
        "register_cumulative: {:?}",
        failure_text(&reg)
    );

    // close, clear nothing, commit; nobody pays; finalize
    let c = &f.circle;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c
        .call_keeper("close_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?)
        .await?;
    let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "commit: {:?}", failure_text(&commit));
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "finalize: {:?}", failure_text(&fin));
    assert!(
        fin.logs().iter().any(|l| l.contains("\"auto_settled\"")),
        "no auto_settled event: {:?}",
        fin.logs()
    );
    assert!(
        !fin.logs()
            .iter()
            .any(|l| l.contains("\"member_defaulted\"")),
        "defaulted: {:?}",
        fin.logs()
    );

    let m = f
        .circle
        .view("member", json!({"account_id": member}))
        .await?;
    assert_eq!(m["status"], "active");
    assert_eq!(m["stake"], (80 * CREDIT).to_string());
    let p = f
        .circle
        .view("net_position", json!({"epoch": 1, "account_id": member}))
        .await?;
    assert_eq!(p["status"], "paid", "{p}");

    // gpu claims the full amount
    let before = f.balance(&f.usdt, gpu.id()).await?;
    let claim = gpu
        .call(f.circle.contract.id(), "claim")
        .args_json(json!({"epoch": 1, "destination": "near"}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert!(claim.is_success(), "claim: {:?}", failure_text(&claim));
    assert_eq!(f.balance(&f.usdt, gpu.id()).await? - before, amount);
    f.assert_d6().await?;
    Ok(())
}

// TC-CT-108-07: finalize with 64 members, 63 opted in and unpaid
#[tokio::test]
async fn gas_finalize_with_64_members_all_auto_settled() -> anyhow::Result<()> {
    const N: usize = 64;
    let name = |i: usize| format!("m{i:02}");
    let mut params: Value = Funded::params_short();
    params["epoch_length"] = json!(3000);
    let mut f = Funded::deploy(Some(params)).await?;
    for i in 0..N {
        let n = name(i);
        let account = f.account(&n).await?;
        assert!(f.join(&n, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    for i in 1..N {
        let r = f.circle.register(&name(i), &name(0), 1, CREDIT).await?;
        assert!(r.is_success(), "{:?}", failure_text(&r));
        let on = f.accounts[&name(i)]
            .call(f.circle.contract.id(), "set_auto_settle")
            .args_json(json!({"enabled": true}))
            .deposit(NearToken::from_yoctonear(1))
            .transact()
            .await?;
        assert!(on.is_success(), "{:?}", failure_text(&on));
    }
    let c = &f.circle;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c
        .call_keeper("close_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?)
        .await?;
    assert!(c
        .call_keeper("commit_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    let gas = tgas(&fin);
    println!("GAS finalize_epoch {N} members, 63 auto-settled: {gas:.1} Tgas");
    assert!(
        gas < 500.0,
        "finalize uses {gas:.1} Tgas, more than half of the 1 PGas limit"
    );
    for i in [1, 31, 63] {
        let m = f
            .circle
            .view(
                "member",
                json!({"account_id": common::circle::account_of(&name(i))}),
            )
            .await?;
        assert_eq!(m["status"], "active");
        assert_eq!(m["stake"], (99 * CREDIT).to_string());
    }
    Ok(())
}
