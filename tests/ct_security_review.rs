//! Security review of the F5 to F8 code (docs/security-review.md): attacks written as sandbox tests. Needs `cargo near build non-reproducible-wasm` first.
//! The tests that pass show a property that held; the ones marked in the review as findings are unit tests in `src` (run with `--ignored`).

mod common;

use common::circle::{account_of, failure_text, CREDIT};
use common::funded::Funded;
use serde_json::{json, Value};

async fn three() -> anyhow::Result<Funded> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in ["scraper", "translator", "summarizer"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    Ok(f)
}

// S-F2: what a member costs the contract when it posts intents. The caller attaches nothing; the storage of the circle account grows.
#[tokio::test]
async fn posting_intents_grows_the_storage_of_the_contract_and_asks_nothing_of_the_caller() -> anyhow::Result<()> {
    let f = three().await?;
    let account = f.accounts["scraper"].clone();
    let storage = |f: &Funded| {
        let id = f.circle.contract.id().clone();
        let worker = f.circle.worker.clone();
        async move { worker.view_account(&id).await.map(|a| (a.storage_usage, a.balance.as_yoctonear())) }
    };
    let (usage_before, _) = storage(&f).await?;
    for n in 0..20u8 {
        let hash = near_sdk::bs58::encode([n + 1; 32]).into_string();
        let r = f.circle.call_as(&account, "post_intent", json!({"intent_hash": hash})).await?;
        assert!(r.is_success(), "{:?}", failure_text(&r));
    }
    let (usage_after, _) = storage(&f).await?;
    let per_intent = (usage_after - usage_before) / 20;
    println!("STORAGE per intent: {per_intent} bytes");
    assert!(per_intent >= 50, "{per_intent}");
    Ok(())
}

// S-A: a signature of a bundle cannot commit another bundle, in another epoch, or with one more leg
#[tokio::test]
async fn a_bundle_signature_is_good_for_its_own_terms_only() -> anyhow::Result<()> {
    let f = three().await?;
    let cycle = [("scraper", "translator", 6 * CREDIT), ("translator", "summarizer", 10 * CREDIT), ("summarizer", "scraper", 8 * CREDIT)];
    let (message, hash) = f.circle.bundle_terms(1, &cycle);
    let sigs: Vec<Value> = ["scraper", "translator", "summarizer"].iter().map(|s| json!({"account_id": account_of(s), "signature": f.circle.sign_terms(s, &message, hash)})).collect();
    // the same signatures on terms with a changed amount, an extra leg, or the legs of another pair order
    let variants: Vec<Vec<(&str, &str, u128)>> = vec![
        vec![("scraper", "translator", 6 * CREDIT + 1), cycle[1], cycle[2]],
        vec![cycle[0], cycle[1], cycle[2], ("scraper", "summarizer", CREDIT)],
        vec![("translator", "scraper", 6 * CREDIT), cycle[1], cycle[2]],
    ];
    for legs in variants {
        let named: Vec<Value> = legs.iter().map(|(a, b, m)| json!({"payer": account_of(a), "payee": account_of(b), "max_amount": m.to_string()})).collect();
        let r = f.circle.call_keeper("commit_bundle", json!({"bundle": {"epoch": 1, "legs": named}, "signatures": sigs})).await?;
        assert!(failure_text(&r).expect("refused").contains("ERR_BUNDLE_SIGNATURES"), "{legs:?}");
    }
    // a search_leg that the signers did not sign (a small leg, so that the cap on the share does not answer first)
    let small = [("scraper", "translator", 50 * CREDIT), ("translator", "summarizer", 50 * CREDIT), ("summarizer", "scraper", CREDIT)];
    let (message, hash) = f.circle.bundle_terms(1, &small);
    let sigs: Vec<Value> = ["scraper", "translator", "summarizer"].iter().map(|s| json!({"account_id": account_of(s), "signature": f.circle.sign_terms(s, &message, hash)})).collect();
    let named: Vec<Value> = small.iter().map(|(a, b, m)| json!({"payer": account_of(a), "payee": account_of(b), "max_amount": m.to_string()})).collect();
    let tagged = f
        .circle
        .call_keeper("commit_bundle", json!({"bundle": {"epoch": 1, "legs": named, "search_leg": {"payer": account_of("summarizer"), "payee": account_of("scraper")}}, "signatures": sigs}))
        .await?;
    assert!(failure_text(&tagged).expect("refused").contains("ERR_BUNDLE_SIGNATURES"), "a declared seeker leg changes the signed terms: {:?}", failure_text(&tagged));
    assert_eq!(f.circle.view("get_bundle", json!({"bundle_id": 1})).await?, Value::Null);
    Ok(())
}

