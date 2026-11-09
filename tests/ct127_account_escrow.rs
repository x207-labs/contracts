//! CT-127 in near-sandbox (0.0.18, P1): the sale of a NEAR account through the locker contract. The threat table of `docs/adr-0.0.18.md`:
//! a leftover key, a fake locker, a refused registration, a wrong caller, a cancel, and the wait of one epoch for a member account (#14).
//! Needs `cargo near build non-reproducible-wasm` first; the locker wasm is built on first use.

mod common;

use common::circle::{events, failure_text, CREDIT};
use common::flow::idle_epoch_finalized;
use common::funded::Funded;
use near_workspaces::types::{Gas, KeyType, PublicKey, SecretKey};
use near_workspaces::{Account, AccountId};
use serde_json::{json, Value};

const PRICE: u128 = 10 * CREDIT;
const HOUR_NS: u64 = 3_600_000_000_000;

fn locker_wasm() -> Vec<u8> {
    static BUILD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let wasm = "target/near/x207_locker/x207_locker.wasm";
    let _guard = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    if !std::path::Path::new(wasm).exists() {
        let status = std::process::Command::new("cargo")
            .args([
                "near",
                "build",
                "non-reproducible-wasm",
                "--manifest-path",
                "locker/Cargo.toml",
            ])
            .status()
            .expect("run cargo near build");
        assert!(status.success(), "cargo near build failed for the locker");
    }
    std::fs::read(wasm).expect("locker wasm")
}

fn fresh_key() -> (SecretKey, PublicKey) {
    let secret = SecretKey::from_random(KeyType::ED25519);
    let public = secret.public_key();
    (secret, public)
}

struct World {
    f: Funded,
    alice: Account,
    bob: Account,
}

async fn world() -> anyhow::Result<World> {
    let mut f = Funded::deploy(None).await?;
    for name in ["alice", "bob", "carol"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 150 * CREDIT).await?.is_success());
    }
    let dao = f.account("dao").await?;
    for asset_type in ["near:account", "near:member_account"] {
        let entry = json!({"asset_type": asset_type, "class": "A", "delivery_methods": ["escrow_account"], "accepted_proofs": [], "terms_hash": "5a".repeat(32)});
        let set = f
            .circle
            .call_as(
                &dao,
                "set_asset_class",
                json!({"asset_type": asset_type, "entry": entry}),
            )
            .await?;
        assert!(set.is_success(), "{:?}", failure_text(&set));
    }
    let (alice, bob) = (f.accounts["alice"].clone(), f.accounts["bob"].clone());
    Ok(World { f, alice, bob })
}

impl World {
    async fn offer(
        &mut self,
        asset_type: &str,
        sold: &AccountId,
        expires_in_ns: u64,
    ) -> anyhow::Result<u64> {
        let expires = self.f.circle.now_ns().await? + expires_in_ns;
        let alice = self.alice.clone();
        let result = self.f.circle.call_as(&alice, "offer_grant", json!({"input": {
            "asset_type": asset_type, "delivery_method": "escrow_account",
            "limits": {"scope": json!({"account_id": sold}).to_string(), "quota": null, "expires_at": expires.to_string()},
            "unit": "once", "unit_price": PRICE.to_string(), "operator": null, "proof_hash": null, "proof_kind": null
        }})).await?;
        assert!(result.is_success(), "{:?}", failure_text(&result));
        Ok(result.json::<u64>()?)
    }

