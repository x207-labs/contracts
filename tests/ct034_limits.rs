//! CT-034 in near-sandbox: the limit recomputation of `finalize_epoch` on a real circle (real `join`,
//! stake, vouchers, clearing, and settle). Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{events, failure_text, CREDIT};
use common::flow::{finalize, s1_committed};
use serde_json::{json, Value};

fn names(log: &[(String, Value)]) -> Vec<&str> {
    log.iter().map(|(n, _)| n.as_str()).collect()
}

// TC-CT-034-01, TC-CT-034-02, TC-CT-034-05 (S1.fin): cash settlement raises only the payer's limit
#[tokio::test]
async fn cash_settlement_raises_limit_after_finalize() -> anyhow::Result<()> {
    let f = s1_committed().await?;
    let scraper = f.accounts["scraper"].clone();
    let pay = f
        .transfer_call(
            &f.usdt,
            &scraper,
            35 * CREDIT,
            r#"{"action":"settle","epoch":1}"#,
        )
        .await?;
    assert!(pay.is_success(), "{:?}", failure_text(&pay));
    assert_eq!(
        f.member("scraper").await?["limit"],
        "80000000",
        "the limit changes only at finalize (K-14)"
    );

    let result = finalize(&f.circle).await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    let log = events(&result);
    assert_eq!(names(&log), ["limits_updated", "epoch_finalized"]);
    assert_eq!(
        log[0].1["changes"],
        json!([{"account_id": "scraper.test.near", "old_limit": "80000000", "new_limit": "83500000", "status": "active"}])
    );
    assert_eq!(
        (
            log[1].1["defaulted_count"].as_u64(),
            log[1].1["pool"].as_str()
        ),
        (Some(0), Some("35000000"))
    );

    assert_eq!(f.member("scraper").await?["limit"], "83500000");
    assert_eq!(f.member("scraper").await?["status"], "active");
    for other in ["gpu", "translator"] {
        assert_eq!(f.member(other).await?["limit"], "80000000", "{other}");
    }
    f.assert_d6().await
}
