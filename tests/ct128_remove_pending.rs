//! CT-128 in near-sandbox (0.0.18, CT-123): an EVM member that joined and never staked is removed again, and its sponsor gets the join deposit back.
//! The owner removes it at once; a stranger may not before 7 days, and may after; a member with stake is never removed.
//!
//! Needs `cargo near build non-reproducible-wasm` first. EIP-712 helpers as in `ct117_relayer_fee.rs`.

mod common;

use common::circle::{events, failure_text, CREDIT};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::Account;
use secp256k1::{Message, Secp256k1, SecretKey};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};

const JOIN_DEPOSIT_MILLINEAR: u128 = 350;
const HOUR_NS: u64 = 3_600_000_000_000;

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

fn join_signature(
    secret: &SecretKey,
    contract: &str,
    member: &str,
    terms: &str,
    deadline: u64,
) -> String {
    let type_hash = ws("Action(string circle,string member,string action,uint64 epoch,string receiver,string termsHash,uint64 nonce,uint64 deadline,uint256 maxFee)");
    let hash = k(&[
        type_hash,
        ws(contract),
        ws(member),
        ws("join"),
        w64(0),
        ws(""),
        ws(terms),
        w64(0),
        w64(deadline),
        w128(0),
    ]
    .concat());
    sign(secret, digest(contract, hash))
}

struct World {
    f: Funded,
    relayer: Account,
    member: String,
}

async fn world() -> anyhow::Result<World> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let contract = f.circle.contract.id().to_string();
    let secret = SecretKey::from_slice(&[11u8; 32])?;
    let member = address(&secret);
    let deadline = f.circle.now_ns().await? / 1_000_000 + 3_600_000;
    let relayer = f.account("relayer").await?;
    let terms = "a".repeat(64);
    let join = relayer
        .call(f.circle.contract.id(), "join_evm")
        .args_json(json!({"address": member, "terms_hash": terms, "deadline": deadline, "signature": join_signature(&secret, &contract, &member, &terms, deadline)}))
        .deposit(NearToken::from_millinear(JOIN_DEPOSIT_MILLINEAR))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert!(join.is_success(), "join_evm: {:?}", failure_text(&join));
    Ok(World { f, relayer, member })
}

impl World {
    async fn member_view(&self) -> anyhow::Result<Value> {
        self.f
            .circle
            .view("member", json!({"account_id": self.member}))
            .await
    }

    async fn near_balance(&self) -> anyhow::Result<u128> {
        Ok(self.relayer.view_account().await?.balance.as_yoctonear())
    }
}

// TC-CT-128-01: the owner removes the pending EVM member; the sponsor gets the deposit back and the slot is free
#[tokio::test]
async fn the_owner_removes_a_pending_evm_member_and_the_sponsor_is_refunded() -> anyhow::Result<()>
{
    let mut w = world().await?;
    assert_eq!(w.member_view().await?["status"], "pending");
    let count_before = w.f.circle.view("get_circle", json!({})).await?["member_count"].as_u64();
    let dao = w.f.account("dao").await?;
    let before = w.near_balance().await?;
    let removed =
        w.f.circle
            .call_as(&dao, "remove_pending", json!({"account_id": w.member}))
            .await?;
    assert!(removed.is_success(), "{:?}", failure_text(&removed));
    assert!(events(&removed).iter().any(|(n, d)| n == "member_removed"
        && d["account_id"] == w.member.as_str()
        && d["by_owner"] == true));
    assert!(w.member_view().await?.is_null());
    let after = w.near_balance().await?;
    assert!(
        after - before >= NearToken::from_millinear(300).as_yoctonear(),
        "the sponsor got the join deposit back: {before} -> {after}"
    );
    if let Some(count) = count_before {
        let now = w.f.circle.view("get_circle", json!({})).await?["member_count"].as_u64();
        assert_eq!(now, Some(count - 1));
    }
    Ok(())
}

// TC-CT-128-02: a stranger removes it only after 7 days; a member with stake is never removed
#[tokio::test]
async fn a_stranger_waits_seven_days_and_a_staked_member_stays() -> anyhow::Result<()> {
    let mut w = world().await?;
    let eve = w.f.account("eve").await?;
    let early =
        w.f.circle
            .call_as(&eve, "remove_pending", json!({"account_id": w.member}))
            .await?;
    assert!(failure_text(&early)
        .unwrap_or_default()
        .contains("ERR_PENDING_TOO_YOUNG"));

    // a member that has a stake is refused for everybody
    let relayer = w.relayer.clone();
    let stake =
        w.f.transfer_call(
            &w.f.usdt.clone(),
            &relayer,
            100 * CREDIT,
            &json!({"action": "stake", "member_id": w.member}).to_string(),
        )
        .await?;
    assert!(stake.is_success(), "stake: {:?}", failure_text(&stake));
    let dao = w.f.account("dao").await?;
    let staked =
        w.f.circle
            .call_as(&dao, "remove_pending", json!({"account_id": w.member}))
            .await?;
    assert!(failure_text(&staked)
        .unwrap_or_default()
        .contains("ERR_NOT_PENDING"));
    assert!(!w.member_view().await?.is_null());
    Ok(())
}

