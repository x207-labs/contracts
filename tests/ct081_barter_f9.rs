//! CT-080 and CT-081 in near-sandbox (F9, ADR-38): quick barter with the example 100 / 10 / 3 of the F9 feature note
//! (value 100, discount 10, searcher share 3). The split is an illustration and is not in the contract: cashback and
//! searcher share are ordinary legs of the bundle, signed by everyone, and the contract only sees pairs and amounts.
//! Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, failure_text, CREDIT};
use common::flow::edge;
use common::funded::Funded;
use serde_json::json;

async fn circle(names: &[&str]) -> anyhow::Result<Funded> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in names {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 200 * CREDIT).await?.is_success());
    }
    Ok(f)
}

/// Delivers every leg in full, closes the epoch, clears `cleared` edges and commits.
async fn deliver_and_clear(f: &mut Funded, bundle_id: u64, legs: &[(&str, &str, u128)], cleared: &[(&str, &str, u128)]) -> anyhow::Result<()> {
    for (payer, payee, amount) in legs {
        let r = f.circle.register_in_bundle(bundle_id, payer, payee, 1, *amount).await?;
        assert!(r.is_success(), "{payer}->{payee}: {:?}", failure_text(&r));
    }
    let c = &f.circle;
    let solver = c.worker.dev_create_account().await?;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
    if !cleared.is_empty() {
        let edges: Vec<_> = cleared.iter().map(|(d, k, a)| edge(d, k, *a)).collect();
        let submit = c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": edges})).await?;
        assert!(submit.is_success(), "{:?}", failure_text(&submit));
    }
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?).await?;
    let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    Ok(())
}

async fn position(f: &Funded, name: &str) -> anyhow::Result<(i64, String)> {
    let v = f.circle.view("net_position", json!({"epoch": 1, "account_id": account_of(name)})).await?;
    if v.is_null() {
        return Ok((0, "none".into()));
    }
    let net: i64 = v["net"].as_str().unwrap().parse()?;
    Ok((net / CREDIT as i64, v["status"].as_str().unwrap().to_string()))
}

// CT-081: pay path. The counterparty pays 93: 90 to Andi, 3 to the searcher.
#[tokio::test]
async fn pay_path_93_is_split_90_and_3() -> anyhow::Result<()> {
    let mut f = circle(&["andi", "lawan", "pencari"]).await?;
    let legs = [("lawan", "andi", 90 * CREDIT), ("lawan", "pencari", 3 * CREDIT)];
    let commit = f.circle.commit_bundle(1, &legs, &["andi", "lawan", "pencari"]).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    deliver_and_clear(&mut f, 1, &legs, &[]).await?;
    assert_eq!(position(&f, "lawan").await?, (-93, "due".to_string()));
    assert_eq!(position(&f, "andi").await?, (90, "claimable".to_string()));
    assert_eq!(position(&f, "pencari").await?, (3, "claimable".to_string()));
    f.assert_d6().await?;
    Ok(())
}

// CT-080 and CT-081: barter path. The counterparty hands B worth 100 and gets A worth 100 plus 7 cashback; Andi also
// owes the searcher 3. Legs: andi -> lawan 107 (B 100 + cashback 7), lawan -> andi 100 (A), andi -> pencari 3.
#[tokio::test]
async fn barter_path_clears_the_goods_and_leaves_cashback_and_share() -> anyhow::Result<()> {
    let mut f = circle(&["andi", "lawan", "pencari"]).await?;
    let legs = [("andi", "lawan", 107 * CREDIT), ("lawan", "andi", 100 * CREDIT), ("andi", "pencari", 3 * CREDIT)];

    // CT-080: the searcher is a participant, so a bundle without the searcher's signature is refused
    let unsigned = f.circle.commit_bundle(1, &legs, &["andi", "lawan"]).await?;
    assert!(failure_text(&unsigned).expect("refused").contains("ERR_BUNDLE_SIGNATURES"));

    let commit = f.circle.commit_bundle(1, &legs, &["andi", "lawan", "pencari"]).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    let view = f.circle.view("get_bundle", json!({"bundle_id": 1})).await?;
    assert_eq!(view["legs"].as_array().unwrap().len(), 3, "goods, cashback and share are stored as legs");
    // Andi reserves 110 of his 160 limit
    assert_eq!(f.circle.view("headroom", json!({"account_id": account_of("andi")})).await?, "50000000");
    // the goods legs clear against each other (100 both ways); cashback 7 and share 3 stay as residual
    deliver_and_clear(&mut f, 1, &legs, &[("andi", "lawan", 100 * CREDIT), ("lawan", "andi", 100 * CREDIT)]).await?;
    assert_eq!(position(&f, "andi").await?, (-10, "due".to_string()), "Andi carries cashback 7 plus share 3");
    assert_eq!(position(&f, "lawan").await?, (7, "claimable".to_string()), "the counterparty gets the cashback");
    assert_eq!(position(&f, "pencari").await?, (3, "claimable".to_string()), "the searcher gets the share");
    f.assert_d6().await?;
    Ok(())
}

// CT-081: circular barter A -> B -> C -> A of value 100 each plus the searcher share 3 from Andi: the ring clears
// completely and only the share is left.
#[tokio::test]
async fn circular_barter_clears_the_ring_and_leaves_the_share() -> anyhow::Result<()> {
    let mut f = circle(&["andi", "budi", "cici", "pencari"]).await?;
    let legs = [("andi", "budi", 100 * CREDIT), ("budi", "cici", 100 * CREDIT), ("cici", "andi", 100 * CREDIT), ("andi", "pencari", 3 * CREDIT)];
    let commit = f.circle.commit_bundle(1, &legs, &["andi", "budi", "cici", "pencari"]).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    deliver_and_clear(&mut f, 1, &legs, &[("andi", "budi", 100 * CREDIT), ("budi", "cici", 100 * CREDIT), ("cici", "andi", 100 * CREDIT)]).await?;
    assert_eq!(position(&f, "andi").await?, (-3, "due".to_string()));
    assert_eq!(position(&f, "pencari").await?, (3, "claimable".to_string()));
    assert_eq!(position(&f, "budi").await?, (0, "none".to_string()));
    assert_eq!(position(&f, "cici").await?, (0, "none".to_string()));
    f.assert_d6().await?;
    Ok(())
}
