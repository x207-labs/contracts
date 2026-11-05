//! CT-036: the residual flow F4 in near-sandbox on a real circle: `net_position`, `ft_transfer_call`
//! settle, `finalize_epoch`, `claim`, and the refusals around them. TC-CT-036-09 is a unit test
//! (TC-CT-035-15); TC-CT-036-10 and -11 run in `ct035_claim.rs`. Needs `cargo near build non-reproducible-wasm`.

mod common;

use common::circle::{account_of, failure_text, receipt_failures, CREDIT};
use common::flow::{edge, finalize, s1_committed};
use common::funded::Funded;
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::{Gas, NearToken};
use serde_json::json;

fn used(result: &ExecutionFinalResult) -> u128 {
    result
        .clone()
        .json::<String>()
        .expect("ft_transfer_call returns U128")
        .parse()
        .unwrap()
}

async fn claim(f: &Funded, who: &str, epoch: u64) -> anyhow::Result<ExecutionFinalResult> {
    Ok(f.accounts[who]
        .call(f.circle.contract.id(), "claim")
        .args_json(json!({"epoch": epoch, "destination": "near"}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?)
}

fn settle_msg(epoch: u64) -> String {
    json!({"action": "settle", "epoch": epoch}).to_string()
}

async fn assert_refused(
    f: &Funded,
    who: &str,
    token_is_wrong: bool,
    msg: String,
    code: &str,
) -> anyhow::Result<()> {
    let token = if token_is_wrong {
        f.palsu.clone()
    } else {
        f.usdt.clone()
    };
    let account = f.accounts[who].clone();
    let before = f.balance(&token, account.id()).await?;
    let result = f.transfer_call(&token, &account, 35 * CREDIT, &msg).await?;
    assert!(
        receipt_failures(&result).contains(code),
        "{code} in {}",
        receipt_failures(&result)
    );
    assert_eq!(used(&result), 0, "{code}: nothing used");
    assert_eq!(
        f.balance(&token, account.id()).await?,
        before,
        "{code}: full refund"
    );
    Ok(())
}

// TC-CT-036-01, -02, -03, -04, -05, -06, -07, -08: one circle, one ordered story
#[tokio::test]
async fn positive_flow_and_every_refusal_on_the_way() -> anyhow::Result<()> {
    let f = s1_committed().await?;
    f.assert_d6().await?;

    let bill = f
        .circle
        .view(
            "net_position",
            json!({"epoch": 1, "account_id": account_of("scraper")}),
        )
        .await?;
    assert_eq!(
        (
            bill["role"].as_str(),
            bill["status"].as_str(),
            bill["amount"].as_str()
        ),
        (Some("debtor"), Some("due"), Some("35000000"))
    );

    // -02 wrong token, -03 wrong epochs
    assert_refused(&f, "scraper", true, settle_msg(1), "ERR_WRONG_TOKEN").await?;
    assert_refused(&f, "scraper", false, settle_msg(99), "ERR_UNKNOWN_EPOCH").await?;
    assert_refused(&f, "scraper", false, settle_msg(2), "ERR_WRONG_PHASE").await?;
    f.assert_d6().await?;

    // -05 overpayment: 40 sent, 35 used, only 35 leaves the wallet
    let scraper = f.accounts["scraper"].clone();
    let before = f.balance(&f.usdt, scraper.id()).await?;
    let paid = f
        .transfer_call(&f.usdt, &scraper, 40 * CREDIT, &settle_msg(1))
        .await?;
    assert!(paid.is_success(), "{:?}", failure_text(&paid));
    assert_eq!(used(&paid), 35 * CREDIT);
    assert_eq!(
        f.balance(&f.usdt, scraper.id()).await?,
        before - 35 * CREDIT
    );
    f.assert_d6().await?;

    // -06 second payment
    assert_refused(&f, "scraper", false, settle_msg(1), "ERR_NOT_DUE").await?;

    // -04 after the payment window, before finalize
    f.circle
        .advance_to(f.circle.epoch_ts(1, "payment_deadline").await?)
        .await?;
    assert_refused(
        &f,
        "scraper",
        false,
        settle_msg(1),
        "ERR_PAYMENT_WINDOW_CLOSED",
    )
    .await?;

    // claims before finalize are refused too (I-20)
    let early = claim(&f, "gpu", 1).await?;
    assert!(failure_text(&early)
        .expect("claim before finalize")
        .contains("ERR_NOT_FINALIZED"));

    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    f.assert_d6().await?;

    // -08 wrong claimants
    let debtor = claim(&f, "scraper", 1).await?;
    assert!(failure_text(&debtor)
        .expect("debtor claim")
        .contains("ERR_NOT_CREDITOR"));
    let none = claim(&f, "translator", 1).await?;
    assert!(failure_text(&none)
        .expect("no position")
        .contains("ERR_NO_POSITION"));

    // -01 the creditor receives exactly 35, -07 a second claim is refused
    let gpu = f.accounts["gpu"].clone();
    let gpu_before = f.balance(&f.usdt, gpu.id()).await?;
    let got = claim(&f, "gpu", 1).await?;
    assert!(got.is_success(), "{:?}", failure_text(&got));
    assert_eq!(
        f.balance(&f.usdt, gpu.id()).await?,
        gpu_before + 35 * CREDIT
    );
    let again = claim(&f, "gpu", 1).await?;
    assert!(failure_text(&again)
        .expect("second claim")
        .contains("ERR_NOT_CLAIMABLE"));
    assert_eq!(
        f.balance(&f.usdt, gpu.id()).await?,
        gpu_before + 35 * CREDIT
    );
    f.assert_d6().await
}

// TC-CT-036-12: several creditors, only the one that claims is paid
#[tokio::test]
async fn one_of_several_creditors_claims() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in ["summarizer", "analyst", "coder"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    for (payer, payee, amount) in [
        ("summarizer", "analyst", 15),
        ("analyst", "summarizer", 15),
        ("analyst", "coder", 10),
        ("coder", "summarizer", 10),
    ] {
        let r = f.circle.register(payer, payee, 1, amount * CREDIT).await?;
        assert!(
            r.is_success(),
            "register {payer}->{payee}: {:?}",
            failure_text(&r)
        );
    }
    let c = &f.circle;
    let solver = c.worker.dev_create_account().await?;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c
        .call_keeper("close_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    let edges = json!([
        edge("summarizer", "analyst", 15 * CREDIT),
        edge("analyst", "summarizer", 5 * CREDIT),
        edge("analyst", "coder", 10 * CREDIT),
        edge("coder", "summarizer", 10 * CREDIT)
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
    assert!(c
        .call_keeper("commit_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    let fin = finalize(c).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));

    let summarizer = f.accounts["summarizer"].clone();
    let before = f.balance(&f.usdt, summarizer.id()).await?;
    let got = claim(&f, "summarizer", 1).await?;
    assert!(got.is_success(), "{:?}", failure_text(&got));
    assert_eq!(
        f.balance(&f.usdt, summarizer.id()).await?,
        before + 10 * CREDIT
    );
    f.assert_d6().await
}
