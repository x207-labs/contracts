//! CT-086 in near-sandbox: gas of the calls that grow with the size of their input or of a list, to set limits (security review, "Not checked").
//! Table: `cargo test --test ct086_gas_limits -- --nocapture --test-threads=1 | grep GAS`. Needs `cargo near build non-reproducible-wasm` first.
//! Views are called as transactions here, because a transaction reports the gas it burnt and a view does not.

mod common;

use common::circle::{failure_text, CREDIT};
use common::funded::Funded;
use near_workspaces::result::ExecutionFinalResult;
use serde_json::json;

fn tgas(result: &ExecutionFinalResult) -> f64 {
    result.total_gas_burnt.as_gas() as f64 / 1e12
}

/// The part of a failure text that names the cause.
fn reason(text: &str) -> String {
    let at = [
        "ERR_",
        "Exceeded",
        "GasLimit",
        "Memory",
        "Smart contract panicked",
        "HostError",
        "FunctionCallError",
    ]
    .iter()
    .filter_map(|k| text.find(k))
    .min()
    .unwrap_or(0);
    text[at..].chars().take(140).collect()
}

fn name(i: usize) -> String {
    format!("m{i:02}")
}

async fn members(f: &mut Funded, n: usize) -> anyhow::Result<()> {
    for i in 0..n {
        let n = name(i);
        let account = f.account(&n).await?;
        assert!(f.join(&n, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    Ok(())
}

async fn commit(f: &Funded, legs: &[(usize, usize)]) -> anyhow::Result<ExecutionFinalResult> {
    let names: Vec<(String, String)> = legs.iter().map(|(a, b)| (name(*a), name(*b))).collect();
    let spec: Vec<(&str, &str, u128)> = names
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str(), 1u128))
        .collect();
    let mut who: Vec<String> = Vec::new();
    for (a, b) in legs {
        for i in [a, b] {
            if !who.contains(&name(*i)) {
                who.push(name(*i));
            }
        }
    }
    let signers: Vec<&str> = who.iter().map(String::as_str).collect();
    f.circle.commit_bundle(1, &spec, &signers).await
}

// TC-CT-086-01: a bundle that is one cycle through n members
#[tokio::test]
async fn commit_bundle_gas_for_a_cycle_of_n_members() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    members(&mut f, 64).await?;
    for n in [2usize, 4, 8, 16, 32, 64] {
        let legs: Vec<(usize, usize)> = (0..n).map(|i| (i, (i + 1) % n)).collect();
        let result = commit(&f, &legs).await?;
        match failure_text(&result) {
            None => println!(
                "GAS commit_bundle cycle participants={n} legs={n}: {:.1} Tgas",
                tgas(&result)
            ),
            Some(text) => println!(
                "GAS commit_bundle cycle participants={n} legs={n}: REFUSED after {:.1} Tgas: {}",
                tgas(&result),
                reason(&text)
            ),
        }
    }
    Ok(())
}

// TC-CT-086-02: a dense bundle, every ordered pair of k members is a leg
#[tokio::test]
async fn commit_bundle_gas_for_a_dense_bundle() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    members(&mut f, 24).await?;
    for k in [16usize, 20, 24] {
        let legs: Vec<(usize, usize)> = (0..k)
            .flat_map(|a| (0..k).filter(move |b| *b != a).map(move |b| (a, b)))
            .collect();
        let result = commit(&f, &legs).await?;
        match failure_text(&result) {
            None => println!(
                "GAS commit_bundle dense participants={k} legs={}: {:.1} Tgas",
                legs.len(),
                tgas(&result)
            ),
            Some(text) => println!(
                "GAS commit_bundle dense participants={k} legs={}: REFUSED after {:.1} Tgas: {}",
                legs.len(),
                tgas(&result),
                reason(&text)
            ),
        }
    }
    Ok(())
}

// TC-CT-086-03: grants_of with a long list for one grantee
#[tokio::test]
async fn grants_of_gas_for_a_long_list() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    members(&mut f, 6).await?;
    let dao = f.account("dao").await?;
    let entry = json!({"asset_type": "near-rpc:access", "class": "B", "delivery_methods": ["near_access_key"], "accepted_proofs": [], "terms_hash": "ab".repeat(32)});
    assert!(f
        .circle
        .call_as(
            &dao,
            "set_asset_class",
            json!({"asset_type": "near-rpc:access", "entry": entry})
        )
        .await?
        .is_success());
    let expires = f.circle.now_ns().await? + 3_600_000_000_000;
    let input = json!({"input": {"asset_type": "near-rpc:access", "delivery_method": "near_access_key",
        "limits": {"scope": "{\"receiver_id\":\"api.near\",\"method_names\":[\"get\"],\"allowance\":\"1\"}", "quota": null, "expires_at": expires.to_string()},
        "unit": "call", "unit_price": "1", "operator": null, "proof_hash": null, "proof_kind": null}});
    let grantee = f.account(&name(5)).await?;
    let mut id = 0u64;
    for g in 0..5 {
        let grantor = f.account(&name(g)).await?;
        for _ in 0..16 {
            assert!(f
                .circle
                .call_as(&grantor, "offer_grant", input.clone())
                .await?
                .is_success());
            id += 1;
            let accept = f
                .circle
                .call_as(
                    &grantee,
                    "accept_grant",
                    json!({"grant_id": id, "delivery_pubkey": null}),
                )
                .await?;
            assert!(accept.is_success(), "{:?}", failure_text(&accept));
        }
        let held = f.circle.call_as(&grantee, "grants_of", json!({"account_id": grantee.id(), "role": "grantee", "from_index": 0, "limit": 100})).await?;
        println!(
            "GAS grants_of grantee list={} page=100: {:.1} Tgas",
            (g + 1) * 16,
            tgas(&held)
        );
        let page = f.circle.call_as(&grantee, "grants_of", json!({"account_id": grantee.id(), "role": "grantee", "from_index": 0, "limit": 10})).await?;
        println!(
            "GAS grants_of grantee list={} page=10: {:.1} Tgas",
            (g + 1) * 16,
            tgas(&page)
        );
    }
    Ok(())
}

// TC-CT-086-04: list_asset_classes with a long registry
#[tokio::test]
async fn list_asset_classes_gas_for_a_long_registry() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let dao = f.account("dao").await?;
    let anyone = f.account("anyone").await?;
    for n in 1..=200usize {
        let t = format!("prov{n}:kind");
        let entry = json!({"asset_type": t, "class": "B", "delivery_methods": ["near_access_key"], "accepted_proofs": [], "terms_hash": "ab".repeat(32)});
        assert!(f
            .circle
            .call_as(
                &dao,
                "set_asset_class",
                json!({"asset_type": t, "entry": entry})
            )
            .await?
            .is_success());
        if [10, 50, 100, 200].contains(&n) {
            let all = f
                .circle
                .call_as(
                    &anyone,
                    "list_asset_classes",
                    json!({"from_index": 0, "limit": 1000}),
                )
                .await?;
            let page = f
                .circle
                .call_as(
                    &anyone,
                    "list_asset_classes",
                    json!({"from_index": 0, "limit": 20}),
                )
                .await?;
            println!("GAS list_asset_classes registry={n} limit=1000: {:.1} Tgas ({}), limit=20: {:.1} Tgas", tgas(&all), if all.is_success() { "ok" } else { "FAILED" }, tgas(&page));
        }
    }
    Ok(())
}
