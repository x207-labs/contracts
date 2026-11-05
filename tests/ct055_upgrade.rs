//! CT-055 in near-sandbox: upgrade by the owner through the 24 hour timelock. The "new" code is the same wasm, which is
//! enough to show the deploy, `migrate` and that the state survives. Fast-forwarding 24 hours of block time takes
//! over half an hour in the sandbox, so after the early refusal the test moves the stored `eta` into the past with
//! `patch_state`; that `eta` is proposal time + 24 hours is checked from the event and in the unit tests. Needs `cargo near build non-reproducible-wasm`.

mod common;

use common::circle::{events, failure_text};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use serde_json::json;
use sha2::{Digest, Sha256};

const DAY_S: u64 = 24 * 3600;
const SEC: u64 = 1_000_000_000;

#[tokio::test]
async fn upgrade_waits_for_the_timelock_and_keeps_the_state() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let wasm = std::fs::read("target/near/x207_circle.wasm")?;
    let hash = near_sdk::bs58::encode(Sha256::digest(&wasm)).into_string();
    let dao = f.account("dao").await?;
    let outsider = f.account("outsider").await?;
    let params_before = f.circle.view("get_params", json!({})).await?;
    let owner_before = f.circle.view("get_owner", json!({})).await?;

    let apply = |who: near_workspaces::Account, code: Vec<u8>| {
        let contract = f.circle.contract.id().clone();
        async move {
            who.call(&contract, "apply_upgrade")
                .args(code)
                .gas(Gas::from_tgas(300))
                .deposit(NearToken::from_yoctonear(0))
                .transact()
                .await
        }
    };

    // nothing pending, and not the owner
    let none = apply(dao.clone(), wasm.clone()).await?;
    assert!(failure_text(&none)
        .expect("refused")
        .contains("ERR_NO_UPGRADE"));
    let propose_by_outsider = f
        .circle
        .call_as(&outsider, "propose_upgrade", json!({"code_hash": hash}))
        .await?;
    assert!(failure_text(&propose_by_outsider)
        .expect("refused")
        .contains("ERR_NOT_OWNER"));

    let propose = f
        .circle
        .call_as(&dao, "propose_upgrade", json!({"code_hash": hash}))
        .await?;
    assert!(propose.is_success(), "{:?}", failure_text(&propose));
    let eta = events(&propose)
        .into_iter()
        .find(|(n, _)| n == "upgrade_proposed")
        .expect("upgrade_proposed")
        .1["eta"]
        .as_str()
        .unwrap()
        .parse::<u64>()?;
    assert_eq!(
        f.circle.view("get_upgrade", json!({})).await?["code_hash"],
        hash.as_str()
    );

    // too early
    let early = apply(dao.clone(), wasm.clone()).await?;
    assert!(failure_text(&early)
        .expect("refused")
        .contains("ERR_TIMELOCK_ACTIVE"));
    let by_outsider = apply(outsider.clone(), wasm.clone()).await?;
    assert!(failure_text(&by_outsider)
        .expect("refused")
        .contains("ERR_NOT_OWNER"));

    let proposed_at = f.circle.now_ns().await?;
    assert!(
        eta > proposed_at
            && eta - proposed_at <= DAY_S * SEC
            && eta - proposed_at > DAY_S * SEC - 120 * SEC,
        "eta is proposal time plus 24 hours"
    );
    // the timelock has run: stored state is `code_hash` (32 bytes) then `eta` (u64 little endian)
    let mut stored = Sha256::digest(&wasm).to_vec();
    stored.extend_from_slice(&1u64.to_le_bytes());
    f.circle
        .worker
        .patch_state(f.circle.contract.id(), b"x207_upgrade", &stored)
        .await?;
    f.circle.worker.fast_forward(2).await?;
    let wrong = apply(dao.clone(), b"not the proposed code".to_vec()).await?;
    assert!(failure_text(&wrong)
        .expect("refused")
        .contains("ERR_CODE_HASH_MISMATCH"));

    let applied = apply(dao.clone(), wasm.clone()).await?;
    assert!(applied.is_success(), "{:?}", failure_text(&applied));
    assert!(events(&applied)
        .iter()
        .any(|(n, d)| n == "upgrade_applied" && d["code_hash"] == hash.as_str()));
    assert_eq!(
        f.circle.view("get_upgrade", json!({})).await?,
        serde_json::Value::Null,
        "the proposal is consumed"
    );
    assert_eq!(f.circle.view("get_params", json!({})).await?, params_before);
    assert_eq!(f.circle.view("get_owner", json!({})).await?, owner_before);
    f.assert_d6().await?;
    Ok(())
}
