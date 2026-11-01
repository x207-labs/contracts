//! Sandbox cases of the contract test file that no other file runs: the epoch edge cases around
//! `close_epoch` and `commit_epoch` (TC-CT-014-09, -015-11/12, -016-11, -010-06, -020-24, -021-02/05/08,
//! -024-06/07/08, -028-13, -031-06, -040-06), the proposal size limit (TC-CT-004-05, -027-12), and
//! the new limit formula at `finalize_epoch` (TC-CT-021-06, -041-07).
//! Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, events, failure_text, receipt_failures, CREDIT};
use common::flow::{edge, staked_members};
use common::funded::Funded;
use near_workspaces::types::NearToken;
use serde_json::{json, Value};

const SEC: u64 = 1_000_000_000;

fn err(result: &near_workspaces::result::ExecutionFinalResult) -> String {
    failure_text(result).expect("the call must fail")
}

async fn debt_and_headroom(f: &Funded, who: &str) -> anyhow::Result<(u128, u128, u128)> {
    let c = &f.circle;
    let get = |v: Value| -> u128 { v.as_str().unwrap().parse().unwrap() };
    let id = json!({"account_id": account_of(who)});
    Ok((get(c.view("outstanding_debt", id.clone()).await?), get(c.view("headroom", id.clone()).await?), get(c.view("limit", id).await?)))
}

