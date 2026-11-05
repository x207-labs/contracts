//! CT-061..065 in near-sandbox (F5 barter bundle, ADR-38): `post_intent`, `commit_bundle` with its refusals,
//! `register_in_bundle` against the reserved headroom, and the clearing of the legs. Needs
//! `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::flow::edge;
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

// scraper -> translator 6, translator -> summarizer 10, summarizer -> scraper 8 (the A, B, C cycle of the vector)
const CYCLE: [(&str, &str, u128); 3] = [
    ("scraper", "translator", 6 * CREDIT),
    ("translator", "summarizer", 10 * CREDIT),
    ("summarizer", "scraper", 8 * CREDIT),
];
const ALL: [&str; 3] = ["scraper", "translator", "summarizer"];

async fn headroom(f: &Funded, name: &str) -> anyhow::Result<String> {
    Ok(f.circle
        .view("headroom", json!({"account_id": account_of(name)}))
        .await?
        .as_str()
        .unwrap()
        .to_string())
}

// TC-CT-062: post_intent
#[tokio::test]
async fn intents_are_posted_once_by_active_members() -> anyhow::Result<()> {
    let mut f = three().await?;
    let hash = near_sdk::bs58::encode([5u8; 32]).into_string();
    let scraper = f.accounts["scraper"].clone();
    let post = f
        .circle
        .call_as(&scraper, "post_intent", json!({"intent_hash": hash}))
        .await?;
    assert!(post.is_success(), "{:?}", failure_text(&post));
    assert!(events(&post)
        .iter()
        .any(|(n, d)| n == "intent_posted" && d["account_id"] == account_of("scraper").as_str()));
    let view = f
        .circle
        .view("get_intent", json!({"intent_hash": hash}))
        .await?;
    assert_eq!(view["account_id"], account_of("scraper").as_str());
    let again = f
        .circle
        .call_as(&scraper, "post_intent", json!({"intent_hash": hash}))
        .await?;
    assert!(failure_text(&again)
        .expect("refused")
        .contains("ERR_INTENT_EXISTS"));
    let newbie = f.account("newbie").await?;
    assert!(f.join("newbie", Funded::join_deposit()).await?.is_success());
    let pending = f
        .circle
        .call_as(
            &newbie,
            "post_intent",
            json!({"intent_hash": near_sdk::bs58::encode([6u8; 32]).into_string()}),
        )
        .await?;
    assert!(failure_text(&pending)
        .expect("refused")
        .contains("ERR_PAYER_NOT_ACTIVE"));
    assert_eq!(
        f.circle
            .view(
                "get_intent",
                json!({"intent_hash": near_sdk::bs58::encode([7u8; 32]).into_string()})
            )
            .await?,
        Value::Null
    );
    Ok(())
}

