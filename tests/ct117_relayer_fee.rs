//! CT-117: relayer fee on `claim_evm` and `withdraw_stake_evm` (0.0.16), on the real contract in near-sandbox.
//! The EVM member `m` is the creditor of 20 credit (gpu pays from its stake through auto_settle), the DAO fee is 0.25 credit.
//! 1. registered relayer takes the fee, payout and fee are separate transfers, D6 holds.
//! 2. relayer unregistered on the token: the payout still lands, the fee stays `claimable` and a later claim collects it.
//! 3. `maxFee` 0 means no fee. 4. withdraw with fee; 5. withdraw with an unregistered relayer: the fee goes to the receiver.
//! 6. deadline more than 24 h ahead is refused; the cap of the DAO parameter.
//!
//! Needs `cargo near build non-reproducible-wasm` first. EIP-712 helpers as in `ct108_auto_settle.rs`.

mod common;

use common::circle::{events, failure_text, CREDIT};
use common::flow::finalize;
use common::funded::Funded;
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::Account;
use secp256k1::{Message, Secp256k1, SecretKey};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};

const FEE: u128 = 250_000;

fn k(bytes: &[u8]) -> [u8; 32] {
    Keccak256::digest(bytes).into()
}

fn ws(s: &str) -> [u8; 32] {
    k(s.as_bytes())
}

