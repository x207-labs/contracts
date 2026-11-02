//! CT-075 in near-sandbox (F8 class A): the escrow of an NFT. Needs `cargo near build non-reproducible-wasm` first; the NFT is the
//! `probes/mock_nft` contract.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::{Account, Contract};
use serde_json::{json, Value};

const PRICE: u128 = 10 * CREDIT;

struct World {
    f: Funded,
    nft: Contract,
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
    let entry = |class: &str| json!({"asset_type": "nft:ticket", "class": class, "delivery_methods": ["escrow_nft"], "accepted_proofs": [], "terms_hash": "5a".repeat(32)});
    assert!(f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": "nft:ticket", "entry": entry("A")})).await?.is_success());
    let wasm = std::fs::read(common::probe_wasm("mock_nft"))?;
    let nft = f.circle.worker.dev_deploy(&wasm).await?;
    nft.call("new").transact().await?.into_result()?;
    let (alice, bob) = (f.accounts["alice"].clone(), f.accounts["bob"].clone());
    Ok(World { f, nft, alice, bob })
}

fn offer(expires_at: u64) -> Value {
    json!({"input": {
        "asset_type": "nft:ticket", "delivery_method": "escrow_nft",
        "limits": {"scope": "{}", "quota": null, "expires_at": expires_at.to_string()},
        "unit": "once", "unit_price": PRICE.to_string(), "operator": null, "proof_hash": null, "proof_kind": null
    }})
}

impl World {
    async fn offer_and_deposit(&mut self, token: &str, expires_in_ns: u64) -> anyhow::Result<u64> {
        let expires = self.f.circle.now_ns().await? + expires_in_ns;
        let id = self.f.circle.call_as(&self.alice, "offer_grant", offer(expires)).await?.json::<u64>()?;
        self.nft.call("nft_mint").args_json(json!({"token_id": token, "owner_id": self.alice.id()})).transact().await?.into_result()?;
        self.deposit(&self.alice.clone(), token, id).await?;
        Ok(id)
    }

    async fn deposit(&self, from: &Account, token: &str, grant_id: u64) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        Ok(from
            .call(self.nft.id(), "nft_transfer_call")
            .args_json(json!({"receiver_id": self.f.circle.contract.id(), "token_id": token, "msg": json!({"grant_id": grant_id}).to_string()}))
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(150))
            .transact()
            .await?)
    }

    async fn owner(&self, token: &str) -> anyhow::Result<String> {
        Ok(self.nft.view("nft_token").args_json(json!({"token_id": token})).await?.json::<Value>()?["owner_id"].as_str().unwrap().to_string())
    }

    async fn buy(&mut self, buyer: &str, token_epoch: u64, cumulative: u128, grant_id: u64) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        let (voucher, signature) = self.f.circle.next_voucher(buyer, "alice", token_epoch, cumulative);
        let account = self.f.accounts[buyer].clone();
        Ok(self.f.circle.call_as(&account, "complete_escrow", json!({"grant_id": grant_id, "voucher": voucher, "signature": signature})).await?)
    }
}

// TC-CT-075-01..05: the asset is held, paid for and delivered
#[tokio::test]
async fn the_buyer_pays_and_gets_the_nft() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer_and_deposit("t1", 3_600_000_000_000).await?;
    assert_eq!(w.owner("t1").await?, w.f.circle.contract.id().to_string());
    let held = w.f.circle.view("get_escrow", json!({"grant_id": id})).await?;
    assert_eq!((held["state"].as_str(), held["token_id"].as_str(), held["buyer"].clone()), (Some("held"), Some("t1"), Value::Null));

    // while the grant is only offered, accept_grant is not the way
    let carol = w.f.account("carol").await?;
    let wrong = w.f.circle.call_as(&carol, "accept_grant", json!({"grant_id": id, "delivery_pubkey": null})).await?;
    assert!(failure_text(&wrong).expect("refused").contains("complete_escrow"));
    // revoke_grant would strand the asset
    let alice = w.alice.clone();
    let strand = w.f.circle.call_as(&alice, "revoke_grant", json!({"grant_id": id, "reason": "grantor"})).await?;
    assert!(failure_text(&strand).expect("refused").contains("ERR_GRANT_REVOKE_DENIED"));

    let epoch = w.f.circle.view("get_epoch", json!({})).await?["epoch"].as_u64().unwrap();
    let done = w.buy("bob", epoch, PRICE, id).await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    let log = events(&done);
    assert!(log.iter().any(|(n, d)| n == "voucher_registered" && d["delta"] == PRICE.to_string()));
    assert!(log.iter().any(|(n, d)| n == "grant_activated" && d["grant_id"] == id && d["grantee"] == w.bob.id().as_str()));
    assert_eq!(w.owner("t1").await?, w.bob.id().to_string());
    let grant = w.f.circle.view("get_grant", json!({"grant_id": id})).await?;
    assert_eq!((grant["status"].as_str(), grant["grantee"].as_str()), (Some("active"), Some(w.bob.id().as_str())));
    assert_eq!(w.f.circle.view("get_escrow", json!({"grant_id": id})).await?["state"], "delivered");
    let obligation = w.f.circle.view("obligation", json!({"epoch": epoch, "debtor": account_of("bob"), "creditor": account_of("alice")})).await?;
    assert_eq!(obligation["obligation"], PRICE.to_string());
    // the same voucher pair is free again for the buyer
    let (voucher, signature) = w.f.circle.next_voucher("bob", "alice", epoch, PRICE + CREDIT);
    let bob = w.bob.clone();
    assert!(w.f.circle.call_as(&bob, "register_cumulative", json!({"voucher": voucher, "signature": signature})).await?.is_success());
    Ok(())
}