// TC-CT-063 and TC-CT-065: refusals leave no trace
#[tokio::test]
async fn commit_refusals_change_nothing() -> anyhow::Result<()> {
    let f = three().await?;
    let before = headroom(&f, "translator").await?;
    assert_eq!(before, "80000000");

    // a signature missing
    let missing = f
        .circle
        .commit_bundle(1, &CYCLE, &["scraper", "translator"])
        .await?;
    assert!(failure_text(&missing)
        .expect("refused")
        .contains("ERR_BUNDLE_SIGNATURES"));
    // a signature by the wrong key: summarizer's slot signed by scraper
    let (message, hash) = f.circle.bundle_terms(1, &CYCLE);
    let signatures = json!([
        {"account_id": account_of("scraper"), "signature": f.circle.sign_terms("scraper", &message, hash)},
        {"account_id": account_of("translator"), "signature": f.circle.sign_terms("translator", &message, hash)},
        {"account_id": account_of("summarizer"), "signature": f.circle.sign_terms("scraper", &message, hash)},
    ]);
    let legs: Vec<Value> = CYCLE.iter().map(|(a, b, m)| json!({"payer": account_of(a), "payee": account_of(b), "max_amount": m.to_string()})).collect();
    let forged = f
        .circle
        .call_keeper(
            "commit_bundle",
            json!({"bundle": {"epoch": 1, "legs": legs}, "signatures": signatures}),
        )
        .await?;
    assert!(failure_text(&forged)
        .expect("refused")
        .contains("ERR_BUNDLE_SIGNATURES"));
    // signatures over other terms
    let other = f.circle.bundle_terms(
        1,
        &[("scraper", "translator", 7 * CREDIT), CYCLE[1], CYCLE[2]],
    );
    let wrong_terms = json!([
        {"account_id": account_of("scraper"), "signature": f.circle.sign_terms("scraper", &other.0, other.1)},
        {"account_id": account_of("translator"), "signature": f.circle.sign_terms("translator", &other.0, other.1)},
        {"account_id": account_of("summarizer"), "signature": f.circle.sign_terms("summarizer", &other.0, other.1)},
    ]);
    let swapped = f
        .circle
        .call_keeper(
            "commit_bundle",
            json!({"bundle": {"epoch": 1, "legs": legs}, "signatures": wrong_terms}),
        )
        .await?;
    assert!(failure_text(&swapped)
        .expect("refused")
        .contains("ERR_BUNDLE_SIGNATURES"));

    // not enough headroom: summarizer would owe 90 with a limit of 80
    let big = f
        .circle
        .commit_bundle(
            1,
            &[
                ("scraper", "translator", 6 * CREDIT),
                ("translator", "summarizer", 10 * CREDIT),
                ("summarizer", "scraper", 90 * CREDIT),
            ],
            &ALL,
        )
        .await?;
    assert!(failure_text(&big)
        .expect("refused")
        .contains("ERR_BUNDLE_HEADROOM"));
    // unknown epoch, other epoch, shape
    assert!(
        failure_text(&f.circle.commit_bundle(9, &CYCLE, &ALL).await?)
            .expect("refused")
            .contains("ERR_UNKNOWN_EPOCH")
    );
    let one_leg = f
        .circle
        .commit_bundle(1, &[CYCLE[0]], &["scraper", "translator"])
        .await?;
    assert!(failure_text(&one_leg)
        .expect("refused")
        .contains("ERR_BAD_BUNDLE"));

    assert_eq!(
        headroom(&f, "translator").await?,
        before,
        "refusals reserve nothing"
    );
    assert_eq!(
        f.circle.view("get_bundle", json!({"bundle_id": 1})).await?,
        Value::Null
    );
    Ok(())
}