fn w64(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

fn w128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

fn digest(contract: &str, struct_hash: [u8; 32]) -> [u8; 32] {
    let network = if contract.ends_with(".testnet") { "testnet" } else { "mainnet" };
    let salt = k(format!("near:{network}:{contract}").as_bytes());
    let domain = k(&[k(b"EIP712Domain(string name,string version,bytes32 salt)"), ws("x207"), ws("1"), salt].concat());
    k(&[&[0x19u8, 0x01][..], &domain, &struct_hash].concat())
}

fn sign(secret: &SecretKey, digest: [u8; 32]) -> String {
    let (id, compact) = Secp256k1::new().sign_ecdsa_recoverable(&Message::from_slice(&digest).unwrap(), secret).serialize_compact();
    let mut bytes = compact.to_vec();
    bytes.push(27 + id.to_i32() as u8);
    format!("eip712:0x{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn address(secret: &SecretKey) -> String {
    let public = secp256k1::PublicKey::from_secret_key(&Secp256k1::new(), secret).serialize_uncompressed();
    let hash = k(&public[1..]);
    format!("0x{}", hash[12..].iter().map(|b| format!("{b:02x}")).collect::<String>())
}

#[allow(clippy::too_many_arguments)]
fn action_sig(secret: &SecretKey, contract: &str, member: &str, action: &str, epoch: u64, receiver: &str, terms: &str, nonce: u64, deadline: u64, max_fee: u128) -> String {
    let type_hash = ws("Action(string circle,string member,string action,uint64 epoch,string receiver,string termsHash,uint64 nonce,uint64 deadline,uint256 maxFee)");
    let hash = k(&[type_hash, ws(contract), ws(member), ws(action), w64(epoch), ws(receiver), ws(terms), w64(nonce), w64(deadline), w128(max_fee)].concat());
    sign(secret, digest(contract, hash))
}

fn voucher_sig(secret: &SecretKey, contract: &str, payer: &str, payee: &str, epoch: u64, seq: u64, cumulative: u128) -> String {
    let type_hash = ws("Voucher(string circle,string payer,string payee,uint64 epoch,uint64 seq,uint256 cumulativeAmount,string unit,string resource)");
    let hash = k(&[type_hash, ws(contract), ws(payer), ws(payee), w64(epoch), w64(seq), w128(cumulative), ws("credit"), ws("/api/x")].concat());
    sign(secret, digest(contract, hash))
}

fn tgas(result: &ExecutionFinalResult) -> f64 {
    result.total_gas_burnt.as_gas() as f64 / 1e12
}

struct World {
    f: Funded,
    secret: SecretKey,
    member: String,
    contract: String,
    deadline: u64,
    relayer: Account,
    recv: Account,
    next_nonce: u64,
}

impl World {
    /// `exit`: the member asks to exit (nonce 0) before the epoch closes.
    async fn finalized(exit: bool) -> anyhow::Result<World> {
        Self::finalized_on(exit, false).await
    }

    /// `intents`: the circle is deployed with the `mock_intents` receiver as `intents_id`.
    async fn finalized_on(exit: bool, intents: bool) -> anyhow::Result<World> {
        let mut params = Funded::params_short();
        params["relayer_fee"] = json!(FEE.to_string());
        let mut f = if intents { Funded::deploy_with_intents(Some(params)).await? } else { Funded::deploy(Some(params)).await? };
        let contract = f.circle.contract.id().to_string();
        let secret = SecretKey::from_slice(&[9u8; 32])?;
        let member = address(&secret);
        let deadline = f.circle.now_ns().await? / 1_000_000 + 20 * 3_600_000;
        let relayer = f.account("relayer").await?;
        let recv = f.account("recv").await?;
        let gpu = f.account("gpu").await?;
        assert!(f.join("gpu", Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&gpu, 100 * CREDIT).await?.is_success());
        let on = gpu.call(f.circle.contract.id(), "set_auto_settle").args_json(json!({"enabled": true})).deposit(NearToken::from_yoctonear(1)).transact().await?;
        assert!(on.is_success(), "{:?}", failure_text(&on));

        let terms = "a".repeat(64);
        let join = relayer
            .call(f.circle.contract.id(), "join_evm")
            .args_json(json!({"address": member, "terms_hash": terms, "deadline": deadline, "signature": action_sig(&secret, &contract, &member, "join", 0, "", &terms, 0, deadline, 0)}))
            .deposit(NearToken::from_millinear(350))
            .gas(Gas::from_tgas(100))
            .transact()
            .await?;
        assert!(join.is_success(), "join_evm: {:?}", failure_text(&join));
        let stake = f.transfer_call(&f.usdt, &relayer, 100 * CREDIT, &json!({"action": "stake", "member_id": member}).to_string()).await?;
        assert!(stake.is_success(), "stake: {:?}", failure_text(&stake));

        let mut w = World { f, secret, member, contract, deadline, relayer, recv, next_nonce: 0 };
        if exit {
            let nonce = w.take_nonce();
            let sig = action_sig(&w.secret, &w.contract, &w.member, "request_exit", 0, "", "", nonce, w.deadline, 0);
            let r = w
                .relayer
                .call(w.f.circle.contract.id(), "request_exit_evm")
                .args_json(json!({"member": w.member, "nonce": nonce, "deadline": w.deadline, "max_fee": "0", "signature": sig}))
                .gas(Gas::from_tgas(100))
                .transact()
                .await?;
            assert!(r.is_success(), "request_exit_evm: {:?}", failure_text(&r));
        }

        // gpu owes the EVM member 20 credit and pays from its stake in finalize
        let amount = 20 * CREDIT;
        let voucher = json!({"circle": w.contract, "payer": common::circle::account_of("gpu"), "payee": w.member, "epoch": 1, "seq": 1, "cumulativeAmount": amount.to_string(), "unit": "credit", "resource": "https://example.test/api"});
        let hash = common::circle::signing_hash(&w.contract, &common::circle::account_of("gpu"), &w.member, 1, 1, &amount.to_string(), "https://example.test/api");
        let signature = {
            use ed25519_dalek::Signer;
            format!("ed25519:{}", near_sdk::bs58::encode(w.f.circle.keys["gpu"].sign(&hash).to_bytes()).into_string())
        };
        let reg = w.relayer.call(w.f.circle.contract.id(), "register_cumulative").args_json(json!({"voucher": voucher, "signature": signature})).gas(Gas::from_tgas(100)).transact().await?;
        assert!(reg.is_success(), "register_cumulative: {:?}", failure_text(&reg));
        let _ = voucher_sig; // kept for symmetry with ct108

        let c = &w.f.circle;
        c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
        assert!(c.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
        c.advance_to(c.epoch_ts(1, "proposal_deadline").await?).await?;
        let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
        assert!(commit.is_success(), "commit: {:?}", failure_text(&commit));
        let fin = finalize(&w.f.circle).await?;
        assert!(fin.is_success(), "finalize: {:?}", failure_text(&fin));
        Ok(w)
    }

    fn take_nonce(&mut self) -> u64 {
        let n = self.next_nonce;
        self.next_nonce += 1;
        n
    }

    async fn claim_evm(&mut self, max_fee: u128, deadline: Option<u64>) -> anyhow::Result<ExecutionFinalResult> {
        let nonce = self.take_nonce();
        let deadline = deadline.unwrap_or(self.deadline);
        let recv = match &self.f.intents {
            Some(i) => i.id().to_string(),
            None => self.recv.id().to_string(),
        };
        let sig = action_sig(&self.secret, &self.contract, &self.member, "claim", 1, &recv, "", nonce, deadline, max_fee);
        Ok(self
            .relayer
            .call(self.f.circle.contract.id(), "claim_evm")
            .args_json(json!({"member": self.member, "epoch": 1, "receiver": recv, "nonce": nonce, "deadline": deadline, "max_fee": max_fee.to_string(), "signature": sig}))
            .gas(Gas::from_tgas(100))
            .transact()
            .await?)
    }

    /// CT-118 (0.0.17): `request_exit_evm` (`action` = "request_exit") or `set_auto_settle_evm` ("auto_settle" / "auto_settle_off") with a fee ceiling.
    async fn small_action_evm(&mut self, action: &str, max_fee: u128) -> anyhow::Result<ExecutionFinalResult> {
        let nonce = self.take_nonce();
        let sig = action_sig(&self.secret, &self.contract, &self.member, action, 0, "", "", nonce, self.deadline, max_fee);
        let (method, args) = if action == "request_exit" {
            ("request_exit_evm", json!({"member": self.member, "nonce": nonce, "deadline": self.deadline, "max_fee": max_fee.to_string(), "signature": sig}))
        } else {
            (
                "set_auto_settle_evm",
                json!({"member": self.member, "enabled": action == "auto_settle", "nonce": nonce, "deadline": self.deadline, "max_fee": max_fee.to_string(), "signature": sig}),
            )
        };
        Ok(self.relayer.call(self.f.circle.contract.id(), method).args_json(args).gas(Gas::from_tgas(100)).transact().await?)
    }

    async fn member_stake(&self) -> anyhow::Result<String> {
        Ok(self.f.circle.view("member", json!({"account_id": self.member})).await?["stake"].as_str().unwrap_or_default().to_string())
    }

    async fn withdraw_evm(&mut self, max_fee: u128) -> anyhow::Result<ExecutionFinalResult> {
        let nonce = self.take_nonce();
        let recv = self.recv.id().to_string();
        let sig = action_sig(&self.secret, &self.contract, &self.member, "withdraw_stake", 0, &recv, "", nonce, self.deadline, max_fee);
        Ok(self
            .relayer
            .call(self.f.circle.contract.id(), "withdraw_stake_evm")
            .args_json(json!({"member": self.member, "receiver": recv, "nonce": nonce, "deadline": self.deadline, "max_fee": max_fee.to_string(), "signature": sig}))
            .gas(Gas::from_tgas(100))
            .transact()
            .await?)
    }

    async fn usdt(&self, who: &Account) -> anyhow::Result<u128> {
        self.f.balance(&self.f.usdt, who.id()).await
    }

    /// The relayer leaves the token (NEP-145 `storage_unregister`): it can no longer receive a transfer.
    async fn unregister_relayer(&self) -> anyhow::Result<()> {
        let r = self.relayer.call(self.f.usdt.id(), "storage_unregister").args_json(json!({"force": true})).deposit(NearToken::from_yoctonear(1)).transact().await?;
        assert!(r.is_success());
        Ok(())
    }

    async fn position(&self) -> anyhow::Result<Value> {
        self.f.circle.view("net_position", json!({"epoch": 1, "account_id": self.member})).await
    }
}

fn has_event(r: &ExecutionFinalResult, name: &str) -> bool {
    events(r).iter().any(|(n, _)| n == name)
}

// TC-CT-117-01
#[tokio::test]
async fn claim_evm_pays_the_registered_relayer() -> anyhow::Result<()> {
    let mut w = World::finalized(false).await?;
    let (relayer_before, recv_before) = (w.usdt(&w.relayer).await?, w.usdt(&w.recv).await?);
    let r = w.claim_evm(FEE, None).await?;
    assert!(r.is_success(), "claim_evm: {:?}", failure_text(&r));
    println!("GAS claim_evm with fee (registered relayer): {:.1} Tgas", tgas(&r));
    assert_eq!(w.usdt(&w.relayer).await? - relayer_before, FEE);
    assert_eq!(w.usdt(&w.recv).await? - recv_before, 20 * CREDIT - FEE);
    assert!(has_event(&r, "relayer_fee_paid") && !has_event(&r, "relayer_fee_failed"), "{:?}", r.logs());
    assert_eq!(w.position().await?["status"], "claimed");
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-117-02: the relayer is not registered on the token: the member still gets the payout; the fee stays claimable
#[tokio::test]
async fn unregistered_relayer_does_not_block_the_payout() -> anyhow::Result<()> {
    let mut w = World::finalized(false).await?;
    w.unregister_relayer().await?;
    let recv_before = w.usdt(&w.recv).await?;
    let r = w.claim_evm(FEE, None).await?;
    assert!(r.is_success(), "claim_evm: {:?}", failure_text(&r));
    assert_eq!(w.usdt(&w.recv).await? - recv_before, 20 * CREDIT - FEE, "payout lands");
    assert!(has_event(&r, "relayer_fee_failed") && !has_event(&r, "relayer_fee_paid"), "{:?}", r.logs());
    let p = w.position().await?;
    assert_eq!((p["status"].as_str(), p["amount"].as_str()), (Some("claimable"), Some(FEE.to_string().as_str())), "{p}");
    w.f.assert_d6().await?;
    // a later claim (no fee) collects the rest
    let again = w.claim_evm(0, None).await?;
    assert!(again.is_success(), "second claim: {:?}", failure_text(&again));
    assert_eq!(w.usdt(&w.recv).await? - recv_before, 20 * CREDIT);
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-117-03: maxFee 0 forbids a fee
#[tokio::test]
async fn max_fee_zero_means_no_fee() -> anyhow::Result<()> {
    let mut w = World::finalized(false).await?;
    let (relayer_before, recv_before) = (w.usdt(&w.relayer).await?, w.usdt(&w.recv).await?);
    let r = w.claim_evm(0, None).await?;
    assert!(r.is_success(), "claim_evm: {:?}", failure_text(&r));
    assert_eq!(w.usdt(&w.relayer).await?, relayer_before);
    assert_eq!(w.usdt(&w.recv).await? - recv_before, 20 * CREDIT);
    assert!(!has_event(&r, "relayer_fee_paid") && !has_event(&r, "relayer_fee_failed"));
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-117-04: withdraw with a fee (signed ceiling 0.1 credit is below the DAO fee)
#[tokio::test]
async fn withdraw_stake_evm_pays_the_relayer() -> anyhow::Result<()> {
    let mut w = World::finalized(true).await?;
    let claim = w.claim_evm(0, None).await?;
    assert!(claim.is_success(), "{:?}", failure_text(&claim));
    let (relayer_before, recv_before) = (w.usdt(&w.relayer).await?, w.usdt(&w.recv).await?);
    let r = w.withdraw_evm(100_000).await?;
    assert!(r.is_success(), "withdraw_stake_evm: {:?}", failure_text(&r));
    println!("GAS withdraw_stake_evm with fee: {:.1} Tgas", tgas(&r));
    assert_eq!(w.usdt(&w.relayer).await? - relayer_before, 100_000);
    assert_eq!(w.usdt(&w.recv).await? - recv_before, 100 * CREDIT - 100_000);
    assert!(has_event(&r, "relayer_fee_paid"), "{:?}", r.logs());
    assert_eq!(w.f.circle.view("member", json!({"account_id": w.member})).await?["status"], "exited");
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-117-05: withdraw, relayer unregistered: the member exits and the fee goes on to the receiver
#[tokio::test]
async fn withdraw_with_unregistered_relayer_sends_the_fee_to_the_receiver() -> anyhow::Result<()> {
    let mut w = World::finalized(true).await?;
    let claim = w.claim_evm(0, None).await?;
    assert!(claim.is_success(), "{:?}", failure_text(&claim));
    w.unregister_relayer().await?;
    let recv_before = w.usdt(&w.recv).await?;
    let r = w.withdraw_evm(FEE).await?;
    assert!(r.is_success(), "withdraw_stake_evm: {:?}", failure_text(&r));
    assert_eq!(w.usdt(&w.recv).await? - recv_before, 100 * CREDIT, "payout and the failed fee both reach the receiver");
    assert!(has_event(&r, "relayer_fee_failed"), "{:?}", r.logs());
    assert_eq!(w.f.circle.view("member", json!({"account_id": w.member})).await?["status"], "exited");
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-117-06: R1a, a deadline more than 24 h ahead is refused
#[tokio::test]
async fn deadline_more_than_24_hours_ahead_is_refused() -> anyhow::Result<()> {
    let mut w = World::finalized(false).await?;
    let now_ms = w.f.circle.now_ns().await? / 1_000_000;
    let r = w.claim_evm(FEE, Some(now_ms + 25 * 3_600_000)).await?;
    assert!(r.is_failure() && failure_text(&r).unwrap_or_default().contains("ERR_EVM_DEADLINE_TOO_FAR"), "{:?}", failure_text(&r));
    Ok(())
}

// TC-CT-117-07: the DAO parameter has a hard cap
#[tokio::test]
async fn relayer_fee_above_the_cap_is_refused() -> anyhow::Result<()> {
    let mut params = Funded::params_short();
    params["relayer_fee"] = json!("1000001");
    let err = Funded::deploy(Some(params)).await.err().map(|e| format!("{e:?}")).unwrap_or_default();
    assert!(err.contains("relayer_fee"), "{err}");
    let mut ok = Funded::params_short();
    ok["relayer_fee"] = json!("1000000");
    assert!(Funded::deploy(Some(ok)).await.is_ok());
    Ok(())
}

// TC-CT-117-09: claim to Intents with a fee; Intents hands 5 credit back: fee paid, 14.75 kept, 5 returns to the member's claimable
#[tokio::test]
async fn intents_partial_refund_with_fee() -> anyhow::Result<()> {
    let mut w = World::finalized_on(false, true).await?;
    let intents = w.f.intents.clone().unwrap();
    intents.call("set_mode").args_json(json!({"mode": "refund", "refund": (5 * CREDIT).to_string()})).transact().await?.into_result()?;
    let relayer_before = w.usdt(&w.relayer).await?;
    let r = w.claim_evm(FEE, None).await?;
    assert!(r.is_success(), "claim_evm: {:?}", failure_text(&r));
    assert_eq!(w.usdt(&w.relayer).await? - relayer_before, FEE, "fee paid");
    assert_eq!(w.f.balance(&w.f.usdt, intents.id()).await?, 20 * CREDIT - FEE - 5 * CREDIT, "Intents keeps payout minus refund");
    let p = w.position().await?;
    assert_eq!((p["status"].as_str(), p["amount"].as_str()), (Some("claimable"), Some((5 * CREDIT).to_string().as_str())), "{p}");
    assert!(has_event(&r, "relayer_fee_paid") && has_event(&r, "residual_claim_failed"), "{:?}", r.logs());
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-117-08: the owner changes `relayer_fee` with `set_params`: refused above the cap, pending until the next epoch, then in force
#[tokio::test]
async fn owner_set_params_changes_relayer_fee_from_the_next_epoch() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    assert_eq!(f.circle.view("get_params", json!({})).await?["relayer_fee"], "0");
    let dao = f.account("dao").await?;
    let mut params = Funded::params_short();
    params["relayer_fee"] = json!("1000001");
    let r = f.circle.call_as(&dao, "set_params", json!({"params": params.clone()})).await?;
    assert!(failure_text(&r).unwrap_or_default().contains("ERR_PARAM_OUT_OF_RANGE"), "{:?}", failure_text(&r));
    params["relayer_fee"] = json!("500000");
    let r = f.circle.call_as(&dao, "set_params", json!({"params": params})).await?;
    assert!(r.is_success(), "{:?}", failure_text(&r));
    assert_eq!(f.circle.view("get_pending_params", json!({})).await?["params"]["relayer_fee"], "500000");
    assert_eq!(f.circle.view("get_params", json!({})).await?["relayer_fee"], "0", "not in force yet");
    let c = &f.circle;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    let close = c.call_keeper("close_epoch", json!({"epoch": 1})).await?;
    assert!(close.is_success(), "{:?}", failure_text(&close));
    assert_eq!(f.circle.view("get_params", json!({})).await?["relayer_fee"], "500000");
    assert_eq!(f.circle.view("get_pending_params", json!({})).await?, Value::Null);
    Ok(())
}

// TC-CT-118-01: request_exit_evm pays the relayer from the free stake; the exit stands; D6 holds
#[tokio::test]
async fn request_exit_evm_pays_the_relayer_from_free_stake() -> anyhow::Result<()> {
    let mut w = World::finalized(false).await?;
    let stake_before: u128 = w.member_stake().await?.parse()?;
    let relayer_before = w.usdt(&w.relayer).await?;
    let r = w.small_action_evm("request_exit", FEE).await?;
    assert!(r.is_success(), "request_exit_evm: {:?}", failure_text(&r));
    println!("GAS request_exit_evm with fee: {:.1} Tgas", tgas(&r));
    assert!(has_event(&r, "exit_requested") && has_event(&r, "relayer_fee_paid") && !has_event(&r, "relayer_fee_failed"), "{:?}", r.logs());
    assert_eq!(w.usdt(&w.relayer).await? - relayer_before, FEE);
    assert_eq!(w.member_stake().await?.parse::<u128>()?, stake_before - FEE);
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-118-02: unregistered relayer: the exit stands, the fee goes back to the member stake, no reserve
#[tokio::test]
async fn request_exit_evm_fee_failure_returns_to_the_stake() -> anyhow::Result<()> {
    let mut w = World::finalized(false).await?;
    let stake_before = w.member_stake().await?;
    w.unregister_relayer().await?;
    let r = w.small_action_evm("request_exit", FEE).await?;
    assert!(r.is_success(), "request_exit_evm: {:?}", failure_text(&r));
    assert!(has_event(&r, "exit_requested") && has_event(&r, "relayer_fee_failed") && !has_event(&r, "relayer_fee_paid"), "{:?}", r.logs());
    assert_eq!(w.member_stake().await?, stake_before, "the fee came back");
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-118-03: maxFee 0 takes nothing; set_auto_settle_evm with a fee takes it and the flag is set
#[tokio::test]
async fn set_auto_settle_evm_fee_and_max_fee_zero() -> anyhow::Result<()> {
    let mut w = World::finalized(false).await?;
    let stake_before: u128 = w.member_stake().await?.parse()?;
    let relayer_before = w.usdt(&w.relayer).await?;
    let r = w.small_action_evm("auto_settle", 0).await?;
    assert!(r.is_success(), "{:?}", failure_text(&r));
    assert!(!has_event(&r, "relayer_fee_paid"));
    assert_eq!(w.usdt(&w.relayer).await?, relayer_before);
    let r = w.small_action_evm("auto_settle_off", FEE).await?;
    assert!(r.is_success(), "{:?}", failure_text(&r));
    println!("GAS set_auto_settle_evm with fee: {:.1} Tgas", tgas(&r));
    assert!(has_event(&r, "relayer_fee_paid"));
    assert_eq!(w.usdt(&w.relayer).await? - relayer_before, FEE);
    assert_eq!(w.member_stake().await?.parse::<u128>()?, stake_before - FEE);
    assert_eq!(w.f.circle.view("is_auto_settle", json!({"account_id": w.member})).await?, json!(false));
    w.f.assert_d6().await?;
    Ok(())
}

// TC-CT-118-04: join_evm with a signed fee is refused with an explicit error
#[tokio::test]
async fn join_evm_refuses_a_fee_ceiling() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let contract = f.circle.contract.id().to_string();
    let secret = SecretKey::from_slice(&[7u8; 32])?;
    let member = address(&secret);
    let deadline = f.circle.now_ns().await? / 1_000_000 + 3_600_000;
    let relayer = f.account("relayer").await?;
    let terms = "a".repeat(64);
    let join = relayer
        .call(f.circle.contract.id(), "join_evm")
        .args_json(json!({"address": member, "terms_hash": terms, "deadline": deadline, "max_fee": "1", "signature": action_sig(&secret, &contract, &member, "join", 0, "", &terms, 0, deadline, 1)}))
        .deposit(NearToken::from_millinear(350))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert!(failure_text(&join).unwrap_or_default().contains("ERR_EVM_FEE_NOT_ALLOWED"), "{:?}", failure_text(&join));
    Ok(())
}

// TC-CT-118-05: get_epoch shows the rules in force when each epoch opened
#[tokio::test]
async fn get_epoch_params_show_the_rules_of_each_epoch() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let dao = f.account("dao").await?;
    let mut params = Funded::params_short();
    params["relayer_fee"] = json!("500000");
    assert!(f.circle.call_as(&dao, "set_params", json!({"params": params})).await?.is_success());
    let c = &f.circle;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
    assert_eq!(f.circle.view("get_epoch", json!({"epoch": 1})).await?["params"]["relayer_fee"], "0");
    assert_eq!(f.circle.view("get_epoch", json!({"epoch": 2})).await?["params"]["relayer_fee"], "500000");
    assert_eq!(f.circle.view("get_params", json!({})).await?["relayer_fee"], "500000");
    Ok(())
}