// TC-CT-075-06..09: the transfer fails, nothing of the payment is left, and the asset is still for sale
#[tokio::test]
async fn a_failed_transfer_leaves_no_obligation() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer_and_deposit("t1", 3_600_000_000_000).await?;
    let epoch = w.f.circle.view("get_epoch", json!({})).await?["epoch"].as_u64().unwrap();
    let before = w.f.circle.view("get_epoch", json!({})).await?;
    w.nft.call("block_receiver").args_json(json!({"account_id": w.bob.id()})).transact().await?.into_result()?;

    let tried = w.buy("bob", epoch, PRICE, id).await?;
    assert!(tried.is_success(), "the callback runs: {:?}", failure_text(&tried));
    let log = events(&tried);
    let failed = log.iter().find(|(n, _)| n == "escrow_failed").expect("escrow_failed").1.clone();
    assert_eq!((failed["step"].as_str(), failed["reverted"].clone(), failed["delta"].as_str()), (Some("complete"), json!(true), Some("10000000")));
    assert!(!log.iter().any(|(n, _)| n == "grant_activated"));
    // the circle holds the NFT, the escrow is held again, the grant is offered
    assert_eq!(w.owner("t1").await?, w.f.circle.contract.id().to_string());
    assert_eq!(w.f.circle.view("get_escrow", json!({"grant_id": id})).await?["state"], "held");
    assert_eq!(w.f.circle.view("get_grant", json!({"grant_id": id})).await?["status"], "offered");
    // no obligation, no debt, and the epoch counters are as they were
    assert_eq!(w.f.circle.view("obligation", json!({"epoch": epoch, "debtor": account_of("bob"), "creditor": account_of("alice")})).await?, Value::Null);
    assert_eq!(w.f.circle.view("outstanding_debt", json!({"account_id": account_of("bob")})).await?, "0");
    let after = w.f.circle.view("get_epoch", json!({})).await?;
    for field in ["edge_count", "participant_count", "total_obligation"] {
        assert_eq!(before[field], after[field], "{field}");
    }
    // the pair is not locked any more: a second buyer pays and gets the token, and bob can still register
    let carol_voucher = w.buy("carol", epoch, PRICE, id).await?;
    assert!(carol_voucher.is_success(), "{:?}", failure_text(&carol_voucher));
    assert_eq!(w.owner("t1").await?, w.f.accounts["carol"].id().to_string());
    // bob's own voucher for the same pair starts again at seq 1 and registers
    let (voucher, signature) = {
        // the helper counted the failed attempt as seq 1, so a gap is fine in cumulative mode
        w.f.circle.next_voucher("bob", "alice", epoch, CREDIT)
    };
    let bob = w.bob.clone();
    assert!(w.f.circle.call_as(&bob, "register_cumulative", json!({"voucher": voucher, "signature": signature})).await?.is_success());
    Ok(())
}

