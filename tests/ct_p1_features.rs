//! P1 features in near-sandbox (interface 0.0.2): CT-018 admission by the owner, CT-079 reactivation of a
//! `suspended` member by stake, CT-051 and CT-052 fees and `claim_solver_fee`. Needs
//! `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::flow::{edge, finalize, s1_committed_on};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::AccountId;
use serde_json::json;

async fn usdt(f: &Funded, id: &AccountId) -> anyhow::Result<u128> {
    f.balance(&f.usdt, id).await
}

// TC-CT-018 in the sandbox: with `admission` dao a stake before `admit_member` is refused and refunded
#[tokio::test]
async fn admission_dao_gates_the_stake() -> anyhow::Result<()> {
    let mut params = Funded::params_short();
    params["admission"] = json!("dao");
    let mut f = Funded::deploy(Some(params)).await?;
    let newbie = f.account("newbie").await?;
    assert!(f.join("newbie", Funded::join_deposit()).await?.is_success());
    let before = usdt(&f, newbie.id()).await?;
    let refused = f.stake(&newbie, 100 * CREDIT).await?;
    assert!(
        failure_text(&refused).is_some() || usdt(&f, newbie.id()).await? == before,
        "{:?}",
        failure_text(&refused)
    );
    assert_eq!(
        usdt(&f, newbie.id()).await?,
        before,
        "the refused stake is refunded"
    );
    assert_eq!(f.member("newbie").await?["status"], "pending");

    let outsider = f.account("outsider").await?;
    let not_owner = f
        .circle
        .call_as(
            &outsider,
            "admit_member",
            json!({"account_id": newbie.id()}),
        )
        .await?;
    assert!(failure_text(&not_owner)
        .expect("refused")
        .contains("ERR_NOT_OWNER"));

    let dao = f.account("dao").await?;
    let admit = f
        .circle
        .call_as(&dao, "admit_member", json!({"account_id": newbie.id()}))
        .await?;
    assert!(admit.is_success(), "{:?}", failure_text(&admit));
    assert!(events(&admit)
        .iter()
        .any(|(n, d)| n == "member_admitted" && d["status"] == "pending"));
    let again = f
        .circle
        .call_as(&dao, "admit_member", json!({"account_id": newbie.id()}))
        .await?;
    assert!(failure_text(&again)
        .expect("refused")
        .contains("ERR_ALREADY_ADMITTED"));

    assert!(f.stake(&newbie, 100 * CREDIT).await?.is_success());
    assert_eq!(f.member("newbie").await?["status"], "active");
    f.assert_d6().await?;
    Ok(())
}

// TC-CT-018-01 in the sandbox: open admission refuses admit_member
#[tokio::test]
async fn admit_member_needs_dao_admission() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    assert!(f.join("newbie", Funded::join_deposit()).await?.is_success());
    let dao = f.account("dao").await?;
    let admit = f
        .circle
        .call_as(
            &dao,
            "admit_member",
            json!({"account_id": account_of("newbie")}),
        )
        .await?;
    assert!(failure_text(&admit)
        .expect("refused")
        .contains("ERR_ADMISSION_NOT_DAO"));
    Ok(())
}

// CT-079: scraper defaults, tops up, and is active again two epochs on with the base limit
#[tokio::test]
async fn suspended_member_comes_back_by_stake() -> anyhow::Result<()> {
    let f = s1_committed_on(Funded::deploy(Some(Funded::params_short())).await?).await?;
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    let scraper = f.accounts["scraper"].clone();
    let m = f.member("scraper").await?;
    assert_eq!(
        (
            m["status"].as_str(),
            m["stake"].as_str(),
            m["limit"].as_str()
        ),
        (Some("suspended"), Some("65000000"), Some("0"))
    );

    let top_up = f.stake(&scraper, 35 * CREDIT).await?;
    assert!(top_up.is_success(), "{:?}", failure_text(&top_up));
    let reactivated = events(&top_up)
        .into_iter()
        .find(|(n, _)| n == "member_reactivated")
        .expect("member_reactivated")
        .1;
    let effective = reactivated["effective_epoch"].as_u64().unwrap();
    assert_eq!(
        (reactivated["stake"].as_str(), reactivated["limit"].as_str()),
        (Some("100000000"), Some("80000000"))
    );
    let m = f.member("scraper").await?;
    assert_eq!(
        (
            m["status"].as_str(),
            m["settled_history"].as_str(),
            m["limit"].as_str()
        ),
        (Some("suspended"), Some("0"), Some("0"))
    );
    assert_eq!(
        f.circle
            .view("headroom", json!({"account_id": account_of("scraper")}))
            .await?,
        "0"
    );

    // open epochs until `effective` is the running one
    let c = &f.circle;
    let mut epoch = c.view("get_epoch", json!({})).await?["epoch"]
        .as_u64()
        .unwrap();
    while epoch < effective {
        c.advance_to(c.epoch_ts(epoch, "cutoff_at").await?).await?;
        assert!(c
            .call_keeper("close_epoch", json!({"epoch": epoch}))
            .await?
            .is_success());
        epoch += 1;
    }
    let m = f.member("scraper").await?;
    assert_eq!(
        (m["status"].as_str(), m["limit"].as_str()),
        (Some("active"), Some("80000000"))
    );
    assert_eq!(
        f.circle
            .view("headroom", json!({"account_id": account_of("scraper")}))
            .await?,
        "80000000"
    );
    let mut f = f;
    let registered = f
        .circle
        .register("scraper", "gpu", epoch, 10 * CREDIT)
        .await?;
    assert!(registered.is_success(), "{:?}", failure_text(&registered));
    Ok(())
}

