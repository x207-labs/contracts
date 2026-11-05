//! CT-071 in near-sandbox (F8, spec 15.3): `revoke_grant`. Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{events, failure_text, CREDIT};
use common::flow::finalize;
use common::funded::Funded;
use serde_json::{json, Value};

const DAY_NS: u64 = 86_400_000_000_000;

fn input(expires_at: u64, operator: Option<&str>) -> Value {
    json!({
        "asset_type": "near-rpc:access", "delivery_method": "near_access_key",
        "limits": {"scope": "{\"receiver_id\":\"api.near\"}", "quota": null, "expires_at": expires_at.to_string()},
        "unit": "day", "unit_price": "5", "operator": operator, "proof_hash": null, "proof_kind": null
    })
}

async fn setup(names: &[&str]) -> anyhow::Result<Funded> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in names {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    let dao = f.account("dao").await?;
    let entry = json!({"asset_type": "near-rpc:access", "class": "B", "delivery_methods": ["near_access_key"], "accepted_proofs": [], "terms_hash": "cd".repeat(32)});
    assert!(f
        .circle
        .call_as(
            &dao,
            "set_asset_class",
            json!({"asset_type": "near-rpc:access", "entry": entry})
        )
        .await?
        .is_success());
    Ok(f)
}

async fn status(f: &Funded, id: u64) -> anyhow::Result<String> {
    Ok(
        f.circle.view("get_grant", json!({"grant_id": id})).await?["status"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

// TC-CT-071-01..06: the grantor, the grantee and the operator, each with its own reason; everybody else is refused
#[tokio::test]
async fn the_parties_revoke_with_their_own_reason() -> anyhow::Result<()> {
    let mut f = setup(&["scraper", "translator", "summarizer"]).await?;
    let (scraper, translator, summarizer) = (
        f.accounts["scraper"].clone(),
        f.account("translator").await?,
        f.account("summarizer").await?,
    );
    let expires = f.circle.now_ns().await? + 30 * DAY_NS;
    for _ in 0..3 {
        let offer = f
            .circle
            .call_as(
                &scraper,
                "offer_grant",
                json!({"input": input(expires, Some(summarizer.id().as_str()))}),
            )
            .await?;
        assert!(offer.is_success(), "{:?}", failure_text(&offer));
    }
    assert!(f
        .circle
        .call_as(
            &translator,
            "accept_grant",
            json!({"grant_id": 1, "delivery_pubkey": null})
        )
        .await?
        .is_success());

    // an outsider, and a party with the wrong reason, are refused and nothing changes
    let outsider = f
        .circle
        .call_as(
            &translator,
            "revoke_grant",
            json!({"grant_id": 2, "reason": "grantee"}),
        )
        .await?;
    assert!(failure_text(&outsider)
        .expect("refused")
        .contains("ERR_GRANT_REVOKE_DENIED"));
    let wrong = f
        .circle
        .call_as(
            &scraper,
            "revoke_grant",
            json!({"grant_id": 1, "reason": "operator"}),
        )
        .await?;
    assert!(failure_text(&wrong)
        .expect("refused")
        .contains("ERR_GRANT_REVOKE_DENIED"));
    let early_expired = f
        .circle
        .call_as(
            &summarizer,
            "revoke_grant",
            json!({"grant_id": 1, "reason": "expired"}),
        )
        .await?;
    assert!(failure_text(&early_expired)
        .expect("refused")
        .contains("ERR_GRANT_REVOKE_DENIED"));
    assert_eq!(status(&f, 1).await?, "active");

    // the grantee revokes an active grant
    let by_grantee = f
        .circle
        .call_as(
            &translator,
            "revoke_grant",
            json!({"grant_id": 1, "reason": "grantee"}),
        )
        .await?;
    assert!(by_grantee.is_success(), "{:?}", failure_text(&by_grantee));
    assert!(events(&by_grantee)
        .iter()
        .any(|(n, d)| n == "grant_revoked" && d["grant_id"] == 1 && d["reason"] == "grantee"));
    assert_eq!(status(&f, 1).await?, "revoked");
    // final: not again, not by anybody
    let twice = f
        .circle
        .call_as(
            &scraper,
            "revoke_grant",
            json!({"grant_id": 1, "reason": "grantor"}),
        )
        .await?;
    assert!(failure_text(&twice)
        .expect("refused")
        .contains("ERR_GRANT_FINAL"));

    // the grantor revokes an offer that nobody accepted; the operator revokes another
    assert!(f
        .circle
        .call_as(
            &scraper,
            "revoke_grant",
            json!({"grant_id": 2, "reason": "grantor"})
        )
        .await?
        .is_success());
    assert!(f
        .circle
        .call_as(
            &summarizer,
            "revoke_grant",
            json!({"grant_id": 3, "reason": "operator"})
        )
        .await?
        .is_success());
    assert_eq!(
        (status(&f, 2).await?, status(&f, 3).await?),
        ("revoked".to_string(), "revoked".to_string())
    );
    let unknown = f
        .circle
        .call_as(
            &scraper,
            "revoke_grant",
            json!({"grant_id": 9, "reason": "grantor"}),
        )
        .await?;
    assert!(failure_text(&unknown)
        .expect("refused")
        .contains("ERR_UNKNOWN_GRANT"));

    // a revoked grant frees a slot of the grantor
    assert!(f
        .circle
        .call_as(
            &scraper,
            "offer_grant",
            json!({"input": input(expires, None)})
        )
        .await?
        .is_success());
    Ok(())
}

// TC-CT-071-07: after expires_at anybody sets it to expired
#[tokio::test]
async fn anybody_expires_a_grant_after_its_time() -> anyhow::Result<()> {
    let mut f = setup(&["scraper", "translator", "summarizer"]).await?;
    let (scraper, translator, summarizer) = (
        f.accounts["scraper"].clone(),
        f.account("translator").await?,
        f.account("summarizer").await?,
    );
    let expires = f.circle.now_ns().await? + 60_000_000_000;
    assert!(f
        .circle
        .call_as(
            &scraper,
            "offer_grant",
            json!({"input": input(expires, None)})
        )
        .await?
        .is_success());
    assert!(f
        .circle
        .call_as(
            &translator,
            "accept_grant",
            json!({"grant_id": 1, "delivery_pubkey": null})
        )
        .await?
        .is_success());
    f.circle.advance_to(expires + 1_000_000_000).await?;
    let done = f
        .circle
        .call_as(
            &summarizer,
            "revoke_grant",
            json!({"grant_id": 1, "reason": "expired"}),
        )
        .await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert!(events(&done)
        .iter()
        .any(|(n, d)| n == "grant_revoked" && d["reason"] == "expired"));
    assert_eq!(status(&f, 1).await?, "expired");
    Ok(())
}

// TC-CT-071-08: once the grantee defaulted (suspended), anybody revokes; before that nobody outside can
#[tokio::test]
async fn anybody_revokes_after_the_grantee_defaulted() -> anyhow::Result<()> {
    let mut f = setup(&["d1", "c1", "g1"]).await?;
    let (g1, d1, c1) = (
        f.accounts["g1"].clone(),
        f.accounts["d1"].clone(),
        f.accounts["c1"].clone(),
    );
    let expires = f.circle.now_ns().await? + 30 * DAY_NS;
    assert!(f
        .circle
        .call_as(&g1, "offer_grant", json!({"input": input(expires, None)}))
        .await?
        .is_success());
    assert!(f
        .circle
        .call_as(
            &d1,
            "accept_grant",
            json!({"grant_id": 1, "delivery_pubkey": null})
        )
        .await?
        .is_success());
    let early = f
        .circle
        .call_as(
            &c1,
            "revoke_grant",
            json!({"grant_id": 1, "reason": "defaulted"}),
        )
        .await?;
    assert!(failure_text(&early)
        .expect("refused")
        .contains("ERR_GRANT_REVOKE_DENIED"));

    // d1 owes c1 30 and never pays: the epoch finalizes with d1 defaulted and suspended
    let r = f.circle.register("d1", "c1", 1, 30 * CREDIT).await?;
    assert!(r.is_success(), "{:?}", failure_text(&r));
    let c = &f.circle;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c
        .call_keeper("close_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?)
        .await?;
    assert!(c
        .call_keeper("commit_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    assert!(finalize(&f.circle).await?.is_success());
    assert_eq!(f.member("d1").await?["status"], "suspended");

    let done = f
        .circle
        .call_as(
            &c1,
            "revoke_grant",
            json!({"grant_id": 1, "reason": "defaulted"}),
        )
        .await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert_eq!(status(&f, 1).await?, "revoked");
    Ok(())
}
