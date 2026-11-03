//! CT-084 in near-sandbox: economic attacks through bundles (security review, "Not checked"): wash trading (ADR-08), a limit above the stake or above the
//! headroom through reservations, and clearing with bundles that are not fully registered, followed by `prune_epoch`. Needs
//! `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, failure_text, CREDIT};
use common::flow::{edge, finalize};
use common::funded::Funded;
use serde_json::{json, Value};

const ALL: [&str; 3] = ["scraper", "translator", "summarizer"];

async fn three() -> anyhow::Result<Funded> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in ALL {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    Ok(f)
}

async fn member(f: &Funded, name: &str) -> anyhow::Result<Value> {
    f.circle.view("member", json!({"account_id": account_of(name)})).await
}

async fn headroom(f: &Funded, name: &str) -> anyhow::Result<u128> {
    Ok(f.circle.view("headroom", json!({"account_id": account_of(name)})).await?.as_str().unwrap().parse()?)
}

async fn clear_and_commit(f: &Funded, edges: Value) -> anyhow::Result<()> {
    let c = &f.circle;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
    if edges != json!([]) {
        let solver = c.worker.dev_create_account().await?;
        let submit = c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": edges})).await?;
        assert!(submit.is_success(), "{:?}", failure_text(&submit));
    }
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?).await?;
    let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    Ok(())
}

// TC-CT-084-01 (ADR-08): three bundles that are cycles, every leg registered, everything cleared: no position, no settled_history, the limit of the next epoch is the base
#[tokio::test]
async fn cycles_through_bundles_do_not_raise_a_limit() -> anyhow::Result<()> {
    let mut f = three().await?;
    let mut total = 0u128;
    for k in 1..=3u128 {
        let x = 10 * k * CREDIT;
        let legs = [("scraper", "translator", x), ("translator", "summarizer", x), ("summarizer", "scraper", x)];
        let commit = f.circle.commit_bundle(1, &legs, &ALL).await?;
        assert!(commit.is_success(), "bundle {k}: {:?}", failure_text(&commit));
        total += x;
        for (payer, payee, _) in legs {
            let register = f.circle.register_in_bundle(k as u64, payer, payee, 1, total).await?;
            assert!(register.is_success(), "{payer}->{payee} bundle {k}: {:?}", failure_text(&register));
        }
    }
    for name in ALL {
        assert_eq!(headroom(&f, name).await?, 80 * CREDIT - total, "{name}: the legs became debt, nothing stays reserved");
    }
    let edges = json!([edge("scraper", "translator", total), edge("translator", "summarizer", total), edge("summarizer", "scraper", total)]);
    clear_and_commit(&f, edges).await?;
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    assert_eq!(f.circle.view("list_positions", json!({"epoch": 1, "from_index": 0, "limit": 10})).await?, json!([]));
    for name in ALL {
        let view = member(&f, name).await?;
        assert_eq!((view["settled_history"].as_str(), view["limit"].as_str()), (Some("0"), Some("80000000")), "{name}");
        assert_eq!(f.circle.view("outstanding_debt", json!({"account_id": account_of(name)})).await?, "0");
        assert_eq!(headroom(&f, name).await?, 80 * CREDIT);
    }
    Ok(())
}

// TC-CT-084-02: reservations and plain vouchers share one limit; debt plus reservation never goes past it
#[tokio::test]
async fn debt_plus_reservation_never_passes_the_limit() -> anyhow::Result<()> {
    let mut f = three().await?;
    let legs = [("scraper", "translator", 60 * CREDIT), ("translator", "scraper", CREDIT)];
    assert!(f.circle.commit_bundle(1, &legs, &["scraper", "translator"]).await?.is_success());
    assert_eq!(headroom(&f, "scraper").await?, 20 * CREDIT);
    // a second bundle cannot reserve what the first one holds
    let second = f.circle.commit_bundle(1, &[("scraper", "summarizer", 30 * CREDIT), ("summarizer", "scraper", CREDIT)], &["scraper", "summarizer"]).await?;
    assert!(failure_text(&second).expect("refused").contains("ERR_BUNDLE_HEADROOM"));
    // plain vouchers use the free part only
    assert!(f.circle.register("scraper", "summarizer", 1, 20 * CREDIT).await?.is_success());
    assert_eq!(headroom(&f, "scraper").await?, 0);
    let over = f.circle.register("scraper", "summarizer", 1, 21 * CREDIT).await?;
    assert!(failure_text(&over).expect("refused").contains("ERR_INSUFFICIENT_CREDIT"));
    // half of the leg: reservation falls by what became debt, headroom stays 0
    assert!(f.circle.register_in_bundle(1, "scraper", "translator", 1, 30 * CREDIT).await?.is_success());
    assert_eq!(headroom(&f, "scraper").await?, 0);
    let too_much = f.circle.register_in_bundle(1, "scraper", "translator", 1, 61 * CREDIT).await?;
    assert!(failure_text(&too_much).expect("refused").contains("ERR_BUNDLE_LEG_EXCEEDED"));
    assert!(f.circle.register_in_bundle(1, "scraper", "translator", 1, 60 * CREDIT).await?.is_success());
    let debt: String = f.circle.view("outstanding_debt", json!({"account_id": account_of("scraper")})).await?.as_str().unwrap().into();
    assert_eq!(debt, (80 * CREDIT).to_string(), "debt is exactly the limit: 20 plain, 60 leg");
    let one_more = f.circle.register("scraper", "summarizer", 1, 21 * CREDIT + 1).await?;
    assert!(failure_text(&one_more).expect("refused").contains("ERR_INSUFFICIENT_CREDIT"));
    let view = member(&f, "scraper").await?;
    assert!(view["limit"].as_str().unwrap().parse::<u128>()? <= view["stake"].as_str().unwrap().parse::<u128>()?);
    Ok(())
}

