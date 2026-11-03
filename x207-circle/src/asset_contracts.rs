//! Official contracts per asset type (CT-096, ADR-43): for an `asset_type` of class A the DAO can name the token contracts that may be escrowed for it, against a fake
//! token with the right name. An empty list (the default) is the old behaviour: any contract named in the scope is taken. With a list, `offer_grant` refuses a scope that
//! names another contract, and the receivers (`nft_on_transfer`, `ft_on_transfer`, `mt_on_transfer`) refuse a deposit from another contract, so a grant offered before the
//! list was set is still checked at the deposit. An escrow that is already held is not touched by a change of the list.

use near_sdk::store::LookupMap;
use near_sdk::{env, near, require, AccountId};

use crate::events::{AssetContractsSet, X207Event};
use crate::grant::{get_class, AssetClass};
use crate::state::{StorageKey, X207Circle, X207CircleExt};

pub const MAX_OFFICIAL_CONTRACTS: usize = 16;

pub(crate) fn lists() -> LookupMap<String, Vec<AccountId>> {
    LookupMap::new(StorageKey::AssetContracts)
}

/// The official contracts of `asset_type`; empty means no list.
pub fn official_contracts(asset_type: &str) -> Vec<AccountId> {
    lists().get(&asset_type.to_string()).cloned().unwrap_or_default()
}

/// `true` when there is no list, or `contract` is in it.
pub fn is_official(asset_type: &str, contract: &AccountId) -> bool {
    let list = official_contracts(asset_type);
    list.is_empty() || list.contains(contract)
}

/// `ERR_CONTRACT_NOT_OFFICIAL` unless `is_official`.
pub fn assert_official(asset_type: &str, contract: &AccountId) {
    require!(is_official(asset_type, contract), format!("ERR_CONTRACT_NOT_OFFICIAL: {contract} is not an official contract of {asset_type}"));
}

#[near]
impl X207Circle {
    /// Owner only (the DAO, by a proposal). The asset type must be registered with class `A`. An empty vector clears the list. At most 16 distinct accounts.
    /// `ERR_NOT_OWNER`, `ERR_ASSET_TYPE_UNKNOWN`, `ERR_ASSET_CLASS_NOT_A`, `ERR_TOO_MANY_CONTRACTS`, `ERR_DUPLICATE_CONTRACT`.
    pub fn set_asset_contracts(&mut self, asset_type: String, contracts: Vec<AccountId>) {
        self.assert_owner();
        let entry = get_class(&asset_type).unwrap_or_else(|| env::panic_str(&format!("ERR_ASSET_TYPE_UNKNOWN: {asset_type}")));
        require!(entry.class == AssetClass::A, format!("ERR_ASSET_CLASS_NOT_A: {asset_type} is class {:?}", entry.class));
        require!(contracts.len() <= MAX_OFFICIAL_CONTRACTS, format!("ERR_TOO_MANY_CONTRACTS: {} given, the limit is {MAX_OFFICIAL_CONTRACTS}", contracts.len()));
        let mut seen = std::collections::BTreeSet::new();
        for c in &contracts {
            require!(seen.insert(c.clone()), format!("ERR_DUPLICATE_CONTRACT: {c}"));
        }
        let mut map = lists();
        if contracts.is_empty() {
            map.remove(&asset_type);
        } else {
            map.insert(asset_type.clone(), contracts.clone());
        }
        map.flush();
        AssetContractsSet { asset_type, contracts }.emit();
    }

    pub fn get_asset_contracts(&self, asset_type: String) -> Vec<AccountId> {
        official_contracts(&asset_type)
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::testing_env;

    use super::*;
    use crate::grant::{set_class_for_test, AssetClassEntry};
    use crate::params::Params;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("expected a panic");
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    fn circle() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        let c = X207Circle::with_config(acc("dao.near"), "agents-main".into(), acc("usdt.near"), acc("intents.near"), Params::default(), 1_000);
        set_class_for_test("token:nep141", AssetClass::A);
        set_class_for_test("saas:seat", AssetClass::B);
        let _: Option<AssetClassEntry> = None;
        c
    }

    fn as_owner() {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("dao.near")).block_timestamp(2_000).build());
    }

    // TC-CT-096-U01
    #[test]
    fn the_owner_sets_reads_and_clears_a_list_and_the_event_says_so() {
        let mut c = circle();
        assert!(c.get_asset_contracts("token:nep141".into()).is_empty());
        assert!(is_official("token:nep141", &acc("anything.near")), "no list: any contract");
        as_owner();
        c.set_asset_contracts("token:nep141".into(), vec![acc("usdc.near"), acc("wnear.near")]);
        assert_eq!(c.get_asset_contracts("token:nep141".into()), vec![acc("usdc.near"), acc("wnear.near")]);
        assert!(get_logs().iter().any(|l| l.contains("asset_contracts_set") && l.contains("usdc.near")));
        assert!(is_official("token:nep141", &acc("usdc.near")) && !is_official("token:nep141", &acc("fake-usdc.near")));
        assert!(panic_text(|| assert_official("token:nep141", &acc("fake-usdc.near"))).contains("ERR_CONTRACT_NOT_OFFICIAL"));
        c.set_asset_contracts("token:nep141".into(), vec![]);
        assert!(c.get_asset_contracts("token:nep141".into()).is_empty() && is_official("token:nep141", &acc("fake-usdc.near")), "an empty list is the old behaviour");
    }

    // TC-CT-096-U02
    #[test]
    fn only_the_owner_only_class_a_known_types_at_most_sixteen_distinct_accounts() {
        let mut c = circle();
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("mallory.near")).block_timestamp(2_000).build());
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.set_asset_contracts("token:nep141".into(), vec![acc("x.near")]))).contains("ERR_NOT_OWNER"));
        as_owner();
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.set_asset_contracts("nope:type".into(), vec![acc("x.near")]))).contains("ERR_ASSET_TYPE_UNKNOWN"));
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.set_asset_contracts("saas:seat".into(), vec![acc("x.near")]))).contains("ERR_ASSET_CLASS_NOT_A"));
        let many: Vec<AccountId> = (0..17).map(|i| acc(&format!("c{i}.near"))).collect();
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.set_asset_contracts("token:nep141".into(), many.clone()))).contains("ERR_TOO_MANY_CONTRACTS"));
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.set_asset_contracts("token:nep141".into(), vec![acc("x.near"), acc("x.near")]))).contains("ERR_DUPLICATE_CONTRACT"));
        c.set_asset_contracts("token:nep141".into(), many[..16].to_vec());
        assert_eq!(c.get_asset_contracts("token:nep141".into()).len(), 16);
        // a list of one type does not touch another type
        assert!(is_official("saas:seat", &acc("x.near")));
    }
}
