//! CT-029: the clearing flow of F3 in near-sandbox: close, simulate, submit, competing proposals,
//! commit. Members are written into storage (`seed_member`) because `join` and stake are separate
//! items; vouchers go through the real `register_cumulative`. Block time is fast-forwarded.
//! Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{events, failure_text, short_params, Circle, CREDIT};
use serde_json::{json, Value};

fn edge(debtor: &str, creditor: &str, cleared: u128) -> Value {
    json!({"debtor": common::circle::account_of(debtor), "creditor": common::circle::account_of(creditor), "cleared": cleared.to_string()})
}

fn p_3c() -> Value {
    json!([edge("summarizer", "analyst", 10 * CREDIT), edge("analyst", "coder", 10 * CREDIT), edge("coder", "summarizer", 10 * CREDIT)])
}

fn p_2c() -> Value {
    json!([edge("summarizer", "analyst", 15 * CREDIT), edge("analyst", "summarizer", 15 * CREDIT)])
}

fn p_opt() -> Value {
    json!([
        edge("summarizer", "analyst", 15 * CREDIT),
        edge("analyst", "summarizer", 5 * CREDIT),
        edge("analyst", "coder", 10 * CREDIT),
        edge("coder", "summarizer", 10 * CREDIT)
    ])
}

/// `S2` before the epoch closes: three members and four registered pairs.
async fn s2() -> anyhow::Result<Circle> {
    let mut c = Circle::deploy(short_params()).await?;
    for member in ["summarizer", "analyst", "coder"] {
        c.seed_member(member, 100 * CREDIT, 80 * CREDIT).await?;
    }
    for (payer, payee, amount) in [("summarizer", "analyst", 15), ("analyst", "summarizer", 15), ("analyst", "coder", 10), ("coder", "summarizer", 10)] {
        let result = c.register(payer, payee, 1, amount * CREDIT).await?;
        assert!(result.is_success(), "register {payer}->{payee}: {:?}", failure_text(&result));
    }
    Ok(c)
}

/// `S2.close`: waits for `cutoff_at`, then `close_epoch(1)`. Returns the events of the close.
async fn close(c: &Circle) -> anyhow::Result<Vec<(String, Value)>> {
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    let result = c.call_keeper("close_epoch", json!({"epoch": 1})).await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    Ok(events(&result))
}

async fn to_proposal_deadline(c: &Circle) -> anyhow::Result<()> {
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?).await
}

/// State that "state tidak berubah" compares: proposal, obligations, epoch record.
async fn snapshot(c: &Circle) -> anyhow::Result<Value> {
    Ok(json!({
        "proposal": c.view("get_clearing_proposal", json!({"epoch": 1})).await?,
        "obligations": c.view("list_obligations", json!({"epoch": 1, "from_index": 0, "limit": 100})).await?,
        "epoch": c.epoch(1).await?,
    }))
}

fn expect_error(result: &near_workspaces::result::ExecutionFinalResult, code: &str) {
    let text = failure_text(result).unwrap_or_else(|| panic!("expected {code}, but the call succeeded"));
    assert!(text.contains(code), "expected {code} in {text}");
}

// TC-CT-029-01
#[tokio::test]
async fn full_flow_with_a_competing_solver() -> anyhow::Result<()> {
    let c = s2().await?;
    let mut log = close(&c).await?;
    let (solver1, solver2) = (c.worker.dev_create_account().await?, c.worker.dev_create_account().await?);

    let simulation = c.view("simulate_clearing", json!({"epoch": 1, "edges": p_3c()})).await?;
    assert_eq!((simulation["valid"].as_bool(), simulation["better_than_current"].as_bool()), (Some(true), Some(true)));
    assert_eq!(simulation["cleared_total"], "30000000");

    for (who, proposal) in [(&solver1, p_3c()), (&solver2, p_opt())] {
        let result = c.call_as(who, "submit_clearing", json!({"epoch": 1, "edges": proposal})).await?;
        assert!(result.is_success(), "{:?}", failure_text(&result));
        log.extend(events(&result));
    }
    let stored = c.view("get_clearing_proposal", json!({"epoch": 1})).await?;
    assert_eq!(stored["solver"], solver2.id().as_str());
    assert_eq!(stored["cleared_total"], "40000000");

    to_proposal_deadline(&c).await?;
    let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    log.extend(events(&commit));

    let names: Vec<&str> = log.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["epoch_cutoff", "clearing_submitted", "clearing_submitted", "clearing_applied", "epoch_committed"]);

    let positions = c.view("list_positions", json!({"epoch": 1, "from_index": 0, "limit": 10})).await?;
    let by = |name: &str| positions.as_array().unwrap().iter().find(|p| p["account_id"] == common::circle::account_of(name).as_str()).cloned();
    let analyst = by("analyst").expect("analyst has a position");
    assert_eq!((analyst["status"].as_str(), analyst["amount"].as_str()), (Some("due"), Some("10000000")));
    let summarizer = by("summarizer").expect("summarizer has a position");
    assert_eq!((summarizer["status"].as_str(), summarizer["amount"].as_str()), (Some("claimable"), Some("10000000")));
    assert!(by("coder").is_none());
    assert!(c.view("get_clearing_proposal", json!({"epoch": 1})).await?["committed"].as_bool().unwrap());
    Ok(())
}

