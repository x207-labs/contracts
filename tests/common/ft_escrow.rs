//! Fixture of the sandbox tests of the class A token escrow (CT-090, CT-091): a circle with three staked members, the asset type `rwa:bond`
//! and the token of `probes/mock_rwa`. `alice` is the grantor and holds `10 * AMOUNT` of it.

use super::circle::{account_of, CREDIT};
use super::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::{Account, Contract};
use serde_json::{json, Value};

pub const PRICE: u128 = 10 * CREDIT;
pub const AMOUNT: u128 = 1_000;

pub struct World {
    pub f: Funded,
    pub rwa: Contract,
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
    let entry = json!({"asset_type": "rwa:bond", "class": "A", "delivery_methods": ["escrow_ft"], "accepted_proofs": [], "terms_hash": "5a".repeat(32)});
    assert!(f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": "rwa:bond", "entry": entry})).await?.is_success());
    let rwa = deploy_rwa(&f).await?;
    let (alice, bob) = (f.accounts["alice"].clone(), f.accounts["bob"].clone());
    rwa.call("mint").args_json(json!({"account_id": alice.id(), "amount": (10 * AMOUNT).to_string()})).transact().await?.into_result()?;
    Ok(World { f, rwa, alice, bob })
}

pub async fn deploy_rwa(f: &Funded) -> anyhow::Result<Contract> {
    let wasm = std::fs::read(super::probe_wasm("mock_rwa"))?;
    let rwa = f.circle.worker.dev_deploy(&wasm).await?;
    rwa.call("new").transact().await?.into_result()?;
    Ok(rwa)
}

pub fn offer(expires_at: u64, ft: &str, amount: u128) -> Value {
    json!({"input": {
        "asset_type": "rwa:bond", "delivery_method": "escrow_ft",
        "limits": {"scope": json!({"ft_contract": ft, "amount": amount.to_string()}).to_string(), "quota": null, "expires_at": expires_at.to_string()},
        "unit": "once", "unit_price": PRICE.to_string(), "operator": null, "proof_hash": null, "proof_kind": null
    }})
}

impl World {
    pub async fn offer(&mut self, expires_in_ns: u64) -> anyhow::Result<u64> {
        let expires = self.f.circle.now_ns().await? + expires_in_ns;
        Ok(self.f.circle.call_as(&self.alice, "offer_grant", offer(expires, self.rwa.id().as_str(), AMOUNT)).await?.json::<u64>()?)
    }

    pub async fn deposit_with(&self, token: &Contract, from: &Account, amount: u128, msg: &str) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        Ok(from
            .call(token.id(), "ft_transfer_call")
            .args_json(json!({"receiver_id": self.f.circle.contract.id(), "amount": amount.to_string(), "memo": null, "msg": msg}))
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(150))
            .transact()
            .await?)
    }

    pub async fn deposit(&self, from: &Account, amount: u128, grant_id: u64) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        self.deposit_with(&self.rwa, from, amount, &json!({"action": "escrow", "grant_id": grant_id}).to_string()).await
    }

    pub async fn balance(&self, token: &Contract, who: &str) -> anyhow::Result<u128> {
        Ok(token.view("ft_balance_of").args_json(json!({"account_id": who})).await?.json::<String>()?.parse()?)
    }

    pub async fn rwa_of(&self, who: &Account) -> anyhow::Result<u128> {
        self.balance(&self.rwa, who.id().as_str()).await
    }

    pub async fn in_circle_of(&self, token: &Contract) -> anyhow::Result<u128> {
        self.balance(token, self.f.circle.contract.id().as_str()).await
    }

    pub async fn rwa_in_circle(&self) -> anyhow::Result<u128> {
        self.balance(&self.rwa, self.f.circle.contract.id().as_str()).await
    }

    pub async fn epoch(&self) -> anyhow::Result<u64> {
        Ok(self.f.circle.view("get_epoch", json!({})).await?["epoch"].as_u64().unwrap())
    }

    pub async fn buy(&mut self, buyer: &str, epoch: u64, cumulative: u128, grant_id: u64) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        let (voucher, signature) = self.f.circle.next_voucher(buyer, "alice", epoch, cumulative);
        let account = self.f.accounts[buyer].clone();
        Ok(self.f.circle.call_as(&account, "complete_escrow", json!({"grant_id": grant_id, "voucher": voucher, "signature": signature})).await?)
    }

    pub async fn whitelist(&self, accounts: &[&str]) -> anyhow::Result<()> {
        self.rwa.call("set_whitelist_on").args_json(json!({"on": true})).transact().await?.into_result()?;
        for a in accounts {
            self.rwa.call("set_whitelisted").args_json(json!({"account_id": a, "whitelisted": true})).transact().await?.into_result()?;
        }
        Ok(())
    }

    /// Nothing of a buyer's payment is left: no obligation, no debt, the escrow is held again and the grant is offered.
    pub async fn assert_clean_after_failure(&self, id: u64, epoch: u64, who: &str) -> anyhow::Result<()> {
        assert_eq!(self.f.circle.view("obligation", json!({"epoch": epoch, "debtor": account_of(who), "creditor": account_of("alice")})).await?, Value::Null);
        assert_eq!(self.f.circle.view("outstanding_debt", json!({"account_id": account_of(who)})).await?, "0");
        assert_eq!(self.f.circle.view("get_ft_escrow", json!({"grant_id": id})).await?["state"], "held");
        assert_eq!(self.f.circle.view("get_grant", json!({"grant_id": id})).await?["status"], "offered");
        Ok(())
    }
}

