//! Helpers shared by the sandbox tests.
#![allow(dead_code)]

use std::process::Command;
use std::sync::Once;

/// Path of the wasm of a probe crate under `probes/<dir>`, building it on first use. CI builds only
/// the contract itself, so a probe test builds its own wasm (cargo-near is installed there).
pub fn probe_wasm(dir: &str) -> String {
    static BUILD: Once = Once::new();
    let wasm = format!("target/near/{dir}/{dir}.wasm");
    BUILD.call_once(|| {
        if std::path::Path::new(&wasm).exists() {
            return;
        }
        let status = Command::new("cargo")
            .args(["near", "build", "non-reproducible-wasm", "--manifest-path"])
            .arg(format!("probes/{dir}/Cargo.toml"))
            .status()
            .expect("run cargo near build");
        assert!(status.success(), "cargo near build failed for probes/{dir}");
    });
    wasm
}

// ---- Circle fixture for sandbox flows (F3 and later) ----------------------------------------

#[allow(dead_code)]
pub mod circle {
    use std::collections::BTreeMap;

    use ed25519_dalek::{Signer, SigningKey};
    use near_sdk::borsh;
    use near_sdk::{AccountId, PublicKey};
    use near_workspaces::network::Sandbox;
    use near_workspaces::result::ExecutionFinalResult;
    use near_workspaces::types::Gas;
    use near_workspaces::{Account, Contract, Worker};
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use x207_circle::member::{Member, MemberStatus};

    pub const CREDIT: u128 = 1_000_000;
    const SEC: u64 = 1_000_000_000;

    /// Short windows inside the legal ranges (spec 3.4): 5 min epoch, 1 min proposals, 15 min payment.
    pub fn short_params() -> Value {
        json!({
            "epoch_length": 300, "payment_window": 900, "proposal_window": 60, "base_limit_ratio": 8000,
            "limit_growth": 1000, "limit_cap": "1000000000", "limit_decay": 0,
            "fees": {"clearing_fee_bps": 0, "solver_fee_bps": 0}, "admission": "open"
        })
    }

    pub struct Circle {
        pub worker: Worker<Sandbox>,
        pub contract: Contract,
        pub keys: BTreeMap<String, SigningKey>,
        pub seqs: BTreeMap<(String, String, u64), u64>,
    }

    pub fn account_of(name: &str) -> String {
        format!("{name}.test.near")
    }

    /// Spec 3.3, written byte by byte and independent of `canonical.rs`.
    pub fn signing_hash(circle: &str, payer: &str, payee: &str, epoch: u64, seq: u64, cumulative: &str, resource: &str) -> [u8; 32] {
        let message = format!(
            r#"{{"circle":"{circle}","payer":"{payer}","payee":"{payee}","epoch":{epoch},"seq":{seq},"cumulativeAmount":"{cumulative}","unit":"credit","resource":"{resource}"}}"#
        );
        let nonce: [u8; 32] = Sha256::digest(format!("x207:{payer}:{payee}:{epoch}:{seq}")).into();
        let mut payload = Vec::new();
        payload.extend_from_slice(&2_147_484_061u32.to_le_bytes());
        payload.extend_from_slice(&(message.len() as u32).to_le_bytes());
        payload.extend_from_slice(message.as_bytes());
        payload.extend_from_slice(&nonce);
        payload.extend_from_slice(&(circle.len() as u32).to_le_bytes());
        payload.extend_from_slice(circle.as_bytes());
        payload.push(0);
        Sha256::digest(&payload).into()
    }

    fn voucher_key(name: &str) -> SigningKey {
        let seed: [u8; 32] = Sha256::digest(format!("voucher-key:{name}")).into();
        SigningKey::from_bytes(&seed)
    }

    fn pubkey_of(key: &SigningKey) -> String {
        format!("ed25519:{}", near_sdk::bs58::encode(key.verifying_key().to_bytes()).into_string())
    }

    impl Circle {
        /// Deploys the built contract and runs `new` with `params`.
        pub async fn deploy(params: Value) -> anyhow::Result<Circle> {
            let worker = near_workspaces::sandbox().await?;
            let contract = worker.dev_deploy(&std::fs::read("target/near/x207_circle.wasm")?).await?;
            contract
                .call("new")
                .args_json(json!({
                    "owner_id": account_of("dao"), "circle_id": "agents-main", "usdt_token_id": account_of("usdt"),
                    "intents_id": account_of("intents"), "params": params
                }))
                .transact()
                .await?
                .into_result()?;
            Ok(Circle { worker, contract, keys: BTreeMap::new(), seqs: BTreeMap::new() })
        }

