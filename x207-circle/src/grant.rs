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

/// Class A (CT-075): final status for a grant whose escrow ended; frees a slot of the grantor and takes the grant out of the indexes.
pub fn close(grant: &mut AccessGrant, status: GrantStatus) {
    grant.status = status;
    unindex_grant(grant);
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

pub fn try_load_grant(grant_id: GrantId) -> Option<AccessGrant> {
    grants().get(&grant_id).cloned()
}

pub fn load_grant(grant_id: GrantId) -> AccessGrant {
    grants().get(&grant_id).cloned().unwrap_or_else(|| env::panic_str(&format!("ERR_UNKNOWN_GRANT: {grant_id}")))
}

/// A grant that ended (`revoked` or `expired`) leaves the index of its grantor and of its grantee, so `grants_of` lists open grants only and the vectors
/// do not grow with the history of an account (security review F5). `get_grant` still answers for it.
fn unindex_grant(grant: &AccessGrant) {
    let mut map = grants_by();
    for (role, account) in [(0u8, Some(&grant.grantor)), (1u8, grant.grantee.as_ref())] {
        let Some(account) = account else { continue };
        let key = (role, account.clone());
        let Some(list) = map.get(&key) else { continue };
        let kept: Vec<GrantId> = list.iter().copied().filter(|id| *id != grant.grant_id).collect();
        map.insert(key, kept);
    }
    map.flush();
}

/// Class A (security review F1): the scope of a class A grant names the NFT contract and the token that may fill its escrow.
pub fn class_a_target(scope: &str) -> Result<(AccountId, String), String> {
    let value: near_sdk::serde_json::Value = near_sdk::serde_json::from_str(scope).map_err(|_| "scope is not JSON".to_string())?;
    let nft = value.get("nft_contract").and_then(|v| v.as_str()).ok_or("scope.nft_contract is missing")?;
    let token = value.get("token_id").and_then(|v| v.as_str()).filter(|t| !t.is_empty() && t.len() <= 128).ok_or("scope.token_id is missing or longer than 128 bytes")?;
    let nft: AccountId = nft.parse().map_err(|_| "scope.nft_contract is not an account id".to_string())?;
    Ok((nft, token.to_string()))
}

/// What the scope of a class A grant names: one NFT (CT-075) or an exact amount of a NEP-141 token (CT-090).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClassAAsset {
    Nft { nft_contract: AccountId, token_id: String },
    Ft { ft_contract: AccountId, amount: u128 },
    Mt { mt_contract: AccountId, token_id: String, amount: u128 },
}