// TC-CT-128-03: with no stake, once the delay ran out anyone removes the row. The delay is 7 days by default; the owner shortens it to the minimum of one hour
// (`set_pending_gc_delay`) so that the sandbox reaches it in about 3,600 blocks. The 7-day default itself is checked by the unit tests TC-CT-123-03 and -05.
#[tokio::test]
async fn once_the_delay_ran_out_anyone_removes_an_unadmitted_row() -> anyhow::Result<()> {
    let mut w = world().await?;
    let dao = w.f.account("dao").await?;
    let set =
        w.f.circle
            .call_as(&dao, "set_pending_gc_delay", json!({"seconds": "3600"}))
            .await?;
    assert!(set.is_success(), "{:?}", failure_text(&set));
    assert_eq!(
        w.f.circle.view("get_pending_gc_delay", json!({})).await?,
        json!("3600")
    );
    let target = w.f.circle.now_ns().await? + HOUR_NS + 60_000_000_000;
    w.f.circle.advance_to(target).await?;
    let eve = w.f.account("eve").await?;
    let sponsor_before = w.near_balance().await?;
    let removed =
        w.f.circle
            .call_as(&eve, "remove_pending", json!({"account_id": w.member}))
            .await?;
    assert!(removed.is_success(), "{:?}", failure_text(&removed));
    assert!(events(&removed)
        .iter()
        .any(|(n, d)| n == "member_removed" && d["by_owner"] == false));
    assert!(w.member_view().await?.is_null());
    // coordinator point 1a: a removal by a stranger keeps the join deposit, so squatting a slot costs 0.35 NEAR
    let sponsor_after = w.near_balance().await?;
    assert!(
        sponsor_after < sponsor_before + NearToken::from_millinear(10).as_yoctonear(),
        "no refund to the sponsor: {sponsor_before} -> {sponsor_after}"
    );
    Ok(())
}

// TC-CT-128-04 (coordinator point 1): an attacker fills all 64 slots with pending rows; a legitimate member is refused while the rows are young, then joins
// once the oldest empty row is older than the delay, and that row is evicted. Every squatted slot cost the 0.35 NEAR join deposit.
#[tokio::test]
async fn a_squatter_cannot_hold_the_circle_for_long() -> anyhow::Result<()> {
    let mut w = world().await?;
    let dao = w.f.account("dao").await?;
    let set =
        w.f.circle
            .call_as(&dao, "set_pending_gc_delay", json!({"seconds": "3600"}))
            .await?;
    assert!(set.is_success(), "{:?}", failure_text(&set));
    let root = w.f.circle.worker.root_account()?;
    // `world` already holds one pending EVM row: 63 more fill the circle
    let mut squatters = Vec::new();
    for i in 0..63 {
        let name = format!("sq{i}");
        let account = root
            .create_subaccount(&name)
            .initial_balance(NearToken::from_near(1))
            .transact()
            .await?
            .into_result()?;
        let key = w.f.circle.key_for(&name);
        let join = account
            .call(w.f.circle.contract.id(), "join")
            .args_json(json!({"circle_id": "agents-main", "pubkey_voucher": key, "terms_hash": "a".repeat(64)}))
            .deposit(NearToken::from_millinear(350))
            .gas(Gas::from_tgas(50))
            .transact()
            .await?;
        assert!(join.is_success(), "{name}: {:?}", failure_text(&join));
        squatters.push(account);
    }
    let circle = w.f.circle.view("get_circle", json!({})).await?;
    assert_eq!(circle["member_count"].as_u64(), Some(64));

    // a legitimate member is refused while every row is young
    let late = w.f.account("late").await?;
    let refused = w.f.join("late", Funded::join_deposit()).await?;
    assert!(
        failure_text(&refused)
            .unwrap_or_default()
            .contains("ERR_MAX_MEMBERS"),
        "{:?}",
        failure_text(&refused)
    );
    // after the delay the oldest empty row makes room: the legitimate member gets in, the circle stays at 64
    let target = w.f.circle.now_ns().await? + HOUR_NS + 60_000_000_000;
    w.f.circle.advance_to(target).await?;
    let joined = w.f.join("late", Funded::join_deposit()).await?;
    assert!(joined.is_success(), "{:?}", failure_text(&joined));
    assert!(events(&joined).iter().any(|(n, d)| n == "member_removed"
        && d["account_id"] == w.member.as_str()
        && d["by_owner"] == false));
    assert!(w.member_view().await?.is_null(), "the oldest row went");
    assert_eq!(
        w.f.circle.view("get_circle", json!({})).await?["member_count"].as_u64(),
        Some(64)
    );
    assert_eq!(
        w.f.circle
            .view("member", json!({"account_id": late.id()}))
            .await?["status"],
        "pending"
    );
    Ok(())
}
