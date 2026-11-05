//! CT-078 in near-sandbox: the exit of ADR-29 (`request_exit`, `withdraw_stake`, `on_withdraw_resolved`)
//! on a real circle with a NEP-141 mock as USDT. Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{events, failure_text, CREDIT};
use common::flow::{
    finalize, idle_epoch_finalized, request_exit, s1_committed_exit, staked_members, withdraw_stake,
};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use serde_json::{json, Value};

fn names(log: &[(String, Value)]) -> Vec<&str> {
    log.iter().map(|(n, _)| n.as_str()).collect()
}

async fn claim_near(
    f: &Funded,
    who: &str,
) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
    Ok(f.accounts[who]
        .call(f.circle.contract.id(), "claim")
        .args_json(json!({"epoch": 1, "destination": "near"}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?)
}

// TC-CT-078-01, TC-CT-078-04, TC-CT-077-17: leave without debt; a failed transfer is retried
#[tokio::test]
async fn exit_without_debt_pays_the_stake_back_and_survives_a_failed_transfer() -> anyhow::Result<()>
{
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    staked_members(&mut f, &["coder", "gpu"]).await?;
    let exit = request_exit(&f, "coder").await?;
    assert!(exit.is_success(), "{:?}", failure_text(&exit));
    assert_eq!(names(&events(&exit)), ["exit_requested"]);
    assert_eq!(f.member("coder").await?["status"], "active");

    let early = withdraw_stake(&f, "coder").await?;
    assert!(failure_text(&early)
        .expect("epoch not final")
        .contains("ERR_EXIT_NOT_READY"));
    idle_epoch_finalized(&f).await?;

    let coder = f.accounts["coder"].clone();
    let outsider = f.circle.worker.dev_create_account().await?;
    let direct = f
        .circle
        .call_as(
            &outsider,
            "on_withdraw_resolved",
            json!({"account_id": coder.id(), "amount": "100000000"}),
        )
        .await?;
    assert!(failure_text(&direct)
        .expect("private callback")
        .contains("Method on_withdraw_resolved is private"));

    coder
        .call(f.usdt.id(), "storage_unregister")
        .args_json(json!({"force": true}))
        .deposit(NearToken::from_yoctonear(1))
        .transact()
        .await?
        .into_result()?;
    let failed = withdraw_stake(&f, "coder").await?;
    assert!(
        failed.is_success(),
        "the callback must not fail: {:?}",
        failure_text(&failed)
    );
    assert!(!failed.clone().json::<bool>()?);
    assert_eq!(names(&events(&failed)), ["stake_withdraw_failed"]);
    let m = f.member("coder").await?;
    assert_eq!(
        (
            m["stake"].as_str(),
            m["limit"].as_str(),
            m["status"].as_str()
        ),
        (Some("100000000"), Some("80000000"), Some("active"))
    );
    f.assert_d6().await?;

    coder
        .call(f.usdt.id(), "storage_deposit")
        .args_json(json!({"account_id": coder.id()}))
        .deposit(NearToken::from_millinear(10))
        .transact()
        .await?
        .into_result()?;
    let done = withdraw_stake(&f, "coder").await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert_eq!(names(&events(&done)), ["stake_withdrawn"]);
    assert_eq!(f.balance(&f.usdt, coder.id()).await?, 1000 * CREDIT);
    let m = f.member("coder").await?;
    assert_eq!(
        (
            m["status"].as_str(),
            m["stake"].as_str(),
            m["limit"].as_str()
        ),
        (Some("exited"), Some("0"), Some("0"))
    );
    f.assert_d6().await?;

    // TC-CT-078-06: no stake after leaving, full refund
    let again = f.stake(&coder, 10 * CREDIT).await?;
    assert!(
        common::circle::receipt_failures(&again).contains("ERR_MEMBER_EXITED"),
        "{}",
        common::circle::receipt_failures(&again)
    );
    assert_eq!(f.balance(&f.usdt, coder.id()).await?, 1000 * CREDIT);
    let second = withdraw_stake(&f, "coder").await?;
    assert!(failure_text(&second)
        .expect("already exited")
        .contains("ERR_MEMBER_EXITED"));
    Ok(())
}

// TC-CT-078-03: vouchers of the exit epoch are accepted, those of the next epoch are not
#[tokio::test]
async fn registration_stops_after_the_exit_epoch() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    staked_members(&mut f, &["coder", "gpu"]).await?;
    assert!(request_exit(&f, "coder").await?.is_success());

    for (payer, payee) in [("coder", "gpu"), ("gpu", "coder")] {
        let ok = f.circle.register(payer, payee, 1, 5 * CREDIT).await?;
        assert!(
            ok.is_success(),
            "epoch 1 {payer}->{payee}: {:?}",
            failure_text(&ok)
        );
    }
    f.circle
        .advance_to(f.circle.epoch_ts(1, "cutoff_at").await?)
        .await?;
    assert!(f
        .circle
        .call_keeper("close_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    for (payer, payee) in [("coder", "gpu"), ("gpu", "coder")] {
        let refused = f.circle.register(payer, payee, 2, 5 * CREDIT).await?;
        assert!(
            failure_text(&refused)
                .expect("exit epoch passed")
                .contains("ERR_EXITING"),
            "epoch 2 {payer}->{payee}"
        );
    }
    Ok(())
}

// TC-CT-078-05, TC-CT-078-07 (S1.fin): a creditor leaves and still claims; a debtor that defaulted gets the leftover back
#[tokio::test]
async fn leavers_keep_their_claims_and_defaulters_their_leftover_stake() -> anyhow::Result<()> {
    let f = s1_committed_exit(
        Funded::deploy(Some(Funded::params_short())).await?,
        &["scraper", "gpu"],
    )
    .await?;
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    assert_eq!(
        names(&events(&fin)),
        ["member_defaulted", "limits_updated", "epoch_finalized"]
    );

    let scraper_wd = withdraw_stake(&f, "scraper").await?;
    assert!(scraper_wd.is_success(), "{:?}", failure_text(&scraper_wd));
    assert_eq!(events(&scraper_wd)[0].1["amount"], "65000000");
    assert_eq!(
        f.balance(&f.usdt, f.accounts["scraper"].id()).await?,
        900 * CREDIT + 65 * CREDIT
    );
    assert_eq!(f.member("scraper").await?["status"], "exited");

    let gpu_wd = withdraw_stake(&f, "gpu").await?;
    assert!(gpu_wd.is_success(), "{:?}", failure_text(&gpu_wd));
    assert_eq!(f.member("gpu").await?["status"], "exited");
    f.assert_d6().await?;

    let claim = claim_near(&f, "gpu").await?;
    assert!(claim.is_success(), "{:?}", failure_text(&claim));
    assert_eq!(
        f.balance(&f.usdt, f.accounts["gpu"].id()).await?,
        1000 * CREDIT + 35 * CREDIT
    );
    f.assert_d6().await
}

// TC-CT-078-02: a `due` position of an older epoch blocks the exit until that epoch is finalized
#[tokio::test]
async fn due_in_an_older_epoch_blocks_the_exit() -> anyhow::Result<()> {
    let f = common::flow::s1_committed().await?;
    assert!(request_exit(&f, "scraper").await?.is_success());
    assert_eq!(
        f.circle.view("get_epoch", json!({})).await?["epoch"],
        2,
        "the exit is requested in epoch 2"
    );

    let c = &f.circle;
    c.advance_to(c.epoch_ts(2, "cutoff_at").await?).await?;
    assert!(c
        .call_keeper("close_epoch", json!({"epoch": 2}))
        .await?
        .is_success());
    c.advance_to(c.epoch_ts(2, "proposal_deadline").await?)
        .await?;
    assert!(c
        .call_keeper("commit_epoch", json!({"epoch": 2}))
        .await?
        .is_success());
    c.advance_to(c.epoch_ts(2, "payment_deadline").await?)
        .await?;
    assert!(c
        .call_keeper("finalize_epoch", json!({"epoch": 2}))
        .await?
        .is_success());

    let blocked = withdraw_stake(&f, "scraper").await?;
    assert!(failure_text(&blocked)
        .expect("due in epoch 1")
        .contains("ERR_HAS_DUE"));
    assert_eq!(
        f.balance(&f.usdt, f.accounts["scraper"].id()).await?,
        900 * CREDIT
    );

    assert!(c
        .call_keeper("finalize_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    let ok = withdraw_stake(&f, "scraper").await?;
    assert!(ok.is_success(), "{:?}", failure_text(&ok));
    assert_eq!(
        f.balance(&f.usdt, f.accounts["scraper"].id()).await?,
        900 * CREDIT + 65 * CREDIT
    );
    f.assert_d6().await
}