    /// The deploy transaction of the seller: the locker code, `new`, and the deletion of the given keys, in one batch of the sold account.
    async fn lock(
        &self,
        sold: &Account,
        grant_id: u64,
        return_key: &PublicKey,
        delete: &[PublicKey],
    ) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        let mut batch = sold.batch(sold.id()).deploy(&locker_wasm()).call(
            near_workspaces::operations::Function::new("new")
                .args_json(json!({
                    "circle": self.f.circle.contract.id(),
                    "grant_id": grant_id,
                    "return_pubkey": return_key.to_string(),
                }))
                .gas(Gas::from_tgas(100)),
        );
        for key in delete {
            batch = batch.delete_key(key.clone());
        }
        Ok(batch.transact().await?)
    }

    async fn keys(&self, id: &AccountId) -> anyhow::Result<Vec<String>> {
        let keys = self.f.circle.worker.view_access_keys(id).await?;
        Ok(keys.into_iter().map(|k| k.public_key.to_string()).collect())
    }

    async fn locker_state(&self, id: &AccountId) -> anyhow::Result<String> {
        let view: Value = self
            .f
            .circle
            .worker
            .view(id, "get_state")
            .await?
            .json::<Value>()?;
        Ok(view["state"].as_str().unwrap_or_default().to_string())
    }

    async fn escrow_state(&self, grant_id: u64) -> anyhow::Result<Value> {
        self.f
            .circle
            .view("get_account_escrow", json!({"grant_id": grant_id}))
            .await
    }

    async fn buy(
        &mut self,
        grant_id: u64,
        delivery: &PublicKey,
    ) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        let epoch = self.f.circle.view("get_epoch", json!({})).await?["epoch"]
            .as_u64()
            .unwrap();
        let (voucher, signature) = self.f.circle.next_voucher("bob", "alice", epoch, PRICE);
        let bob = self.bob.clone();
        self.f
            .circle
            .call_as(
                &bob,
                "complete_account_escrow",
                json!({"grant_id": grant_id, "voucher": voucher, "signature": signature, "delivery_pubkey": delivery.to_string()}),
            )
            .await
    }
}

// TC-CT-127-01 (#11): lock, pay, release: the buyer holds the only key, the seller is paid, the locker is final
#[tokio::test]
async fn the_buyer_pays_and_gets_the_only_key() -> anyhow::Result<()> {
    let mut w = world().await?;
    let sold = w.f.account("sold").await?;
    let sold_id = sold.id().clone();
    let (_, seller_key) = fresh_key();
    let (_, buyer_key) = fresh_key();
    let id = w.offer("near:account", &sold_id, 24 * HOUR_NS).await?;

    let lock = w
        .lock(&sold, id, &seller_key, &[sold.secret_key().public_key()])
        .await?;
    assert!(lock.is_success(), "{:?}", failure_text(&lock));
    assert!(w.keys(&sold_id).await?.is_empty(), "no key is left");
    assert_eq!(w.locker_state(&sold_id).await?, "held");
    let held = w.escrow_state(id).await?;
    assert_eq!(held["state"], "held");
    assert_eq!(held["account_id"], sold_id.as_str());

    let done = w.buy(id, &buyer_key).await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert!(events(&done)
        .iter()
        .any(|(n, d)| n == "grant_activated" && d["grant_id"] == id));
    assert_eq!(w.locker_state(&sold_id).await?, "released");
    assert_eq!(w.keys(&sold_id).await?, vec![buyer_key.to_string()]);
    assert_eq!(w.escrow_state(id).await?["state"], "delivered");

    // a second sale of the same grant or a late `give_back` finds a final locker and changes nothing
    let again = w.buy(id, &buyer_key).await?;
    assert!(failure_text(&again).is_some());
    assert_eq!(w.keys(&sold_id).await?, vec![buyer_key.to_string()]);
    Ok(())
}

// TC-CT-127-02 (threat: a leftover key): a lock that leaves a key behind is visible to the pre-pay check of the SDK
#[tokio::test]
async fn a_leftover_key_shows_in_the_key_list() -> anyhow::Result<()> {
    let mut w = world().await?;
    let sold = w.f.account("sold").await?;
    let sold_id = sold.id().clone();
    let (_, seller_key) = fresh_key();
    let id = w.offer("near:account", &sold_id, 24 * HOUR_NS).await?;
    // the seller deploys the locker and keeps its own key
    let lock = w.lock(&sold, id, &seller_key, &[]).await?;
    assert!(lock.is_success(), "{:?}", failure_text(&lock));
    assert_eq!(w.locker_state(&sold_id).await?, "held");
    let keys = w.keys(&sold_id).await?;
    assert_eq!(
        keys,
        vec![sold.secret_key().public_key().to_string()],
        "the leftover full access key is listed, so the SDK refuses to pay"
    );
    Ok(())
}

