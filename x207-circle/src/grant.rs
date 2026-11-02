//! F8 access asset adapter (CT-069..072, spec 15): D10 registry of asset classes and D11 access grants. Everything lives
//! under its own storage keys (`StorageKey::AssetClasses`, raw key `x207_asset_types`, and later `Grants`), so the contract
//! root keeps its layout.

use near_sdk::borsh::BorshDeserialize;
use near_sdk::json_types::{U128, U64};
use near_sdk::store::LookupMap;
use near_sdk::{env, near, require, AccountId, PublicKey};

use crate::events::{AssetClassSet, GrantActivated, GrantOffered, GrantRevoked, X207Event};
use crate::settlement::PositionStatus;
use crate::member::MemberStatus;
use crate::state::{StorageKey, X207Circle, X207CircleExt};

const ASSET_TYPES_KEY: &[u8] = b"x207_asset_types";
const SEQ_KEY: &[u8] = b"x207_grant_seq";
/// Offered and active grants one grantor may hold at the same time (spec 15.2).
pub const MAX_ACTIVE_GRANTS_PER_MEMBER: u32 = 16;
const UNITS: [&str; 4] = ["call", "minute", "day", "once"];
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

pub type GrantId = u64;

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GrantStatus {
    Offered,
    Active,
    Revoked,
    Expired,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RevokeReason {
    Grantor,
    Grantee,
    Operator,
    Expired,
    Defaulted,
}

#[near(serializers = [json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GrantRole {
    Grantor,
    Grantee,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantLimits {
    /// JSON text; class B: `receiver_id`, `method_names`, `allowance`.
    pub scope: String,
    /// `null`: no quota.
    pub quota: Option<U128>,
    /// Nanoseconds.
    pub expires_at: U64,
}

/// Argument of `offer_grant`. `class` is read from D10, not from the input.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantInput {
    pub asset_type: String,
    pub delivery_method: String,
    pub limits: GrantLimits,
    pub unit: String,
    pub unit_price: U128,
    pub operator: Option<AccountId>,
    pub proof_hash: Option<String>,
    pub proof_kind: Option<String>,
}

/// D11 record. No secret fields.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessGrant {
    pub grant_id: GrantId,
    pub asset_type: String,
    pub grantor: AccountId,
    /// `null` until `accept_grant`.
    pub grantee: Option<AccountId>,
    pub class: AssetClass,
    pub limits: GrantLimits,
    pub unit: String,
    pub unit_price: U128,
    pub operator: Option<AccountId>,
    pub delivery_pubkey: Option<PublicKey>,
    pub status: GrantStatus,
    pub proof_hash: Option<String>,
    pub proof_kind: Option<String>,
}

/// Class A (CT-075): the grant becomes `active` for `grantee` when the escrow delivered the asset.
pub fn activate(grant: &mut AccessGrant, grantee: AccountId, delivery_pubkey: Option<PublicKey>) {
    grant.status = GrantStatus::Active;
    grant.grantee = Some(grantee.clone());
    grant.delivery_pubkey = delivery_pubkey;
    let id = grant.grant_id;
    let mut map = grants();
    map.insert(id, grant.clone());
    map.flush();
    index(1, &grantee, id);
}

/// Class A (CT-075): final status for a grant whose escrow ended; frees a slot of the grantor.
pub fn close(grant: &mut AccessGrant, status: GrantStatus) {
    grant.status = status;
    let (id, grantor) = (grant.grant_id, grant.grantor.clone());
    let mut map = grants();
    map.insert(id, grant.clone());
    map.flush();
    let mut counts = grant_counts();
    let open = counts.get(&grantor).copied().unwrap_or(1);
    counts.insert(grantor, open.saturating_sub(1));
    counts.flush();
}

fn grants() -> LookupMap<GrantId, AccessGrant> {
    LookupMap::new(StorageKey::Grants)
}

/// `(0 grantor | 1 grantee, account) -> grant ids`.
fn grants_by() -> LookupMap<(u8, AccountId), Vec<GrantId>> {
    LookupMap::new(StorageKey::GrantsBy)
}

/// Offered and active grants per grantor.
fn grant_counts() -> LookupMap<AccountId, u32> {
    LookupMap::new(StorageKey::GrantCounts)
}

pub fn load_grant(grant_id: GrantId) -> AccessGrant {
    grants().get(&grant_id).cloned().unwrap_or_else(|| env::panic_str(&format!("ERR_UNKNOWN_GRANT: {grant_id}")))
}