/// `{"nft_contract": ..., "token_id": ...}` or `{"ft_contract": ..., "amount": "..."}` (the amount is a decimal string above 0). Not both.
pub fn class_a_asset(scope: &str) -> Result<ClassAAsset, String> {
    let value: near_sdk::serde_json::Value = near_sdk::serde_json::from_str(scope).map_err(|_| "scope is not JSON".to_string())?;
    if value.get("mt_contract").is_some() {
        if ["nft_contract", "ft_contract"].iter().any(|k| value.get(*k).is_some()) {
            return Err("scope names more than one kind of asset".to_string());
        }
        let mt: AccountId = value.get("mt_contract").and_then(|v| v.as_str()).ok_or("scope.mt_contract is not text")?.parse().map_err(|_| "scope.mt_contract is not an account id".to_string())?;
        let token = value.get("token_id").and_then(|v| v.as_str()).filter(|t| !t.is_empty() && t.len() <= 128).ok_or("scope.token_id is missing or longer than 128 bytes")?;
        let amount = value.get("amount").and_then(|v| v.as_str()).and_then(|t| t.parse::<u128>().ok()).filter(|a| *a > 0).ok_or("scope.amount is missing, not a decimal string or 0")?;
        return Ok(ClassAAsset::Mt { mt_contract: mt, token_id: token.to_string(), amount });
    }
    if value.get("ft_contract").is_none() {
        return class_a_target(scope).map(|(nft_contract, token_id)| ClassAAsset::Nft { nft_contract, token_id });
    }
    if value.get("nft_contract").is_some() || value.get("token_id").is_some() {
        return Err("scope names an NFT and a token at once".to_string());
    }
    let ft: AccountId = value.get("ft_contract").and_then(|v| v.as_str()).ok_or("scope.ft_contract is not text")?.parse().map_err(|_| "scope.ft_contract is not an account id".to_string())?;
    let amount = value.get("amount").and_then(|v| v.as_str()).and_then(|t| t.parse::<u128>().ok()).filter(|a| *a > 0).ok_or("scope.amount is missing, not a decimal string or 0")?;
    Ok(ClassAAsset::Ft { ft_contract: ft, amount })
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
    /// The grantee is `suspended` or `exited`, or is not `active` and has a D5 position `defaulted` in the running epoch or the two before it. A member that
    /// was reactivated (`active` again) is not defaulted any more: an old default must not let a stranger revoke the grants it accepted since.
    fn grantee_defaulted(&self, grant: &AccessGrant) -> bool {
        let Some(grantee) = &grant.grantee else { return false };
        let Some(status) = self.members.get(grantee).map(|m| self.member_now(m).status) else { return false };
        // an `exited` member has no stake left in the circle (security review 3, I-5): anybody may end the grants it still holds
        if matches!(status, MemberStatus::Suspended | MemberStatus::Exited) {
            return true;
        }
        if status == MemberStatus::Active {
            return false;
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
        crate::freeze::assert_not_frozen(&grantor, "offer a grant");
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
        if entry.class == AssetClass::A {
            if let Err(reason) = class_a_asset(&input.limits.scope) {
                env::panic_str(&format!("ERR_BAD_GRANT: a class A scope is {{\"nft_contract\": ..., \"token_id\": ...}}, {{\"ft_contract\": ..., \"amount\": \"...\"}} or {{\"mt_contract\": ..., \"token_id\": ..., \"amount\": \"...\"}}: {reason}"));
            }
        }
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
        crate::freeze::assert_not_frozen(&caller, "accept a grant");
        require!(caller != grant.grantor, format!("ERR_GRANT_SELF: {caller}"));
        require!(grant.status == GrantStatus::Offered, format!("ERR_GRANT_NOT_OFFERED: {:?}", grant.status));
        require!(grant.class != AssetClass::A, "ERR_GRANT_NOT_OFFERED: a class A grant is bought with complete_escrow");
        // security review 3, I-5: the offer of a grantor that left or was suspended is not taken (the lessee would hold a grant of somebody with no stake in the circle)
        let grantor_active = self.members.get(&grant.grantor).map(|m| self.member_now(m)).is_some_and(|m| m.status == MemberStatus::Active);
        require!(grantor_active, format!("ERR_GRANT_NOT_ACTIVE_MEMBER: the grantor {} is not an active member", grant.grantor));
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
        unindex_grant(&grant);
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

    /// Open grants (`offered` or `active`) where `account_id` is the grantor or the grantee, oldest first, `limit` of them from `from_index`. Ended grants
    /// are not in the index; read them with `get_grant`. `limit` 0 or a `from_index` past the end gives `[]`.
    pub fn grants_of(&self, account_id: AccountId, role: GrantRole, from_index: u32, limit: u32) -> Vec<AccessGrant> {
        let code = if role == GrantRole::Grantor { 0 } else { 1 };
        let ids = grants_by().get(&(code, account_id)).cloned().unwrap_or_default();
        ids.iter().skip(from_index as usize).take(limit as usize).filter_map(|id| grants().get(id).cloned()).collect()
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

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn member(name: &str, status: MemberStatus) -> crate::member::Member {
        crate::member::Member {
            account_id: acc(name), pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse().unwrap(), terms_hash: "a".repeat(64),
            stake: 100, limit: 80, status, admitted: false, joined_at: 1, settled_history: 0, last_active_epoch: 1, exit_requested_epoch: None,
            withdraw_pending: false, reactivate_epoch: None,
        }
    }

    /// A circle with an active grant of `gpu.near`, whose member record has `status` and who has a defaulted position in epoch 1.
    fn circle_with_defaulted_grantee(status: MemberStatus) -> X207Circle {
        near_sdk::testing_env!(near_sdk::test_utils::VMContextBuilder::new().block_timestamp(1_000).build());
        let mut c = X207Circle::with_config(acc("dao.near"), "agents-main".into(), acc("usdt.near"), acc("intents.near"), crate::params::Params::default(), 1_000);
        c.members.insert(acc("gpu.near"), member("gpu.near", status));
        c.put_position(1, acc("gpu.near"), crate::settlement::Position {
            net: -5, role: crate::settlement::PositionRole::Debtor, status: PositionStatus::Defaulted, amount: 5, recovered: 0, payment_deadline: 9, updated_at_block: 5,
        });
        store_grant_for_test(&AccessGrant {
            grant_id: 3, asset_type: "near-rpc:access".into(), grantor: acc("scraper.near"), grantee: Some(acc("gpu.near")), class: AssetClass::B,
            limits: GrantLimits { scope: "{}".into(), quota: None, expires_at: U64(9_000_000_000_000) }, unit: "day".into(), unit_price: U128(1),
            operator: None, delivery_pubkey: None, status: GrantStatus::Active, proof_hash: None, proof_kind: None,
        });
        c
    }

    fn stranger_revokes(c: &mut X207Circle) -> bool {
        near_sdk::testing_env!(near_sdk::test_utils::VMContextBuilder::new().predecessor_account_id(acc("stranger.near")).block_timestamp(2_000).build());
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.revoke_grant(3, RevokeReason::Defaulted))).is_ok()
    }

    // TC-CT-071-S01 (security review): a member that was reactivated is not defaulted any more; a stranger cannot revoke its new grants with an old default
    #[test]
    fn a_reactivated_member_keeps_its_grants_against_a_stranger() {
        let mut c = circle_with_defaulted_grantee(MemberStatus::Active);
        assert!(!stranger_revokes(&mut c), "an active member was revoked for a default of an earlier epoch");
        assert_eq!(load_grant(3).status, GrantStatus::Active);
    }

    // TC-CT-071-S02: while the member is suspended, anybody may revoke (spec 15.3), with or without the position in the window
    #[test]
    fn a_suspended_member_loses_its_grants_to_anybody() {
        let mut c = circle_with_defaulted_grantee(MemberStatus::Suspended);
        assert!(stranger_revokes(&mut c));
        assert_eq!(load_grant(3).status, GrantStatus::Revoked);
    }

    // security review 3, I-5 (b): a grantee that has `exited` can have its grants ended by anybody with the reason `defaulted`
    #[test]
    fn a_stranger_ends_the_grant_of_an_exited_grantee() {
        let mut c = circle_with_defaulted_grantee(MemberStatus::Exited);
        c.positions.remove(&(1, acc("gpu.near")));
        assert!(stranger_revokes(&mut c), "the grant of an exited member could not be revoked");
        assert_eq!(load_grant(3).status, GrantStatus::Revoked);
    }

    // security review 3, I-5 (a): nobody accepts the offer of a grantor that is not an active member
    #[test]
    fn the_offer_of_a_grantor_that_is_not_active_is_not_accepted() {
        for status in [MemberStatus::Exited, MemberStatus::Suspended, MemberStatus::Pending] {
            let mut c = circle_with_defaulted_grantee(MemberStatus::Active);
            c.members.insert(acc("scraper.near"), member("scraper.near", status));
            c.members.insert(acc("gina.near"), member("gina.near", MemberStatus::Active));
            store_grant_for_test(&AccessGrant {
                grant_id: 4, asset_type: "near-rpc:access".into(), grantor: acc("scraper.near"), grantee: None, class: AssetClass::B,
                limits: GrantLimits { scope: "{}".into(), quota: None, expires_at: U64(9_000_000_000_000) }, unit: "day".into(), unit_price: U128(1),
                operator: None, delivery_pubkey: None, status: GrantStatus::Offered, proof_hash: None, proof_kind: None,
            });
            near_sdk::testing_env!(near_sdk::test_utils::VMContextBuilder::new().predecessor_account_id(acc("gina.near")).block_timestamp(2_000).build());
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.accept_grant(4, None)));
            let text = result.err().and_then(|e| e.downcast_ref::<String>().cloned()).unwrap_or_default();
            assert!(text.contains("ERR_GRANT_NOT_ACTIVE_MEMBER"), "{status:?}: {text}");
            assert_eq!(load_grant(4).status, GrantStatus::Offered);
        }
    }
}

/// Unit tests of the escrow need a grant in storage without going through `offer_grant`.
#[cfg(test)]
pub(crate) fn store_grant_for_test(grant: &AccessGrant) {
    let mut map = grants();
    map.insert(grant.grant_id, grant.clone());
    map.flush();
}
