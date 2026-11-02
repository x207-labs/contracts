//! F8 access asset adapter (CT-069..072, spec 15): D10 registry of asset classes and D11 access grants. Everything lives
//! under its own storage keys (`StorageKey::AssetClasses`, raw key `x207_asset_types`, and later `Grants`), so the contract
//! root keeps its layout.

use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::store::LookupMap;
use near_sdk::{env, near, require};

use crate::events::{AssetClassSet, X207Event};
use crate::state::{StorageKey, X207Circle, X207CircleExt};

const ASSET_TYPES_KEY: &[u8] = b"x207_asset_types";
const KNOWN_PROOFS: [&str; 3] = ["rpc_access_key", "tee_attestation", "zktls"];

/// D10 class of an asset (F8, ADR-28): `A` escrow, `B` delegated access, `C` operator-mediated, `D` attested, `X` forbidden.
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetClass {
    A,
    B,
    C,
    D,
    X,
}

/// D10 entry.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetClassEntry {
    /// `<provider>:<kind>`.
    pub asset_type: String,
    pub class: AssetClass,
    pub delivery_methods: Vec<String>,
    /// Subset of `rpc_access_key`, `tee_attestation`, `zktls`.
    pub accepted_proofs: Vec<String>,
    /// 64 lowercase hex.
    pub terms_hash: String,
}

fn asset_classes() -> LookupMap<String, AssetClassEntry> {
    LookupMap::new(StorageKey::AssetClasses)
}

fn asset_types() -> Vec<String> {
    env::storage_read(ASSET_TYPES_KEY).map_or_else(Vec::new, |b| Vec::<String>::try_from_slice(&b).expect("asset type list"))
}

fn is_hex64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `<provider>:<kind>`: two non-empty parts of `[a-z0-9._-]` split by one colon.
pub fn valid_asset_type(text: &str) -> bool {
    let mut parts = text.split(':');
    let (Some(provider), Some(kind), None) = (parts.next(), parts.next(), parts.next()) else { return false };
    let ok = |s: &str| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'));
    ok(provider) && ok(kind)
}

pub fn get_class(asset_type: &str) -> Option<AssetClassEntry> {
    asset_classes().get(&asset_type.to_string()).cloned()
}

#[near]
impl X207Circle {
    /// F8 (CT-069): owner only. `asset_type` has the form `<provider>:<kind>` and equals `entry.asset_type`; proofs are
    /// among the three known kinds; `terms_hash` is 64 lowercase hex. Writing a known `asset_type` replaces its entry.
    pub fn set_asset_class(&mut self, asset_type: String, entry: AssetClassEntry) {
        self.assert_owner();
        require!(valid_asset_type(&asset_type), format!("ERR_BAD_ASSET_TYPE: {asset_type}"));
        require!(entry.asset_type == asset_type, "ERR_BAD_ASSET_TYPE: entry.asset_type differs from asset_type");
        require!(is_hex64(&entry.terms_hash), "ERR_BAD_ASSET_TYPE: terms_hash must be 64 lowercase hex");
        require!(entry.delivery_methods.iter().all(|m| !m.is_empty()), "ERR_BAD_ASSET_TYPE: empty delivery method");
        require!(entry.accepted_proofs.iter().all(|p| KNOWN_PROOFS.contains(&p.as_str())), "ERR_BAD_ASSET_TYPE: unknown proof kind");
        let mut map = asset_classes();
        if map.get(&asset_type).is_none() {
            let mut list = asset_types();
            list.push(asset_type.clone());
            env::storage_write(ASSET_TYPES_KEY, &borsh_vec(&list));
        }
        let class = entry.class;
        map.insert(asset_type.clone(), entry);
        map.flush();
        AssetClassSet { asset_type, class }.emit();
    }

    pub fn get_asset_class(&self, asset_type: String) -> Option<AssetClassEntry> {
        get_class(&asset_type)
    }

    /// Entries in registration order; `limit` 0 or `from_index` past the end gives `[]`.
    pub fn list_asset_classes(&self, from_index: u32, limit: u32) -> Vec<AssetClassEntry> {
        asset_types().iter().skip(from_index as usize).take(limit as usize).filter_map(|t| get_class(t)).collect()
    }
}

fn borsh_vec(list: &[String]) -> Vec<u8> {
    near_sdk::borsh::to_vec(&list.to_vec()).expect("borsh")
}

#[cfg(test)]
mod tests {
    use super::*;

    // TC-CT-069: format `<provider>:<kind>`
    #[test]
    fn asset_type_format() {
        for ok in ["near-rpc:access", "openai:gpt-5.1", "a:b", "gpu_farm:h100.8x"] {
            assert!(valid_asset_type(ok), "{ok}");
        }
        for bad in ["", "nocolon", ":kind", "provider:", "a:b:c", "UPPER:kind", "a b:c", "a:b c", "é:x"] {
            assert!(!valid_asset_type(bad), "{bad}");
        }
        assert!(!valid_asset_type(&format!("{}:x", "a".repeat(65))));
    }
}
