//! CT-095 in near-sandbox: `freeze_member` by the owner (the DAO account of the fixture) after a timelock. A frozen member starts nothing new but moves no money and keeps its
//! stake. The seller that already handed over its goods can still register the voucher of a frozen buyer in the epoch of the freeze. Needs `cargo near build
//! non-reproducible-wasm` first. Run one test at a time on a loaded machine.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::funded::Funded;
use near_workspaces::Account;
use serde_json::{json, Value};

const STAKE: u128 = 150 * CREDIT;

struct World {
    f: Funded,
    dao: Account,
}

async fn world() -> anyhow::Result<World> {
    let mut f = Funded::deploy(None).await?;
    for name in ["alice", "bob", "carol"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, STAKE).await?.is_success());
    }
    let dao = f.account("dao").await?;
    Ok(World { f, dao })
}

impl World {
    async fn propose(&self, who: &str) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        Ok(self.f.circle.call_as(&self.dao, "propose_freeze", json!({"account_id": account_of(who), "reason_hash": "ab".repeat(32)})).await?)
    }

    /// Moves the end of the timelock into the past, as ct055 does for the upgrade, then applies the freeze.
    async fn freeze(&self, who: &str) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        assert!(self.propose(who).await?.is_success());
        let mut record = vec![0u8]; // state pending
        record.extend_from_slice(&1u64.to_le_bytes()); // eta in the past
        record.push(1); // Some(reason_hash)
        record.extend_from_slice(&64u32.to_le_bytes());
        record.extend_from_slice("ab".repeat(32).as_bytes());
        record.extend_from_slice(&0u64.to_le_bytes()); // from_epoch
        let key = [b"x207_freeze:".as_slice(), account_of(who).as_bytes()].concat();
        self.f.circle.worker.patch_state(self.f.circle.contract.id(), &key, &record).await?;
        let anybody = self.f.accounts["carol"].clone();
        Ok(self.f.circle.call_as(&anybody, "apply_freeze", json!({"account_id": account_of(who)})).await?)
    }

    async fn usdt_of_circle(&self) -> anyhow::Result<u128> {
        self.f.balance(&self.f.usdt, self.f.circle.contract.id()).await
    }
}

// TC-CT-095-01: only the owner proposes; the timelock is real; apply by anybody; the views
#[tokio::test]
async fn the_timelock_is_real_and_only_the_owner_proposes() -> anyhow::Result<()> {
    let w = world().await?;
    let carol = w.f.accounts["carol"].clone();
    let not_owner = w.f.circle.call_as(&carol, "propose_freeze", json!({"account_id": account_of("bob"), "reason_hash": null})).await?;
    assert!(failure_text(&not_owner).expect("refused").contains("ERR_NOT_OWNER"));
    let proposed = w.propose("bob").await?;
    assert!(proposed.is_success(), "{:?}", failure_text(&proposed));
    assert!(events(&proposed).iter().any(|(n, d)| n == "freeze_proposed" && d["account_id"] == account_of("bob")));
    let early = w.f.circle.call_as(&carol, "apply_freeze", json!({"account_id": account_of("bob")})).await?;
    assert!(failure_text(&early).expect("refused").contains("ERR_FREEZE_TIMELOCK"));
    assert_eq!(w.f.circle.view("is_frozen", json!({"account_id": account_of("bob")})).await?, false);
    let pending = w.f.circle.view("get_freeze", json!({"account_id": account_of("bob")})).await?;
    assert_eq!(pending["state"], "pending");
    // a pending freeze blocks nothing: bob still posts an intent
    let bob = w.f.accounts["bob"].clone();
    let hash = near_sdk::bs58::encode([7u8; 32]).into_string();
    assert!(w.f.circle.call_as(&bob, "post_intent", json!({"intent_hash": hash})).await?.is_success());
    // the owner cancels it
    let cancelled = w.f.circle.call_as(&w.dao, "cancel_freeze", json!({"account_id": account_of("bob")})).await?;
    assert!(cancelled.is_success(), "{:?}", failure_text(&cancelled));
    assert_eq!(w.f.circle.view("get_freeze", json!({"account_id": account_of("bob")})).await?, Value::Null);
    Ok(())
}

