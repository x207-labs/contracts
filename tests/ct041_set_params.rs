//! CT-041: `set_params` in near-sandbox: owner only, ranges, effect from the next epoch, and the
//! readers of the new values (join/stake admission, registration limit, epoch timing).
//! Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{failure_text, receipt_failures, short_params, CREDIT};
use common::funded::Funded;
use serde_json::{json, Value};

const SEC: u64 = 1_000_000_000;

fn with(field: &str, value: Value) -> Value {
    let mut params = short_params();
    match field.split_once('.') {
        Some((outer, inner)) => params[outer][inner] = value,
        None => params[field] = value,
    }
    params
}

async fn set_params(f: &mut Funded, caller: &str, params: Value) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
    let account = f.account(caller).await?;
    f.circle.call_as(&account, "set_params", json!({"params": params})).await
}

/// Waits for the cutoff of `epoch` and closes it with the keeper (anyone).
async fn close(f: &Funded, epoch: u64) -> anyhow::Result<()> {
    f.circle.advance_to(f.circle.epoch_ts(epoch, "cutoff_at").await?).await?;
    let result = f.circle.call_keeper("close_epoch", json!({"epoch": epoch})).await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    Ok(())
}

// TC-CT-041-01
#[tokio::test]
async fn only_the_owner_may_set_params() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(short_params())).await?;
    for caller in ["translator", "facilitator", "solver1"] {
        let result = set_params(&mut f, caller, with("limit_decay", json!(100))).await?;
        assert!(failure_text(&result).expect("fails").contains("ERR_NOT_OWNER"), "{caller}");
    }
    assert_eq!(f.circle.view("get_pending_params", json!({})).await?, Value::Null);
    let owner = set_params(&mut f, "dao", with("limit_decay", json!(100))).await?;
    assert!(owner.is_success(), "{:?}", failure_text(&owner));
    Ok(())
}

// TC-CT-041-02
#[tokio::test]
async fn every_field_outside_its_range_is_rejected() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(short_params())).await?;
    let before = f.circle.view("get_params", json!({})).await?;
    // (field, minimum, maximum) of spec 3.4
    let ranges: [(&str, u64, u64); 9] = [
        ("epoch_length", 300, 604_800),
        ("payment_window", 900, 604_800),
        ("proposal_window", 60, 86_400),
        ("base_limit_ratio", 0, 10_000),
        ("limit_growth", 0, 10_000),
        ("limit_cap", 1_000_000, 100_000_000_000),
        ("limit_decay", 0, 5_000),
        ("fees.clearing_fee_bps", 0, 100),
        ("fees.solver_fee_bps", 0, 100),
    ];
    for (field, min, max) in ranges {
        let wire = |n: u64| if field == "limit_cap" { json!(n.to_string()) } else { json!(n) };
        let mut bad = vec![max + 1];
        if min > 0 {
            bad.push(min - 1);
        }
        for value in bad {
            let result = set_params(&mut f, "dao", with(field, wire(value))).await?;
            let text = failure_text(&result).unwrap_or_else(|| panic!("{field}={value} was accepted"));
            assert!(text.contains(&format!("ERR_PARAM_OUT_OF_RANGE: {field}")), "{field}={value}: {text}");
        }
    }
    assert_eq!(f.circle.view("get_params", json!({})).await?, before);
    assert_eq!(f.circle.view("get_pending_params", json!({})).await?, Value::Null);
    Ok(())
}

