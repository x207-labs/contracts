//! CT-103: the residual through the REAL Intents contract. Until now (CT-036, BE-034, CT-092) the contract of Intents was `probes/mock_intents`. Here the wasm of `intents.near` taken from
//! mainnet (`scripts/fetch-intents-wasm.sh`, read only, sha256 checked) runs in near-sandbox as the `intents_id` of the circle, and one residual is paid and claimed through it:
//!   scraper deposits USDT to Intents (`ft_transfer_call` with its account as the message), pays its residual with `mt_transfer_call` of Intents to the circle (`mt_on_transfer`, `settle`),
//!   the epoch is finalized, `withdraw_intents_balance` calls the real `ft_withdraw`, and gpu claims its credit in USDT.
//! Needs `scripts/fetch-intents-wasm.sh` first; without the file the test says so and passes (CI does not fetch from mainnet). `X207_INTENTS_WASM` points to another path.
mod common;

use common::circle::{events, failure_text, Circle, CREDIT};
use common::flow::{finalize, s1_committed_on};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// sha256 of the code of `intents.near` at block 222990644 (code_hash BtA1BEFNS619KkvXFsgcpQ21Tb5yMm32aaUkn7xNothk).
const INTENTS_SHA256: &str = "a1afab98916d9f213746544dcf8e4e6bdb22c0be6bc5dfbd8ea8814c9922b547";

fn intents_wasm() -> Option<Vec<u8>> {
    let path = std::env::var("X207_INTENTS_WASM").unwrap_or_else(|_| "target/intents-near/intents.wasm".to_string());
    let bytes = std::fs::read(&path).ok()?;
    let hex: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, INTENTS_SHA256, "{path} is not the wasm of intents.near that this test was written for");
    Some(bytes)
}

/// `Funded::deploy_with_intents`, with the real Intents contract instead of the mock.
async fn deploy_with_real_intents(wasm: &[u8]) -> anyhow::Result<Funded> {
    let worker = near_workspaces::sandbox().await?;
    let token = |w: near_workspaces::Worker<near_workspaces::network::Sandbox>| async move {
        let ft = w.dev_deploy(&std::fs::read(common::probe_wasm("mock_ft"))?).await?;
        ft.call("new").transact().await?.into_result()?;
        anyhow::Ok(ft)
    };
    let usdt = token(worker.clone()).await?;
    let palsu = token(worker.clone()).await?;
    let intents = worker.dev_deploy(wasm).await?;
    let admin = worker.dev_create_account().await?;
    let init = intents
        .call("new")
        .args_json(json!({
            "config": {
                "wnear_id": "wrap.test.near",
                "fees": { "fee": 0, "fee_collector": admin.id() },
                "roles": { "super_admins": [admin.id()], "admins": {}, "grantees": {} }
            }
        }))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert!(init.is_success(), "intents new: {:?}", failure_text(&init));
    let contract = worker.dev_deploy(&std::fs::read("target/near/x207_circle.wasm")?).await?;
    contract
        .call("new")
        .args_json(json!({
            "owner_id": common::circle::account_of("dao"), "circle_id": "agents-main", "usdt_token_id": usdt.id(),
            "intents_id": intents.id(), "params": Funded::params_short()
        }))
        .transact()
        .await?
        .into_result()?;
    let keeper = worker.dev_create_account().await?;
    let circle = Circle { worker, contract, keys: BTreeMap::new(), seqs: BTreeMap::new(), max_jump: 4000, keeper };
    Ok(Funded { circle, usdt, palsu, intents: Some(intents), accounts: BTreeMap::new() })
}

async fn mt_balance(f: &Funded, who: &str) -> anyhow::Result<String> {
    let intents = f.intents.as_ref().expect("intents");
    Ok(intents.view("mt_balance_of").args_json(json!({"account_id": who, "token_id": format!("nep141:{}", f.usdt.id())})).await?.json::<String>()?)
}

