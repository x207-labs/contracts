//! CT-048 (and the sandbox cases of CT-046 and CT-047): the circle owned by a Sputnik DAO in near-sandbox.
//! The DAO wasm is `tests/fixtures/sputnik/sputnikdao2_original.wasm` (pinned, see the README there); it is only
//! deployed to the sandbox. Proposal and vote arguments come from the scripts in `dao/` through
//! `tests/e2e/dao_args.mjs`. Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, failure_text, events, CREDIT};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::Account;
use serde_json::{json, Value};

const SEC: u64 = 1_000_000_000;

fn node(args: &[&str]) -> Value {
    let out = std::process::Command::new("node").arg("tests/e2e/dao_args.mjs").args(args).output().expect("run node");
    assert!(out.status.success(), "dao_args {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("json")
}

struct Dao {
    f: Funded,
    dao: Account,
    council: Vec<Account>,
    outsider: Account,
}

/// Circle with `owner_id` = `dao.test.near`, which is a Sputnik DAO of three council members.
async fn setup() -> anyhow::Result<Dao> {
    setup_with(Funded::params_short()).await
}

async fn setup_with(params: Value) -> anyhow::Result<Dao> {
    let mut f = Funded::deploy(Some(params)).await?;
    let root = f.circle.worker.root_account()?;
    let dao = root.create_subaccount("dao").initial_balance(NearToken::from_near(30)).transact().await?.into_result()?;
    assert_eq!(dao.id().as_str(), account_of("dao"));
    let mut council = Vec::new();
    for name in ["c1", "c2", "c3"] {
        council.push(f.account(name).await?);
    }
    let outsider = f.account("outsider").await?;
    let wasm = std::fs::read("tests/fixtures/sputnik/sputnikdao2_original.wasm")?;
    let dao = dao.deploy(&wasm).await?.into_result()?;
    let members = council.iter().map(|a| a.id().to_string()).collect::<Vec<_>>().join(",");
    let init = dao
        .call("new")
        .args_json(json!({"config": {"name": "x207", "purpose": "test", "metadata": ""}, "policy": node(&["policy", &members])}))
        .transact()
        .await?;
    assert!(init.is_success(), "{:?}", failure_text(&init));
    Ok(Dao { f, dao: dao.as_account().clone(), council, outsider })
}

impl Dao {
    fn dao_id(&self) -> &near_workspaces::AccountId {
        self.dao.id()
    }

    async fn propose(&self, who: &Account, params: &Value) -> anyhow::Result<u64> {
        let args = node(&["propose", self.f.circle.contract.id().as_str(), &params.to_string()]);
        let r = who.call(self.dao_id(), "add_proposal").args_json(args).deposit(NearToken::from_millinear(100)).gas(Gas::from_tgas(100)).transact().await?;
        assert!(r.is_success(), "{:?}", failure_text(&r));
        Ok(r.json::<u64>()?)
    }

    async fn act(&self, who: &Account, id: u64, action: &str) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        let args = node(&["act", &id.to_string(), action]);
        Ok(who.call(self.dao_id(), "act_proposal").args_json(args).gas(Gas::from_tgas(250)).transact().await?)
    }

    async fn proposal(&self, id: u64) -> anyhow::Result<Value> {
        Ok(self.f.circle.worker.view(self.dao_id(), "get_proposal").args_json(json!({"id": id})).await?.json()?)
    }

    async fn status(&self, id: u64) -> anyhow::Result<String> {
        Ok(self.proposal(id).await?["status"].as_str().unwrap().to_string())
    }

    async fn pending(&self) -> anyhow::Result<Value> {
        self.f.circle.view("get_pending_params", json!({})).await
    }
}

fn with(field: &str, value: Value) -> Value {
    let mut params = Funded::params_short();
    params[field] = value;
    params
}

