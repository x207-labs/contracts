//! CT-082 in near-sandbox (F9): `max_search_share_bps` caps the seeker leg of a bundle. The seeker leg is named in
//! the signed terms (`search_leg`). Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, failure_text, CREDIT};
use common::funded::Funded;
use serde_json::json;

async fn three() -> anyhow::Result<Funded> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in ["scraper", "translator", "summarizer"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    Ok(f)
}

const ALL: [&str; 3] = ["scraper", "translator", "summarizer"];

// default 500 bps; legs sum 100 credit: the seeker leg of 5 is at the cap, 6 is over it
#[tokio::test]
async fn seeker_leg_is_capped_by_the_parameter() -> anyhow::Result<()> {
    let f = three().await?;
    let params = f.circle.view("get_params", json!({})).await?;
    assert_eq!(params["max_search_share_bps"], 500);

    // scraper -> translator 55, translator -> summarizer 40, summarizer -> scraper 6 (the seeker leg): 6 of 101 > 5%
    let over = [("scraper", "translator", 55 * CREDIT), ("translator", "summarizer", 40 * CREDIT), ("summarizer", "scraper", 6 * CREDIT)];
    let refused = f.circle.commit_bundle_with_search(1, &over, ("summarizer", "scraper"), &ALL, false).await?;
    assert!(failure_text(&refused).expect("refused").contains("ERR_SEARCH_SHARE_EXCEEDED"), "{:?}", failure_text(&refused));
    assert!(f.circle.view("get_bundle", json!({"bundle_id": 1})).await?.is_null());

    // 5 of 100 is exactly 500 bps
    let at_cap = [("scraper", "translator", 55 * CREDIT), ("translator", "summarizer", 40 * CREDIT), ("summarizer", "scraper", 5 * CREDIT)];
    let ok = f.circle.commit_bundle_with_search(1, &at_cap, ("summarizer", "scraper"), &ALL, false).await?;
    assert!(ok.is_success(), "{:?}", failure_text(&ok));
    assert_eq!(f.circle.view("get_bundle", json!({"bundle_id": 1})).await?["legs"].as_array().unwrap().len(), 3);
    Ok(())
}

// the seeker leg must be a leg, and it must be inside what the participants signed
#[tokio::test]
async fn seeker_leg_must_exist_and_be_signed() -> anyhow::Result<()> {
    let f = three().await?;
    let legs = [("scraper", "translator", 55 * CREDIT), ("translator", "summarizer", 40 * CREDIT), ("summarizer", "scraper", CREDIT)];
    let stranger = f.circle.commit_bundle_with_search(1, &legs, ("scraper", "summarizer"), &ALL, false).await?;
    assert!(failure_text(&stranger).expect("refused").contains("ERR_BAD_BUNDLE"));
    let unsigned = f.circle.commit_bundle_with_search(1, &legs, ("summarizer", "scraper"), &ALL, true).await?;
    assert!(failure_text(&unsigned).expect("refused").contains("ERR_BUNDLE_SIGNATURES"));
    // without a declared seeker leg the cap does not apply
    let plain = [("scraper", "translator", 10 * CREDIT), ("translator", "summarizer", 10 * CREDIT), ("summarizer", "scraper", 10 * CREDIT)];
    assert!(f.circle.commit_bundle(1, &plain, &ALL).await?.is_success());
    Ok(())
}

// the owner can change the cap; 10 001 is out of range
#[tokio::test]
async fn the_owner_sets_the_cap() -> anyhow::Result<()> {
    let mut f = three().await?;
    let dao = f.account("dao").await?;
    let mut params = f.circle.view("get_params", json!({})).await?;
    params["max_search_share_bps"] = json!(10_001);
    let bad = f.circle.call_as(&dao, "set_params", json!({"params": params.clone()})).await?;
    assert!(failure_text(&bad).expect("refused").contains("ERR_PARAM_OUT_OF_RANGE: max_search_share_bps"));
    params["max_search_share_bps"] = json!(0);
    let good = f.circle.call_as(&dao, "set_params", json!({"params": params})).await?;
    assert!(good.is_success(), "{:?}", failure_text(&good));
    assert_eq!(f.circle.view("get_pending_params", json!({})).await?["params"]["max_search_share_bps"], 0);
    Ok(())
}
