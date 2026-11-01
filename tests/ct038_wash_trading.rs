//! CT-038: anti wash trading (ADR-08). Members are written into storage (`seed_member`), because
//! `join` and stake were separate items when this file was written. TC-CT-038-01 runs through `finalize_epoch`
//! on seeded members and TC-CT-038-02 on joined and staked ones. Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, failure_text, short_params, Circle, CREDIT};
use common::flow::{finalize, s1_committed};
use near_sdk::borsh;
use near_sdk::PublicKey;
use serde_json::{json, Value};
use x207_circle::member::{Member, MemberStatus};
use x207_circle::params::Params;
use x207_circle::risk::compute_limit;

fn edge(debtor: &str, creditor: &str, cleared: u128) -> Value {
    json!({"debtor": account_of(debtor), "creditor": account_of(creditor), "cleared": cleared.to_string()})
}

/// Rewrites a seeded member as `pending` with stake 0, as `join` leaves it before any stake.
async fn make_pending(c: &Circle, name: &str) -> anyhow::Result<()> {
    let id = account_of(name).parse()?;
    let pubkey = PublicKey::from_parts(near_sdk::CurveType::ED25519, c.keys[name].verifying_key().to_bytes().to_vec())?;
    let member = Member {
        account_id: id,
        pubkey_voucher: pubkey,
        terms_hash: "a".repeat(64),
        stake: 0,
        limit: 0,
        status: MemberStatus::Pending,
        admitted: false,
        joined_at: 1,
        settled_history: 0,
        last_active_epoch: 1,
        exit_requested_epoch: None,
        withdraw_pending: false,
        reactivate_epoch: None,
    };
    let mut key = b"m".to_vec();
    key.extend(borsh::to_vec(&member.account_id)?);
    c.worker.patch_state(c.contract.id(), &key, &borsh::to_vec(&member)?).await?;
    Ok(())
}

// TC-CT-038-01: a fully cleared cycle leaves no D5 record, `settled_history` 0, limit 80, also after finalize
#[tokio::test]
async fn cycle_clearing_does_not_raise_limits() -> anyhow::Result<()> {
    let mut c = Circle::deploy(short_params()).await?;
    for member in ["translator", "gpu"] {
        c.seed_member(member, 100 * CREDIT, 80 * CREDIT).await?;
    }
    assert!(c.register("translator", "gpu", 1, 50 * CREDIT).await?.is_success());
    assert!(c.register("gpu", "translator", 1, 50 * CREDIT).await?.is_success());

    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
    let edges = json!([edge("translator", "gpu", 50 * CREDIT), edge("gpu", "translator", 50 * CREDIT)]);
    let solver = c.worker.dev_create_account().await?;
    let submitted = c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": edges})).await?;
    assert!(submitted.is_success(), "{:?}", failure_text(&submitted));
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?).await?;
    let committed = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(committed.is_success(), "{:?}", failure_text(&committed));
    let fin = finalize(&c).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));

    let positions = c.view("list_positions", json!({"epoch": 1, "from_index": 0, "limit": 10})).await?;
    assert_eq!(positions, json!([]));
    for member in ["translator", "gpu"] {
        let view = c.view("member", json!({"account_id": account_of(member)})).await?;
        assert_eq!((view["settled_history"].as_str(), view["limit"].as_str()), (Some("0"), Some("80000000")), "{member}");
        let limit = c.view("limit", json!({"account_id": account_of(member)})).await?;
        assert_eq!(limit, "80000000");
        let debt = c.view("outstanding_debt", json!({"account_id": account_of(member)})).await?;
        assert_eq!(debt, "0");
    }
    Ok(())
}

// TC-CT-038-03: the growth term is clamped to the stake
#[test]
fn growth_is_clamped_to_stake() {
    let params = Params::default();
    assert_eq!(compute_limit(100 * CREDIT, 500 * CREDIT, &params), 100 * CREDIT);
    assert_eq!(compute_limit(100 * CREDIT, 0, &params), 80 * CREDIT);
}

// TC-CT-038-04: a member without stake can be neither payer nor payee
#[tokio::test]
async fn member_without_stake_cannot_register() -> anyhow::Result<()> {
    let mut c = Circle::deploy(short_params()).await?;
    for member in ["translator", "unstaked"] {
        c.seed_member(member, 100 * CREDIT, 80 * CREDIT).await?;
    }
    make_pending(&c, "unstaked").await?;

    let as_payer = c.register("unstaked", "translator", 1, CREDIT).await?;
    assert!(failure_text(&as_payer).expect("rejected").contains("ERR_PAYER_NOT_ACTIVE"));
    let as_payee = c.register("translator", "unstaked", 1, CREDIT).await?;
    assert!(failure_text(&as_payee).expect("rejected").contains("ERR_PAYEE_NOT_ACTIVE"));
    let list = c.view("list_obligations", json!({"epoch": 1, "from_index": 0, "limit": 10})).await?;
    assert_eq!(list, json!([]));
    Ok(())
}

// TC-CT-038-02: S1.fin, the debtor pays cash inside the window and finalize raises the limit but never above the stake
#[tokio::test]
async fn cash_settlement_raises_the_limit() -> anyhow::Result<()> {
    let f = s1_committed().await?;
    let scraper = f.accounts["scraper"].clone();
    let pay = f.transfer_call(&f.usdt, &scraper, 35 * CREDIT, r#"{"action":"settle","epoch":1}"#).await?;
    assert!(pay.is_success(), "{:?}", failure_text(&pay));
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    let member = f.member("scraper").await?;
    assert_eq!(member["limit"], "83500000", "{member}");
    let stake: u128 = member["stake"].as_str().unwrap().parse()?;
    assert!(83_500_000 <= stake);
    Ok(())
}