fn index(role: u8, account: &AccountId, grant_id: GrantId) {
    let mut map = grants_by();
    let key = (role, account.clone());
    let mut list = map.get(&key).cloned().unwrap_or_default();
    list.push(grant_id);
    map.insert(key, list);
    map.flush();
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

impl X207Circle {
    /// The caller as a member whose effective status is `active`; otherwise `ERR_GRANT_NOT_ACTIVE_MEMBER`.
    fn assert_active_caller(&self) -> AccountId {
        let caller = env::predecessor_account_id();
        let active = self.members.get(&caller).map(|m| self.member_now(m)).is_some_and(|m| m.status == MemberStatus::Active);
        require!(active, format!("ERR_GRANT_NOT_ACTIVE_MEMBER: {caller}"));
        caller
    }
}

impl X207Circle {
    /// The grantee is `suspended` or has a D5 position `defaulted` in the running epoch or the two before it.
    fn grantee_defaulted(&self, grant: &AccessGrant) -> bool {
        let Some(grantee) = &grant.grantee else { return false };
        if self.members.get(grantee).map(|m| self.member_now(m)).is_some_and(|m| m.status == MemberStatus::Suspended) {
            return true;
        }
        let newest = self.current_epoch;
        (newest.saturating_sub(2).max(1)..=newest).any(|epoch| {
            self.positions.get(&(epoch, grantee.clone())).is_some_and(|p| p.status == PositionStatus::Defaulted)
        })
    }
}

#[near]
impl X207Circle {
    /// F8 (CT-070): an `active` member offers access to an asset. Checks in order: member (`ERR_NOT_MEMBER`), active
    /// (`ERR_GRANT_NOT_ACTIVE_MEMBER`), asset type in D10 (`ERR_ASSET_TYPE_UNKNOWN`), class not `X`
    /// (`ERR_ASSET_CLASS_FORBIDDEN`), method in `delivery_methods` (`ERR_DELIVERY_NOT_ALLOWED`), then the shape of the grant
    /// (`ERR_BAD_GRANT`: unknown `unit`, `unit_price` 0, `expires_at` not in the future, bad `scope`, more than
    /// `MAX_ACTIVE_GRANTS_PER_MEMBER` open grants). The class is taken from D10. Returns the new `grant_id`.
    pub fn offer_grant(&mut self, input: GrantInput) -> GrantId {
        let grantor = env::predecessor_account_id();
        require!(self.members.get(&grantor).is_some(), format!("ERR_NOT_MEMBER: {grantor}"));
        self.assert_active_caller();
        let entry = get_class(&input.asset_type).unwrap_or_else(|| env::panic_str(&format!("ERR_ASSET_TYPE_UNKNOWN: {}", input.asset_type)));
        require!(entry.class != AssetClass::X, format!("ERR_ASSET_CLASS_FORBIDDEN: {}", input.asset_type));
        require!(entry.delivery_methods.contains(&input.delivery_method), format!("ERR_DELIVERY_NOT_ALLOWED: {}", input.delivery_method));
        // CT-072: a proof is a hash and a kind, both or none; the kind must be one the asset type accepts. The proof itself stays off-chain.
        match (&input.proof_hash, &input.proof_kind) {
            (None, None) => {}
            (Some(hash), Some(kind)) => {
                require!(is_hex64(hash), "ERR_BAD_GRANT: proof_hash must be 64 lowercase hex (sha256)");
                require!(entry.accepted_proofs.contains(kind), format!("ERR_PROOF_KIND_NOT_ACCEPTED: {kind}"));
            }
            _ => env::panic_str("ERR_BAD_GRANT: proof_hash and proof_kind are given together or not at all"),
        }
        require!(UNITS.contains(&input.unit.as_str()), format!("ERR_BAD_GRANT: unknown unit {}", input.unit));
        require!(input.unit_price.0 > 0, "ERR_BAD_GRANT: unit_price must be above 0");
        require!(input.limits.expires_at.0 > env::block_timestamp(), "ERR_BAD_GRANT: expires_at must be in the future");
        require!(
            !input.limits.scope.is_empty()
                && input.limits.scope.len() <= 1024
                && near_sdk::serde_json::from_str::<near_sdk::serde_json::Value>(&input.limits.scope).is_ok(),
            "ERR_BAD_GRANT: scope must be JSON text of at most 1024 bytes"
        );
        require!(input.limits.quota.map_or(true, |q| q.0 > 0), "ERR_BAD_GRANT: quota must be above 0 or null");
        let mut counts = grant_counts();
        let open = counts.get(&grantor).copied().unwrap_or(0);
        require!(open < MAX_ACTIVE_GRANTS_PER_MEMBER, format!("ERR_BAD_GRANT: {grantor} already has {open} open grants"));

        let grant_id = env::storage_read(SEQ_KEY).map_or(1, |b| u64::from_le_bytes(b.try_into().expect("8 bytes")) + 1);
        env::storage_write(SEQ_KEY, &grant_id.to_le_bytes());
        let grant = AccessGrant {
            grant_id,
            asset_type: input.asset_type,
            grantor: grantor.clone(),
            grantee: None,
            class: entry.class,
            limits: input.limits,
            unit: input.unit,
            unit_price: input.unit_price,
            operator: input.operator,
            delivery_pubkey: None,
            status: GrantStatus::Offered,
            proof_hash: input.proof_hash,
            proof_kind: input.proof_kind,
        };
        let mut map = grants();
        map.insert(grant_id, grant.clone());
        map.flush();
        index(0, &grantor, grant_id);
        counts.insert(grantor.clone(), open + 1);
        counts.flush();
        GrantOffered {
            grant_id,
            asset_type: grant.asset_type,
            grantor,
            class: grant.class,
            unit: grant.unit,
            unit_price: grant.unit_price,
            expires_at: grant.limits.expires_at,
        }
        .emit();
        grant_id
    }

    /// F8 (CT-070): an `active` member other than the grantor accepts an `offered` grant before `expires_at`; the grant
    /// becomes `active` with the caller as `grantee`. Checks in order: `ERR_UNKNOWN_GRANT`, `ERR_GRANT_NOT_ACTIVE_MEMBER`,
    /// `ERR_GRANT_SELF`, `ERR_GRANT_NOT_OFFERED`, `ERR_GRANT_EXPIRED`.
    pub fn accept_grant(&mut self, grant_id: GrantId, delivery_pubkey: Option<PublicKey>) {
        let mut grant = load_grant(grant_id);
        let caller = self.assert_active_caller();
        require!(caller != grant.grantor, format!("ERR_GRANT_SELF: {caller}"));
        require!(grant.status == GrantStatus::Offered, format!("ERR_GRANT_NOT_OFFERED: {:?}", grant.status));
        require!(grant.class != AssetClass::A, "ERR_GRANT_NOT_OFFERED: a class A grant is bought with complete_escrow");
        require!(env::block_timestamp() < grant.limits.expires_at.0, format!("ERR_GRANT_EXPIRED: expires_at {}", grant.limits.expires_at.0));
        grant.status = GrantStatus::Active;
        grant.grantee = Some(caller.clone());
        grant.delivery_pubkey = delivery_pubkey.clone();
        let mut map = grants();
        map.insert(grant_id, grant);
        map.flush();
        index(1, &caller, grant_id);
        GrantActivated { grant_id, grantee: caller, delivery_pubkey }.emit();
    }

    /// F8 (CT-071, spec 15.3): final. The grantor, the grantee or the `operator` may revoke at any time with the matching
    /// `reason` (`grantor`, `grantee`, `operator`); anybody may when `now >= expires_at` (`expired`, status `expired`) or
    /// when the grantee is `suspended` or defaulted (`defaulted`). Otherwise `ERR_GRANT_REVOKE_DENIED`; a grant that is
    /// already `revoked` or `expired` gives `ERR_GRANT_FINAL`.
    pub fn revoke_grant(&mut self, grant_id: GrantId, reason: RevokeReason) {
        let mut grant = load_grant(grant_id);
        require!(matches!(grant.status, GrantStatus::Offered | GrantStatus::Active), format!("ERR_GRANT_FINAL: {:?}", grant.status));
        let caller = env::predecessor_account_id();
        require!(!crate::escrow::holds_asset(grant_id), format!("ERR_GRANT_REVOKE_DENIED: the escrow of grant {grant_id} holds an asset, use cancel_escrow"));
        let (allowed, status) = match reason {
            RevokeReason::Grantor => (caller == grant.grantor, GrantStatus::Revoked),
            RevokeReason::Grantee => (grant.grantee.as_ref() == Some(&caller), GrantStatus::Revoked),
            RevokeReason::Operator => (grant.operator.as_ref() == Some(&caller), GrantStatus::Revoked),
            RevokeReason::Expired => (env::block_timestamp() >= grant.limits.expires_at.0, GrantStatus::Expired),
            RevokeReason::Defaulted => (self.grantee_defaulted(&grant), GrantStatus::Revoked),
        };
        require!(allowed, format!("ERR_GRANT_REVOKE_DENIED: {caller} cannot revoke grant {grant_id} as {reason:?}"));
        grant.status = status;
        let grantor = grant.grantor.clone();
        let mut map = grants();
        map.insert(grant_id, grant);
        map.flush();
        let mut counts = grant_counts();
        let open = counts.get(&grantor).copied().unwrap_or(1);
        counts.insert(grantor, open.saturating_sub(1));
        counts.flush();
        GrantRevoked { grant_id, reason }.emit();
    }

    pub fn get_grant(&self, grant_id: GrantId) -> Option<AccessGrant> {
        grants().get(&grant_id).cloned()
    }

    /// Grants where `account_id` is the grantor or the grantee, oldest first.
    pub fn grants_of(&self, account_id: AccountId, role: GrantRole) -> Vec<AccessGrant> {
        let code = if role == GrantRole::Grantor { 0 } else { 1 };
        let ids = grants_by().get(&(code, account_id)).cloned().unwrap_or_default();
        ids.iter().filter_map(|id| grants().get(id).cloned()).collect()
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
