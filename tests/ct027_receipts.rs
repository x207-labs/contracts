//! TC-CT-027-13: a valid `submit_clearing` in near-sandbox runs in one receipt, with no promise (ADR-03).
//! Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{failure_text, CREDIT};
use common::flow::edge;
use common::funded::Funded;
use serde_json::json;

#[tokio::test]
async fn submit_clearing_is_one_receipt() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in ["scraper", "gpu", "translator"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    for (payer, payee, amount) in [
        ("scraper", "gpu", 60),
        ("gpu", "translator", 25),
        ("translator", "scraper", 25),
    ] {
        assert!(f
            .circle
            .register(payer, payee, 1, amount * CREDIT)
            .await?
            .is_success());
    }
    let c = &f.circle;
    let solver = c.worker.dev_create_account().await?;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c
        .call_keeper("close_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    let edges = json!([
        edge("scraper", "gpu", 25 * CREDIT),
        edge("gpu", "translator", 25 * CREDIT),
        edge("translator", "scraper", 25 * CREDIT)
    ]);
    let submit = c
        .call_as(
            &solver,
            "submit_clearing",
            json!({"epoch": 1, "edges": edges}),
        )
        .await?;
    assert!(submit.is_success(), "{:?}", failure_text(&submit));
    let receipts: Vec<_> = submit
        .receipt_outcomes()
        .iter()
        .filter(|o| o.executor_id.as_str() == c.contract.id().as_str())
        .collect();
    assert_eq!(
        submit.receipt_outcomes().len(),
        1,
        "one receipt in all: {:?}",
        submit
            .receipt_outcomes()
            .iter()
            .map(|o| o.executor_id.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(receipts.len(), 1, "the one receipt ran in the contract");
    Ok(())
}
