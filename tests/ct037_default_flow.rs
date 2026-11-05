//! CT-037: failure to pay in near-sandbox: the debtor does not settle, `finalize_epoch` cuts only that
//! debtor's stake, the creditor is paid in full. Needs `cargo near build non-reproducible-wasm`.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::flow::{finalize, s1_committed_on};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use serde_json::json;

// TC-CT-037-01, -02, -03, -04, -05
#[tokio::test]
async fn default_path_from_commit_to_claim() -> anyhow::Result<()> {
    // A longer epoch keeps epoch 2 open while epoch 1 runs through its windows (TC-CT-037-03).
    let mut params = Funded::params_short();
    params["epoch_length"] = json!(1500);
    let mut f = s1_committed_on(Funded::deploy(Some(params)).await?).await?;
    f.assert_d6().await?;

    // -02 too early: refused, nothing changes
    let early = f
        .circle
        .call_keeper("finalize_epoch", json!({"epoch": 1}))
        .await?;
    assert!(failure_text(&early)
        .expect("too early")
        .contains("ERR_PAYMENT_WINDOW_OPEN"));
    assert_eq!(f.member("scraper").await?["stake"], "100000000");
    assert_eq!(f.circle.epoch(1).await?["phase"], "payment_window");

    // -01, -05 the debtor never pays
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    let log = events(&fin);
    assert_eq!(
        log.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["member_defaulted", "limits_updated", "epoch_finalized"]
    );
    assert_eq!(log[0].1["from_stake"], "35000000");

    let scraper = f.member("scraper").await?;
    assert_eq!(
        (
            scraper["stake"].as_str(),
            scraper["status"].as_str(),
            scraper["limit"].as_str()
        ),
        (Some("65000000"), Some("suspended"), Some("0"))
    );
    let position = f
        .circle
        .view(
            "net_position",
            json!({"epoch": 1, "account_id": account_of("scraper")}),
        )
        .await?;
    assert_eq!(position["status"], "defaulted");
    for other in ["translator", "gpu"] {
        assert_eq!(f.member(other).await?["stake"], "100000000", "{other}");
    }
    // -04 accounting after the cut: 265 staked, 35 in the epoch pool
    let pool = f.circle.view("get_pool", json!({})).await?;
    assert_eq!(
        (pool["total_stake"].as_str(), pool["epoch_pools"].as_str()),
        (Some("265000000"), Some("35000000"))
    );
    f.assert_d6().await?;

    // -03 a suspended member cannot be a payer in epoch 2
    let refused = f.circle.register("scraper", "gpu", 2, CREDIT).await?;
    let text = failure_text(&refused).expect("suspended payer");
    assert!(text.contains("ERR_PAYER_NOT_ACTIVE"), "{text}");

    // the creditor gets its 35 in full
    let gpu = f.accounts["gpu"].clone();
    let before = f.balance(&f.usdt, gpu.id()).await?;
    let claim = gpu
        .call(f.circle.contract.id(), "claim")
        .args_json(json!({"epoch": 1, "destination": "near"}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?;
    assert!(claim.is_success(), "{:?}", failure_text(&claim));
    assert_eq!(f.balance(&f.usdt, gpu.id()).await?, before + 35 * CREDIT);
    f.assert_d6().await
}