// TC-CT-075-10..14: refusals leave nothing behind
#[tokio::test]
async fn refusals_change_nothing() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer_and_deposit("t1", 3_600_000_000_000).await?;
    let epoch = w.f.circle.view("get_epoch", json!({})).await?["epoch"].as_u64().unwrap();

    // another account puts its token against this grant: the NFT contract gives it back
    w.nft.call("nft_mint").args_json(json!({"token_id": "t2", "owner_id": w.bob.id()})).transact().await?.into_result()?;
    let bob = w.bob.clone();
    let intruder = w.deposit(&bob, "t2", id).await?;
    let _ = intruder;
    assert_eq!(w.owner("t2").await?, w.bob.id().to_string());
    // a second token for the same grant
    w.nft.call("nft_mint").args_json(json!({"token_id": "t3", "owner_id": w.alice.id()})).transact().await?.into_result()?;
    let alice = w.alice.clone();
    w.deposit(&alice, "t3", id).await?;
    assert_eq!(w.owner("t3").await?, w.alice.id().to_string());
    assert_eq!(w.f.circle.view("get_escrow", json!({"grant_id": id})).await?["token_id"], "t1");
    // a message that names no grant
    let bad = w.alice.call(w.nft.id(), "nft_transfer_call").args_json(json!({"receiver_id": w.f.circle.contract.id(), "token_id": "t3", "msg": "hello"}))
        .deposit(NearToken::from_yoctonear(1)).gas(Gas::from_tgas(150)).transact().await?;
    let _ = bad;
    assert_eq!(w.owner("t3").await?, w.alice.id().to_string());

    // buyer refusals: too cheap, wrong payee, the grantor itself, an unknown grant
    let cheap = w.buy("bob", epoch, PRICE - 1, id).await?;
    assert!(failure_text(&cheap).expect("refused").contains("ERR_ESCROW_PRICE"));
    let (mut voucher, signature) = w.f.circle.next_voucher("bob", "alice", epoch, PRICE);
    voucher["payee"] = json!(account_of("carol"));
    let wrong_payee = w.f.circle.call_as(&bob, "complete_escrow", json!({"grant_id": id, "voucher": voucher, "signature": signature})).await?;
    assert!(failure_text(&wrong_payee).expect("refused").contains("ERR_ESCROW_VOUCHER"));
    let (voucher, signature) = w.f.circle.next_voucher("alice", "bob", epoch, PRICE);
    let own = w.f.circle.call_as(&alice, "complete_escrow", json!({"grant_id": id, "voucher": voucher, "signature": signature})).await?;
    assert!(failure_text(&own).expect("refused").contains("ERR_GRANT_SELF"));
    let nothing = w.buy("bob", epoch, PRICE, 99).await?;
    assert!(failure_text(&nothing).expect("refused").contains("ERR_UNKNOWN_GRANT"));
    // no obligation came from any of them
    assert_eq!(w.f.circle.view("obligation", json!({"epoch": epoch, "debtor": account_of("bob"), "creditor": account_of("alice")})).await?, Value::Null);
    assert_eq!(w.f.circle.view("get_escrow", json!({"grant_id": id})).await?["state"], "held");
    Ok(())
}

// TC-CT-075-15..17: nobody buys, the grantor cancels after the time of the grant
#[tokio::test]
async fn the_grantor_takes_the_nft_back_after_the_time() -> anyhow::Result<()> {
    let mut w = world().await?;
    let id = w.offer_and_deposit("t1", 90_000_000_000).await?;
    let alice = w.alice.clone();
    let bob = w.bob.clone();
    let early = w.f.circle.call_as(&alice, "cancel_escrow", json!({"grant_id": id})).await?;
    assert!(failure_text(&early).expect("refused").contains("ERR_ESCROW_NOT_EXPIRED"));
    let expires = w.f.circle.view("get_grant", json!({"grant_id": id})).await?["limits"]["expires_at"].as_str().unwrap().parse::<u64>()?;
    w.f.circle.advance_to(expires + 2_000_000_000).await?;
    let stranger = w.f.circle.call_as(&bob, "cancel_escrow", json!({"grant_id": id})).await?;
    assert!(failure_text(&stranger).expect("refused").contains("ERR_ESCROW_NOT_GRANTOR"));
    let late = w.buy("bob", 1, PRICE, id).await?;
    assert!(failure_text(&late).expect("refused").contains("ERR_GRANT_EXPIRED"));

    let done = w.f.circle.call_as(&alice, "cancel_escrow", json!({"grant_id": id})).await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert!(events(&done).iter().any(|(n, d)| n == "grant_revoked" && d["grant_id"] == id && d["reason"] == "grantor"));
    assert_eq!(w.owner("t1").await?, w.alice.id().to_string());
    assert_eq!(w.f.circle.view("get_escrow", json!({"grant_id": id})).await?["state"], "cancelled");
    assert_eq!(w.f.circle.view("get_grant", json!({"grant_id": id})).await?["status"], "revoked");
    let again = w.f.circle.call_as(&alice, "cancel_escrow", json!({"grant_id": id})).await?;
    assert!(failure_text(&again).expect("refused").contains("ERR_ESCROW_NOT_HELD"));
    Ok(())
}