// TC-CT-127-03 (threat: a refused registration): the circle refuses the lock, the locker gives the account back to the seller
#[tokio::test]
async fn a_refused_registration_returns_the_key() -> anyhow::Result<()> {
    let mut w = world().await?;
    let sold = w.f.account("sold").await?;
    let sold_id = sold.id().clone();
    let (_, seller_key) = fresh_key();
    // grant 999 does not exist
    let lock = w
        .lock(&sold, 999, &seller_key, &[sold.secret_key().public_key()])
        .await?;
    assert!(lock.is_success(), "the deploy itself succeeds");
    assert_eq!(w.locker_state(&sold_id).await?, "returned");
    assert_eq!(w.keys(&sold_id).await?, vec![seller_key.to_string()]);
    assert!(w.escrow_state(999).await?.is_null());
    Ok(())
}

// TC-CT-129-03 (CT-129): a grant that runs longer than the limit (default 72 hours) is refused at the lock with `ERR_ESCROW_TTL_TOO_LONG`, the locker gives
// the account back, and the same account locks with a grant inside the limit
#[tokio::test]
async fn a_grant_longer_than_the_escrow_ttl_is_refused_at_the_lock() -> anyhow::Result<()> {
    let mut w = world().await?;
    let ttl: Value = w.f.circle.view("get_max_escrow_ttl", json!({})).await?;
    assert_eq!(ttl.as_str(), Some("259200000000000"), "default 72 hours");
    let sold = w.f.account("sold").await?;
    let sold_id = sold.id().clone();
    let (_, seller_key) = fresh_key();
    let long = w.offer("near:account", &sold_id, 80 * HOUR_NS).await?;
    let lock = w
        .lock(&sold, long, &seller_key, &[sold.secret_key().public_key()])
        .await?;
    assert!(lock.is_success(), "the deploy itself succeeds");
    assert_eq!(w.locker_state(&sold_id).await?, "returned");
    assert_eq!(w.keys(&sold_id).await?, vec![seller_key.to_string()]);
    assert!(w.escrow_state(long).await?.is_null());
    let logs = format!("{:?}", lock.logs());
    let receipts = format!("{:?}", lock.outcomes());
    assert!(
        logs.contains("ERR_ESCROW_TTL_TOO_LONG") || receipts.contains("ERR_ESCROW_TTL_TOO_LONG"),
        "the circle names the limit: {logs} {receipts}"
    );
    Ok(())
}

// TC-CT-127-04 (threat: a fake circle or a wrong caller): only the circle moves the account; the keys stay out
#[tokio::test]
async fn only_the_circle_can_release_or_give_back() -> anyhow::Result<()> {
    let mut w = world().await?;
    let sold = w.f.account("sold").await?;
    let sold_id = sold.id().clone();
    let (_, seller_key) = fresh_key();
    let (_, thief_key) = fresh_key();
    let id = w.offer("near:account", &sold_id, 24 * HOUR_NS).await?;
    let lock = w
        .lock(&sold, id, &seller_key, &[sold.secret_key().public_key()])
        .await?;
    assert!(lock.is_success(), "{:?}", failure_text(&lock));
    let carol = w.f.account("carol").await?;
    for (method, args) in [
        (
            "release",
            json!({"grant_id": id, "pubkey": thief_key.to_string()}),
        ),
        ("give_back", json!({"grant_id": id})),
    ] {
        let result = carol
            .call(&sold_id, method)
            .args_json(args)
            .gas(Gas::from_tgas(30))
            .transact()
            .await?;
        let text = failure_text(&result).unwrap_or_default();
        assert!(text.contains("ERR_LOCKER_NOT_CIRCLE"), "{method}: {text}");
    }
    // `account_on_lock` called by a plain account that is not the one named in the grant
    let wrong =
        w.f.circle
            .call_as(&carol, "account_on_lock", json!({"grant_id": id}))
            .await?;
    assert!(failure_text(&wrong)
        .unwrap_or_default()
        .contains("ERR_ESCROW_ACCOUNT_MISMATCH"));
    assert_eq!(w.locker_state(&sold_id).await?, "held");
    assert!(w.keys(&sold_id).await?.is_empty());
    Ok(())
}

