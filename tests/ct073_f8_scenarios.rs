//! CT-073 in near-sandbox (F8): grants of two members in opposite directions are paid for with vouchers (98 against 50
//! credit); after `commit_epoch` the difference of 48 credit stays an obligation in D2, and revoking after
//! `finalize_epoch` leaves D2 and D5 as they were. Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::flow::finalize;
use common::funded::Funded;
use serde_json::{json, Value};

const DAY_NS: u64 = 86_400_000_000_000;

fn input(asset_type: &str, price: u128, expires_at: u64) -> Value {
    json!({
        "asset_type": asset_type, "delivery_method": "near_access_key",
        "limits": {"scope": "{\"receiver_id\":\"api.near\"}", "quota": "1", "expires_at": expires_at.to_string()},
        "unit": "once", "unit_price": price.to_string(), "operator": null, "proof_hash": null, "proof_kind": null
    })
}

#[tokio::test]
async fn opposite_grants_net_to_48_credit_and_revoking_after_finalize_changes_nothing_else(
) -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in ["alice", "bob", "carol"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 150 * CREDIT).await?.is_success());
    }
    let (alice, bob, carol, dao) = (
        f.accounts["alice"].clone(),
        f.accounts["bob"].clone(),
        f.accounts["carol"].clone(),
        f.account("dao").await?,
    );
    let entry = |t: &str, class: &str| json!({"asset_type": t, "class": class, "delivery_methods": ["near_access_key"], "accepted_proofs": [], "terms_hash": "11".repeat(32)});
    for (t, c) in [
        ("rpc:alice-api", "B"),
        ("rpc:bob-api", "B"),
        ("pay:nft", "X"),
    ] {
        assert!(f
            .circle
            .call_as(
                &dao,
                "set_asset_class",
                json!({"asset_type": t, "entry": entry(t, c)})
            )
            .await?
            .is_success());
    }
    let expires = f.circle.now_ns().await? + 30 * DAY_NS;

    // refusals of F8 that must not leave a grant behind: class X, an unregistered type, a pending member
    let x = f
        .circle
        .call_as(
            &alice,
            "offer_grant",
            json!({"input": input("pay:nft", 1, expires)}),
        )
        .await?;
    assert!(failure_text(&x)
        .expect("refused")
        .contains("ERR_ASSET_CLASS_FORBIDDEN"));
    let unknown = f
        .circle
        .call_as(
            &alice,
            "offer_grant",
            json!({"input": input("rpc:nothing", 1, expires)}),
        )
        .await?;
    assert!(failure_text(&unknown)
        .expect("refused")
        .contains("ERR_ASSET_TYPE_UNKNOWN"));
    assert_eq!(
        f.circle.view("get_grant", json!({"grant_id": 1})).await?,
        Value::Null
    );

    // alice grants bob access for 98, bob grants alice access for 50; both accept
    assert_eq!(
        f.circle
            .call_as(
                &alice,
                "offer_grant",
                json!({"input": input("rpc:alice-api", 98 * CREDIT, expires)})
            )
            .await?
            .json::<u64>()?,
        1
    );
    assert_eq!(
        f.circle
            .call_as(
                &bob,
                "offer_grant",
                json!({"input": input("rpc:bob-api", 50 * CREDIT, expires)})
            )
            .await?
            .json::<u64>()?,
        2
    );
    let own = f
        .circle
        .call_as(
            &alice,
            "accept_grant",
            json!({"grant_id": 1, "delivery_pubkey": null}),
        )
        .await?;
    assert!(failure_text(&own)
        .expect("refused")
        .contains("ERR_GRANT_SELF"));
    assert!(f
        .circle
        .call_as(
            &bob,
            "accept_grant",
            json!({"grant_id": 1, "delivery_pubkey": null})
        )
        .await?
        .is_success());
    assert!(f
        .circle
        .call_as(
            &alice,
            "accept_grant",
            json!({"grant_id": 2, "delivery_pubkey": null})
        )
        .await?
        .is_success());
    // carol cannot revoke what is not hers while it runs
    let stranger = f
        .circle
        .call_as(
            &carol,
            "revoke_grant",
            json!({"grant_id": 1, "reason": "grantee"}),
        )
        .await?;
    assert!(failure_text(&stranger)
        .expect("refused")
        .contains("ERR_GRANT_REVOKE_DENIED"));

    // payment by voucher in both directions; the cycle clears 50 and leaves 48 from bob to alice
    for (payer, payee, amount) in [("bob", "alice", 98), ("alice", "bob", 50)] {
        let r = f.circle.register(payer, payee, 1, amount * CREDIT).await?;
        assert!(r.is_success(), "{payer}->{payee}: {:?}", failure_text(&r));
    }
    let c = &f.circle;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c
        .call_keeper("close_epoch", json!({"epoch": 1}))
        .await?
        .is_success());
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?)
        .await?;
    let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
    assert!(commit.is_success(), "{:?}", failure_text(&commit));
    let bob_pos = f
        .circle
        .view(
            "net_position",
            json!({"epoch": 1, "account_id": account_of("bob")}),
        )
        .await?;
    let alice_pos = f
        .circle
        .view(
            "net_position",
            json!({"epoch": 1, "account_id": account_of("alice")}),
        )
        .await?;
    assert_eq!(
        (
            bob_pos["role"].as_str(),
            bob_pos["status"].as_str(),
            bob_pos["amount"].as_str()
        ),
        (Some("debtor"), Some("due"), Some("48000000"))
    );
    assert_eq!(
        (
            alice_pos["role"].as_str(),
            alice_pos["status"].as_str(),
            alice_pos["amount"].as_str()
        ),
        (Some("creditor"), Some("claimable"), Some("48000000"))
    );
    assert_eq!(
        f.circle
            .view("outstanding_debt", json!({"account_id": account_of("bob")}))
            .await?,
        "48000000"
    );

    // the grants live on through the clearing; bob does not pay, the epoch finalizes
    assert_eq!(
        f.circle.view("get_grant", json!({"grant_id": 1})).await?["status"],
        "active"
    );
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    let before = (
        f.circle.view("get_pool", json!({})).await?,
        f.circle
            .view(
                "net_position",
                json!({"epoch": 1, "account_id": account_of("alice")}),
            )
            .await?,
    );

    // revoking after finalize_epoch: the grantor can, and so can anybody for the grant whose grantee (bob) defaulted
    let by_grantor = f
        .circle
        .call_as(
            &bob,
            "revoke_grant",
            json!({"grant_id": 2, "reason": "grantor"}),
        )
        .await?;
    assert!(by_grantor.is_success(), "{:?}", failure_text(&by_grantor));
    let by_anyone = f
        .circle
        .call_as(
            &carol,
            "revoke_grant",
            json!({"grant_id": 1, "reason": "defaulted"}),
        )
        .await?;
    assert!(by_anyone.is_success(), "{:?}", failure_text(&by_anyone));
    assert!(events(&by_anyone)
        .iter()
        .any(|(n, d)| n == "grant_revoked" && d["reason"] == "defaulted"));
    let twice = f
        .circle
        .call_as(
            &alice,
            "revoke_grant",
            json!({"grant_id": 1, "reason": "grantor"}),
        )
        .await?;
    assert!(failure_text(&twice)
        .expect("refused")
        .contains("ERR_GRANT_FINAL"));
    let after = (
        f.circle.view("get_pool", json!({})).await?,
        f.circle
            .view(
                "net_position",
                json!({"epoch": 1, "account_id": account_of("alice")}),
            )
            .await?,
    );
    assert_eq!(
        before, after,
        "revoking touches neither the pool nor the positions"
    );
    f.assert_d6().await?;
    Ok(())
}
