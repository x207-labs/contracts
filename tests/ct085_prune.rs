//! CT-085 in near-sandbox: `prune_epoch` on a real circle. The claim that was not taken must survive the prune. Needs
//! `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, failure_text, CREDIT};
use common::flow::{finalize, s1_committed};
use near_workspaces::types::{Gas, NearToken};
use serde_json::{json, Value};

const GPU_START: u128 = 900 * CREDIT; // 1000 minted, 100 staked

// TC-CT-085-06: only a finalized epoch; anybody may call; the creditor that had not claimed yet still claims in full afterwards
#[tokio::test]
async fn an_unclaimed_position_can_still_be_claimed_after_the_epoch_is_pruned() -> anyhow::Result<()>
{
    let f = s1_committed().await?; // payment window of epoch 1: scraper owes, gpu is the creditor
    let outsider = f.circle.worker.dev_create_account().await?;
    let early = f
        .circle
        .call_as(
            &outsider,
            "prune_epoch",
            json!({"epoch": 1, "max_items": 10}),
        )
        .await?;
    assert!(failure_text(&early)
        .expect("refused")
        .contains("ERR_NOT_FINALIZED"));

    let fin = finalize(&f.circle).await?; // the debtor does not pay: scraper defaulted, gpu claimable
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    let position = |name: &'static str| {
        f.circle.view(
            "net_position",
            json!({"epoch": 1, "account_id": account_of(name)}),
        )
    };
    let (scraper_before, gpu_before): (Value, Value) =
        (position("scraper").await?, position("gpu").await?);
    assert_eq!(
        (
            gpu_before["status"].as_str(),
            scraper_before["status"].as_str()
        ),
        (Some("claimable"), Some("defaulted"))
    );
    let list_before = f
        .circle
        .view(
            "list_positions",
            json!({"epoch": 1, "from_index": 0, "limit": 50}),
        )
        .await?;

    // CT-099 (B8-F1): too early while the running epoch is not two epochs after epoch 1; the edges stay readable from `list_obligations`
    let too_early = f
        .circle
        .call_as(
            &outsider,
            "prune_epoch",
            json!({"epoch": 1, "max_items": 10}),
        )
        .await?;
    assert!(failure_text(&too_early)
        .expect("refused")
        .contains("ERR_PRUNE_TOO_EARLY"));
    assert!(!f
        .circle
        .view(
            "list_obligations",
            json!({"epoch": 1, "from_index": 0, "limit": 10})
        )
        .await?
        .as_array()
        .unwrap()
        .is_empty());
    common::flow::pass_prune_delay(&f.circle).await?;

    // 3 pairs and 3 participants: prune in small steps, a stranger pays the gas
    let mut removed = 0u64;
    loop {
        let step = f
            .circle
            .call_as(
                &outsider,
                "prune_epoch",
                json!({"epoch": 1, "max_items": 2}),
            )
            .await?;
        assert!(step.is_success(), "{:?}", failure_text(&step));
        let n = step.json::<u64>()?;
        assert!(n <= 2);
        if n == 0 {
            break;
        }
        removed += n;
    }
    assert_eq!(removed, 6, "three pairs and three participants");

    // D5 is as it was, and the epoch record keeps its figures
    assert_eq!(
        (position("scraper").await?, position("gpu").await?),
        (scraper_before, gpu_before)
    );
    assert_eq!(
        f.circle
            .view(
                "list_positions",
                json!({"epoch": 1, "from_index": 0, "limit": 50})
            )
            .await?,
        list_before
    );
    let epoch = f.circle.view("get_epoch", json!({"epoch": 1})).await?;
    assert_eq!(epoch["phase"], "finalized");

    // the claim that was not taken
    let gpu = f.accounts["gpu"].clone();
    let claim = gpu
        .call(f.circle.contract.id(), "claim")
        .args_json(json!({"epoch": 1, "destination": "near"}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?;
    assert!(claim.is_success(), "{:?}", failure_text(&claim));
    assert_eq!(f.balance(&f.usdt, gpu.id()).await?, GPU_START + 35 * CREDIT);
    assert_eq!(position("gpu").await?["status"], "claimed");
    f.assert_d6().await
}