// TC-CT-127-05: the grant runs out, the grantor cancels, the key goes back to the seller
#[tokio::test]
async fn cancel_after_expiry_returns_the_key() -> anyhow::Result<()> {
    let mut w = world().await?;
    let sold = w.f.account("sold").await?;
    let sold_id = sold.id().clone();
    let (_, seller_key) = fresh_key();
    let id = w.offer("near:account", &sold_id, HOUR_NS).await?;
    let lock = w
        .lock(&sold, id, &seller_key, &[sold.secret_key().public_key()])
        .await?;
    assert!(lock.is_success(), "{:?}", failure_text(&lock));
    let alice = w.alice.clone();
    let early =
        w.f.circle
            .call_as(&alice, "cancel_escrow", json!({"grant_id": id}))
            .await?;
    assert!(failure_text(&early).is_some(), "not before the expiry");
    let target = w.f.circle.now_ns().await? + 2 * HOUR_NS;
    w.f.circle.advance_to(target).await?;
    let cancel =
        w.f.circle
            .call_as(&alice, "cancel_escrow", json!({"grant_id": id}))
            .await?;
    assert!(cancel.is_success(), "{:?}", failure_text(&cancel));
    assert_eq!(w.locker_state(&sold_id).await?, "returned");
    assert_eq!(w.keys(&sold_id).await?, vec![seller_key.to_string()]);
    // after the cancel the buyer cannot pay for it
    let late = w.buy(id, &fresh_key().1).await?;
    assert!(failure_text(&late).is_some());
    assert_eq!(w.keys(&sold_id).await?, vec![seller_key.to_string()]);
    Ok(())
}

// TC-CT-127-06 (#14): a member account offered as near:account is refused at the lock and the key goes back
#[tokio::test]
async fn a_member_offered_as_a_plain_account_gets_its_key_back() -> anyhow::Result<()> {
    let mut w = world().await?;
    let dan = w.f.account("dan").await?;
    assert!(w.f.join("dan", Funded::join_deposit()).await?.is_success());
    assert!(w.f.stake(&dan, 150 * CREDIT).await?.is_success());
    let dan_id = dan.id().clone();
    let (_, seller_key) = fresh_key();
    // the wrong asset type for a member
    let wrong = {
        let expires = w.f.circle.now_ns().await? + 24 * HOUR_NS;
        let alice = w.alice.clone();
        w.f.circle.call_as(&alice, "offer_grant", json!({"input": {
            "asset_type": "near:account", "delivery_method": "escrow_account",
            "limits": {"scope": json!({"account_id": dan_id}).to_string(), "quota": null, "expires_at": expires.to_string()},
            "unit": "once", "unit_price": PRICE.to_string(), "operator": null, "proof_hash": null, "proof_kind": null
        }})).await?
    };
    assert!(wrong.is_success(), "the grant is checked at the lock");
    let wrong_id = wrong.json::<u64>()?;
    let refused = w
        .lock(
            &dan,
            wrong_id,
            &seller_key,
            &[dan.secret_key().public_key()],
        )
        .await?;
    assert!(refused.is_success());
    assert_eq!(
        w.locker_state(&dan_id).await?,
        "returned",
        "ERR_ACCOUNT_ASSET_TYPE gives the key back"
    );
    assert_eq!(w.keys(&dan_id).await?, vec![seller_key.to_string()]);
    Ok(())
}