// TC-CT-046-01, 02, 05, TC-CT-047-01, 02, TC-CT-048-01, TC-CT-048-05
#[tokio::test]
async fn proposal_vote_execute_and_effect() -> anyhow::Result<()> {
    let d = setup().await?;
    let params = with("epoch_length", json!(600));
    let before = d.f.circle.view("get_params", json!({})).await?;

    let id = d.propose(&d.council[0], &params).await?;
    let proposal = d.proposal(id).await?;
    let call = &proposal["kind"]["FunctionCall"];
    assert_eq!(call["receiver_id"], d.f.circle.contract.id().as_str());
    assert_eq!(call["actions"][0]["method_name"], "set_params");
    use near_sdk::base64::Engine;
    let decoded = near_sdk::base64::engine::general_purpose::STANDARD.decode(call["actions"][0]["args"].as_str().unwrap())?;
    assert_eq!(serde_json::from_slice::<Value>(&decoded)?, json!({"params": params}), "TC-CT-046-02");
    assert_eq!(d.status(id).await?, "InProgress");
    // TC-CT-046-05: the active parameters the print script reads are the view `get_params`
    let printed = std::process::Command::new("node").args(["dao/print-params.mjs", "--contract", d.f.circle.contract.id().as_str(), "--network", "testnet"]).output()?;
    assert!(String::from_utf8_lossy(&printed.stdout).contains("get_params"));

    let first = d.act(&d.council[0], id, "VoteApprove").await?;
    assert!(first.is_success(), "{:?}", failure_text(&first));
    assert_eq!(d.status(id).await?, "InProgress", "one vote of three is not a majority");
    assert_eq!(d.pending().await?, Value::Null);

    let second = d.act(&d.council[1], id, "VoteApprove").await?;
    assert!(second.is_success(), "{:?}", failure_text(&second));
    assert_eq!(d.status(id).await?, "Approved");
    let log = events(&second);
    let updated = log.iter().find(|(name, _)| name == "params_updated").unwrap_or_else(|| panic!("no params_updated in {log:?}"));
    assert_eq!(updated.1["new"]["epoch_length"], 600);
    assert_eq!(d.pending().await?["params"]["epoch_length"], 600);
    assert_eq!(d.f.circle.view("get_params", json!({})).await?, before, "active params change only after close_epoch");

    // TC-CT-048-05: the next cutoff uses the new epoch_length
    let c = &d.f.circle;
    let cutoff1 = c.epoch_ts(1, "cutoff_at").await?;
    c.advance_to(cutoff1).await?;
    let close = c.call_keeper("close_epoch", json!({"epoch": 1})).await?;
    assert!(close.is_success(), "{:?}", failure_text(&close));
    let cutoff = events(&close).into_iter().find(|(n, _)| n == "epoch_cutoff").expect("epoch_cutoff").1;
    assert_eq!(cutoff["next_cutoff_at"].as_str().unwrap().parse::<u64>()?, cutoff1 + 600 * SEC);
    assert_eq!(c.view("get_params", json!({})).await?["epoch_length"], 600);
    let _ = CREDIT;
    Ok(())
}

// TC-CT-046-03, TC-CT-046-04, TC-CT-048-02: bad JSON makes no proposal, an out-of-range value fails on execution
#[tokio::test]
async fn out_of_range_fails_at_execution() -> anyhow::Result<()> {
    let d = setup().await?;
    let bad = std::process::Command::new("node").args(["dao/propose-set-params.mjs", "--dao", "x207.test.near", "--contract", "c.test.near", "--signer", "a.test.near", "--params", "{not json"]).output()?;
    assert!(!bad.status.success(), "TC-CT-046-03");
    assert_eq!(d.f.circle.worker.view(d.dao_id(), "get_last_proposal_id").await?.json::<u64>()?, 0);

    let id = d.propose(&d.council[0], &with("payment_window", json!(899))).await?;
    assert_eq!(d.status(id).await?, "InProgress");
    assert!(d.act(&d.council[0], id, "VoteApprove").await?.is_success());
    let last = d.act(&d.council[1], id, "VoteApprove").await?;
    // the DAO runs set_params in a callback and reports its panic as the failure of the voting transaction
    assert!(failure_text(&last).expect("set_params panics").contains("ERR_PARAM_OUT_OF_RANGE: payment_window"), "{:?}", failure_text(&last));
    // TC-CT-048-02 expects `Failed`; the pinned sputnikdao2_original.wasm keeps the proposal `Approved` when the
    // call it runs panics. Only the unchanged D7 is the contract's side of the case.
    assert_eq!(d.status(id).await?, "Approved");
    assert_eq!(d.pending().await?, Value::Null);
    assert!(last.logs().iter().all(|l| !l.contains("params_updated")));
    Ok(())
}