// TC-CT-103-01: a residual paid through the real Intents and claimed in USDT
#[tokio::test]
async fn a_residual_is_paid_through_the_real_intents_contract_and_claimed() -> anyhow::Result<()> {
    let Some(wasm) = intents_wasm() else {
        eprintln!("CT-103: no wasm of intents.near (run scripts/fetch-intents-wasm.sh); the test did not run");
        return Ok(());
    };
    let f = s1_committed_on(deploy_with_real_intents(&wasm).await?).await?;
    let intents = f.intents.as_ref().expect("intents").clone();
    let circle_id = f.circle.contract.id().to_string();
    let scraper = f.accounts["scraper"].clone();
    let gpu = f.accounts["gpu"].clone();
    let usdt_token = format!("nep141:{}", f.usdt.id());

    // 1. scraper deposits 35 USDT to Intents: the multi-token balance of scraper appears
    let deposit = scraper
        .call(f.usdt.id(), "ft_transfer_call")
        .args_json(json!({"receiver_id": intents.id(), "amount": (35 * CREDIT).to_string(), "msg": scraper.id()}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert!(deposit.is_success(), "deposit to Intents: {:?}", failure_text(&deposit));
    assert_eq!(mt_balance(&f, scraper.id().as_str()).await?, (35 * CREDIT).to_string(), "the deposit is a multi-token balance of scraper at Intents");

    // 2. scraper pays its residual with `mt_transfer_call` of the real Intents: the circle books it in the pool
    let paid = scraper
        .call(intents.id(), "mt_transfer_call")
        .args_json(json!({
            "receiver_id": circle_id, "token_id": usdt_token, "amount": (35 * CREDIT).to_string(),
            "msg": json!({"action": "settle", "epoch": 1}).to_string()
        }))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(200))
        .transact()
        .await?;
    assert!(paid.is_success(), "mt_transfer_call: {:?}", failure_text(&paid));
    let log = events(&paid);
    assert!(log.iter().any(|(n, d)| n == "residual_paid" && d["amount"] == "35000000"), "{log:?}");
    assert_eq!(mt_balance(&f, scraper.id().as_str()).await?, "0");
    assert_eq!(mt_balance(&f, &circle_id).await?, (35 * CREDIT).to_string(), "the tokens now sit at Intents under the account of the circle");
    assert_eq!(f.circle.view("intents_balance", json!({})).await?.as_str(), Some("35000000"));

    // 3. the epoch is finalized; the NEP-141 balance of the circle does not hold the 35 yet
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));

    // 4. `withdraw_intents_balance` calls the real `ft_withdraw` of Intents: the USDT comes to the NEP-141 account of the circle
    let before = f.balance(&f.usdt, f.circle.contract.id()).await?;
    let anyone = f.circle.worker.dev_create_account().await?;
    let done = anyone.call(f.circle.contract.id(), "withdraw_intents_balance").gas(Gas::from_tgas(300)).transact().await?;
    assert!(done.is_success(), "withdraw_intents_balance: {:?}", failure_text(&done));
    assert_eq!(done.clone().json::<String>()?, "35000000");
    assert!(events(&done).iter().any(|(n, d)| n == "intents_withdrawn" && d["amount"] == "35000000"));
    assert_eq!(f.balance(&f.usdt, f.circle.contract.id()).await?, before + 35 * CREDIT);
    assert_eq!(mt_balance(&f, &circle_id).await?, "0");
    assert_eq!(f.circle.view("intents_balance", json!({})).await?.as_str(), Some("0"));

    // 5. gpu claims its 35 credit in USDT
    let gpu_before = f.balance(&f.usdt, gpu.id()).await?;
    let claimed = gpu
        .call(f.circle.contract.id(), "claim")
        .args_json(json!({"epoch": 1, "destination": "near"}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?;
    assert!(claimed.is_success(), "claim: {:?}", failure_text(&claimed));
    assert_eq!(f.balance(&f.usdt, gpu.id()).await?, gpu_before + 35 * CREDIT);
    let position: Value = f.circle.view("net_position", json!({"epoch": 1, "account_id": common::circle::account_of("gpu")})).await?;
    assert_eq!(position["status"], "claimed");
    Ok(())
}