async fn committed_with_fees() -> anyhow::Result<(Funded, near_workspaces::Account)> {
    let mut params = Funded::params_short();
    params["fees"] = json!({"clearing_fee_bps": 100, "solver_fee_bps": 100});
    let mut f = Funded::deploy(Some(params)).await?;
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
    let solver = f.account("solver1").await?;
    let c = &f.circle;
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
    assert!(c
        .call_as(
            &solver,
            "submit_clearing",
            json!({"epoch": 1, "edges": edges})
        )
        .await?
        .is_success());
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?)
        .await?;
    let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    Ok((f, solver))
}

async fn claim_fee(
    f: &Funded,
    solver: &near_workspaces::Account,
) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
    Ok(solver
        .call(f.circle.contract.id(), "claim_solver_fee")
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?)
}

// CT-052: the transfer is refused (the solver unregistered from the token), the callback restores the balance
#[tokio::test]
async fn failed_solver_fee_transfer_restores_the_balance() -> anyhow::Result<()> {
    let (f, solver) = committed_with_fees().await?;
    solver
        .call(f.usdt.id(), "storage_unregister")
        .args_json(json!({"force": true}))
        .deposit(NearToken::from_yoctonear(1))
        .transact()
        .await?
        .into_result()?;
    let before = usdt(&f, solver.id()).await?;
    let claim = claim_fee(&f, &solver).await?;
    assert!(
        events(&claim)
            .iter()
            .any(|(n, d)| n == "solver_fee_claim_failed" && d["amount"] == "750000"),
        "{:?}",
        events(&claim)
    );
    assert!(!events(&claim)
        .iter()
        .any(|(n, _)| n == "solver_fee_claimed"));
    assert_eq!(
        f.circle
            .view("solver_fee_balance", json!({"account_id": solver.id()}))
            .await?,
        "750000",
        "balance restored"
    );
    let pool = f.circle.view("get_pool", json!({})).await?;
    assert_eq!(
        (pool["solver_fees"].as_str(), pool["in_flight"].as_str()),
        (Some("750000"), Some("0"))
    );
    assert_eq!(usdt(&f, solver.id()).await?, before, "no USDT moved");
    f.assert_d6().await?;
    Ok(())
}

// CT-051 and CT-052: fees at commit_epoch, the solver claims its share as USDT
#[tokio::test]
async fn fees_reach_the_reserve_and_the_solver() -> anyhow::Result<()> {
    let mut params = Funded::params_short();
    params["fees"] = json!({"clearing_fee_bps": 100, "solver_fee_bps": 100});
    let mut f = Funded::deploy(Some(params)).await?;
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
    let solver = f.account("solver1").await?;
    let c = &f.circle;
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
    assert!(c
        .call_as(
            &solver,
            "submit_clearing",
            json!({"epoch": 1, "edges": edges})
        )
        .await?
        .is_success());
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?)
        .await?;
    let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    // cleared 75: clearing 0.75 to the reserve, solver 0.75
    let applied = events(&commit)
        .into_iter()
        .find(|(n, _)| n == "clearing_applied")
        .expect("clearing_applied")
        .1;
    assert_eq!(
        (
            applied["clearing_fee"].as_str(),
            applied["solver_fee"].as_str()
        ),
        (Some("750000"), Some("750000"))
    );
    let pool = f.circle.view("get_pool", json!({})).await?;
    assert_eq!(
        (
            pool["reserve"].as_str(),
            pool["solver_fees"].as_str(),
            pool["total_stake"].as_str()
        ),
        (Some("750000"), Some("750000"), Some("298500000"))
    );
    assert_eq!(
        f.circle
            .view("solver_fee_balance", json!({"account_id": solver.id()}))
            .await?,
        "750000"
    );
    f.assert_d6().await?;

    let before = usdt(&f, solver.id()).await?;
    let claim = solver
        .call(f.circle.contract.id(), "claim_solver_fee")
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?;
    assert!(claim.is_success(), "{:?}", failure_text(&claim));
    assert!(events(&claim)
        .iter()
        .any(|(n, d)| n == "solver_fee_claimed" && d["amount"] == "750000"));
    assert_eq!(usdt(&f, solver.id()).await?, before + 750_000);
    assert_eq!(
        f.circle
            .view("solver_fee_balance", json!({"account_id": solver.id()}))
            .await?,
        "0"
    );
    let nothing = solver
        .call(f.circle.contract.id(), "claim_solver_fee")
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?;
    assert!(failure_text(&nothing)
        .expect("refused")
        .contains("ERR_NOTHING_TO_CLAIM"));
    f.assert_d6().await?;
    Ok(())
}