// TC-CT-048-03, TC-CT-047-03: a council member calling the circle directly, and an outsider voting
#[tokio::test]
async fn only_the_dao_owns_and_only_council_votes() -> anyhow::Result<()> {
    let d = setup().await?;
    let direct = d.f.circle.call_as(&d.council[0], "set_params", json!({"params": with("epoch_length", json!(600))})).await?;
    assert!(failure_text(&direct).expect("refused").contains("ERR_NOT_OWNER"));

    let id = d.propose(&d.outsider, &with("epoch_length", json!(600))).await?;
    let vote = d.act(&d.outsider, id, "VoteApprove").await?;
    assert!(failure_text(&vote).is_some(), "the DAO must refuse a vote from outside the council");
    assert_eq!(d.status(id).await?, "InProgress");
    Ok(())
}

// TC-CT-048-04: a majority of VoteReject leaves the circle untouched
#[tokio::test]
async fn rejected_proposal_changes_nothing() -> anyhow::Result<()> {
    let d = setup().await?;
    let id = d.propose(&d.council[0], &with("epoch_length", json!(600))).await?;
    assert!(d.act(&d.council[0], id, "VoteReject").await?.is_success());
    let last = d.act(&d.council[1], id, "VoteReject").await?;
    assert!(last.is_success(), "{:?}", failure_text(&last));
    assert_eq!(d.status(id).await?, "Rejected");
    assert_eq!(d.pending().await?, Value::Null);
    assert!(last.logs().iter().all(|l| !l.contains("params_updated")));
    Ok(())
}

// TC-CT-047-04: a vote after the one hour proposal period cannot approve any more
#[tokio::test]
async fn late_vote_does_not_execute() -> anyhow::Result<()> {
    let mut d = setup().await?;
    d.f.circle.max_jump = 600;
    let id = d.propose(&d.council[0], &with("epoch_length", json!(600))).await?;
    let submitted = d.proposal(id).await?["submission_time"].as_str().unwrap().parse::<u64>()?;
    d.f.circle.advance_to(submitted + 3601 * SEC).await?;
    let late = d.act(&d.council[0], id, "VoteApprove").await?;
    let _ = late;
    assert_ne!(d.status(id).await?, "Approved");
    assert_eq!(d.pending().await?, Value::Null);
    assert_eq!(d.f.circle.view("get_params", json!({})).await?["epoch_length"], 300);
    Ok(())
}

// CT-056: a council proposal admits a new account to a circle with `admission` dao; its stake then activates it
#[tokio::test]
async fn proposal_admits_a_new_member() -> anyhow::Result<()> {
    let mut params = Funded::params_short();
    params["admission"] = json!("dao");
    let mut d = setup_with(params).await?;
    let newbie = d.f.account("newbie").await?;
    assert!(d.f.join("newbie", Funded::join_deposit()).await?.is_success());

    let args = node(&["admit", d.f.circle.contract.id().as_str(), newbie.id().as_str()]);
    let r = d.council[0].call(d.dao_id(), "add_proposal").args_json(args).deposit(NearToken::from_millinear(100)).gas(Gas::from_tgas(100)).transact().await?;
    assert!(r.is_success(), "{:?}", failure_text(&r));
    let id = r.json::<u64>()?;
    assert!(d.act(&d.council[0], id, "VoteApprove").await?.is_success());
    let last = d.act(&d.council[1], id, "VoteApprove").await?;
    assert!(last.is_success(), "{:?}", failure_text(&last));
    assert_eq!(d.status(id).await?, "Approved");
    assert!(events(&last).iter().any(|(n, data)| n == "member_admitted" && data["account_id"] == newbie.id().as_str()));
    assert_eq!(d.f.member("newbie").await?["admitted"], true);

    assert!(d.f.stake(&newbie, 100 * CREDIT).await?.is_success());
    assert_eq!(d.f.member("newbie").await?["status"], "active");
    Ok(())
}