// TC-CT-041-03
#[tokio::test]
async fn new_params_apply_from_the_next_epoch() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(short_params())).await?;
    let result = set_params(&mut f, "dao", with("base_limit_ratio", json!(5000))).await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    assert_eq!(f.circle.view("get_params", json!({})).await?["base_limit_ratio"], 8000, "epoch 1 keeps the old value");
    let pending = f.circle.view("get_pending_params", json!({})).await?;
    assert_eq!((pending["effective_epoch"].as_u64(), pending["params"]["base_limit_ratio"].as_u64()), (Some(2), Some(5000)));

    close(&f, 1).await?;
    assert_eq!(f.circle.epoch(1).await?["params"]["base_limit_ratio"], 8000);
    assert_eq!(f.circle.epoch(2).await?["params"]["base_limit_ratio"], 5000);
    assert_eq!(f.circle.view("get_params", json!({})).await?["base_limit_ratio"], 5000);
    assert_eq!(f.circle.view("get_pending_params", json!({})).await?, Value::Null);
    Ok(())
}

// TC-CT-041-04
#[tokio::test]
async fn dao_admission_is_read_by_join_and_stake() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(short_params())).await?;
    assert!(set_params(&mut f, "dao", with("admission", json!("dao"))).await?.is_success());
    close(&f, 1).await?;

    let newcomer = f.account("newcomer").await?;
    let join = f.join("newcomer", Funded::join_deposit()).await?;
    assert!(join.is_success(), "{:?}", failure_text(&join));
    assert_eq!(f.member("newcomer").await?["status"], "pending");

    let stake = f.stake(&newcomer, 100 * CREDIT).await?;
    assert!(receipt_failures(&stake).contains("ERR_NOT_ADMITTED"), "{}", receipt_failures(&stake));
    assert_eq!(f.balance(&f.usdt, newcomer.id()).await?, 1000 * CREDIT, "full refund");
    assert_eq!(f.member("newcomer").await?["status"], "pending");
    f.assert_d6().await
}

// TC-CT-041-05
#[tokio::test]
async fn limit_ratio_is_read_by_stake_and_registration() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(short_params())).await?;
    assert!(set_params(&mut f, "dao", with("base_limit_ratio", json!(5000))).await?.is_success());
    close(&f, 1).await?;

    for name in ["newbie", "payee"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    assert_eq!(f.member("newbie").await?["limit"], "50000000");

    let ok = f.circle.register("newbie", "payee", 2, 50 * CREDIT).await?;
    assert!(ok.is_success(), "{:?}", failure_text(&ok));
    let over = f.circle.register("newbie", "payee", 2, 50 * CREDIT + 1).await?;
    assert!(failure_text(&over).expect("over the limit").contains("ERR_INSUFFICIENT_CREDIT"));
    Ok(())
}

// TC-CT-041-06
#[tokio::test]
async fn epoch_timing_is_read_at_close() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(short_params())).await?;
    let mut params = with("epoch_length", json!(7200));
    params["proposal_window"] = json!(1200);
    assert!(set_params(&mut f, "dao", params).await?.is_success());

    let cutoff1 = f.circle.epoch_ts(1, "cutoff_at").await?;
    close(&f, 1).await?;
    let closed1 = f.circle.epoch_ts(1, "closed_at").await?;
    assert_eq!(f.circle.epoch_ts(1, "proposal_deadline").await?, closed1 + 60 * SEC, "epoch 1 keeps its own proposal window (K-12)");
    assert_eq!(f.circle.epoch_ts(2, "cutoff_at").await?, cutoff1 + 7200 * SEC);

    // Closing epoch 2 would need 7200 s of block time (about seven minutes of fast-forward), so
    // its proposal deadline is checked through the params snapshot that `close_epoch` uses (TC-CT-024).
    let epoch2 = f.circle.epoch(2).await?;
    assert_eq!((epoch2["params"]["epoch_length"].as_u64(), epoch2["params"]["proposal_window"].as_u64()), (Some(7200), Some(1200)));
    Ok(())
}

// TC-CT-041-08
#[tokio::test]
async fn payment_window_of_fifteen_minutes_is_accepted() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(with("payment_window", json!(1800)))).await?;
    let result = set_params(&mut f, "dao", with("payment_window", json!(900))).await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    let pending = f.circle.view("get_pending_params", json!({})).await?;
    assert_eq!(pending["params"]["payment_window"], 900);
    Ok(())
}