// TC-CT-095-02..06: a frozen member starts nothing new, keeps its stake, and no money moves
#[tokio::test]
async fn a_frozen_member_starts_nothing_new_and_no_money_moves() -> anyhow::Result<()> {
    let mut w = world().await?;
    let before = w.usdt_of_circle().await?;
    let applied = w.freeze("bob").await?;
    assert!(applied.is_success(), "{:?}", failure_text(&applied));
    assert!(events(&applied).iter().any(|(n, d)| n == "member_frozen" && d["account_id"] == account_of("bob") && d["from_epoch"] == 1));
    assert_eq!(w.f.circle.view("is_frozen", json!({"account_id": account_of("bob")})).await?, true);
    assert_eq!(w.usdt_of_circle().await?, before, "freezing moves no token");
    assert_eq!(w.f.member("bob").await?["stake"], STAKE.to_string());

    let bob = w.f.account("bob").await?;
    let alice = w.f.account("alice").await?;
    // new things are refused
    let hash = near_sdk::bs58::encode([8u8; 32]).into_string();
    assert!(failure_text(&w.f.circle.call_as(&bob, "post_intent", json!({"intent_hash": hash})).await?).expect("refused").contains("ERR_FROZEN"));
    let grant = json!({"input": {"asset_type": "x", "delivery_method": "y", "limits": {"scope": "{}", "quota": null, "expires_at": "9999999999999999999"}, "unit": "once", "unit_price": "1", "operator": null, "proof_hash": null, "proof_kind": null}});
    assert!(failure_text(&w.f.circle.call_as(&bob, "offer_grant", grant).await?).expect("refused").contains("ERR_FROZEN"));
    let bundle = w.f.circle.commit_bundle(1, &[("alice", "bob", 6 * CREDIT), ("bob", "carol", 6 * CREDIT), ("carol", "alice", 6 * CREDIT)], &["alice", "bob", "carol"]).await?;
    assert!(failure_text(&bundle).expect("refused").contains("ERR_FROZEN"));
    // but he can still do what leaves things as they are: add stake, ask to leave
    assert!(w.f.stake(&bob, 10 * CREDIT).await?.is_success());
    assert_eq!(w.f.member("bob").await?["stake"], (STAKE + 10 * CREDIT).to_string());
    let exit = bob.call(w.f.circle.contract.id(), "request_exit").deposit(near_workspaces::types::NearToken::from_yoctonear(1)).transact().await?;
    assert!(exit.is_success(), "a frozen member can leave: {:?}", failure_text(&exit));
    let _ = alice;
    // the owner lifts it at once, and bob posts an intent again
    let lifted = w.f.circle.call_as(&w.dao, "unfreeze_member", json!({"account_id": account_of("bob")})).await?;
    assert!(lifted.is_success(), "{:?}", failure_text(&lifted));
    assert_eq!(w.f.circle.view("is_frozen", json!({"account_id": account_of("bob")})).await?, false);
    assert!(events(&lifted).iter().any(|(n, _)| n == "member_unfrozen"));
    Ok(())
}

// TC-CT-095-07: the honest seller. alice sold to bob and handed the goods over; bob is frozen in the same epoch; alice can still register the voucher of bob until the
// cutoff; in the next epoch a voucher of bob is refused.
#[tokio::test]
async fn the_seller_that_delivered_can_still_register_the_voucher_of_a_frozen_buyer_in_the_epoch_of_the_freeze() -> anyhow::Result<()> {
    let mut w = world().await?;
    assert!(w.f.circle.register("bob", "alice", 1, 4 * CREDIT).await?.is_success());
    assert!(w.freeze("bob").await?.is_success());
    // the same epoch: a higher voucher of bob to alice still registers
    let more = w.f.circle.register("bob", "alice", 1, 9 * CREDIT).await?;
    assert!(more.is_success(), "the honest seller is paid: {:?}", failure_text(&more));
    assert_eq!(w.f.circle.view("obligation", json!({"epoch": 1, "debtor": account_of("bob"), "creditor": account_of("alice")})).await?["obligation"], (9 * CREDIT).to_string());
    // the next epoch: refused, as payer and as payee
    let cutoff = w.f.circle.epoch_ts(1, "cutoff_at").await?;
    w.f.circle.advance_to(cutoff).await?;
    assert!(w.f.circle.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
    let epoch = w.f.circle.view("get_epoch", json!({})).await?["epoch"].as_u64().unwrap();
    assert_eq!(epoch, 2);
    let as_payer = w.f.circle.register("bob", "alice", 2, CREDIT).await?;
    assert!(failure_text(&as_payer).expect("refused").contains("ERR_FROZEN"), "{:?}", failure_text(&as_payer));
    let as_payee = w.f.circle.register("alice", "bob", 2, CREDIT).await?;
    assert!(failure_text(&as_payee).expect("refused").contains("ERR_FROZEN"), "{:?}", failure_text(&as_payee));
    // others are not affected
    assert!(w.f.circle.register("alice", "carol", 2, CREDIT).await?.is_success());
    Ok(())
}
