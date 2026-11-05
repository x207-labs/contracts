//! CT-008: `new`, owner, and `get_circle` in near-sandbox, plus ABI checks that no method lets the
//! owner move D6 or be replaced. Needs `cargo near build non-reproducible-wasm` first.

use near_workspaces::network::Sandbox;
use near_workspaces::{Contract, Worker};
use serde_json::{json, Value};

const WASM: &str = "target/near/x207_circle.wasm";
const ABI: &str = "target/near/x207_circle_abi.json";

async fn deployed() -> anyhow::Result<(Worker<Sandbox>, Contract)> {
    let worker = near_workspaces::sandbox().await?;
    let contract = worker.dev_deploy(&std::fs::read(WASM)?).await?;
    Ok((worker, contract))
}

fn init_args(circle_id: &str) -> Value {
    json!({
        "owner_id": "dao.test.near", "circle_id": circle_id,
        "usdt_token_id": "usdt.test.near", "intents_id": "intents.test.near", "params": null
    })
}

// TC-CT-008-01
#[tokio::test]
async fn init_succeeds_and_views_report_it() -> anyhow::Result<()> {
    let (worker, contract) = deployed().await?;
    contract
        .call("new")
        .args_json(init_args("agents-main"))
        .transact()
        .await?
        .into_result()?;

    let owner: String = contract.view("get_owner").await?.json()?;
    assert_eq!(owner, "dao.test.near");

    let circle: Value = contract.view("get_circle").await?.json()?;
    assert_eq!(circle["circle_id"], "agents-main");
    assert_eq!(circle["contract_id"], contract.id().as_str());
    assert_eq!(circle["network"], "x207:agents-main");
    assert_eq!(circle["usdt_token_id"], "usdt.test.near");
    assert_eq!(circle["intents_id"], "intents.test.near");
    assert_eq!(
        (
            circle["unit"].as_str(),
            circle["decimals"].as_u64(),
            circle["member_count"].as_u64()
        ),
        (Some("credit"), Some(6), Some(0))
    );

    let epoch: Value = contract
        .view("get_epoch")
        .args_json(json!({"epoch": null}))
        .await?
        .json()?;
    assert_eq!(
        (epoch["epoch"].as_u64(), epoch["phase"].as_str()),
        (Some(1), Some("open"))
    );
    let opened: u128 = epoch["opened_at"].as_str().unwrap().parse()?;
    let cutoff: u128 = epoch["cutoff_at"].as_str().unwrap().parse()?;
    assert_eq!(cutoff, opened + 3600 * 1_000_000_000);
    let now = worker.view_block().await?.timestamp() as u128;
    assert!(
        opened <= now && now - opened < 120 * 1_000_000_000,
        "opened_at {opened} vs block {now}"
    );

    let pool: Value = contract.view("get_pool").await?.json()?;
    for field in [
        "total_stake",
        "reserve",
        "epoch_pools",
        "solver_fees",
        "in_flight",
        "accounted",
    ] {
        assert_eq!(pool[field], "0", "{field}");
    }
    let version: String = contract.view("interface_version").await?.json()?;
    assert_eq!(version, x207_circle::INTERFACE_VERSION);
    Ok(())
}

// TC-CT-007-10, TC-CT-008-04: a rejected `new` leaves the contract uninitialized
#[tokio::test]
async fn bad_input_leaves_contract_uninitialized() -> anyhow::Result<()> {
    let (_worker, contract) = deployed().await?;
    let bad_id = contract
        .call("new")
        .args_json(init_args("agents.main"))
        .transact()
        .await?;
    assert!(format!("{:?}", bad_id.into_result().unwrap_err()).contains("ERR_BAD_CIRCLE_ID"));

    let mut args = init_args("agents-main");
    args["params"] = json!({
        "epoch_length": 100, "payment_window": 1800, "proposal_window": 600, "base_limit_ratio": 8000,
        "limit_growth": 1000, "limit_cap": "1000000000", "limit_decay": 0,
        "fees": {"clearing_fee_bps": 0, "solver_fee_bps": 0}, "admission": "open"
    });
    let bad_params = contract.call("new").args_json(args).transact().await?;
    assert!(format!("{:?}", bad_params.into_result().unwrap_err())
        .contains("ERR_PARAM_OUT_OF_RANGE: epoch_length"));

    let owner = contract.view("get_owner").await;
    assert!(format!("{:?}", owner.unwrap_err()).contains("The contract is not initialized"));
    Ok(())
}

