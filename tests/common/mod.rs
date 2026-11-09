//! Helpers shared by the sandbox tests.
#![allow(dead_code)]

use std::process::Command;
use std::sync::Mutex;

/// Path of the wasm of a probe crate under `probes/<dir>`, building it on first use. CI builds only
/// the contract itself, so a probe test builds its own wasm (cargo-near is installed there).
pub fn probe_wasm(dir: &str) -> String {
    // One build at a time: tests run in parallel and cargo-near locks the target folder anyway.
    static BUILD: Mutex<()> = Mutex::new(());
    let wasm = format!("target/near/{dir}/{dir}.wasm");
    let _guard = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    if std::path::Path::new(&wasm).exists() {
        return wasm;
    }
    let status = Command::new("cargo")
        .args(["near", "build", "non-reproducible-wasm", "--manifest-path"])
        .arg(format!("probes/{dir}/Cargo.toml"))
        .status()
        .expect("run cargo near build");
    assert!(status.success(), "cargo near build failed for probes/{dir}");
    wasm
}

#[allow(dead_code)]
pub mod ft_escrow;
#[allow(dead_code)]
pub mod mt_escrow;

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
        /// Most blocks one `fast_forward` may skip. A sandbox keeps outcomes of old blocks only for a few
        /// epochs of height, so a follower that reads the chain through RPC needs short jumps.
        pub max_jump: u64,
        /// Account that sends the permissionless calls (`close_epoch`, `commit_epoch`, vouchers). A call the
        /// contract signs for itself runs its receipt in the block that includes it, which a block-by-block
        /// RPC reader would see differently from real traffic.
        pub keeper: near_workspaces::Account,
    }

    pub fn account_of(name: &str) -> String {
        format!("{name}.test.near")
    }

    /// Spec 3.3, written byte by byte and independent of `canonical.rs`.
    pub fn signing_hash(
        circle: &str,
        payer: &str,
        payee: &str,
        epoch: u64,
        seq: u64,
        cumulative: &str,
        resource: &str,
    ) -> [u8; 32] {
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
        format!(
            "ed25519:{}",
            near_sdk::bs58::encode(key.verifying_key().to_bytes()).into_string()
        )
    }

    impl Circle {
        /// Deploys the built contract and runs `new` with `params`.
        pub async fn deploy(params: Value) -> anyhow::Result<Circle> {
            Self::deploy_with(params, account_of("usdt")).await
        }

        /// Same, with `usdt` as the token account the contract accepts.
        pub async fn deploy_with(params: Value, usdt: String) -> anyhow::Result<Circle> {
            // The sandbox node is pinned to 2.13.4 (the one of near-workspaces 0.23): with the 2.14.0 of near-workspaces 0.24 the
            // BE-022 indexer test halts every run with "UnknownBlockError ... garbage collected" (3 of 3 runs, also on a quiet machine).
            let worker = near_workspaces::sandbox_with_version("2.13.4").await?;
            let contract = worker
                .dev_deploy(&std::fs::read("target/near/x207_circle.wasm")?)
                .await?;
            contract
                .call("new")
                .args_json(json!({
                    "owner_id": account_of("dao"), "circle_id": "agents-main", "usdt_token_id": usdt,
                    "intents_id": account_of("intents"), "params": params
                }))
                .transact()
                .await?
                .into_result()?;
            let keeper = worker.dev_create_account().await?;
            Ok(Circle {
                worker,
                contract,
                keys: BTreeMap::new(),
                seqs: BTreeMap::new(),
                max_jump: 4000,
                keeper,
            })
        }

        /// Writes an `active` member straight into contract storage (D1, prefix `m`) with a
        /// voucher key of its own. Stands in for `join` plus stake, which are separate items.
        pub async fn seed_member(
            &mut self,
            name: &str,
            stake: u128,
            limit: u128,
        ) -> anyhow::Result<()> {
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
            self.worker
                .patch_state(self.contract.id(), &storage_key, &borsh::to_vec(&member)?)
                .await?;
            self.keys.insert(name.to_string(), key);
            Ok(())
        }

        /// `register_cumulative` for the next `seq` of the pair in `epoch`. May fail; the caller checks.
        pub async fn register(
            &mut self,
            payer: &str,
            payee: &str,
            epoch: u64,
            cumulative: u128,
        ) -> anyhow::Result<ExecutionFinalResult> {
            let seq = self
                .seqs
                .entry((payer.into(), payee.into(), epoch))
                .or_insert(0);
            *seq += 1;
            let seq = *seq;
            self.register_at(payer, payee, epoch, seq, cumulative).await
        }

        /// `register_cumulative` with an explicit `seq` (cumulative mode allows gaps).
        pub async fn register_at(
            &self,
            payer: &str,
            payee: &str,
            epoch: u64,
            seq: u64,
            cumulative: u128,
        ) -> anyhow::Result<ExecutionFinalResult> {
            let resource = "https://example.test/api";
            let (circle, payer_id, payee_id) = (
                self.contract.id().to_string(),
                account_of(payer),
                account_of(payee),
            );
            let hash = signing_hash(
                &circle,
                &payer_id,
                &payee_id,
                epoch,
                seq,
                &cumulative.to_string(),
                resource,
            );
            let signature = format!(
                "ed25519:{}",
                near_sdk::bs58::encode(self.keys[payer].sign(&hash).to_bytes()).into_string()
            );
            let voucher = json!({
                "circle": circle, "payer": payer_id, "payee": payee_id, "epoch": epoch, "seq": seq,
                "cumulativeAmount": cumulative.to_string(), "unit": "credit", "resource": resource
            });
            Ok(self
                .keeper
                .call(self.contract.id(), "register_cumulative")
                .args_json(json!({"voucher": voucher, "signature": signature}))
                .gas(Gas::from_tgas(100))
                .transact()
                .await?)
        }

        /// A voucher of the next `seq` of the pair and its signature, for methods that take them as arguments (CT-075).
        pub fn next_voucher(
            &mut self,
            payer: &str,
            payee: &str,
            epoch: u64,
            cumulative: u128,
        ) -> (Value, String) {
            let seq = self
                .seqs
                .entry((payer.into(), payee.into(), epoch))
                .or_insert(0);
            *seq += 1;
            let seq = *seq;
            let resource = "https://example.test/api";
            let (circle, payer_id, payee_id) = (
                self.contract.id().to_string(),
                account_of(payer),
                account_of(payee),
            );
            let hash = signing_hash(
                &circle,
                &payer_id,
                &payee_id,
                epoch,
                seq,
                &cumulative.to_string(),
                resource,
            );
            let signature = format!(
                "ed25519:{}",
                near_sdk::bs58::encode(self.keys[payer].sign(&hash).to_bytes()).into_string()
            );
            let voucher = json!({
                "circle": circle, "payer": payer_id, "payee": payee_id, "epoch": epoch, "seq": seq,
                "cumulativeAmount": cumulative.to_string(), "unit": "credit", "resource": resource
            });
            (voucher, signature)
        }

        /// `register_in_bundle` for the next `seq` of the pair (CT-064).
        pub async fn register_in_bundle(
            &mut self,
            bundle_id: u64,
            payer: &str,
            payee: &str,
            epoch: u64,
            cumulative: u128,
        ) -> anyhow::Result<ExecutionFinalResult> {
            let seq = self
                .seqs
                .entry((payer.into(), payee.into(), epoch))
                .or_insert(0);
            *seq += 1;
            let seq = *seq;
            let resource = "https://example.test/api";
            let (circle, payer_id, payee_id) = (
                self.contract.id().to_string(),
                account_of(payer),
                account_of(payee),
            );
            let hash = signing_hash(
                &circle,
                &payer_id,
                &payee_id,
                epoch,
                seq,
                &cumulative.to_string(),
                resource,
            );
            let signature = format!(
                "ed25519:{}",
                near_sdk::bs58::encode(self.keys[payer].sign(&hash).to_bytes()).into_string()
            );
            let voucher = json!({
                "circle": circle, "payer": payer_id, "payee": payee_id, "epoch": epoch, "seq": seq,
                "cumulativeAmount": cumulative.to_string(), "unit": "credit", "resource": resource
            });
            Ok(self
                .keeper
                .call(self.contract.id(), "register_in_bundle")
                .args_json(
                    json!({"bundle_id": bundle_id, "voucher": voucher, "signature": signature}),
                )
                .gas(Gas::from_tgas(100))
                .transact()
                .await?)
        }

        /// Canonical terms of a bundle, its `terms_hash` and the signature of `signer` (sdk/spec/barter-format.md,
        /// written byte by byte and independent of `bundle.rs`). Legs are `(payer, payee, max_amount)` by name.
        pub fn bundle_terms(&self, epoch: u64, legs: &[(&str, &str, u128)]) -> (String, [u8; 32]) {
            let circle = self.contract.id().to_string();
            let mut named: Vec<(String, String, u128)> = legs
                .iter()
                .map(|(a, b, m)| (account_of(a), account_of(b), *m))
                .collect();
            named.sort_by(|x, y| {
                (x.0.as_bytes(), x.1.as_bytes()).cmp(&(y.0.as_bytes(), y.1.as_bytes()))
            });
            let legs_json: Vec<String> = named
                .iter()
                .map(|(a, b, m)| format!(r#"{{"payer":"{a}","payee":"{b}","max_amount":"{m}"}}"#))
                .collect();
            let message = format!(
                r#"{{"circle":"{circle}","epoch":{epoch},"legs":[{}]}}"#,
                legs_json.join(",")
            );
            let hash: [u8; 32] = Sha256::digest(message.as_bytes()).into();
            (message, hash)
        }

        /// Like `commit_bundle`, with `search` = `(payer, payee)` named as the seeker leg inside the signed terms (CT-082).
        /// `sign_without_search` signs the terms without that field, for the refusal test.
        pub async fn commit_bundle_with_search(
            &self,
            epoch: u64,
            legs: &[(&str, &str, u128)],
            search: (&str, &str),
            signers: &[&str],
            sign_without_search: bool,
        ) -> anyhow::Result<ExecutionFinalResult> {
            let (plain, _) = self.bundle_terms(epoch, legs);
            let message = if sign_without_search {
                plain
            } else {
                let cut = plain.len() - 1;
                format!(
                    r#"{},"search_leg":{{"payer":"{}","payee":"{}"}}}}"#,
                    &plain[..cut],
                    account_of(search.0),
                    account_of(search.1)
                )
            };
            let hash: [u8; 32] = Sha256::digest(message.as_bytes()).into();
            let signatures: Vec<serde_json::Value> =
                signers.iter().map(|s| json!({"account_id": account_of(s), "signature": self.sign_terms(s, &message, hash)})).collect();
            let legs_json: Vec<serde_json::Value> =
                legs.iter().map(|(a, b, m)| json!({"payer": account_of(a), "payee": account_of(b), "max_amount": m.to_string()})).collect();
            let bundle = json!({"epoch": epoch, "legs": legs_json, "search_leg": {"payer": account_of(search.0), "payee": account_of(search.1)}});
            self.call_keeper(
                "commit_bundle",
                json!({"bundle": bundle, "signatures": signatures}),
            )
            .await
        }

        /// CT-094: like `commit_bundle` for a swap whose signed terms name an arbiter: `deal` = `(arbiter account id, agreement_hash, confirm_by in ns)`. The three keys come
        /// after the legs, in this order, as in `terms_message` of the contract.
        pub async fn commit_bundle_with_deal(
            &self,
            epoch: u64,
            legs: &[(&str, &str, u128)],
            deal: (&str, Option<&str>, u64),
            signers: &[&str],
        ) -> anyhow::Result<ExecutionFinalResult> {
            let (plain, _) = self.bundle_terms(epoch, legs);
            let cut = plain.len() - 1;
            let agreement = deal
                .1
                .map(|h| format!(r#","agreement_hash":"{h}""#))
                .unwrap_or_default();
            let message = format!(
                r#"{},"arbiter":"{}"{agreement},"confirm_by":"{}"}}"#,
                &plain[..cut],
                deal.0,
                deal.2
            );
            let hash: [u8; 32] = Sha256::digest(message.as_bytes()).into();
            let signatures: Vec<serde_json::Value> =
                signers.iter().map(|s| json!({"account_id": account_of(s), "signature": self.sign_terms(s, &message, hash)})).collect();
            let legs_json: Vec<serde_json::Value> =
                legs.iter().map(|(a, b, m)| json!({"payer": account_of(a), "payee": account_of(b), "max_amount": m.to_string()})).collect();
            let mut bundle = json!({"epoch": epoch, "legs": legs_json, "arbiter": deal.0, "confirm_by": deal.2.to_string()});
            if let Some(h) = deal.1 {
                bundle["agreement_hash"] = json!(h);
            }
            self.call_keeper(
                "commit_bundle",
                json!({"bundle": bundle, "signatures": signatures}),
            )
            .await
        }

        pub fn sign_terms(&self, signer: &str, message: &str, terms_hash: [u8; 32]) -> String {
            let circle = self.contract.id().to_string();
            let mut payload = Vec::new();
            payload.extend_from_slice(&2_147_484_061u32.to_le_bytes());
            payload.extend_from_slice(&(message.len() as u32).to_le_bytes());
            payload.extend_from_slice(message.as_bytes());
            payload.extend_from_slice(&terms_hash);
            payload.extend_from_slice(&(circle.len() as u32).to_le_bytes());
            payload.extend_from_slice(circle.as_bytes());
            payload.push(0);
            let digest: [u8; 32] = Sha256::digest(&payload).into();
            format!(
                "ed25519:{}",
                near_sdk::bs58::encode(self.keys[signer].sign(&digest).to_bytes()).into_string()
            )
        }

        /// `commit_bundle` by the keeper with signatures from `signers` over the terms.
        pub async fn commit_bundle(
            &self,
            epoch: u64,
            legs: &[(&str, &str, u128)],
            signers: &[&str],
        ) -> anyhow::Result<ExecutionFinalResult> {
            let (message, hash) = self.bundle_terms(epoch, legs);
            let signatures: Vec<serde_json::Value> =
                signers.iter().map(|s| json!({"account_id": account_of(s), "signature": self.sign_terms(s, &message, hash)})).collect();
            let legs_json: Vec<serde_json::Value> =
                legs.iter().map(|(a, b, m)| json!({"payer": account_of(a), "payee": account_of(b), "max_amount": m.to_string()})).collect();
            self.call_keeper(
                "commit_bundle",
                json!({"bundle": {"epoch": epoch, "legs": legs_json}, "signatures": signatures}),
            )
            .await
        }

        /// The voucher public key of `name`, created on first use.
        pub fn key_for(&mut self, name: &str) -> String {
            let key = self
                .keys
                .entry(name.to_string())
                .or_insert_with(|| voucher_key(name));
            pubkey_of(key)
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
            loop {
                // One read per pass: a second read can already be past the target.
                let now = self.now_ns().await?;
                if now >= target_ns {
                    break;
                }
                let missing_s = (target_ns - now).div_ceil(SEC);
                self.worker
                    .fast_forward(missing_s.clamp(20, self.max_jump.max(20)))
                    .await?;
            }
            Ok(())
        }

        /// A timestamp field of `get_epoch` as nanoseconds.
        pub async fn epoch_ts(&self, epoch: u64, field: &str) -> anyhow::Result<u64> {
            Ok(self.epoch(epoch).await?[field]
                .as_str()
                .expect(field)
                .parse()?)
        }

        pub async fn call_as(
            &self,
            who: &Account,
            method: &str,
            args: Value,
        ) -> anyhow::Result<ExecutionFinalResult> {
            Ok(who
                .call(self.contract.id(), method)
                .args_json(args)
                .gas(Gas::from_tgas(200))
                .transact()
                .await?)
        }

        pub async fn call_keeper(
            &self,
            method: &str,
            args: Value,
        ) -> anyhow::Result<ExecutionFinalResult> {
            Ok(self
                .keeper
                .call(self.contract.id(), method)
                .args_json(args)
                .gas(Gas::from_tgas(200))
                .transact()
                .await?)
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
            .map(|v| {
                (
                    v["event"].as_str().unwrap().to_string(),
                    v["data"][0].clone(),
                )
            })
            .collect()
    }

    /// Text of every failed receipt (not only the final status), for refund tests where the
    /// token call succeeds and the receiver's `ft_on_transfer` fails.
    pub fn receipt_failures(result: &ExecutionFinalResult) -> String {
        format!("{:?}", result.failures())
    }
}

/// The circle with a real NEP-141 mock as USDT, a second mock as a wrong token, and member accounts.
#[allow(dead_code)]
pub mod funded {
    use std::collections::BTreeMap;

    use near_workspaces::result::ExecutionFinalResult;
    use near_workspaces::types::{Gas, NearToken};
    use near_workspaces::{Account, Contract};
    use serde_json::{json, Value};

    use super::circle::{account_of, short_params, Circle, CREDIT};

    pub struct Funded {
        pub circle: Circle,
        pub usdt: Contract,
        pub palsu: Contract,
        /// `mock_intents` when deployed with `deploy_with_intents`, as the `intents_id` of the circle.
        pub intents: Option<Contract>,
        pub accounts: BTreeMap<String, Account>,
    }

    async fn circle_worker_keeper(
        worker: &near_workspaces::Worker<near_workspaces::network::Sandbox>,
    ) -> anyhow::Result<Account> {
        Ok(worker.dev_create_account().await?)
    }

    async fn token(
        circle_worker: &near_workspaces::Worker<near_workspaces::network::Sandbox>,
    ) -> anyhow::Result<Contract> {
        let wasm = std::fs::read(super::probe_wasm("mock_ft"))?;
        let ft = circle_worker.dev_deploy(&wasm).await?;
        ft.call("new").transact().await?.into_result()?;
        Ok(ft)
    }

    impl Funded {
        /// `F0`: contract with default or short params, USDT mock, wrong-token mock. No members yet.
        pub async fn deploy(params: Option<Value>) -> anyhow::Result<Funded> {
            Self::deploy_inner(params, false).await
        }

        /// Same, with the `mock_intents` receiver as `intents_id` (claims to Intents, ADR-19).
        pub async fn deploy_with_intents(params: Option<Value>) -> anyhow::Result<Funded> {
            Self::deploy_inner(params, true).await
        }

        async fn deploy_inner(params: Option<Value>, with_intents: bool) -> anyhow::Result<Funded> {
            let worker = near_workspaces::sandbox_with_version("2.13.4").await?;
            let usdt = token(&worker).await?;
            let palsu = token(&worker).await?;
            let wasm = std::fs::read("target/near/x207_circle.wasm")?;
            let contract = worker.dev_deploy(&wasm).await?;
            let intents = if with_intents {
                let mock = worker
                    .dev_deploy(&std::fs::read(super::probe_wasm("mock_intents"))?)
                    .await?;
                mock.call("new").transact().await?.into_result()?;
                Some(mock)
            } else {
                None
            };
            let intents_id = intents
                .as_ref()
                .map_or_else(|| account_of("intents"), |m| m.id().to_string());
            contract
                .call("new")
                .args_json(json!({
                    "owner_id": account_of("dao"), "circle_id": "agents-main", "usdt_token_id": usdt.id(),
                    "intents_id": intents_id, "params": params
                }))
                .transact()
                .await?
                .into_result()?;
            let keeper = circle_worker_keeper(&worker).await?;
            let circle = Circle {
                worker,
                contract,
                keys: BTreeMap::new(),
                seqs: BTreeMap::new(),
                max_jump: 4000,
                keeper,
            };
            Ok(Funded {
                circle,
                usdt,
                palsu,
                intents,
                accounts: BTreeMap::new(),
            })
        }

        pub fn params_short() -> Value {
            short_params()
        }

        /// Creates `name.test.near` with 1.000 USDT and 1.000 of the wrong token.
        pub async fn account(&mut self, name: &str) -> anyhow::Result<Account> {
            if let Some(a) = self.accounts.get(name) {
                return Ok(a.clone());
            }
            let root = self.circle.worker.root_account()?;
            let account = root
                .create_subaccount(name)
                .initial_balance(NearToken::from_near(20))
                .transact()
                .await?
                .into_result()?;
            for ft in [&self.usdt, &self.palsu] {
                ft.call("mint")
                    .args_json(
                        json!({"account_id": account.id(), "amount": (1000 * CREDIT).to_string()}),
                    )
                    .transact()
                    .await?
                    .into_result()?;
            }
            self.accounts.insert(name.to_string(), account.clone());
            Ok(account)
        }

        /// `join` with `deposit` and the voucher key of `name`.
        pub async fn join(
            &mut self,
            name: &str,
            deposit: NearToken,
        ) -> anyhow::Result<ExecutionFinalResult> {
            let account = self.account(name).await?;
            let pubkey = self.circle.key_for(name);
            Ok(account
                .call(self.circle.contract.id(), "join")
                .args_json(json!({"circle_id": "agents-main", "pubkey_voucher": pubkey, "terms_hash": "a".repeat(64)}))
                .deposit(deposit)
                .gas(Gas::from_tgas(100))
                .transact()
                .await?)
        }

        pub fn join_deposit() -> NearToken {
            NearToken::from_millinear(350)
        }

        /// `token.ft_transfer_call(contract, amount, msg)` from `who`; the result value is the used amount.
        pub async fn transfer_call(
            &self,
            token: &Contract,
            who: &Account,
            amount: u128,
            msg: &str,
        ) -> anyhow::Result<ExecutionFinalResult> {
            Ok(who
                .call(token.id(), "ft_transfer_call")
                .args_json(json!({"receiver_id": self.circle.contract.id(), "amount": amount.to_string(), "memo": null, "msg": msg}))
                .deposit(NearToken::from_yoctonear(1))
                .gas(Gas::from_tgas(150))
                .transact()
                .await?)
        }

        pub async fn stake(
            &self,
            who: &Account,
            amount: u128,
        ) -> anyhow::Result<ExecutionFinalResult> {
            self.transfer_call(&self.usdt, who, amount, r#"{"action":"stake"}"#)
                .await
        }

        pub async fn balance(
            &self,
            token: &Contract,
            account: &near_workspaces::AccountId,
        ) -> anyhow::Result<u128> {
            let value: String = token
                .view("ft_balance_of")
                .args_json(json!({"account_id": account}))
                .await?
                .json()?;
            Ok(value.parse()?)
        }

        pub async fn member(&self, name: &str) -> anyhow::Result<Value> {
            self.circle
                .view("member", json!({"account_id": account_of(name)}))
                .await
        }

        /// I-04 (`assert_d6`): the five components sum to the USDT balance of the contract, and
        /// `total_stake` is the sum of the member stakes.
        pub async fn assert_d6(&self) -> anyhow::Result<()> {
            let pool = self.circle.view("get_pool", json!({})).await?;
            let balance = self.balance(&self.usdt, self.circle.contract.id()).await?;
            let accounted: u128 = pool["accounted"].as_str().unwrap().parse()?;
            assert_eq!(
                accounted, balance,
                "accounted {accounted} != USDT balance {balance}"
            );
            let members = self
                .circle
                .view("list_members", json!({"from_index": 0, "limit": 100}))
                .await?;
            let stake: u128 = members
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["stake"].as_str().unwrap().parse::<u128>().unwrap())
                .sum();
            assert_eq!(
                pool["total_stake"].as_str().unwrap().parse::<u128>()?,
                stake,
                "total_stake != sum of stakes"
            );
            Ok(())
        }
    }
}

/// Full-flow helpers built on the real `join`, stake, and voucher paths.
#[allow(dead_code)]
pub mod flow {
    use serde_json::{json, Value};

    use super::circle::{account_of, failure_text, Circle, CREDIT};
    use super::funded::Funded;

    pub fn edge(debtor: &str, creditor: &str, cleared: u128) -> Value {
        json!({"debtor": account_of(debtor), "creditor": account_of(creditor), "cleared": cleared.to_string()})
    }

    /// `S1` of the test case file, on a real circle: three staked members (100 each), vouchers
    /// scraper->gpu 60, gpu->translator 25, translator->scraper 25, closed, cleared by one cycle of 25,
    /// and committed. scraper now owes 35, gpu is owed 35. The clock stands at `commit`.
    pub async fn s1_committed() -> anyhow::Result<Funded> {
        s1_committed_on(Funded::deploy(Some(Funded::params_short())).await?).await
    }

    /// Same on a circle deployed by the caller (for example `deploy_with_intents`).
    pub async fn s1_committed_on(f: Funded) -> anyhow::Result<Funded> {
        s1_committed_exit(f, &[]).await
    }

    /// `s1_committed_on` where each of `exiters` calls `request_exit` in epoch 1, before the epoch closes.
    pub async fn s1_committed_exit(mut f: Funded, exiters: &[&str]) -> anyhow::Result<Funded> {
        for name in ["scraper", "gpu", "translator"] {
            let account = f.account(name).await?;
            let join = f.join(name, Funded::join_deposit()).await?;
            assert!(join.is_success(), "join {name}: {:?}", failure_text(&join));
            let stake = f.stake(&account, 100 * CREDIT).await?;
            assert!(
                stake.is_success(),
                "stake {name}: {:?}",
                failure_text(&stake)
            );
        }
        for (payer, payee, amount) in [
            ("scraper", "gpu", 60),
            ("gpu", "translator", 25),
            ("translator", "scraper", 25),
        ] {
            let result = f.circle.register(payer, payee, 1, amount * CREDIT).await?;
            assert!(
                result.is_success(),
                "register {payer}->{payee}: {:?}",
                failure_text(&result)
            );
        }
        for name in exiters {
            let exit = request_exit(&f, name).await?;
            assert!(
                exit.is_success(),
                "request_exit {name}: {:?}",
                failure_text(&exit)
            );
        }
        let c = &f.circle;
        // The solver account is created first: the proposal window is only 60 s of block time.
        let solver = c.worker.dev_create_account().await?;
        c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
        let close = c.call_keeper("close_epoch", json!({"epoch": 1})).await?;
        assert!(close.is_success(), "{:?}", failure_text(&close));
        let edges = json!([
            edge("scraper", "gpu", 25 * CREDIT),
            edge("gpu", "translator", 25 * CREDIT),
            edge("translator", "scraper", 25 * CREDIT)
        ]);
        let submit = c
            .call_as(
                &solver,
                "submit_clearing",
                json!({"epoch": 1, "edges": edges}),
            )
            .await?;
        assert!(submit.is_success(), "{:?}", failure_text(&submit));
        c.advance_to(c.epoch_ts(1, "proposal_deadline").await?)
            .await?;
        let commit = c.call_keeper("commit_epoch", json!({"epoch": 1})).await?;
        assert!(commit.is_success(), "{:?}", failure_text(&commit));
        Ok(f)
    }

    pub async fn finalize(
        c: &Circle,
    ) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        c.advance_to(c.epoch_ts(1, "payment_deadline").await?)
            .await?;
        c.call_keeper("finalize_epoch", json!({"epoch": 1})).await
    }

    pub async fn request_exit(
        f: &Funded,
        name: &str,
    ) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        Ok(f.accounts[name]
            .call(f.circle.contract.id(), "request_exit")
            .deposit(near_workspaces::types::NearToken::from_yoctonear(1))
            .gas(near_workspaces::types::Gas::from_tgas(100))
            .transact()
            .await?)
    }

    pub async fn withdraw_stake(
        f: &Funded,
        name: &str,
    ) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        Ok(f.accounts[name]
            .call(f.circle.contract.id(), "withdraw_stake")
            .deposit(near_workspaces::types::NearToken::from_yoctonear(1))
            .gas(near_workspaces::types::Gas::from_tgas(100))
            .transact()
            .await?)
    }

    /// CT-099 (B8-F1): `prune_epoch` waits for `prune_delay_epochs` (2): closes the running epochs until epoch 3 runs, so that epoch 1 may be pruned.
    pub async fn pass_prune_delay(c: &Circle) -> anyhow::Result<()> {
        loop {
            let running = c.view("get_epoch", json!({})).await?["epoch"]
                .as_u64()
                .unwrap();
            if running >= 3 {
                return Ok(());
            }
            c.advance_to(c.epoch_ts(running, "cutoff_at").await?)
                .await?;
            let close = c
                .call_keeper("close_epoch", json!({"epoch": running}))
                .await?;
            assert!(
                close.is_success(),
                "close_epoch {running}: {:?}",
                failure_text(&close)
            );
        }
    }

    /// Epoch 1 of a circle with no vouchers: closed, committed, and finalized after its windows.
    pub async fn idle_epoch_finalized(f: &Funded) -> anyhow::Result<()> {
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
        let fin = finalize(c).await?;
        assert!(fin.is_success(), "{:?}", failure_text(&fin));
        Ok(())
    }

    /// Joins and stakes 100 for each name.
    pub async fn staked_members(f: &mut Funded, names: &[&str]) -> anyhow::Result<()> {
        for name in names {
            let account = f.account(name).await?;
            assert!(f.join(name, Funded::join_deposit()).await?.is_success());
            assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
        }
        Ok(())
    }
}