// S-B: a bundle cannot reserve the headroom of an account that did not sign it, and two bundles cannot reserve more than the headroom together
#[tokio::test]
async fn bundles_cannot_reserve_more_than_the_headroom_or_the_headroom_of_a_stranger() -> anyhow::Result<()> {
    let f = three().await?;
    let all = ["scraper", "translator", "summarizer"];
    // limit 80: the first bundle takes 60 of scraper
    assert!(f.circle.commit_bundle(1, &[("scraper", "translator", 60 * CREDIT), ("translator", "scraper", CREDIT)], &["scraper", "translator"]).await?.is_success());
    // a second bundle that needs another 30 of scraper is refused although everybody signed it
    let second = f.circle.commit_bundle(1, &[("scraper", "summarizer", 30 * CREDIT), ("summarizer", "scraper", CREDIT)], &["scraper", "summarizer"]).await?;
    assert!(failure_text(&second).expect("refused").contains("ERR_BUNDLE_HEADROOM"));
    // the payee of a leg must sign too: translator cannot be dragged into a bundle by the others
    let (message, hash) = f.circle.bundle_terms(1, &[("summarizer", "translator", CREDIT), ("scraper", "summarizer", CREDIT)]);
    let sigs = json!([
        {"account_id": account_of("summarizer"), "signature": f.circle.sign_terms("summarizer", &message, hash)},
        {"account_id": account_of("scraper"), "signature": f.circle.sign_terms("scraper", &message, hash)},
    ]);
    let legs = json!([
        {"payer": account_of("summarizer"), "payee": account_of("translator"), "max_amount": CREDIT.to_string()},
        {"payer": account_of("scraper"), "payee": account_of("summarizer"), "max_amount": CREDIT.to_string()},
    ]);
    let dragged = f.circle.call_keeper("commit_bundle", json!({"bundle": {"epoch": 1, "legs": legs}, "signatures": sigs})).await?;
    assert!(failure_text(&dragged).expect("refused").contains("ERR_BUNDLE_SIGNATURES"));
    let _ = all;
    Ok(())
}

// S-C: a voucher of a leg is a payment from the payer to the payee of the leg and nothing else: it cannot be registered against the leg of another pair
#[tokio::test]
async fn a_leg_voucher_cannot_be_registered_on_another_pair_or_beyond_the_leg() -> anyhow::Result<()> {
    let mut f = three().await?;
    let cycle = [("scraper", "translator", 6 * CREDIT), ("translator", "summarizer", 10 * CREDIT), ("summarizer", "scraper", 8 * CREDIT)];
    assert!(f.circle.commit_bundle(1, &cycle, &["scraper", "translator", "summarizer"]).await?.is_success());
    // a voucher of scraper to summarizer is not a leg of the bundle
    let other_pair = f.circle.register_in_bundle(1, "scraper", "summarizer", 1, CREDIT).await?;
    assert!(failure_text(&other_pair).expect("refused").contains("ERR_BUNDLE_LEG"));
    // 7 on a leg of 6: refused, and nothing of it is booked
    let over = f.circle.register_in_bundle(1, "scraper", "translator", 1, 7 * CREDIT).await?;
    assert!(failure_text(&over).expect("refused").contains("ERR_BUNDLE_LEG_EXCEEDED"));
    assert_eq!(f.circle.view("outstanding_debt", json!({"account_id": account_of("scraper")})).await?, "0");
    // a bundle of epoch 1 does not take a voucher of epoch 2
    let wrong_epoch = f.circle.register_in_bundle(1, "scraper", "translator", 2, CREDIT).await?;
    assert!(failure_text(&wrong_epoch).expect("refused").contains("ERR_EPOCH_MISMATCH"));
    Ok(())
}
