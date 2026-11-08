//! CT-126 (0.0.18, P1): escrow of a NEAR account that is sold through the locker contract (`locker/`, design in `docs/adr-0.0.18.md`).
//! The types, the storage and the rules for accounts live here; the methods that move the escrow (`account_on_lock`, `complete_account_escrow`,
//! `cancel_escrow`, `on_escrow_resolved`) sit in `escrow.rs` beside the other three kinds, because they share the voucher registration and its take-back.

use near_sdk::borsh::BorshDeserialize;
use near_sdk::near;
use near_sdk::store::LookupMap;
use near_sdk::{env, require, AccountId, PublicKey};

use crate::escrow::{EscrowState, Snapshot};
use crate::grant::GrantId;
use crate::member::MemberStatus;
use crate::state::{StorageKey, X207Circle};
use crate::types::EpochId;

/// D10 asset type of an account without a position in this circle (#11, #12, #13).
pub const ASSET_TYPE_ACCOUNT: &str = "near:account";
/// D10 asset type of an account that is a member of this circle (#14). The owner decides whether member accounts can be sold by registering it.
pub const ASSET_TYPE_MEMBER_ACCOUNT: &str = "near:member_account";

const LOCKER_HASHES_KEY: &[u8] = b"x207_locker_code_hashes";
/// At most this many official locker code hashes (the list is for the SDK to read, so it stays short).
pub const MAX_LOCKER_HASHES: usize = 8;

/// Class A escrow of one NEAR account held by its locker. Kept apart from `Escrow`, `FtEscrow` and `MtEscrow` so that the stored rows of 0.0.17 read unchanged.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountEscrow {
    /// The account that is sold and carries the locker.
    pub account_id: AccountId,
    pub state: EscrowState,
    pub buyer: Option<AccountId>,
    pub snapshot: Option<Snapshot>,
    /// The epoch in which the locker registered. A member account is sold only in a later epoch.
    pub locked_epoch: EpochId,
    /// The key that the buyer gets, set by `complete_account_escrow`.
    pub delivery_pubkey: Option<PublicKey>,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountEscrowView {
    pub grant_id: GrantId,
    pub account_id: AccountId,
    pub state: EscrowState,
    pub buyer: Option<AccountId>,
    pub locked_epoch: EpochId,
    pub delivery_pubkey: Option<PublicKey>,
}

fn account_escrows() -> LookupMap<GrantId, AccountEscrow> {
    LookupMap::new(StorageKey::AccountEscrows)
}

pub fn load_account_escrow(grant_id: GrantId) -> Option<AccountEscrow> {
    account_escrows().get(&grant_id).cloned()
}

pub(crate) fn save_account_escrow(grant_id: GrantId, escrow: AccountEscrow) {
    let mut map = account_escrows();
    map.insert(grant_id, escrow);
    map.flush();
}

/// The official code hashes of the locker (base58 sha256, as `view_code` of the RPC and `code_hash` of the account show them). The contract cannot read the
/// code of another account, so the list is not enforced here: the SDK of the buyer compares the account with it before it pays.
pub fn locker_code_hashes() -> Vec<String> {
    env::storage_read(LOCKER_HASHES_KEY)
        .and_then(|bytes| Vec::<String>::try_from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub(crate) fn set_locker_code_hashes(hashes: &[String]) {
    require!(
        hashes.len() <= MAX_LOCKER_HASHES,
        format!("ERR_TOO_MANY_HASHES: at most {MAX_LOCKER_HASHES}")
    );
    let mut seen = std::collections::BTreeSet::new();
    for h in hashes {
        require!(
            (43..=44).contains(&h.len())
                && h.bytes()
                    .all(|b| b.is_ascii_alphanumeric() && !matches!(b, b'0' | b'O' | b'I' | b'l')),
            format!("ERR_BAD_HASH: {h} is not a base58 sha256")
        );
        require!(seen.insert(h.clone()), format!("ERR_DUPLICATE_HASH: {h}"));
    }
    if hashes.is_empty() {
        env::storage_remove(LOCKER_HASHES_KEY);
    } else {
        env::storage_write(
            LOCKER_HASHES_KEY,
            &near_sdk::borsh::to_vec(&hashes.to_vec()).expect("borsh"),
        );
    }
}

/// `{"account_id": "<account>"}`: the scope of an account grant. Nothing else may be in it.
pub fn account_scope(scope: &str) -> Result<AccountId, String> {
    let value: near_sdk::serde_json::Value =
        near_sdk::serde_json::from_str(scope).map_err(|_| "scope is not JSON".to_string())?;
    let object = value.as_object().ok_or("scope is not an object")?;
    if object.keys().any(|k| k != "account_id") {
        return Err("scope names more than the account".to_string());
    }
    object
        .get("account_id")
        .and_then(|v| v.as_str())
        .ok_or("scope.account_id is missing")?
        .parse::<AccountId>()
        .map_err(|_| "scope.account_id is not an account id".to_string())
}

impl X207Circle {
    /// Rules of an account that is sold. `account` is the sold account, `grantor` the seller (a different account: the buyer pays the grantor, so a grantor that
    /// sold its own account would be paid by the buyer into the account that the buyer receives).
    ///
    /// - The asset type must match: `near:member_account` exactly when the account is a member of this circle, `near:account` otherwise
    ///   (`ERR_ACCOUNT_ASSET_TYPE`).
    /// - A member account must be `active`, not frozen, without a bond, without a withdrawal or an exit in flight; an EVM member is not sellable
    ///   (`ERR_ACCOUNT_NOT_SELLABLE`). The buyer inherits stake and debt.
    pub(crate) fn assert_account_sellable(
        &self,
        account: &AccountId,
        grantor: &AccountId,
        asset_type: &str,
    ) {
        require!(
            account != grantor,
            format!("ERR_ACCOUNT_SELF: the grantor {grantor} cannot sell its own account")
        );
        let member = self.members.get(account);
        let expected = if member.is_some() {
            ASSET_TYPE_MEMBER_ACCOUNT
        } else {
            ASSET_TYPE_ACCOUNT
        };
        require!(
            asset_type == expected,
            format!("ERR_ACCOUNT_ASSET_TYPE: {account} is sold as {expected}, the grant says {asset_type}")
        );
        if let Some(m) = member {
            require!(
                !crate::evm::is_evm_member(account),
                format!("ERR_ACCOUNT_NOT_SELLABLE: {account} is an EVM member")
            );
            require!(
                m.status == MemberStatus::Active,
                format!("ERR_ACCOUNT_NOT_SELLABLE: {account} is {:?}", m.status)
            );
            require!(
                m.exit_requested_epoch.is_none() && !m.withdraw_pending,
                format!("ERR_ACCOUNT_NOT_SELLABLE: {account} is leaving the circle")
            );
            require!(
                crate::deal::bond_locked_of(account) == 0,
                format!("ERR_ACCOUNT_NOT_SELLABLE: {account} has a bond locked")
            );
            crate::freeze::assert_not_frozen(account, "be sold");
        }
    }
}