        /// Writes an `active` member straight into contract storage (D1, prefix `m`) with a
        /// voucher key of its own. Stands in for `join` plus stake, which are separate items.
        pub async fn seed_member(&mut self, name: &str, stake: u128, limit: u128) -> anyhow::Result<()> {
            let key = voucher_key(name);
            let id: AccountId = account_of(name).parse()?;
            let member = Member {
                account_id: id.clone(),
                pubkey_voucher: pubkey_of(&key).parse::<PublicKey>()?,
                terms_hash: "a".repeat(64),
                stake,
                limit,
                status: MemberStatus::Active,
                admitted: false,
                joined_at: 1,
                settled_history: 0,
                last_active_epoch: 1,
                exit_requested_epoch: None,
                withdraw_pending: false,
                reactivate_epoch: None,
            };
            let mut storage_key = b"m".to_vec();
            storage_key.extend(borsh::to_vec(&id)?);
            self.worker.patch_state(self.contract.id(), &storage_key, &borsh::to_vec(&member)?).await?;
            self.keys.insert(name.to_string(), key);
            Ok(())
        }

        /// `register_cumulative` for the next `seq` of the pair in `epoch`. May fail; the caller checks.
        pub async fn register(&mut self, payer: &str, payee: &str, epoch: u64, cumulative: u128) -> anyhow::Result<ExecutionFinalResult> {
            let seq = self.seqs.entry((payer.into(), payee.into(), epoch)).or_insert(0);
            *seq += 1;
            let seq = *seq;
            let resource = "https://example.test/api";
            let (circle, payer_id, payee_id) = (self.contract.id().to_string(), account_of(payer), account_of(payee));
            let hash = signing_hash(&circle, &payer_id, &payee_id, epoch, seq, &cumulative.to_string(), resource);
            let signature = format!("ed25519:{}", near_sdk::bs58::encode(self.keys[payer].sign(&hash).to_bytes()).into_string());
            let voucher = json!({
                "circle": circle, "payer": payer_id, "payee": payee_id, "epoch": epoch, "seq": seq,
                "cumulativeAmount": cumulative.to_string(), "unit": "credit", "resource": resource
            });
            Ok(self
                .contract
                .call("register_cumulative")
                .args_json(json!({"voucher": voucher, "signature": signature}))
                .gas(Gas::from_tgas(100))
                .transact()
                .await?)
        }

        pub async fn view(&self, method: &str, args: Value) -> anyhow::Result<Value> {
            Ok(self.contract.view(method).args_json(args).await?.json()?)
        }

        pub async fn epoch(&self, epoch: u64) -> anyhow::Result<Value> {
            self.view("get_epoch", json!({"epoch": epoch})).await
        }

        pub async fn now_ns(&self) -> anyhow::Result<u64> {
            Ok(self.worker.view_block().await?.timestamp())
        }

        /// Fast-forwards blocks until the block time reaches `target_ns`.
        pub async fn advance_to(&self, target_ns: u64) -> anyhow::Result<()> {
            while self.now_ns().await? < target_ns {
                let missing_s = (target_ns - self.now_ns().await?).div_ceil(SEC);
                self.worker.fast_forward(missing_s.clamp(20, 4000)).await?;
            }
            Ok(())
        }

        /// A timestamp field of `get_epoch` as nanoseconds.
        pub async fn epoch_ts(&self, epoch: u64, field: &str) -> anyhow::Result<u64> {
            Ok(self.epoch(epoch).await?[field].as_str().expect(field).parse()?)
        }

        pub async fn call_as(&self, who: &Account, method: &str, args: Value) -> anyhow::Result<ExecutionFinalResult> {
            Ok(who.call(self.contract.id(), method).args_json(args).gas(Gas::from_tgas(200)).transact().await?)
        }

        pub async fn call_keeper(&self, method: &str, args: Value) -> anyhow::Result<ExecutionFinalResult> {
            Ok(self.contract.call(method).args_json(args).gas(Gas::from_tgas(200)).transact().await?)
        }
    }

    /// Text of a failed outcome (all error layers), or `None` when it succeeded.
    pub fn failure_text(result: &ExecutionFinalResult) -> Option<String> {
        result.clone().into_result().err().map(|e| format!("{e:?}"))
    }

    /// The `EVENT_JSON:` logs of a result in order, as `(event, data[0])`.
    pub fn events(result: &ExecutionFinalResult) -> Vec<(String, Value)> {
        result
            .logs()
            .iter()
            .filter_map(|l| l.strip_prefix("EVENT_JSON:"))
            .map(|body| serde_json::from_str::<Value>(body).expect("event json"))
            .map(|v| (v["event"].as_str().unwrap().to_string(), v["data"][0].clone()))
            .collect()
    }
}