// TC-CT-084-03: a bundle that is registered in part, then clearing, finalize and prune_epoch: no reservation stays, the unclaimed claim survives, the epoch after is clean
#[tokio::test]
async fn a_partly_registered_bundle_leaves_nothing_behind_after_prune() -> anyhow::Result<()> {
    let mut f = three().await?;
    let legs = [("scraper", "translator", 10 * CREDIT), ("translator", "summarizer", 10 * CREDIT), ("summarizer", "scraper", 10 * CREDIT)];
    assert!(f.circle.commit_bundle(1, &legs, &ALL).await?.is_success());
    assert_eq!(headroom(&f, "scraper").await?, 70 * CREDIT);
    // only one leg is registered, 4 of 10
    assert!(f.circle.register_in_bundle(1, "scraper", "translator", 1, 4 * CREDIT).await?.is_success());
    assert_eq!(headroom(&f, "scraper").await?, 70 * CREDIT);
    clear_and_commit(&f, json!([])).await?; // a single edge cannot clear: nothing to clear
    let scraper = f.accounts["scraper"].clone();
    let pay = f.transfer_call(&f.usdt, &scraper, 4 * CREDIT, r#"{"action":"settle","epoch":1}"#).await?;
    assert!(pay.is_success(), "{:?}", failure_text(&pay));
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    let translator_before = f.circle.view("net_position", json!({"epoch": 1, "account_id": account_of("translator")})).await?;
    assert_eq!(translator_before["status"], "claimable");

    // epoch 2 is clean: the reservation of epoch 1 does not count
    for name in ALL {
        let limit: u128 = member(&f, name).await?["limit"].as_str().unwrap().parse()?;
        assert_eq!(headroom(&f, name).await?, limit, "{name}: no debt, no reservation");
    }
    let epoch = f.circle.view("get_epoch", json!({})).await?["epoch"].as_u64().unwrap();
    assert_eq!(epoch, 2);

    // prune the old epoch: everything of the bundle goes, the position stays
    let outsider = f.circle.worker.dev_create_account().await?;
    let mut removed = 0u64;
    loop {
        let step = f.circle.call_as(&outsider, "prune_epoch", json!({"epoch": 1, "max_items": 50})).await?;
        assert!(step.is_success(), "{:?}", failure_text(&step));
        let n = step.json::<u64>()?;
        if n == 0 {
            break;
        }
        removed += n;
    }
    assert!(removed > 0);
    assert_eq!(f.circle.view("get_bundle", json!({"bundle_id": 1})).await?, Value::Null);
    assert_eq!(f.circle.view("net_position", json!({"epoch": 1, "account_id": account_of("translator")})).await?, translator_before);
    // the old bundle cannot be used any more (the running epoch is past its cutoff in the sandbox, so the refusal is not `ERR_UNKNOWN_BUNDLE`)
    let late = f.circle.register_in_bundle(1, "scraper", "translator", 2, 5 * CREDIT).await?;
    assert!(failure_text(&late).is_some());
    for name in ALL {
        let limit: u128 = member(&f, name).await?["limit"].as_str().unwrap().parse()?;
        assert_eq!(headroom(&f, name).await?, limit, "{name}: nothing reserved after the prune");
    }
    f.assert_d6().await
}