// TC-CT-063, TC-CT-064, TC-CT-065: commit, register on the legs, clear, residual
#[tokio::test]
async fn bundle_legs_register_clear_and_leave_the_difference_as_residual() -> anyhow::Result<()> {
    let mut f = three().await?;
    let commit = f.circle.commit_bundle(1, &CYCLE, &ALL).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    let committed = events(&commit)
        .into_iter()
        .find(|(n, _)| n == "bundle_committed")
        .expect("bundle_committed")
        .1;
    assert_eq!(
        (committed["bundle_id"].as_u64(), committed["epoch"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(committed["legs"].as_array().unwrap().len(), 3);
    let (_, terms_hash) = f.circle.bundle_terms(1, &CYCLE);
    assert_eq!(
        committed["terms_hash"].as_str().unwrap(),
        terms_hash
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    assert_eq!(commit.json::<u64>()?, 1);
    let view = f.circle.view("get_bundle", json!({"bundle_id": 1})).await?;
    assert_eq!(
        (view["status"].as_str(), view["epoch"].as_u64()),
        (Some("active"), Some(1))
    );

    // headroom falls by the value of the legs of each payer
    assert_eq!(headroom(&f, "scraper").await?, "74000000");
    assert_eq!(headroom(&f, "translator").await?, "70000000");
    assert_eq!(headroom(&f, "summarizer").await?, "72000000");
    // the same terms cannot be committed again
    let again = f.circle.commit_bundle(1, &CYCLE, &ALL).await?;
    assert!(failure_text(&again)
        .expect("refused")
        .contains("ERR_BUNDLE_EXISTS"));
    assert_eq!(
        headroom(&f, "scraper").await?,
        "74000000",
        "no second reservation"
    );

    // plain vouchers cannot use the reserved headroom: scraper has 74 free, 75 is refused
    let too_much = f
        .circle
        .register("scraper", "summarizer", 1, 75 * CREDIT)
        .await?;
    assert!(failure_text(&too_much)
        .expect("refused")
        .contains("ERR_INSUFFICIENT_CREDIT"));

    // legs: scraper and translator deliver fully, summarizer only 3 of 8
    assert!(f
        .circle
        .register_in_bundle(1, "scraper", "translator", 1, 6 * CREDIT)
        .await?
        .is_success());
    assert!(f
        .circle
        .register_in_bundle(1, "translator", "summarizer", 1, 10 * CREDIT)
        .await?
        .is_success());
    assert!(f
        .circle
        .register_in_bundle(1, "summarizer", "scraper", 1, 3 * CREDIT)
        .await?
        .is_success());
    // a leg is a limit: 8 + 1 is refused, and a pair that is not a leg is refused
    let over = f
        .circle
        .register_in_bundle(1, "summarizer", "scraper", 1, 9 * CREDIT)
        .await?;
    assert!(failure_text(&over)
        .expect("refused")
        .contains("ERR_BUNDLE_LEG_EXCEEDED"));
    let stranger = f
        .circle
        .register_in_bundle(1, "scraper", "summarizer", 1, CREDIT)
        .await?;
    assert!(failure_text(&stranger)
        .expect("refused")
        .contains("ERR_BUNDLE_LEG:"));
    let unknown = f
        .circle
        .register_in_bundle(9, "scraper", "translator", 1, 7 * CREDIT)
        .await?;
    assert!(failure_text(&unknown)
        .expect("refused")
        .contains("ERR_UNKNOWN_BUNDLE"));
    // the legs consumed their reservation: free headroom of the payers did not change
    assert_eq!(headroom(&f, "scraper").await?, "74000000");
    assert_eq!(
        headroom(&f, "summarizer").await?,
        "72000000",
        "5 credit of the summarizer leg are still reserved"
    );
    let after = f.circle.view("get_bundle", json!({"bundle_id": 1})).await?;
    let registered: Vec<&str> = after["legs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["registered"].as_str().unwrap())
        .collect();
    assert_eq!(
        registered,
        ["6000000", "10000000", "3000000"],
        "legs in the order they were committed"
    );

    // clearing: the cycle clears 3 on every edge (the summarizer leg is 3), the rest is residual
    let c = &f.circle;
    let solver = c.worker.dev_create_account().await?;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c
        .call_keeper("close_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    let edges = json!([
        edge("scraper", "translator", 3 * CREDIT),
        edge("translator", "summarizer", 3 * CREDIT),
        edge("summarizer", "scraper", 3 * CREDIT)
    ]);
    let submit = c
        .call_as(
            &solver,
            "submit_clearing",
            json!({"epoch": 1, "edges": edges}),
        )
        .await?;
    assert!(submit.is_success(), "{:?}", failure_text(&submit));
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?)
        .await?;
    assert!(c
        .call_keeper("commit_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    // nets: scraper +3 -6 = -3; translator +6 -10 = -4; summarizer +10 -3 = +7
    let net = |name: &'static str| {
        let c = &f.circle;
        async move {
            c.view(
                "net_position",
                json!({"epoch": 1, "account_id": account_of(name)}),
            )
            .await
        }
    };
    assert_eq!(net("scraper").await?["amount"], "3000000");
    assert_eq!(net("translator").await?["amount"], "4000000");
    let summarizer = net("summarizer").await?;
    assert_eq!(
        (summarizer["amount"].as_str(), summarizer["status"].as_str()),
        (Some("7000000"), Some("claimable"))
    );
    f.assert_d6().await?;
    Ok(())
}

// security review F2: a participant is in at most 16 bundles of an epoch, so two accounts cannot use up the slots of a third
#[tokio::test]
async fn a_participant_is_in_at_most_16_bundles_per_epoch() -> anyhow::Result<()> {
    let f = three().await?;
    for n in 1..=16u128 {
        let legs = [
            ("scraper", "translator", n),
            ("translator", "summarizer", n),
            ("summarizer", "scraper", n),
        ];
        let commit = f.circle.commit_bundle(1, &legs, &ALL).await?;
        assert!(
            commit.is_success(),
            "bundle {n}: {:?}",
            failure_text(&commit)
        );
    }
    let legs = [
        ("scraper", "translator", 17),
        ("translator", "summarizer", 17),
        ("summarizer", "scraper", 17),
    ];
    let refused = f.circle.commit_bundle(1, &legs, &ALL).await?;
    assert!(failure_text(&refused)
        .expect("refused")
        .contains("ERR_BUNDLE_LIMIT"));
    Ok(())
}
