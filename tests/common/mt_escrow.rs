//! Fixture of the sandbox tests of the class A multi-token escrow (CT-092): a circle with three staked members, the asset type `mt:usdc` and the
//! NEP-245 token of `probes/mock_mt`. `alice` is the grantor and holds `10 * AMOUNT` of the token `TOKEN`.

use super::circle::CREDIT;
use super::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::{Account, Contract};
use serde_json::{json, Value};

pub const PRICE: u128 = 10 * CREDIT;
pub const AMOUNT: u128 = 1_000;
pub const TOKEN: &str = "nep141:usdc.test.near";

pub struct World {
    pub f: Funded,
    pub mt: Contract,
    pub alice: Account,
    pub bob: Account,
}

pub async fn world() -> anyhow::Result<World> {
    let mut f = Funded::deploy(None).await?;
    for name in ["alice", "bob", "carol"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 150 * CREDIT).await?.is_success());
    }
    let dao = f.account("dao").await?;
    let entry = json!({"asset_type": "mt:usdc", "class": "A", "delivery_methods": ["escrow_mt"], "accepted_proofs": [], "terms_hash": "5a".repeat(32)});
    assert!(f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": "mt:usdc", "entry": entry})).await?.is_success());
    let mt = deploy_mt(&f).await?;
    let (alice, bob) = (f.accounts["alice"].clone(), f.accounts["bob"].clone());
    mt.call("mint").args_json(json!({"account_id": alice.id(), "token_id": TOKEN, "amount": (10 * AMOUNT).to_string()})).transact().await?.into_result()?;
    Ok(World { f, mt, alice, bob })
}

pub async fn deploy_mt(f: &Funded) -> anyhow::Result<Contract> {
    let wasm = std::fs::read(super::probe_wasm("mock_mt"))?;
    let mt = f.circle.worker.dev_deploy(&wasm).await?;
    mt.call("new").transact().await?.into_result()?;
    Ok(mt)
}

pub fn offer(expires_at: u64, mt: &str, token: &str, amount: u128) -> Value {
    json!({"input": {
        "asset_type": "mt:usdc", "delivery_method": "escrow_mt",
        "limits": {"scope": json!({"mt_contract": mt, "token_id": token, "amount": amount.to_string()}).to_string(), "quota": null, "expires_at": expires_at.to_string()},
        "unit": "once", "unit_price": PRICE.to_string(), "operator": null, "proof_hash": null, "proof_kind": null
    }})
}

impl World {
    pub async fn offer(&mut self, expires_in_ns: u64) -> anyhow::Result<u64> {
        let expires = self.f.circle.now_ns().await? + expires_in_ns;
        Ok(self.f.circle.call_as(&self.alice, "offer_grant", offer(expires, self.mt.id().as_str(), TOKEN, AMOUNT)).await?.json::<u64>()?)
    }

    pub async fn deposit_with(
        &self, mt: &Contract, from: &Account, token: &str, amount: u128, msg: &str,
    ) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        Ok(from
            .call(mt.id(), "mt_transfer_call")
            .args_json(json!({"receiver_id": self.f.circle.contract.id(), "token_id": token, "amount": amount.to_string(), "approval": null, "memo": null, "msg": msg}))
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(150))
            .transact()
            .await?)
    }

    pub async fn deposit(&self, from: &Account, amount: u128, grant_id: u64) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        self.deposit_with(&self.mt, from, TOKEN, amount, &json!({"action": "escrow", "grant_id": grant_id}).to_string()).await
    }

    pub async fn balance(&self, mt: &Contract, who: &str, token: &str) -> anyhow::Result<u128> {
        Ok(mt.view("mt_balance_of").args_json(json!({"account_id": who, "token_id": token})).await?.json::<String>()?.parse()?)
    }

    pub async fn of(&self, who: &Account) -> anyhow::Result<u128> {
        self.balance(&self.mt, who.id().as_str(), TOKEN).await
    }

    pub async fn in_circle(&self) -> anyhow::Result<u128> {
        self.balance(&self.mt, self.f.circle.contract.id().as_str(), TOKEN).await
    }

    pub async fn epoch(&self) -> anyhow::Result<u64> {
        Ok(self.f.circle.view("get_epoch", json!({})).await?["epoch"].as_u64().unwrap())
    }

    pub async fn buy(&mut self, buyer: &str, epoch: u64, cumulative: u128, grant_id: u64) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        let (voucher, signature) = self.f.circle.next_voucher(buyer, "alice", epoch, cumulative);
        let account = self.f.accounts[buyer].clone();
        Ok(self.f.circle.call_as(&account, "complete_escrow", json!({"grant_id": grant_id, "voucher": voucher, "signature": signature})).await?)
    }
}