// TC-CT-016-11, -010-06, -015-11, -021-02, -021-08, -024-07, -028-13, -040-06, -024-08, -024-06, -014-09,
// -015-12, -021-05, -031-06, -020-24: one epoch, in order
#[tokio::test]
async fn epoch_edge_cases_in_one_story() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in ["scraper", "gpu", "translator"] {
        f.account(name).await?;
    }
    // TC-CT-016-11: a join with the wrong circle_id returns the whole deposit; only gas is spent
    let scraper = f.accounts["scraper"].clone();
    let before = scraper.view_account().await?.balance;
    let wrong = scraper
        .call(f.circle.contract.id(), "join")
        .args_json(json!({"circle_id": "other-circle", "pubkey_voucher": f.circle.key_for("scraper"), "terms_hash": "a".repeat(64)}))
        .deposit(NearToken::from_near(1))
        .max_gas()
        .transact()
        .await?;
    assert!(err(&wrong).contains("ERR_WRONG_CIRCLE_ID"));
    let spent = before.as_yoctonear() - scraper.view_account().await?.balance.as_yoctonear();
    assert!(spent < NearToken::from_millinear(10).as_yoctonear(), "only gas may be spent, spent {spent} yocto");

    staked_members(&mut f, &["scraper", "gpu", "translator"]).await?;

    // TC-CT-010-06: a refused transfer leaves the pool and the contract balance as they were
    let pool_before = f.circle.view("get_pool", json!({})).await?;
    let balance_before = f.balance(&f.usdt, f.circle.contract.id()).await?;
    let refused = f.transfer_call(&f.usdt, &scraper, 10 * CREDIT, r#"{"action":"burn"}"#).await?;
    assert!(receipt_failures(&refused).contains("ERR_BAD_MSG"));
    assert_eq!(f.circle.view("get_pool", json!({})).await?, pool_before);
    assert_eq!(f.balance(&f.usdt, f.circle.contract.id()).await?, balance_before);

    // TC-CT-015-11: the limit view
    assert_eq!(f.circle.view("limit", json!({"account_id": account_of("translator")})).await?, "80000000");
    assert_eq!(f.circle.view("limit", json!({"account_id": account_of("outsider")})).await?, "0");

    // TC-CT-021-02, -021-08: seq 1, 5, 6 give last_seq 6; headroom + debt = limit after every registration
    for (seq, cumulative) in [(1u64, 20u128), (5, 40), (6, 60)] {
        let r = f.circle.register_at("scraper", "gpu", 1, seq, cumulative * CREDIT).await?;
        assert!(r.is_success(), "seq {seq}: {:?}", failure_text(&r));
        let (debt, headroom, limit) = debt_and_headroom(&f, "scraper").await?;
        assert_eq!((debt, debt + headroom), (cumulative * CREDIT, limit), "after seq {seq}");
    }
    assert_eq!(f.circle.view("last_seq", json!({"payer": account_of("scraper"), "payee": account_of("gpu"), "epoch": 1})).await?, 6);
    for (payer, payee) in [("gpu", "translator"), ("translator", "scraper")] {
        assert!(f.circle.register(payer, payee, 1, 25 * CREDIT).await?.is_success());
    }

    // TC-CT-040-06, -028-13: set_params in the running epoch does not change its own windows
    let dao = f.account("dao").await?;
    let mut next = Funded::params_short();
    next["payment_window"] = json!(1800);
    let set = f.circle.call_as(&dao, "set_params", json!({"params": next})).await?;
    assert!(set.is_success(), "{:?}", failure_text(&set));

    // TC-CT-024-07: between cutoff_at and close_epoch epoch 1 is past its cutoff, epoch 2 is not open yet
    let c = &f.circle;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    let late = c.register_at("scraper", "gpu", 1, 7, 61 * CREDIT).await?;
    assert!(err(&late).contains("ERR_PAST_CUTOFF"));
    let next_epoch = c.register_at("scraper", "gpu", 2, 1, CREDIT).await?;
    assert!(err(&next_epoch).contains("ERR_EPOCH_MISMATCH"));

    // TC-CT-024-08: everything registered before the cutoff is in the snapshot
    let obligations_before = c.view("list_obligations", json!({"epoch": 1, "from_index": 0, "limit": 100})).await?;
    let solver = c.worker.dev_create_account().await?;
    let close = c.call_keeper("close_epoch", json!({"epoch": 1})).await?;
    assert!(close.is_success(), "{:?}", failure_text(&close));
    let cut = &events(&close)[0];
    assert_eq!((cut.0.as_str(), cut.1["total_obligation"].as_str(), cut.1["edge_count"].as_u64()), ("epoch_cutoff", Some("110000000"), Some(3)));

    // TC-CT-024-06: after the close a voucher for epoch 1 is refused and leaves no trace
    let after = c.register_at("scraper", "gpu", 1, 7, 61 * CREDIT).await?;
    assert!(err(&after).contains("ERR_EPOCH_MISMATCH"));
    assert_eq!(c.view("list_obligations", json!({"epoch": 1, "from_index": 0, "limit": 100})).await?, obligations_before);

    // TC-CT-014-09: while epoch 1 clears, the running epoch is 2 and open
    let running = c.view("get_epoch", json!({})).await?;
    assert_eq!((running["epoch"].as_u64(), running["phase"].as_str()), (Some(2), Some("open")));

    let edges = json!([edge("scraper", "gpu", 25 * CREDIT), edge("gpu", "translator", 25 * CREDIT), edge("translator", "scraper", 25 * CREDIT)]);
    assert!(c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": edges})).await?.is_success());
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?).await?;
    let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    let epoch = c.epoch(1).await?;
    let (committed, deadline): (u64, u64) = (epoch["committed_at"].as_str().unwrap().parse()?, epoch["payment_deadline"].as_str().unwrap().parse()?);
    assert_eq!(deadline - committed, 900 * SEC, "the params snapshot of epoch 1 (900 s) applies, not the pending 1800 s");
    assert_eq!(c.view("get_params", json!({})).await?["payment_window"], 1800, "epoch 2 runs on the new value");

    // TC-CT-015-12: clearing never raises settled_history
    for name in ["scraper", "gpu", "translator"] {
        assert_eq!(f.member(name).await?["settled_history"], "0", "{name}");
    }
    // TC-CT-021-05: the residual keeps headroom: 80 - 35 = 45
    assert_eq!(debt_and_headroom(&f, "scraper").await?, (35 * CREDIT, 45 * CREDIT, 80 * CREDIT));
    // TC-CT-031-06: net positions as in the 207 report example
    let net = |v: Value| v["net"].as_str().map(str::to_string);
    assert_eq!(net(c.view("net_position", json!({"epoch": 1, "account_id": account_of("scraper")})).await?), Some("-35000000".into()));
    assert_eq!(net(c.view("net_position", json!({"epoch": 1, "account_id": account_of("gpu")})).await?), Some("35000000".into()));
    assert_eq!(c.view("net_position", json!({"epoch": 1, "account_id": account_of("translator")})).await?, Value::Null);

    // TC-CT-020-24: the old residual binds epoch 2: 45.000001 is refused, 45 is accepted
    let over = c.register_at("scraper", "gpu", 2, 1, 45 * CREDIT + 1).await?;
    assert!(err(&over).contains("ERR_INSUFFICIENT_CREDIT"));
    let exact = c.register_at("scraper", "gpu", 2, 2, 45 * CREDIT).await?;
    assert!(exact.is_success(), "{:?}", failure_text(&exact));
    Ok(())
}