// TC-CT-029-02, -03, -04, -05
#[tokio::test]
async fn invalid_and_weaker_proposals_are_rejected_without_a_trace() -> anyhow::Result<()> {
    let c = s2().await?;
    close(&c).await?;
    let before = snapshot(&c).await?;
    let (solver1, solver2) = (c.worker.dev_create_account().await?, c.worker.dev_create_account().await?);

    // -02: an edge outside the snapshot
    let result = c.call_as(&solver1, "submit_clearing", json!({"epoch": 1, "edges": [edge("coder", "analyst", 5 * CREDIT)]})).await?;
    expect_error(&result, "ERR_EDGE_NOT_IN_SNAPSHOT");

    // -03: a negative amount fails in the deserializer, without an ERR_ code (K-16)
    let negative = json!([{"debtor": common::circle::account_of("coder"), "creditor": common::circle::account_of("summarizer"), "cleared": "-1"}]);
    let result = c.call_as(&solver1, "submit_clearing", json!({"epoch": 1, "edges": negative})).await?;
    let text = failure_text(&result).expect("negative cleared fails");
    assert!(!text.contains("ERR_"), "{text}");
    // -03: more than the obligation
    let result = c.call_as(&solver1, "submit_clearing", json!({"epoch": 1, "edges": [edge("coder", "summarizer", 10 * CREDIT + 1)]})).await?;
    expect_error(&result, "ERR_CLEARED_EXCEEDS_OBLIGATION");

    // -04: a member whose in and out differ
    let result = c.call_as(&solver1, "submit_clearing", json!({"epoch": 1, "edges": [edge("summarizer", "analyst", 15 * CREDIT)]})).await?;
    expect_error(&result, "ERR_UNBALANCED_MEMBER");
    assert_eq!(snapshot(&c).await?, before, "a rejected proposal must leave no trace");

    // -05: a smaller proposal does not replace the stored one
    assert!(c.call_as(&solver1, "submit_clearing", json!({"epoch": 1, "edges": p_opt()})).await?.is_success());
    let result = c.call_as(&solver2, "submit_clearing", json!({"epoch": 1, "edges": p_3c()})).await?;
    expect_error(&result, "ERR_PROPOSAL_NOT_BETTER");
    let stored = c.view("get_clearing_proposal", json!({"epoch": 1})).await?;
    assert_eq!((stored["solver"].as_str(), stored["cleared_total"].as_str()), (Some(solver1.id().as_str()), Some("40000000")));
    Ok(())
}

// TC-CT-029-06: the tie-break works in both directions (same total 30, fewer edges win)
#[tokio::test]
async fn tie_break_prefers_fewer_edges() -> anyhow::Result<()> {
    let c = s2().await?;
    close(&c).await?;
    let (solver1, solver2) = (c.worker.dev_create_account().await?, c.worker.dev_create_account().await?);
    assert!(c.call_as(&solver1, "submit_clearing", json!({"epoch": 1, "edges": p_3c()})).await?.is_success());
    let result = c.call_as(&solver2, "submit_clearing", json!({"epoch": 1, "edges": p_2c()})).await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    let stored = c.view("get_clearing_proposal", json!({"epoch": 1})).await?;
    assert_eq!((stored["solver"].as_str(), stored["edge_count"].as_u64()), (Some(solver2.id().as_str()), Some(2)));
    let replaced = events(&result);
    assert_eq!(replaced[0].1["replaced_solver"], solver1.id().as_str());
    Ok(())
}

#[tokio::test]
async fn tie_break_rejects_more_edges() -> anyhow::Result<()> {
    let c = s2().await?;
    close(&c).await?;
    let (solver1, solver2) = (c.worker.dev_create_account().await?, c.worker.dev_create_account().await?);
    assert!(c.call_as(&solver1, "submit_clearing", json!({"epoch": 1, "edges": p_2c()})).await?.is_success());
    let result = c.call_as(&solver2, "submit_clearing", json!({"epoch": 1, "edges": p_3c()})).await?;
    expect_error(&result, "ERR_PROPOSAL_NOT_BETTER");
    let stored = c.view("get_clearing_proposal", json!({"epoch": 1})).await?;
    assert_eq!(stored["solver"], solver1.id().as_str());
    Ok(())
}

// TC-CT-029-07, TC-CT-029-08
#[tokio::test]
async fn phases_and_windows() -> anyhow::Result<()> {
    let c = s2().await?;
    let solver = c.worker.dev_create_account().await?;

    // submit while the epoch is still open
    let result = c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": p_3c()})).await?;
    expect_error(&result, "ERR_WRONG_PHASE");

    close(&c).await?;
    assert!(c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": p_3c()})).await?.is_success());

    // commit before the proposal window ends
    let result = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    expect_error(&result, "ERR_PROPOSAL_WINDOW_OPEN");

    // submit after the window
    to_proposal_deadline(&c).await?;
    let result = c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": p_opt()})).await?;
    expect_error(&result, "ERR_PROPOSAL_WINDOW_CLOSED");

    let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    Ok(())
}

// TC-CT-029-09
#[tokio::test]
async fn snapshot_is_locked_at_close() -> anyhow::Result<()> {
    let mut c = s2().await?;
    close(&c).await?;
    let solver = c.worker.dev_create_account().await?;

    // epoch 2 is open: coder owes summarizer 10 more there
    let result = c.register("coder", "summarizer", 2, 10 * CREDIT).await?;
    assert!(result.is_success(), "{:?}", failure_text(&result));
    let obligation = |epoch: u64| {
        let c = &c;
        async move {
            let args = json!({"epoch": epoch, "debtor": common::circle::account_of("coder"), "creditor": common::circle::account_of("summarizer")});
            c.view("obligation", args).await
        }
    };
    assert_eq!(obligation(2).await?["obligation"], "10000000");
    assert_eq!(obligation(1).await?["obligation"], "10000000");

    let result = c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": [edge("coder", "summarizer", 20 * CREDIT)]})).await?;
    expect_error(&result, "ERR_CLEARED_EXCEEDS_OBLIGATION");
    assert_eq!(obligation(1).await?["obligation"], "10000000");
    Ok(())
}