// TC-CT-127-07 (#14): the epoch wait and the key rotation of a member account
#[tokio::test]
async fn the_member_key_is_rotated_after_the_wait() -> anyhow::Result<()> {
    let mut w = world().await?;
    // a fresh account is joined as a member; its keys are replaced by the locker
    let eve = w.f.account("eve").await?;
    assert!(w.f.join("eve", Funded::join_deposit()).await?.is_success());
    assert!(w.f.stake(&eve, 150 * CREDIT).await?.is_success());
    let eve_id = eve.id().clone();
    let (_, seller_key) = fresh_key();
    // the buyer's key is one whose signing key the fixture holds, so that its vouchers can be signed below
    let buyer_key: PublicKey = w.f.circle.key_for("neweve").parse().unwrap();
    let id = w
        .offer("near:member_account", &eve_id, 72 * HOUR_NS)
        .await?;
    let lock = w
        .lock(&eve, id, &seller_key, &[eve.secret_key().public_key()])
        .await?;
    assert!(lock.is_success(), "{:?}", failure_text(&lock));
    assert_eq!(w.escrow_state(id).await?["state"], "held");

    let same_epoch = w.buy(id, &buyer_key).await?;
    assert!(
        failure_text(&same_epoch)
            .unwrap_or_default()
            .contains("ERR_ESCROW_EPOCH_WAIT"),
        "{:?}",
        failure_text(&same_epoch)
    );
    assert!(w.keys(&eve_id).await?.is_empty());

    // while the account is held the seller's voucher key adds no debt (ERR_ACCOUNT_LOCKED), whoever submits the voucher
    let epoch_now = w.f.circle.view("get_epoch", json!({})).await?["epoch"]
        .as_u64()
        .unwrap();
    let locked =
        w.f.circle
            .register("eve", "carol", epoch_now, CREDIT)
            .await?;
    assert!(
        failure_text(&locked)
            .unwrap_or_default()
            .contains("ERR_ACCOUNT_LOCKED"),
        "{:?}",
        failure_text(&locked)
    );

    idle_epoch_finalized(&w.f).await?;
    let epoch_now = w.f.circle.view("get_epoch", json!({})).await?["epoch"]
        .as_u64()
        .unwrap();
    // signed with the old key before the sale, submitted after it
    let (old_voucher, old_signature) = w.f.circle.next_voucher("eve", "carol", epoch_now, CREDIT);
    let done = w.buy(id, &buyer_key).await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert_eq!(w.keys(&eve_id).await?, vec![buyer_key.to_string()]);
    let member = w.f.member("eve").await?;
    assert_eq!(
        member["pubkey_voucher"],
        buyer_key.to_string(),
        "the member key of eve is the key of the buyer"
    );
    let keeper = w.f.circle.keeper.clone();
    let late =
        w.f.circle
            .call_as(
                &keeper,
                "register_cumulative",
                json!({"voucher": old_voucher, "signature": old_signature}),
            )
            .await?;
    assert!(
        failure_text(&late).is_some(),
        "a voucher signed by the old key before the sale is refused after it"
    );
    // a voucher signed by the old key after the sale is refused as well; one signed by the new key is accepted
    let after =
        w.f.circle
            .register("eve", "carol", epoch_now, CREDIT)
            .await?;
    assert!(
        failure_text(&after).is_some(),
        "the old key adds no debt after the sale"
    );
    let new_signing = w.f.circle.keys["neweve"].clone();
    w.f.circle.keys.insert("eve".to_string(), new_signing);
    let fresh =
        w.f.circle
            .register("eve", "carol", epoch_now, 2 * CREDIT)
            .await?;
    assert!(
        fresh.is_success(),
        "the key of the buyer registers: {:?}",
        failure_text(&fresh)
    );
    Ok(())
}

// TC-CT-127-08 (coordinator point 3): anyone cancels an expired account escrow once; the key goes to the return key only
#[tokio::test]
async fn a_stranger_cancels_an_expired_account_escrow_once() -> anyhow::Result<()> {
    let mut w = world().await?;
    let sold = w.f.account("sold").await?;
    let sold_id = sold.id().clone();
    let (_, seller_key) = fresh_key();
    let id = w.offer("near:account", &sold_id, HOUR_NS).await?;
    let lock = w
        .lock(&sold, id, &seller_key, &[sold.secret_key().public_key()])
        .await?;
    assert!(lock.is_success(), "{:?}", failure_text(&lock));
    let carol = w.f.account("carol").await?;
    let early =
        w.f.circle
            .call_as(
                &carol,
                "cancel_expired_account_escrow",
                json!({"grant_id": id}),
            )
            .await?;
    assert!(failure_text(&early)
        .unwrap_or_default()
        .contains("ERR_ESCROW_NOT_EXPIRED"));
    let target = w.f.circle.now_ns().await? + 2 * HOUR_NS;
    w.f.circle.advance_to(target).await?;
    let cancel =
        w.f.circle
            .call_as(
                &carol,
                "cancel_expired_account_escrow",
                json!({"grant_id": id}),
            )
            .await?;
    assert!(cancel.is_success(), "{:?}", failure_text(&cancel));
    assert_eq!(w.locker_state(&sold_id).await?, "returned");
    assert_eq!(
        w.keys(&sold_id).await?,
        vec![seller_key.to_string()],
        "the key goes to the seller's return key, not to the caller"
    );
    let again =
        w.f.circle
            .call_as(
                &carol,
                "cancel_expired_account_escrow",
                json!({"grant_id": id}),
            )
            .await?;
    assert!(failure_text(&again).is_some(), "not twice");
    assert_eq!(w.keys(&sold_id).await?, vec![seller_key.to_string()]);
    Ok(())
}