// TC-CT-008-02
#[tokio::test]
async fn second_init_fails_and_owner_stays() -> anyhow::Result<()> {
    let (_worker, contract) = deployed().await?;
    contract
        .call("new")
        .args_json(init_args("agents-main"))
        .transact()
        .await?
        .into_result()?;
    let mut again = init_args("other");
    again["owner_id"] = json!("outsider.test.near");
    let second = contract.call("new").args_json(again).transact().await?;
    let error = format!("{:?}", second.into_result().unwrap_err());
    assert!(error.contains("already been initialized"), "{error}");
    assert!(!error.contains("ERR_"), "{error}");
    let owner: String = contract.view("get_owner").await?.json()?;
    assert_eq!(owner, "dao.test.near");
    Ok(())
}

// TC-CT-008-03
#[tokio::test]
async fn methods_refuse_before_init() -> anyhow::Result<()> {
    let (_worker, contract) = deployed().await?;
    let view = contract.view("get_owner").await;
    assert!(format!("{:?}", view.unwrap_err()).contains("The contract is not initialized"));
    let call = contract
        .call("close_epoch")
        .args_json(json!({"epoch": 1}))
        .transact()
        .await?;
    let error = format!("{:?}", call.into_result().unwrap_err());
    assert!(error.contains("The contract is not initialized"), "{error}");
    Ok(())
}

fn abi_functions() -> Vec<Value> {
    let abi: Value =
        serde_json::from_str(&std::fs::read_to_string(ABI).expect("run cargo near build first"))
            .unwrap();
    abi["body"]["functions"].as_array().unwrap().clone()
}

fn arg_names(function: &Value) -> Vec<String> {
    function["params"]["args"]
        .as_array()
        .map(|args| {
            args.iter()
                .map(|a| a["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

// TC-CT-008-08, TC-CT-008-09 (ABI part)
#[test]
fn no_method_moves_d6_or_replaces_the_owner() {
    let denied = [
        "owner",
        "withdraw",
        "sweep",
        "drain",
        "rescue",
        "recover",
        "force",
        "transfer_stake",
        "set_stake",
        "move_pool",
    ];
    for function in abi_functions() {
        let name = function["name"].as_str().unwrap();
        let init = function["modifiers"]
            .as_array()
            .is_some_and(|m| m.iter().any(|x| x == "init"));
        if name == "get_owner"
            || name == "withdraw_stake"
            || name == "withdraw_stake_evm"
            || name == "on_withdraw_resolved"
            || name == "on_withdraw_resolved_fee"
            || name == "on_action_fee_resolved"
        {
            continue; // the member's own exit (CT-077; CT-105 for an EVM member, gated by its signature), not the owner
        }
        if name == "withdraw_intents_balance" || name == "on_intents_withdrawn" {
            continue; // BE-034: permissionless, the receiver is the circle itself, the amount is what `mt_on_transfer` booked; no owner is involved (and its private callback)
        }
        assert!(
            !denied.iter().any(|d| name.contains(d)),
            "method {name} looks like an owner D6 or owner-change path"
        );
        if !init {
            // `previous_owner_id` of the NEP-171 receiver `nft_on_transfer` (CT-075) is the account that sent the token, not an owner of the circle
            // `previous_owner_ids` of the NEP-245 receiver `mt_on_transfer` (BE-034) is the same: the accounts that sent the tokens
            let own = |a: &String| {
                a.contains("owner")
                    && !(name == "nft_on_transfer" && a == "previous_owner_id")
                    && !(name == "mt_on_transfer" && a == "previous_owner_ids")
            };
            assert!(
                !arg_names(&function).iter().any(own),
                "{name} takes an owner argument"
            );
        }
    }
}
