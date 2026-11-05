//! CT-033 / CT-034 in near-sandbox: the whole F6 path with real `join`, stake through
//! `ft_transfer_call`, `register_cumulative`, close, submit, commit, settle, and `finalize_epoch`.
//! Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, events, failure_text};
use common::flow::{finalize, s1_committed};
use serde_json::{json, Value};

fn names(log: &[(String, Value)]) -> Vec<&str> {
    log.iter().map(|(n, _)| n.as_str()).collect()
}

// TC-CT-033-13, TC-CT-034-06 (S3.fin): the debtor does not pay
#[tokio::test]
async fn default_cuts_only_the_debtor_stake_and_keeps_d6() -> anyhow::Result<()> {
    let f = s1_committed().await?;
    f.assert_d6().await?;

    let result = finalize(&f.circle).await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    let log = events(&result);
    assert_eq!(
        names(&log),
        ["member_defaulted", "limits_updated", "epoch_finalized"]
    );
    assert_eq!(
        (
            log[0].1["account_id"].as_str(),
            log[0].1["from_stake"].as_str(),
            log[0].1["shortfall"].as_str()
        ),
        (Some("scraper.test.near"), Some("35000000"), Some("0"))
    );
    assert_eq!(log[1].1["changes"], json!([]));
    assert_eq!(
        (
            log[2].1["defaulted_count"].as_u64(),
            log[2].1["pool"].as_str()
        ),
        (Some(1), Some("35000000"))
    );

    let scraper = f.member("scraper").await?;
    assert_eq!(
        (
            scraper["status"].as_str(),
            scraper["stake"].as_str(),
            scraper["limit"].as_str()
        ),
        (Some("suspended"), Some("65000000"), Some("0"))
    );
    for other in ["gpu", "translator"] {
        let m = f.member(other).await?;
        assert_eq!(
            (
                m["status"].as_str(),
                m["stake"].as_str(),
                m["limit"].as_str()
            ),
            (Some("active"), Some("100000000"), Some("80000000")),
            "{other}"
        );
    }
    let position = f
        .circle
        .view(
            "net_position",
            json!({"epoch": 1, "account_id": account_of("scraper")}),
        )
        .await?;
    assert_eq!(
        (position["status"].as_str(), position["recovered"].as_str()),
        (Some("defaulted"), Some("35000000"))
    );
    assert_eq!(f.circle.epoch(1).await?["phase"], "finalized");
    assert_eq!(
        f.circle
            .view(
                "outstanding_debt",
                json!({"account_id": account_of("scraper")})
            )
            .await?,
        "0"
    );
    let pool = f.circle.view("get_pool", json!({})).await?;
    assert_eq!(pool["epoch_pools"], "35000000");
    f.assert_d6().await
}

// TC-CT-033-04 / -05 against the real clock: too early and twice
#[tokio::test]
async fn finalize_before_the_deadline_and_twice_is_refused() -> anyhow::Result<()> {
    let f = s1_committed().await?;
    let early = f
        .circle
        .call_keeper("finalize_epoch", json!({"epoch": 1}))
        .await?;
    assert!(failure_text(&early)
        .expect("too early")
        .contains("ERR_PAYMENT_WINDOW_OPEN"));
    assert_eq!(f.circle.epoch(1).await?["phase"], "payment_window");

    assert!(finalize(&f.circle).await?.is_success());
    let again = f
        .circle
        .call_keeper("finalize_epoch", json!({"epoch": 1}))
        .await?;
    assert!(failure_text(&again)
        .expect("second finalize")
        .contains("ERR_WRONG_PHASE"));
    let unknown = f
        .circle
        .call_keeper("finalize_epoch", json!({"epoch": 99}))
        .await?;
    assert!(failure_text(&unknown)
        .expect("unknown epoch")
        .contains("ERR_UNKNOWN_EPOCH"));
    Ok(())
}