// TC-CT-004-05, -027-12: exactly MAX_EDGES_PER_TX (300) edges are accepted, 301 are refused
#[tokio::test]
async fn proposal_size_limit() -> anyhow::Result<()> {
    const MEMBERS: usize = 25;
    const OFFSETS: usize = 12; // 25 * 12 = 300 edges
    let name = |i: usize| format!("m{i:02}");
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let names: Vec<String> = (0..MEMBERS).map(name).collect();
    staked_members(&mut f, &names.iter().map(String::as_str).collect::<Vec<_>>()).await?;
    let mut edges = Vec::new();
    for k in 1..=OFFSETS {
        for i in 0..MEMBERS {
            let (payer, payee) = (name(i), name((i + k) % MEMBERS));
            let r = f.circle.register(&payer, &payee, 1, CREDIT).await?;
            assert!(r.is_success(), "{payer}->{payee}: {:?}", failure_text(&r));
            edges.push(edge(&payer, &payee, CREDIT));
        }
    }
    let c = &f.circle;
    let solver = c.worker.dev_create_account().await?;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());

    let too_many: Vec<Value> = edges.iter().cloned().chain(std::iter::once(edge("m00", "m01", CREDIT))).collect();
    let refused = c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": too_many})).await?;
    assert!(err(&refused).contains("ERR_TOO_MANY_EDGES"));
    assert_eq!(c.view("get_clearing_proposal", json!({"epoch": 1})).await?, Value::Null);

    let accepted = c.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": edges})).await?;
    assert!(accepted.is_success(), "{:?}", failure_text(&accepted));
    let tgas = accepted.total_gas_burnt.as_gas() as f64 / 1e12;
    assert!(tgas < 500.0, "300 edges burn {tgas:.1} Tgas");
    let stored = c.view("get_clearing_proposal", json!({"epoch": 1})).await?;
    assert_eq!((stored["edge_count"].as_u64(), stored["cleared_total"].as_str()), (Some(300), Some("300000000")));
    Ok(())
}

// TC-CT-041-07, -021-06: limit_growth 2000 set in epoch 1 applies when epoch 2 is finalized
#[tokio::test]
async fn new_limit_growth_is_used_at_finalize() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    staked_members(&mut f, &["scraper", "gpu"]).await?;
    let dao = f.account("dao").await?;
    let mut next = Funded::params_short();
    next["limit_growth"] = json!(2000);
    assert!(f.circle.call_as(&dao, "set_params", json!({"params": next})).await?.is_success());

    f.circle.advance_to(f.circle.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(f.circle.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
    assert_eq!(f.circle.view("get_params", json!({})).await?["limit_growth"], 2000, "epoch 2 runs on the new params");

    // epoch 2: scraper owes gpu 35, no cycle, so nothing clears
    assert!(f.circle.register("scraper", "gpu", 2, 35 * CREDIT).await?.is_success());
    let c = &f.circle;
    c.advance_to(c.epoch_ts(2, "cutoff_at").await?).await?;
    assert!(c.call_keeper("close_epoch", json!({"epoch": 2})).await?.is_success());
    c.advance_to(c.epoch_ts(2, "proposal_deadline").await?).await?;
    assert!(c.call_keeper("commit_epoch", json!({"epoch": 2})).await?.is_success());
    let scraper = f.accounts["scraper"].clone();
    let pay = f.transfer_call(&f.usdt, &scraper, 35 * CREDIT, r#"{"action":"settle","epoch":2}"#).await?;
    assert!(pay.is_success(), "{:?}", failure_text(&pay));
    let c = &f.circle;
    c.advance_to(c.epoch_ts(2, "payment_deadline").await?).await?;
    let fin = c.call_keeper("finalize_epoch", json!({"epoch": 2})).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));

    // 100 * 80% + 35 * 20% = 87, with debt 0 the whole limit is headroom (TC-CT-021-06 pattern)
    assert_eq!(f.member("scraper").await?["limit"], "87000000");
    assert_eq!(debt_and_headroom(&f, "scraper").await?, (0, 87 * CREDIT, 87 * CREDIT));
    f.assert_d6().await
}
