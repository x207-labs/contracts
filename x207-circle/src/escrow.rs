//! F8 class A escrow (CT-075, spec 15 and the note Fitur F8): the grantor of a class A grant hands an NFT to the circle with
//! `nft_transfer_call` (`nft_on_transfer`), the buyer pays with a voucher and receives it (`complete_escrow`, `on_escrow_resolved`), or the grantor
//! takes it back after the time of the grant (`cancel_escrow`). Everything lives under `StorageKey::Escrows` and one raw lock key.
//!
//! Order of `complete_escrow`: the voucher is registered first (all checks of `register_cumulative`), then `nft_transfer` to the buyer. If that
//! transfer fails, `on_escrow_resolved` takes the registration back exactly (pair, debt, totals, edge), which is only possible because the pair
//! is locked against other vouchers (`ERR_ESCROW_PENDING`) until then and because `complete_escrow` refuses to start within
//! `ESCROW_CUTOFF_MARGIN_NS` of the cutoff. Accounts of NEAR through a locker contract are not part of this build.

use near_sdk::borsh::BorshDeserialize;
use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::store::LookupMap;
use near_sdk::{
    env, near, require, AccountId, Gas, NearToken, Promise, PromiseError, PromiseOrValue, PublicKey,
};

use crate::canonical::Voucher;
use crate::clearing::EpochPhase;
use crate::events::{
    EscrowFailed, EscrowHeld, FtEscrowHeld, GrantActivated, GrantRevoked, MtEscrowHeld, X207Event,
};
use crate::grant::{load_grant, AssetClass, GrantId, GrantStatus, RevokeReason};
use crate::state::{StorageKey, X207Circle, X207CircleExt};
use crate::types::EpochId;
use crate::voucher::{PairState, ParticipantTotals, RegisterMode};

/// `complete_escrow` is refused this close to the cutoff of the epoch, so that the callback cannot meet a closed epoch.
pub const ESCROW_CUTOFF_MARGIN_NS: u64 = 120_000_000_000;
const NFT_TRANSFER_GAS: Gas = Gas::from_tgas(30);
const RESOLVE_GAS: Gas = Gas::from_tgas(30);
/// `release` or `give_back` of the locker: a state check and one `AddKey` action.
const LOCKER_GAS: Gas = Gas::from_tgas(15);
const LOCK_PREFIX: &[u8] = b"x207_escrow_lock:";

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EscrowState {
    /// The NFT is in the circle and the grant is waiting for a buyer.
    Held,
    /// The voucher is registered and `nft_transfer` to the buyer is in flight.
    Completing,
    /// `nft_transfer` back to the grantor is in flight.
    Cancelling,
    /// The buyer has the NFT.
    Delivered,
    /// The grantor has the NFT back.
    Cancelled,
    /// The transfer failed and the registration could not be taken back (the pair or the epoch moved). The NFT stays in the circle.
    Stuck,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EscrowStep {
    Complete,
    Cancel,
}

/// What `complete_escrow` changed, enough to take it back.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    epoch: EpochId,
    payer: AccountId,
    payee: AccountId,
    pair_before: Option<PairState>,
    delta: u128,
    payer_new: bool,
    payee_new: bool,
    seq: u64,
    cumulative: u128,
}

#[near(serializers = [borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Escrow {
    pub nft_contract: AccountId,
    pub token_id: String,
    pub state: EscrowState,
    pub buyer: Option<AccountId>,
    pub snapshot: Option<Snapshot>,
}

/// Class A escrow of an exact amount of a NEP-141 token (CT-090). Same life cycle and same take-back as the NFT escrow; kept apart so that the
/// stored `Escrow` of 0.0.9 reads unchanged after an upgrade.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FtEscrow {
    pub ft_contract: AccountId,
    pub amount: u128,
    pub state: EscrowState,
    pub buyer: Option<AccountId>,
    pub snapshot: Option<Snapshot>,
}

/// Class A escrow of an exact amount of one NEP-245 multi-token (CT-092). Same life cycle and take-back as the NFT and the NEP-141 escrow.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MtEscrow {
    pub mt_contract: AccountId,
    pub token_id: String,
    pub amount: u128,
    pub state: EscrowState,
    pub buyer: Option<AccountId>,
    pub snapshot: Option<Snapshot>,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MtEscrowView {
    pub grant_id: GrantId,
    pub mt_contract: AccountId,
    pub token_id: String,
    pub amount: U128,
    pub state: EscrowState,
    pub buyer: Option<AccountId>,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FtEscrowView {
    pub grant_id: GrantId,
    pub ft_contract: AccountId,
    pub amount: U128,
    pub state: EscrowState,
    pub buyer: Option<AccountId>,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EscrowView {
    pub grant_id: GrantId,
    pub nft_contract: AccountId,
    pub token_id: String,
    pub state: EscrowState,
    pub buyer: Option<AccountId>,
}

fn escrows() -> LookupMap<GrantId, Escrow> {
    LookupMap::new(StorageKey::Escrows)
}

pub fn load_escrow(grant_id: GrantId) -> Option<Escrow> {
    escrows().get(&grant_id).cloned()
}

fn save(grant_id: GrantId, escrow: Escrow) {
    let mut map = escrows();
    map.insert(grant_id, escrow);
    map.flush();
}

fn ft_escrows() -> LookupMap<GrantId, FtEscrow> {
    LookupMap::new(StorageKey::FtEscrows)
}

pub fn load_ft_escrow(grant_id: GrantId) -> Option<FtEscrow> {
    ft_escrows().get(&grant_id).cloned()
}

fn mt_escrows() -> LookupMap<GrantId, MtEscrow> {
    LookupMap::new(StorageKey::MtEscrows)
}

pub fn load_mt_escrow(grant_id: GrantId) -> Option<MtEscrow> {
    mt_escrows().get(&grant_id).cloned()
}

/// The escrow of a grant, whichever asset it holds. A grant has at most one.
#[derive(Clone, Debug)]
enum Held {
    Nft(Escrow),
    Ft(FtEscrow),
    Mt(MtEscrow),
    Account(crate::account_escrow::AccountEscrow),
}

impl Held {
    fn load(grant_id: GrantId) -> Option<Held> {
        load_escrow(grant_id)
            .map(Held::Nft)
            .or_else(|| load_ft_escrow(grant_id).map(Held::Ft))
            .or_else(|| load_mt_escrow(grant_id).map(Held::Mt))
            .or_else(|| crate::account_escrow::load_account_escrow(grant_id).map(Held::Account))
    }

    fn save(self, grant_id: GrantId) {
        match self {
            Held::Nft(e) => save(grant_id, e),
            Held::Ft(e) => {
                let mut map = ft_escrows();
                map.insert(grant_id, e);
                map.flush();
            }
            Held::Mt(e) => {
                let mut map = mt_escrows();
                map.insert(grant_id, e);
                map.flush();
            }
            Held::Account(e) => crate::account_escrow::save_account_escrow(grant_id, e),
        }
    }

    /// The contract of the token that is held.
    fn contract(&self) -> &AccountId {
        match self {
            Held::Nft(e) => &e.nft_contract,
            Held::Ft(e) => &e.ft_contract,
            Held::Mt(e) => &e.mt_contract,
            Held::Account(e) => &e.account_id,
        }
    }

    fn state(&self) -> EscrowState {
        match self {
            Held::Nft(e) => e.state,
            Held::Ft(e) => e.state,
            Held::Mt(e) => e.state,
            Held::Account(e) => e.state,
        }
    }

    fn set_state(&mut self, state: EscrowState) {
        match self {
            Held::Nft(e) => e.state = state,
            Held::Ft(e) => e.state = state,
            Held::Mt(e) => e.state = state,
            Held::Account(e) => e.state = state,
        }
    }

    fn set_buyer(&mut self, buyer: Option<AccountId>) {
        match self {
            Held::Nft(e) => e.buyer = buyer,
            Held::Ft(e) => e.buyer = buyer,
            Held::Mt(e) => e.buyer = buyer,
            Held::Account(e) => e.buyer = buyer,
        }
    }

    fn buyer(&self) -> Option<AccountId> {
        match self {
            Held::Nft(e) => e.buyer.clone(),
            Held::Ft(e) => e.buyer.clone(),
            Held::Mt(e) => e.buyer.clone(),
            Held::Account(e) => e.buyer.clone(),
        }
    }

    fn set_snapshot(&mut self, snapshot: Option<Snapshot>) {
        match self {
            Held::Nft(e) => e.snapshot = snapshot,
            Held::Ft(e) => e.snapshot = snapshot,
            Held::Mt(e) => e.snapshot = snapshot,
            Held::Account(e) => e.snapshot = snapshot,
        }
    }

    fn take_snapshot(&mut self) -> Snapshot {
        match self {
            Held::Nft(e) => e.snapshot.take(),
            Held::Ft(e) => e.snapshot.take(),
            Held::Mt(e) => e.snapshot.take(),
            Held::Account(e) => e.snapshot.take(),
        }
        .expect("snapshot of a completing escrow")
    }

    /// The transfer that hands the asset to `receiver`, followed by `on_escrow_resolved`.
    fn send(
        &self,
        circle: &X207Circle,
        receiver: AccountId,
        grant_id: GrantId,
        step: EscrowStep,
    ) -> Promise {
        match self {
            Held::Nft(e) => circle.send_and_resolve(
                e.nft_contract.clone(),
                e.token_id.clone(),
                receiver,
                grant_id,
                step,
            ),
            Held::Ft(e) => circle.send_ft_and_resolve(
                e.ft_contract.clone(),
                e.amount,
                receiver,
                grant_id,
                step,
            ),
            Held::Mt(e) => circle.send_mt_and_resolve(
                e.mt_contract.clone(),
                e.token_id.clone(),
                e.amount,
                receiver,
                grant_id,
                step,
            ),
            // the locker moves the keys of the account: to the key of the buyer on `Complete`, back to the seller on `Cancel`
            Held::Account(e) => circle.send_locker_and_resolve(
                e.account_id.clone(),
                e.delivery_pubkey.clone(),
                grant_id,
                step,
            ),
        }
    }
}

fn lock_key(epoch: EpochId, payer: &AccountId, payee: &AccountId) -> Vec<u8> {
    [
        LOCK_PREFIX,
        epoch.to_le_bytes().as_slice(),
        payer.as_bytes(),
        b">",
        payee.as_bytes(),
    ]
    .concat()
}

/// `register_with` asks this before it touches a pair: a pair with an escrow payment in flight takes no other voucher.
pub fn pair_locked(epoch: EpochId, payer: &AccountId, payee: &AccountId) -> bool {
    env::storage_has_key(&lock_key(epoch, payer, payee))
}

/// An escrow that holds an asset keeps its grant from being revoked: `cancel_escrow` is the way out.
pub fn holds_asset(grant_id: GrantId) -> bool {
    Held::load(grant_id).is_some_and(|e| {
        matches!(
            e.state(),
            EscrowState::Held
                | EscrowState::Completing
                | EscrowState::Cancelling
                | EscrowState::Stuck
        )
    })
}

#[near]
impl X207Circle {
    /// NEP-171 receiver. `msg` is `{"grant_id": N}`. Only a class A grant that is `offered`, whose grantor sends the token itself and has no
    /// escrow yet. Returns `false`: the token stays. A panic gives the token back to the sender (the NFT contract resolves it).
    pub fn nft_on_transfer(
        &mut self,
        sender_id: AccountId,
        previous_owner_id: AccountId,
        token_id: String,
        msg: String,
    ) -> PromiseOrValue<bool> {
        let _ = sender_id;
        let nft_contract = env::predecessor_account_id();
        let grant_id = near_sdk::serde_json::from_str::<near_sdk::serde_json::Value>(&msg)
            .ok()
            .and_then(|v| v.get("grant_id").and_then(|g| g.as_u64()))
            .unwrap_or_else(|| env::panic_str("ERR_BAD_MSG: msg must be {\"grant_id\": N}"));
        let grant = load_grant(grant_id);
        require!(
            grant.class == AssetClass::A,
            format!(
                "ERR_ESCROW_NOT_CLASS_A: grant {grant_id} is class {:?}",
                grant.class
            )
        );
        require!(
            grant.status == GrantStatus::Offered,
            format!("ERR_GRANT_NOT_OFFERED: {:?}", grant.status)
        );
        crate::asset_contracts::assert_official(&grant.asset_type, &nft_contract);
        // security review F1: only the NFT contract and the token that the grantor named in the scope may fill the escrow
        let target = crate::grant::class_a_target(&grant.limits.scope);
        require!(
            target.as_ref().is_ok_and(|(nft, token)| *nft == nft_contract && *token == token_id),
            format!("ERR_ESCROW_NFT_MISMATCH: grant {grant_id} takes {:?}, got {nft_contract} {token_id}", target.as_ref().ok())
        );
        require!(
            previous_owner_id == grant.grantor,
            format!(
                "ERR_ESCROW_DEPOSITOR: only {} may put the asset of grant {grant_id}",
                grant.grantor
            )
        );
        require!(
            Held::load(grant_id).is_none(),
            format!("ERR_ESCROW_EXISTS: grant {grant_id}")
        );
        require!(
            env::block_timestamp() < grant.limits.expires_at.0,
            "ERR_GRANT_EXPIRED: the grant ran out"
        );
        save(
            grant_id,
            Escrow {
                nft_contract: nft_contract.clone(),
                token_id: token_id.clone(),
                state: EscrowState::Held,
                buyer: None,
                snapshot: None,
            },
        );
        EscrowHeld {
            grant_id,
            nft_contract,
            token_id,
        }
        .emit();
        PromiseOrValue::Value(false)
    }

    pub fn get_escrow(&self, grant_id: GrantId) -> Option<EscrowView> {
        load_escrow(grant_id).map(|e| EscrowView {
            grant_id,
            nft_contract: e.nft_contract,
            token_id: e.token_id,
            state: e.state,
            buyer: e.buyer,
        })
    }

    pub fn get_mt_escrow(&self, grant_id: GrantId) -> Option<MtEscrowView> {
        load_mt_escrow(grant_id).map(|e| MtEscrowView {
            grant_id,
            mt_contract: e.mt_contract,
            token_id: e.token_id,
            amount: U128(e.amount),
            state: e.state,
            buyer: e.buyer,
        })
    }

    pub fn get_ft_escrow(&self, grant_id: GrantId) -> Option<FtEscrowView> {
        load_ft_escrow(grant_id).map(|e| FtEscrowView {
            grant_id,
            ft_contract: e.ft_contract,
            amount: U128(e.amount),
            state: e.state,
            buyer: e.buyer,
        })
    }

    /// The buyer pays and receives the asset. The caller is the payer of `voucher`, the payee is the grantor, and the voucher raises the
    /// pair by at least `unit_price`. Registers like `register_cumulative`, then sends the NFT; the grant becomes `active` with the buyer as
    /// grantee in `on_escrow_resolved`. Checks in order: `ERR_UNKNOWN_GRANT`, `ERR_ESCROW_NOT_CLASS_A`, `ERR_GRANT_NOT_OFFERED`,
    /// `ERR_GRANT_EXPIRED`, `ERR_ESCROW_UNKNOWN`, `ERR_ESCROW_NOT_HELD`, `ERR_GRANT_SELF`, `ERR_ESCROW_VOUCHER`, `ERR_ESCROW_PRICE`,
    /// `ERR_ESCROW_TOO_LATE`, `ERR_ESCROW_PENDING`, then the checks of the voucher itself.
    pub fn complete_escrow(
        &mut self,
        grant_id: GrantId,
        voucher: Voucher,
        signature: String,
    ) -> Promise {
        self.complete_inner(grant_id, voucher, signature, None)
    }

    /// `complete_escrow` for the escrow of a NEAR account (CT-126, 0.0.18): the buyer also names the full-access key that the locker adds to the account
    /// (`delivery_pubkey`). Same checks as `complete_escrow`, plus the rules of an account (`ERR_ACCOUNT_*`, `ERR_ESCROW_EPOCH_WAIT` for a member account).
    /// On a token escrow it fails with `ERR_ESCROW_NOT_ACCOUNT`; `complete_escrow` on an account escrow fails with `ERR_ESCROW_NEEDS_PUBKEY`.
    pub fn complete_account_escrow(
        &mut self,
        grant_id: GrantId,
        voucher: Voucher,
        signature: String,
        delivery_pubkey: PublicKey,
    ) -> Promise {
        self.complete_inner(grant_id, voucher, signature, Some(delivery_pubkey))
    }

    /// `account_on_lock`: called by the locker that the seller put on the sold account (`predecessor` is that account). Checks: class A grant that is `offered`,
    /// not expired, with the scope `{"account_id": predecessor}`, a grantor that is not frozen, the rules of `assert_account_sellable`, no escrow yet.
    /// Records the escrow as `held` and emits `account_escrow_held`. A panic makes the locker return the keys to the seller. `get_account_escrow` reads it.
    pub fn account_on_lock(&mut self, grant_id: GrantId) {
        let account = env::predecessor_account_id();
        let grant = load_grant(grant_id);
        require!(
            grant.class == AssetClass::A,
            format!("ERR_ESCROW_NOT_CLASS_A: grant {grant_id}")
        );
        require!(
            grant.status == GrantStatus::Offered,
            format!("ERR_GRANT_NOT_OFFERED: {:?}", grant.status)
        );
        require!(
            env::block_timestamp() < grant.limits.expires_at.0,
            "ERR_GRANT_EXPIRED: the grant ran out"
        );
        crate::account_escrow::assert_escrow_ttl(grant.limits.expires_at.0);
        let named = crate::account_escrow::account_scope(&grant.limits.scope);
        require!(
            named.as_ref().is_ok_and(|a| *a == account),
            format!(
                "ERR_ESCROW_ACCOUNT_MISMATCH: grant {grant_id} sells {:?}, the locker is on {account}",
                named.as_ref().ok()
            )
        );
        require!(
            Held::load(grant_id).is_none(),
            format!("ERR_ESCROW_EXISTS: grant {grant_id}")
        );
        crate::freeze::assert_not_frozen(&grant.grantor, "sell an account");
        self.assert_account_sellable(&account, &grant.grantor, &grant.asset_type);
        if self.members.contains_key(&account) {
            crate::account_escrow::mark_locked(&account, grant_id);
        }
        crate::account_escrow::save_account_escrow(
            grant_id,
            crate::account_escrow::AccountEscrow {
                account_id: account.clone(),
                state: EscrowState::Held,
                buyer: None,
                snapshot: None,
                locked_epoch: self.current_epoch,
                delivery_pubkey: None,
            },
        );
        crate::events::AccountEscrowHeld {
            grant_id,
            account_id: account,
        }
        .emit();
    }

    pub fn get_account_escrow(
        &self,
        grant_id: GrantId,
    ) -> Option<crate::account_escrow::AccountEscrowView> {
        crate::account_escrow::load_account_escrow(grant_id).map(|e| {
            crate::account_escrow::AccountEscrowView {
                grant_id,
                account_id: e.account_id,
                state: e.state,
                buyer: e.buyer,
                locked_epoch: e.locked_epoch,
                delivery_pubkey: e.delivery_pubkey,
            }
        })
    }

    /// Owner only (the DAO). The official code hashes of the locker (base58 sha256, at most 8), for the SDK to compare with the code of an account that is for sale.
    /// An empty list clears it. `ERR_NOT_OWNER`, `ERR_TOO_MANY_HASHES`, `ERR_BAD_HASH`, `ERR_DUPLICATE_HASH`.
    pub fn set_locker_code_hashes(&mut self, hashes: Vec<String>) {
        self.assert_owner();
        crate::account_escrow::set_locker_code_hashes(&hashes);
    }

    pub fn get_locker_code_hashes(&self) -> Vec<String> {
        crate::account_escrow::locker_code_hashes()
    }

    /// Owner only (the DAO). CT-129: the longest time, in nanoseconds, that the grant of an account may still run when the locker registers
    /// (`account_on_lock`, `ERR_ESCROW_TTL_TOO_LONG`). Default 72 hours; `ERR_BAD_ESCROW_TTL` outside 1 hour to 30 days. `ERR_NOT_OWNER`.
    pub fn set_max_escrow_ttl(&mut self, ttl_ns: near_sdk::json_types::U64) {
        self.assert_owner();
        crate::account_escrow::set_max_escrow_ttl_ns(ttl_ns.0);
    }

    /// The limit in force, in nanoseconds.
    pub fn get_max_escrow_ttl(&self) -> near_sdk::json_types::U64 {
        near_sdk::json_types::U64(crate::account_escrow::max_escrow_ttl_ns())
    }
}

impl X207Circle {
    fn complete_inner(
        &mut self,
        grant_id: GrantId,
        voucher: Voucher,
        signature: String,
        delivery_pubkey: Option<PublicKey>,
    ) -> Promise {
        let grant = load_grant(grant_id);
        require!(
            grant.class == AssetClass::A,
            format!("ERR_ESCROW_NOT_CLASS_A: grant {grant_id}")
        );
        require!(
            grant.status == GrantStatus::Offered,
            format!("ERR_GRANT_NOT_OFFERED: {:?}", grant.status)
        );
        let now = env::block_timestamp();
        require!(
            now < grant.limits.expires_at.0,
            "ERR_GRANT_EXPIRED: the grant ran out"
        );
        let mut escrow = Held::load(grant_id)
            .unwrap_or_else(|| env::panic_str(&format!("ERR_ESCROW_UNKNOWN: grant {grant_id}")));
        require!(
            escrow.state() == EscrowState::Held,
            format!("ERR_ESCROW_NOT_HELD: {:?}", escrow.state())
        );
        // CT-096 (review C1): a token put in escrow before the owner set the list is not sold after it; `cancel_escrow` stays open so that the grantor takes it back
        crate::asset_contracts::assert_official(&grant.asset_type, escrow.contract());
        match (&mut escrow, delivery_pubkey) {
            (Held::Account(account), Some(key)) => {
                // a locker adds a full-access key: any curve of NEAR is fine for an account without a position, a member account keeps ed25519 (voucher key)
                self.assert_account_sellable(
                    &account.account_id,
                    &grant.grantor,
                    &grant.asset_type,
                );
                if self.members.contains_key(&account.account_id) {
                    require!(
                        key.curve_type() == near_sdk::CurveType::ED25519,
                        "ERR_BAD_PUBKEY: the key of the buyer of a member account is ed25519"
                    );
                    self.assert_epochs_settled(account.locked_epoch);
                }
                account.delivery_pubkey = Some(key);
            }
            (Held::Account(_), None) => {
                env::panic_str("ERR_ESCROW_NEEDS_PUBKEY: use complete_account_escrow")
            }
            (_, Some(_)) => env::panic_str(
                "ERR_ESCROW_NOT_ACCOUNT: this escrow holds a token, use complete_escrow",
            ),
            (_, None) => {}
        }
        let buyer = env::predecessor_account_id();
        crate::freeze::assert_not_frozen(&buyer, "buy an escrowed asset");
        require!(buyer != grant.grantor, format!("ERR_GRANT_SELF: {buyer}"));
        require!(
            voucher.payer == buyer && voucher.payee == grant.grantor,
            "ERR_ESCROW_VOUCHER: the caller pays the grantor"
        );

        let epoch_id = voucher.epoch;
        let key = (epoch_id, voucher.payer.clone(), voucher.payee.clone());
        let pair_before = self.pairs.get(&key).cloned();
        let delta = voucher
            .cumulative_amount
            .0
            .saturating_sub(pair_before.as_ref().map_or(0, |p| p.cumulative));
        require!(
            delta >= grant.unit_price.0,
            format!(
                "ERR_ESCROW_PRICE: the voucher raises the pair by {delta}, the price is {}",
                grant.unit_price.0
            )
        );
        if let Some(epoch) = self.epochs.get(&epoch_id) {
            require!(
                now + ESCROW_CUTOFF_MARGIN_NS < epoch.cutoff_at,
                format!("ERR_ESCROW_TOO_LATE: cutoff_at {}", epoch.cutoff_at)
            );
        }
        require!(
            !pair_locked(epoch_id, &voucher.payer, &voucher.payee),
            "ERR_ESCROW_PENDING: another escrow payment of this pair is in flight"
        );
        let payer_new = self
            .totals
            .get(&(epoch_id, voucher.payer.clone()))
            .is_none();
        let payee_new = self
            .totals
            .get(&(epoch_id, voucher.payee.clone()))
            .is_none();
        let (seq, cumulative) = (voucher.seq, voucher.cumulative_amount.0);

        self.register_with(voucher.clone(), signature, RegisterMode::Cumulative, None);

        env::storage_write(&lock_key(epoch_id, &voucher.payer, &voucher.payee), &[1]);
        escrow.set_state(EscrowState::Completing);
        escrow.set_buyer(Some(buyer.clone()));
        escrow.set_snapshot(Some(Snapshot {
            epoch: epoch_id,
            payer: voucher.payer,
            payee: voucher.payee,
            pair_before,
            delta,
            payer_new,
            payee_new,
            seq,
            cumulative,
        }));
        let call = escrow.send(self, buyer, grant_id, EscrowStep::Complete);
        escrow.save(grant_id);
        call
    }
}

#[near]
impl X207Circle {
    /// The grantor takes the asset back after `expires_at` of the grant, when nobody bought it.
    pub fn cancel_escrow(&mut self, grant_id: GrantId) -> Promise {
        let grant = load_grant(grant_id);
        require!(
            env::predecessor_account_id() == grant.grantor,
            format!("ERR_ESCROW_NOT_GRANTOR: only {} cancels", grant.grantor)
        );
        self.cancel_inner(grant_id, grant)
    }

    /// Anyone may take an account back to the seller once the grant ran out (CT-126): the key goes only to the `return_pubkey` that the locker recorded at the
    /// lock, so the caller gains nothing, and a seller who lost its grantor account is not left with a locked account. Same rules as `cancel_escrow`: only
    /// after `expires_at`, only while the escrow is `held` (so a second call, or a call while a sale completes, is refused). Refused for token escrows
    /// (`ERR_ESCROW_NOT_ACCOUNT`).
    pub fn cancel_expired_account_escrow(&mut self, grant_id: GrantId) -> Promise {
        let grant = load_grant(grant_id);
        require!(
            crate::account_escrow::load_account_escrow(grant_id).is_some(),
            format!("ERR_ESCROW_NOT_ACCOUNT: grant {grant_id} holds no account")
        );
        self.cancel_inner(grant_id, grant)
    }
}

impl X207Circle {
    fn cancel_inner(&mut self, grant_id: GrantId, grant: crate::grant::AccessGrant) -> Promise {
        let mut escrow = Held::load(grant_id)
            .unwrap_or_else(|| env::panic_str(&format!("ERR_ESCROW_UNKNOWN: grant {grant_id}")));
        require!(
            escrow.state() == EscrowState::Held,
            format!("ERR_ESCROW_NOT_HELD: {:?}", escrow.state())
        );
        require!(
            grant.status == GrantStatus::Offered,
            format!("ERR_GRANT_NOT_OFFERED: {:?}", grant.status)
        );
        require!(
            env::block_timestamp() >= grant.limits.expires_at.0,
            format!(
                "ERR_ESCROW_NOT_EXPIRED: expires_at {}",
                grant.limits.expires_at.0
            )
        );
        escrow.set_state(EscrowState::Cancelling);
        let call = escrow.send(self, grant.grantor, grant_id, EscrowStep::Cancel);
        escrow.save(grant_id);
        call
    }
}

#[near]
impl X207Circle {
    /// Callback of the `nft_transfer` of `complete_escrow` or `cancel_escrow`.
    #[private]
    pub fn on_escrow_resolved(&mut self, grant_id: GrantId, step: EscrowStep) {
        // `TooLong` means the call succeeded and returned more than 64 bytes (a non-standard NFT contract); it is not a failure
        let ok = !matches!(
            env::promise_result_checked(0, 64),
            Err(PromiseError::Failed)
        );
        let mut escrow =
            Held::load(grant_id).unwrap_or_else(|| env::panic_str("ERR_ESCROW_UNKNOWN"));
        let mut grant = load_grant(grant_id);
        match (step, ok) {
            (EscrowStep::Complete, true) => {
                let snapshot = escrow.take_snapshot();
                env::storage_remove(&lock_key(snapshot.epoch, &snapshot.payer, &snapshot.payee));
                let buyer = escrow.buyer().expect("buyer of a completing escrow");
                escrow.set_state(EscrowState::Delivered);
                let delivery_pubkey = match &escrow {
                    Held::Account(e) => e.delivery_pubkey.clone(),
                    _ => None,
                };
                // CT-126 (#14): the voucher key of a member account becomes the key of the buyer, so that vouchers signed by the seller's old key cannot add debt
                if let (Held::Account(e), Some(key)) = (&escrow, &delivery_pubkey) {
                    if let Some(member) = self.members.get_mut(&e.account_id) {
                        member.pubkey_voucher = key.clone();
                    }
                }
                if let Held::Account(e) = &escrow {
                    crate::account_escrow::unmark_locked(&e.account_id);
                }
                crate::grant::activate(&mut grant, buyer.clone(), delivery_pubkey.clone());
                escrow.save(grant_id);
                GrantActivated {
                    grant_id,
                    grantee: buyer,
                    delivery_pubkey,
                }
                .emit();
            }
            (EscrowStep::Complete, false) => {
                let snapshot = escrow.take_snapshot();
                env::storage_remove(&lock_key(snapshot.epoch, &snapshot.payer, &snapshot.payee));
                let reverted = self.take_back(&snapshot);
                escrow.set_buyer(None);
                escrow.set_state(if reverted {
                    EscrowState::Held
                } else {
                    EscrowState::Stuck
                });
                escrow.save(grant_id);
                EscrowFailed {
                    grant_id,
                    step: EscrowStep::Complete,
                    epoch: snapshot.epoch,
                    payer: snapshot.payer,
                    payee: snapshot.payee,
                    delta: U128(if reverted { snapshot.delta } else { 0 }),
                    reverted,
                }
                .emit();
            }
            (EscrowStep::Cancel, true) => {
                if let Held::Account(e) = &escrow {
                    crate::account_escrow::unmark_locked(&e.account_id);
                }
                escrow.set_state(EscrowState::Cancelled);
                escrow.save(grant_id);
                crate::grant::close(&mut grant, GrantStatus::Revoked);
                GrantRevoked {
                    grant_id,
                    reason: RevokeReason::Grantor,
                }
                .emit();
            }
            (EscrowStep::Cancel, false) => {
                escrow.set_state(EscrowState::Held);
                let grantor = grant.grantor.clone();
                escrow.save(grant_id);
                EscrowFailed {
                    grant_id,
                    step: EscrowStep::Cancel,
                    epoch: 0,
                    payer: grantor.clone(),
                    payee: grantor,
                    delta: U128(0),
                    reverted: false,
                }
                .emit();
            }
        }
    }
}

impl X207Circle {
    /// `release(grant_id, delivery_pubkey)` of the locker on `Complete`, `give_back(grant_id)` on `Cancel`, then `on_escrow_resolved`.
    fn send_locker_and_resolve(
        &self,
        account: AccountId,
        delivery_pubkey: Option<PublicKey>,
        grant_id: GrantId,
        step: EscrowStep,
    ) -> Promise {
        let call = match (step, delivery_pubkey) {
            (EscrowStep::Complete, Some(key)) => Promise::new(account).function_call(
                "release".to_string(),
                json!({"grant_id": grant_id, "pubkey": key})
                    .to_string()
                    .into_bytes(),
                NearToken::from_yoctonear(0),
                LOCKER_GAS,
            ),
            (EscrowStep::Complete, None) => env::panic_str("ERR_ESCROW_NEEDS_PUBKEY"),
            (EscrowStep::Cancel, _) => Promise::new(account).function_call(
                "give_back".to_string(),
                json!({"grant_id": grant_id}).to_string().into_bytes(),
                NearToken::from_yoctonear(0),
                LOCKER_GAS,
            ),
        };
        call.then(
            Self::ext(env::current_account_id())
                .with_static_gas(RESOLVE_GAS)
                .on_escrow_resolved(grant_id, step),
        )
    }

    fn send_and_resolve(
        &self,
        nft: AccountId,
        token_id: String,
        receiver: AccountId,
        grant_id: GrantId,
        step: EscrowStep,
    ) -> Promise {
        Promise::new(nft)
            .function_call(
                "nft_transfer".to_string(),
                json!({"receiver_id": receiver, "token_id": token_id, "memo": format!("x207 escrow grant {grant_id}")}).to_string().into_bytes(),
                NearToken::from_yoctonear(1),
                NFT_TRANSFER_GAS,
            )
            .then(Self::ext(env::current_account_id()).with_static_gas(RESOLVE_GAS).on_escrow_resolved(grant_id, step))
    }

    fn send_ft_and_resolve(
        &self,
        ft: AccountId,
        amount: u128,
        receiver: AccountId,
        grant_id: GrantId,
        step: EscrowStep,
    ) -> Promise {
        Promise::new(ft)
            .function_call(
                "ft_transfer".to_string(),
                json!({"receiver_id": receiver, "amount": U128(amount), "memo": format!("x207 escrow grant {grant_id}")}).to_string().into_bytes(),
                NearToken::from_yoctonear(1),
                NFT_TRANSFER_GAS,
            )
            .then(Self::ext(env::current_account_id()).with_static_gas(RESOLVE_GAS).on_escrow_resolved(grant_id, step))
    }

    fn send_mt_and_resolve(
        &self,
        mt: AccountId,
        token_id: String,
        amount: u128,
        receiver: AccountId,
        grant_id: GrantId,
        step: EscrowStep,
    ) -> Promise {
        Promise::new(mt)
            .function_call(
                "mt_transfer".to_string(),
                json!({"receiver_id": receiver, "token_id": token_id, "amount": U128(amount), "approval": null, "memo": format!("x207 escrow grant {grant_id}")})
                    .to_string()
                    .into_bytes(),
                NearToken::from_yoctonear(1),
                NFT_TRANSFER_GAS,
            )
            .then(Self::ext(env::current_account_id()).with_static_gas(RESOLVE_GAS).on_escrow_resolved(grant_id, step))
    }

    /// The `escrow` action of `mt_on_transfer` (CT-092): exactly one multi-token, exactly the amount of the scope `{"mt_contract","token_id","amount"}`, put
    /// by the grantor itself (`previous_owner_ids` is the grantor alone), and the caller is the `mt_contract` of the scope. The USDT of the circle held at
    /// Intents (`nep141:<usdt>`) is not an escrow asset. A refusal does **not** panic: it logs the reason (`x207 mt escrow refused: ERR_…`) and returns
    /// every amount as unused, so that `mt_resolve_transfer` of the multi-token contract hands everything back to the sender, whatever that contract does
    /// with a failed receipt. Returns `[0]` when the deposit is kept.
    pub(crate) fn escrow_mt_from_transfer(
        &mut self,
        mt_contract: AccountId,
        previous_owner_ids: Vec<AccountId>,
        token_ids: Vec<String>,
        amounts: Vec<U128>,
        grant_id: Option<GrantId>,
    ) -> PromiseOrValue<Vec<U128>> {
        let refund = amounts.clone();
        macro_rules! refuse_unless {
            ($cond:expr, $msg:expr) => {
                if !($cond) {
                    env::log_str(&format!("x207 mt escrow refused: {}", $msg));
                    return PromiseOrValue::Value(refund);
                }
            };
        }
        let Some(grant_id) = grant_id else {
            refuse_unless!(
                false,
                "ERR_BAD_MSG: msg must be {\"action\":\"escrow\",\"grant_id\":N}"
            );
            unreachable!()
        };
        refuse_unless!(
            token_ids.len() == 1 && amounts.len() == 1 && previous_owner_ids.len() == 1,
            format!("ERR_ESCROW_MT_BATCH: one token, one amount and one owner per deposit, got {} tokens", token_ids.len())
        );
        let (token_id, amount, owner) = (
            token_ids[0].clone(),
            amounts[0].0,
            previous_owner_ids[0].clone(),
        );
        refuse_unless!(amount > 0, "ERR_ZERO_AMOUNT");
        refuse_unless!(
            !(mt_contract == self.intents_id
                && token_id == format!("nep141:{}", self.usdt_token_id)),
            "ERR_ESCROW_USDT: the USDT of the circle is not an escrow asset"
        );
        let Some(grant) = crate::grant::try_load_grant(grant_id) else {
            refuse_unless!(false, format!("ERR_UNKNOWN_GRANT: {grant_id}"));
            unreachable!()
        };
        refuse_unless!(
            grant.class == AssetClass::A,
            format!(
                "ERR_ESCROW_NOT_CLASS_A: grant {grant_id} is class {:?}",
                grant.class
            )
        );
        refuse_unless!(
            grant.status == GrantStatus::Offered,
            format!("ERR_GRANT_NOT_OFFERED: {:?}", grant.status)
        );
        refuse_unless!(
            crate::asset_contracts::is_official(&grant.asset_type, &mt_contract),
            format!(
                "ERR_CONTRACT_NOT_OFFICIAL: {mt_contract} is not an official contract of {}",
                grant.asset_type
            )
        );
        let want = crate::grant::class_a_asset(&grant.limits.scope);
        refuse_unless!(
            matches!(&want, Ok(crate::grant::ClassAAsset::Mt { mt_contract: c, token_id: t, .. }) if *c == mt_contract && *t == token_id),
            format!("ERR_ESCROW_MT_MISMATCH: grant {grant_id} takes {want:?}, got {mt_contract} {token_id}")
        );
        if let Ok(crate::grant::ClassAAsset::Mt {
            amount: expected, ..
        }) = want
        {
            refuse_unless!(
                amount == expected,
                format!("ERR_ESCROW_MT_AMOUNT: grant {grant_id} takes {expected}, got {amount}")
            );
        }
        refuse_unless!(
            owner == grant.grantor,
            format!(
                "ERR_ESCROW_DEPOSITOR: only {} may put the asset of grant {grant_id}",
                grant.grantor
            )
        );
        refuse_unless!(
            Held::load(grant_id).is_none(),
            format!("ERR_ESCROW_EXISTS: grant {grant_id}")
        );
        refuse_unless!(
            env::block_timestamp() < grant.limits.expires_at.0,
            "ERR_GRANT_EXPIRED: the grant ran out"
        );
        Held::Mt(MtEscrow {
            mt_contract: mt_contract.clone(),
            token_id: token_id.clone(),
            amount,
            state: EscrowState::Held,
            buyer: None,
            snapshot: None,
        })
        .save(grant_id);
        MtEscrowHeld {
            grant_id,
            mt_contract,
            token_id,
            amount: U128(amount),
        }
        .emit();
        PromiseOrValue::Value(vec![U128(0)])
    }

    /// The `escrow` action of `ft_on_transfer` (CT-090) for a token that is not the USDT of the circle. `msg` is `{"action":"escrow","grant_id":N}`.
    /// Only a class A grant that is `offered`, whose scope names this token contract and exactly this amount, that the grantor funds itself and
    /// that has no escrow yet. Every refusal is a panic, so that the token gives everything back to the sender. Returns `0`: all of it stays.
    pub(crate) fn escrow_ft_from_transfer(
        &mut self,
        ft_contract: AccountId,
        sender_id: AccountId,
        amount: u128,
        grant_id: GrantId,
    ) -> PromiseOrValue<U128> {
        require!(
            ft_contract != self.usdt_token_id,
            "ERR_ESCROW_USDT: the USDT of the circle is not an escrow asset"
        );
        let grant = load_grant(grant_id);
        require!(
            grant.class == AssetClass::A,
            format!(
                "ERR_ESCROW_NOT_CLASS_A: grant {grant_id} is class {:?}",
                grant.class
            )
        );
        require!(
            grant.status == GrantStatus::Offered,
            format!("ERR_GRANT_NOT_OFFERED: {:?}", grant.status)
        );
        crate::asset_contracts::assert_official(&grant.asset_type, &ft_contract);
        let want = crate::grant::class_a_asset(&grant.limits.scope);
        require!(
            matches!(&want, Ok(crate::grant::ClassAAsset::Ft { ft_contract: c, .. }) if *c == ft_contract),
            format!("ERR_ESCROW_FT_MISMATCH: grant {grant_id} takes {want:?}, got {ft_contract}")
        );
        if let Ok(crate::grant::ClassAAsset::Ft {
            amount: expected, ..
        }) = want
        {
            require!(
                amount == expected,
                format!("ERR_ESCROW_FT_AMOUNT: grant {grant_id} takes {expected}, got {amount}")
            );
        }
        require!(
            sender_id == grant.grantor,
            format!(
                "ERR_ESCROW_DEPOSITOR: only {} may put the asset of grant {grant_id}",
                grant.grantor
            )
        );
        require!(
            Held::load(grant_id).is_none(),
            format!("ERR_ESCROW_EXISTS: grant {grant_id}")
        );
        require!(
            env::block_timestamp() < grant.limits.expires_at.0,
            "ERR_GRANT_EXPIRED: the grant ran out"
        );
        Held::Ft(FtEscrow {
            ft_contract: ft_contract.clone(),
            amount,
            state: EscrowState::Held,
            buyer: None,
            snapshot: None,
        })
        .save(grant_id);
        FtEscrowHeld {
            grant_id,
            ft_contract,
            amount: U128(amount),
        }
        .emit();
        PromiseOrValue::Value(U128(0))
    }

    /// Takes back exactly what `complete_escrow` registered. Only when the epoch is still `open` and the pair is as the voucher left it.
    fn take_back(&mut self, s: &Snapshot) -> bool {
        let key = (s.epoch, s.payer.clone(), s.payee.clone());
        let open = self
            .epochs
            .get(&s.epoch)
            .is_some_and(|e| e.phase == EpochPhase::Open)
            && s.epoch == self.current_epoch;
        let unchanged = self
            .pairs
            .get(&key)
            .is_some_and(|p| p.last_seq == s.seq && p.cumulative == s.cumulative);
        let debt = self.outstanding_debt.get(&s.payer).copied().unwrap_or(0);
        if !open || !unchanged || debt < s.delta {
            return false;
        }
        match &s.pair_before {
            Some(before) => {
                self.pairs.insert(key, before.clone());
            }
            None => {
                self.pairs.remove(&key);
            }
        }
        self.outstanding_debt
            .insert(s.payer.clone(), debt - s.delta);
        for (account, out, inn, is_new) in [
            (&s.payer, s.delta, 0, s.payer_new),
            (&s.payee, 0, s.delta, s.payee_new),
        ] {
            let tkey = (s.epoch, account.clone());
            let mut t: ParticipantTotals = self.totals.get(&tkey).cloned().unwrap_or_default();
            t.owed_out = t.owed_out.saturating_sub(out);
            t.owed_in = t.owed_in.saturating_sub(inn);
            if is_new && t.owed_out == 0 && t.owed_in == 0 {
                self.totals.remove(&tkey);
                let epoch = self.epochs.get_mut(&s.epoch).expect("open epoch");
                if let Some(i) = epoch.participants.iter().position(|a| a == account) {
                    epoch.participants.swap_remove(i as u32);
                    epoch.participant_count -= 1;
                }
            } else {
                self.totals.insert(tkey, t);
            }
        }
        let epoch = self.epochs.get_mut(&s.epoch).expect("open epoch");
        epoch.total_obligation = epoch.total_obligation.saturating_sub(s.delta);
        if s.pair_before.is_none() {
            if let Some(i) = epoch
                .edges
                .iter()
                .position(|(a, b)| a == &s.payer && b == &s.payee)
            {
                epoch.edges.swap_remove(i as u32);
                epoch.edge_count -= 1;
            }
        }
        true
    }
}

#[allow(dead_code)]
fn _assert_borsh<T: BorshDeserialize>() {}

#[cfg(test)]
mod tests {
    use near_sdk::json_types::{U128, U64};
    use near_sdk::serde_json::{self, Value};
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::{test_vm_config, testing_env, PromiseResult, RuntimeFeesConfig};

    use super::*;
    use crate::grant::{AccessGrant, GrantLimits};
    use crate::params::Params;

    const CREDIT: u128 = 1_000_000;
    const GRANT: GrantId = 7;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn circle() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        X207Circle::with_config(
            acc("dao.near"),
            "agents-main".into(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            1_000,
        )
    }

    /// The state that `complete_escrow` leaves after `register_with` for a new pair `bob -> alice` of `delta`, and the snapshot it keeps.
    fn registered(c: &mut X207Circle, delta: u128) -> Snapshot {
        let (payer, payee) = (acc("bob.near"), acc("alice.near"));
        c.pairs.insert(
            (1, payer.clone(), payee.clone()),
            PairState {
                cumulative: delta,
                last_seq: 1,
                cleared: 0,
            },
        );
        c.outstanding_debt.insert(payer.clone(), delta);
        c.totals.insert(
            (1, payer.clone()),
            ParticipantTotals {
                owed_out: delta,
                owed_in: 0,
            },
        );
        c.totals.insert(
            (1, payee.clone()),
            ParticipantTotals {
                owed_out: 0,
                owed_in: delta,
            },
        );
        let epoch = c.epochs.get_mut(&1).unwrap();
        epoch.participants.push(payer.clone());
        epoch.participants.push(payee.clone());
        epoch.participant_count = 2;
        epoch.total_obligation = delta;
        epoch.edges.push((payer.clone(), payee.clone()));
        epoch.edge_count = 1;
        Snapshot {
            epoch: 1,
            payer,
            payee,
            pair_before: None,
            delta,
            payer_new: true,
            payee_new: true,
            seq: 1,
            cumulative: delta,
        }
    }

    /// Everything `take_back` may touch, to compare before and after.
    #[allow(clippy::type_complexity)]
    fn picture(
        c: &X207Circle,
    ) -> (
        Option<PairState>,
        u128,
        Option<ParticipantTotals>,
        Option<ParticipantTotals>,
        u32,
        u32,
        u128,
        usize,
        usize,
    ) {
        let e = c.epochs.get(&1).unwrap();
        (
            c.pairs
                .get(&(1, acc("bob.near"), acc("alice.near")))
                .cloned(),
            c.outstanding_debt
                .get(&acc("bob.near"))
                .copied()
                .unwrap_or(0),
            c.totals.get(&(1, acc("bob.near"))).cloned(),
            c.totals.get(&(1, acc("alice.near"))).cloned(),
            e.participant_count,
            e.edge_count,
            e.total_obligation,
            e.participants.len() as usize,
            e.edges.len() as usize,
        )
    }

    // TC-CT-075-U01: the baseline, so that the refusals below mean something
    #[test]
    fn take_back_undoes_a_registration_exactly() {
        let mut c = circle();
        let empty = picture(&c);
        let snapshot = registered(&mut c, 10 * CREDIT);
        assert_ne!(picture(&c), empty);
        assert!(c.take_back(&snapshot));
        assert_eq!(picture(&c), empty);
    }

    // TC-CT-075-U02: a payment on top of an earlier voucher of the pair returns to that voucher
    #[test]
    fn take_back_returns_the_pair_to_its_earlier_voucher() {
        let mut c = circle();
        let before = PairState {
            cumulative: 4 * CREDIT,
            last_seq: 1,
            cleared: 0,
        };
        c.pairs
            .insert((1, acc("bob.near"), acc("alice.near")), before.clone());
        c.outstanding_debt.insert(acc("bob.near"), 4 * CREDIT);
        c.totals.insert(
            (1, acc("bob.near")),
            ParticipantTotals {
                owed_out: 4 * CREDIT,
                owed_in: 0,
            },
        );
        c.totals.insert(
            (1, acc("alice.near")),
            ParticipantTotals {
                owed_out: 0,
                owed_in: 4 * CREDIT,
            },
        );
        // the escrow voucher raises the pair from 4 to 14
        c.pairs.insert(
            (1, acc("bob.near"), acc("alice.near")),
            PairState {
                cumulative: 14 * CREDIT,
                last_seq: 2,
                cleared: 0,
            },
        );
        c.outstanding_debt.insert(acc("bob.near"), 14 * CREDIT);
        c.totals.insert(
            (1, acc("bob.near")),
            ParticipantTotals {
                owed_out: 14 * CREDIT,
                owed_in: 0,
            },
        );
        c.totals.insert(
            (1, acc("alice.near")),
            ParticipantTotals {
                owed_out: 0,
                owed_in: 14 * CREDIT,
            },
        );
        let snapshot = Snapshot {
            epoch: 1,
            payer: acc("bob.near"),
            payee: acc("alice.near"),
            pair_before: Some(before.clone()),
            delta: 10 * CREDIT,
            payer_new: false,
            payee_new: false,
            seq: 2,
            cumulative: 14 * CREDIT,
        };
        assert!(c.take_back(&snapshot));
        assert_eq!(
            c.pairs.get(&(1, acc("bob.near"), acc("alice.near"))),
            Some(&before)
        );
        assert_eq!(
            c.outstanding_debt.get(&acc("bob.near")),
            Some(&(4 * CREDIT))
        );
        assert_eq!(
            c.totals.get(&(1, acc("bob.near"))),
            Some(&ParticipantTotals {
                owed_out: 4 * CREDIT,
                owed_in: 0
            })
        );
    }

    // TC-CT-075-U03: the epoch is not `open` any more between the registration and the callback
    #[test]
    fn take_back_refuses_when_the_epoch_is_no_longer_open() {
        let mut c = circle();
        let snapshot = registered(&mut c, 10 * CREDIT);
        c.epochs
            .get_mut(&1)
            .unwrap()
            .start_clearing(3_600_000_001_000);
        let before = picture(&c);
        assert!(!c.take_back(&snapshot));
        assert_eq!(
            picture(&c),
            before,
            "nothing may change when the take back is refused"
        );
    }

    // TC-CT-075-U04: the pair moved on (another voucher raised it)
    #[test]
    fn take_back_refuses_when_the_pair_changed() {
        for (seq, cumulative) in [(2, 10 * CREDIT), (1, 12 * CREDIT), (3, 25 * CREDIT)] {
            let mut c = circle();
            let snapshot = registered(&mut c, 10 * CREDIT);
            c.pairs.insert(
                (1, acc("bob.near"), acc("alice.near")),
                PairState {
                    cumulative,
                    last_seq: seq,
                    cleared: 0,
                },
            );
            let before = picture(&c);
            assert!(!c.take_back(&snapshot), "seq {seq} cumulative {cumulative}");
            assert_eq!(picture(&c), before);
        }
        // the pair vanished altogether
        let mut c = circle();
        let snapshot = registered(&mut c, 10 * CREDIT);
        c.pairs.remove(&(1, acc("bob.near"), acc("alice.near")));
        assert!(!c.take_back(&snapshot));
    }

    // TC-CT-075-U05: the debt of the payer fell below the delta (cleared or paid in between)
    #[test]
    fn take_back_refuses_when_the_debt_of_the_payer_is_lower_than_the_delta() {
        let mut c = circle();
        let snapshot = registered(&mut c, 10 * CREDIT);
        c.outstanding_debt.insert(acc("bob.near"), 10 * CREDIT - 1);
        let before = picture(&c);
        assert!(!c.take_back(&snapshot));
        assert_eq!(picture(&c), before);
        c.outstanding_debt.remove(&acc("bob.near"));
        assert!(!c.take_back(&snapshot), "no debt at all");
    }

    fn grant_offered() -> AccessGrant {
        AccessGrant {
            grant_id: GRANT,
            asset_type: "nft:ticket".into(),
            grantor: acc("alice.near"),
            grantee: None,
            class: AssetClass::A,
            limits: GrantLimits {
                scope: "{\"nft_contract\":\"nft.near\",\"token_id\":\"t1\"}".into(),
                quota: None,
                expires_at: U64(9_000_000_000_000),
            },
            unit: "once".into(),
            unit_price: U128(10 * CREDIT),
            operator: None,
            delivery_pubkey: None,
            status: GrantStatus::Offered,
            proof_hash: None,
            proof_kind: None,
        }
    }

    /// `complete_escrow` has run: state `Completing`, the voucher registered, the pair locked, the grant still `offered`.
    fn completing(c: &mut X207Circle) -> Snapshot {
        let snapshot = registered(c, 10 * CREDIT);
        crate::grant::store_grant_for_test(&grant_offered());
        env::storage_write(&lock_key(1, &snapshot.payer, &snapshot.payee), &[1]);
        save(
            GRANT,
            Escrow {
                nft_contract: acc("nft.near"),
                token_id: "t1".into(),
                state: EscrowState::Completing,
                buyer: Some(acc("bob.near")),
                snapshot: Some(snapshot.clone()),
            },
        );
        snapshot
    }

    fn callback(c: &mut X207Circle, result: PromiseResult) {
        testing_env!(
            VMContextBuilder::new()
                .current_account_id(acc("circle.near"))
                .predecessor_account_id(acc("circle.near"))
                .block_timestamp(5_000)
                .build(),
            test_vm_config(),
            RuntimeFeesConfig::test(),
            Default::default(),
            vec![result]
        );
        c.on_escrow_resolved(GRANT, EscrowStep::Complete);
    }

    fn failed_event() -> Value {
        let log = get_logs()
            .into_iter()
            .find(|l| l.contains("escrow_failed"))
            .expect("escrow_failed event");
        serde_json::from_str::<Value>(log.strip_prefix("EVENT_JSON:").unwrap()).unwrap()["data"][0]
            .clone()
    }

    // TC-CT-075-U06: a failed transfer that cannot be taken back leaves the escrow `stuck`, the payment in place and the NFT in the circle
    #[test]
    fn a_failed_transfer_that_cannot_be_taken_back_leaves_the_escrow_stuck() {
        for breakage in ["epoch", "pair", "debt"] {
            let mut c = circle();
            completing(&mut c);
            match breakage {
                "epoch" => c
                    .epochs
                    .get_mut(&1)
                    .unwrap()
                    .start_clearing(3_600_000_001_000),
                "pair" => {
                    c.pairs.insert(
                        (1, acc("bob.near"), acc("alice.near")),
                        PairState {
                            cumulative: 12 * CREDIT,
                            last_seq: 2,
                            cleared: 0,
                        },
                    );
                }
                _ => {
                    c.outstanding_debt.insert(acc("bob.near"), 1);
                }
            }
            let before = picture(&c);
            callback(&mut c, PromiseResult::Failed);
            let escrow = load_escrow(GRANT).unwrap();
            assert_eq!(escrow.state, EscrowState::Stuck, "{breakage}");
            assert_eq!(escrow.buyer, None, "{breakage}");
            assert!(escrow.snapshot.is_none(), "{breakage}");
            // the registration stays, the lock is gone, the grant is still offered and the asset is still held
            assert_eq!(picture(&c), before, "{breakage}");
            assert!(
                !pair_locked(1, &acc("bob.near"), &acc("alice.near")),
                "{breakage}"
            );
            assert_eq!(load_grant(GRANT).status, GrantStatus::Offered, "{breakage}");
            assert!(
                holds_asset(GRANT),
                "{breakage}: a stuck escrow still blocks revoke_grant"
            );
            let event = failed_event();
            assert_eq!(
                (
                    event["reverted"].clone(),
                    event["delta"].as_str(),
                    event["step"].as_str()
                ),
                (Value::Bool(false), Some("0"), Some("complete")),
                "{breakage}"
            );
        }
    }

    // TC-CT-075-U07: the same failed transfer with nothing in the way goes back to `held` and the registration is gone
    #[test]
    fn a_failed_transfer_that_can_be_taken_back_returns_the_escrow_to_held() {
        let mut c = circle();
        completing(&mut c);
        callback(&mut c, PromiseResult::Failed);
        let escrow = load_escrow(GRANT).unwrap();
        assert_eq!((escrow.state, escrow.buyer), (EscrowState::Held, None));
        assert_eq!(c.pairs.get(&(1, acc("bob.near"), acc("alice.near"))), None);
        assert_eq!(c.outstanding_debt.get(&acc("bob.near")), Some(&0));
        let event = failed_event();
        assert_eq!(
            (event["reverted"].clone(), event["delta"].as_str()),
            (Value::Bool(true), Some("10000000"))
        );
    }

    // TC-CT-075-U08: a delivered NFT activates the grant for the buyer and releases the lock
    #[test]
    fn a_delivered_nft_activates_the_grant_and_releases_the_pair() {
        let mut c = circle();
        completing(&mut c);
        callback(&mut c, PromiseResult::Successful(vec![]));
        let escrow = load_escrow(GRANT).unwrap();
        assert_eq!(escrow.state, EscrowState::Delivered);
        assert!(!pair_locked(1, &acc("bob.near"), &acc("alice.near")));
        let grant = load_grant(GRANT);
        assert_eq!(
            (grant.status, grant.grantee),
            (GrantStatus::Active, Some(acc("bob.near")))
        );
        assert!(!holds_asset(GRANT));
        assert_eq!(
            c.pairs
                .get(&(1, acc("bob.near"), acc("alice.near")))
                .map(|p| p.cumulative),
            Some(10 * CREDIT)
        );
    }

    // TC-CT-075-S01 (security review): a transfer that succeeded and returned more than 64 bytes is a success, not a failure
    #[test]
    fn a_nft_contract_that_returns_a_long_value_still_delivers() {
        let mut c = circle();
        completing(&mut c);
        callback(&mut c, PromiseResult::Successful(vec![7u8; 200]));
        let escrow = load_escrow(GRANT).unwrap();
        assert_eq!(
            escrow.state,
            EscrowState::Delivered,
            "the NFT moved, so the payment must stay and the grant must be active"
        );
        assert_eq!(load_grant(GRANT).status, GrantStatus::Active);
        assert_eq!(
            c.pairs
                .get(&(1, acc("bob.near"), acc("alice.near")))
                .map(|p| p.cumulative),
            Some(10 * CREDIT)
        );
    }

    // TC-CT-075-S02 (security review F1): only the NFT contract and token named in the scope of the grant fill its escrow.
    #[test]
    fn an_account_that_is_not_the_nft_of_the_grant_cannot_fill_the_escrow() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_offered());
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("mallory.near"))
            .block_timestamp(5_000)
            .build());
        // mallory is a plain account, not a NFT contract, and calls the receiver directly with the grantor as previous owner
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = c.nft_on_transfer(
                acc("alice.near"),
                acc("alice.near"),
                "fake".into(),
                format!("{{\"grant_id\":{GRANT}}}"),
            );
        }));
        assert!(
            result.is_err(),
            "the escrow of the grant was filled by an account that never held a token"
        );
        assert!(load_escrow(GRANT).is_none());
    }

    #[test]
    fn the_right_nft_contract_with_another_token_cannot_fill_the_escrow() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_offered());
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("nft.near"))
            .block_timestamp(5_000)
            .build());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = c.nft_on_transfer(
                acc("alice.near"),
                acc("alice.near"),
                "t2".into(),
                format!("{{\"grant_id\":{GRANT}}}"),
            );
        }));
        assert!(result.is_err(), "another token filled the escrow");
        assert!(load_escrow(GRANT).is_none());
    }

    #[test]
    fn the_named_nft_contract_and_token_fill_the_escrow() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_offered());
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("nft.near"))
            .block_timestamp(5_000)
            .build());
        let _ = c.nft_on_transfer(
            acc("alice.near"),
            acc("alice.near"),
            "t1".into(),
            format!("{{\"grant_id\":{GRANT}}}"),
        );
        assert_eq!(load_escrow(GRANT).map(|e| e.state), Some(EscrowState::Held));
    }

    // ---- CT-090: escrow of an exact amount of a NEP-141 token ----

    const RWA_AMOUNT: u128 = 1_000;

    fn grant_ft() -> AccessGrant {
        let mut g = grant_offered();
        g.asset_type = "ft:rwa".into();
        g.limits.scope = format!("{{\"ft_contract\":\"rwa.near\",\"amount\":\"{RWA_AMOUNT}\"}}");
        g
    }

    fn deposit(
        c: &mut X207Circle,
        token: &str,
        sender: &str,
        amount: u128,
        msg: &str,
    ) -> Result<u128, String> {
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc(token))
            .block_timestamp(5_000)
            .build());
        let (sender, msg) = (acc(sender), msg.to_string());
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.ft_on_transfer(sender, U128(amount), msg)
        }))
        .map(|r| match r {
            PromiseOrValue::Value(v) => v.0,
            PromiseOrValue::Promise(_) => u128::MAX,
        })
        .map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default()
        })
    }

    fn escrow_msg() -> String {
        format!("{{\"action\":\"escrow\",\"grant_id\":{GRANT}}}")
    }

    fn assert_refused(r: Result<u128, String>, code: &str) {
        let err = r.expect_err(&format!("expected {code}"));
        assert!(err.contains(code), "expected {code}, got {err}");
        assert!(
            load_ft_escrow(GRANT).is_none() && load_escrow(GRANT).is_none(),
            "{code}: a refused deposit must leave no escrow"
        );
    }

    // TC-CT-090-U01
    #[test]
    fn the_named_token_contract_and_amount_fill_the_ft_escrow() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_ft());
        let unused =
            deposit(&mut c, "rwa.near", "alice.near", RWA_AMOUNT, &escrow_msg()).expect("accepted");
        assert_eq!(unused, 0, "all of it stays in the circle");
        let e = load_ft_escrow(GRANT).expect("escrow");
        assert_eq!(
            (e.ft_contract, e.amount, e.state, e.buyer),
            (acc("rwa.near"), RWA_AMOUNT, EscrowState::Held, None)
        );
        assert_eq!(
            c.get_ft_escrow(GRANT).map(|v| v.amount),
            Some(U128(RWA_AMOUNT))
        );
        assert!(
            c.get_escrow(GRANT).is_none(),
            "the NFT view does not show it"
        );
        assert!(holds_asset(GRANT));
        let log = get_logs()
            .into_iter()
            .find(|l| l.contains("ft_escrow_held"))
            .expect("ft_escrow_held");
        let data = serde_json::from_str::<Value>(log.strip_prefix("EVENT_JSON:").unwrap()).unwrap()
            ["data"][0]
            .clone();
        assert_eq!(
            (data["ft_contract"].as_str(), data["amount"].as_str()),
            (Some("rwa.near"), Some("1000"))
        );
    }

    // TC-CT-090-S01: attacks on the receiver; each one is a panic, so the token returns everything to the sender
    #[test]
    fn a_deposit_that_does_not_match_the_grant_is_refused() {
        let msg = escrow_msg();
        for (token, sender, amount, code) in [
            (
                "fake.near",
                "alice.near",
                RWA_AMOUNT,
                "ERR_ESCROW_FT_MISMATCH",
            ),
            (
                "rwa.near",
                "alice.near",
                RWA_AMOUNT - 1,
                "ERR_ESCROW_FT_AMOUNT",
            ),
            (
                "rwa.near",
                "alice.near",
                RWA_AMOUNT + 1,
                "ERR_ESCROW_FT_AMOUNT",
            ),
            (
                "rwa.near",
                "mallory.near",
                RWA_AMOUNT,
                "ERR_ESCROW_DEPOSITOR",
            ),
            ("rwa.near", "alice.near", 0, "ERR_ZERO_AMOUNT"),
        ] {
            let mut c = circle();
            crate::grant::store_grant_for_test(&grant_ft());
            assert_refused(deposit(&mut c, token, sender, amount, &msg), code);
        }
    }

    // TC-CT-090-S02
    #[test]
    fn a_second_deposit_and_a_deposit_for_the_wrong_kind_of_grant_are_refused() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_ft());
        deposit(&mut c, "rwa.near", "alice.near", RWA_AMOUNT, &escrow_msg()).expect("first");
        let again = deposit(&mut c, "rwa.near", "alice.near", RWA_AMOUNT, &escrow_msg())
            .expect_err("second");
        assert!(again.contains("ERR_ESCROW_EXISTS"), "{again}");
        assert_eq!(
            load_ft_escrow(GRANT).map(|e| e.amount),
            Some(RWA_AMOUNT),
            "the first escrow is untouched"
        );

        // a grant for an NFT takes no token, and a grant for a token takes no NFT
        let mut c = circle();
        let mut map = ft_escrows();
        map.remove(&GRANT);
        map.flush();
        crate::grant::store_grant_for_test(&grant_offered());
        assert_refused(
            deposit(&mut c, "rwa.near", "alice.near", RWA_AMOUNT, &escrow_msg()),
            "ERR_ESCROW_FT_MISMATCH",
        );
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_ft());
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("rwa.near"))
            .block_timestamp(5_000)
            .build());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = c.nft_on_transfer(
                acc("alice.near"),
                acc("alice.near"),
                "t1".into(),
                format!("{{\"grant_id\":{GRANT}}}"),
            );
        }));
        assert!(r.is_err() && load_escrow(GRANT).is_none() && load_ft_escrow(GRANT).is_none());
    }

    // TC-CT-090-S03: not class A, not offered, expired, unknown grant
    #[test]
    fn a_deposit_for_a_grant_that_cannot_take_it_is_refused() {
        for (name, code) in [
            ("class", "ERR_ESCROW_NOT_CLASS_A"),
            ("status", "ERR_GRANT_NOT_OFFERED"),
            ("expired", "ERR_GRANT_EXPIRED"),
        ] {
            let mut c = circle();
            let mut g = grant_ft();
            match name {
                "class" => g.class = AssetClass::B,
                "status" => g.status = GrantStatus::Active,
                _ => g.limits.expires_at = U64(4_000),
            }
            crate::grant::store_grant_for_test(&g);
            assert_refused(
                deposit(&mut c, "rwa.near", "alice.near", RWA_AMOUNT, &escrow_msg()),
                code,
            );
        }
        let mut c = circle();
        assert_refused(
            deposit(
                &mut c,
                "rwa.near",
                "alice.near",
                RWA_AMOUNT,
                "{\"action\":\"escrow\",\"grant_id\":99}",
            ),
            "ERR_UNKNOWN_GRANT",
        );
    }

    // TC-CT-090-S04: the USDT of the circle is not an escrow asset, and no other token can stake or settle
    #[test]
    fn usdt_is_not_an_escrow_asset_and_other_tokens_cannot_stake_or_settle() {
        let mut c = circle();
        let mut g = grant_ft();
        g.limits.scope = format!("{{\"ft_contract\":\"usdt.near\",\"amount\":\"{RWA_AMOUNT}\"}}");
        crate::grant::store_grant_for_test(&g);
        assert_refused(
            deposit(&mut c, "usdt.near", "alice.near", RWA_AMOUNT, &escrow_msg()),
            "ERR_ESCROW_USDT",
        );
        for msg in [
            "{\"action\":\"stake\",\"member_id\":null}",
            "{\"action\":\"settle\",\"epoch\":1,\"member_id\":null}",
            "not json",
            "{\"action\":\"escrow\"}",
        ] {
            let mut c = circle();
            crate::grant::store_grant_for_test(&grant_ft());
            let err = deposit(&mut c, "rwa.near", "alice.near", RWA_AMOUNT, msg).expect_err(msg);
            assert!(
                err.contains("ERR_WRONG_TOKEN") || err.contains("ERR_BAD_MSG"),
                "{msg}: {err}"
            );
        }
    }

    // TC-CT-090-S05: the scope of a class A grant
    #[test]
    fn the_scope_of_a_class_a_grant_names_one_asset() {
        use crate::grant::{class_a_asset, ClassAAsset};
        assert_eq!(
            class_a_asset("{\"ft_contract\":\"rwa.near\",\"amount\":\"5\"}"),
            Ok(ClassAAsset::Ft {
                ft_contract: acc("rwa.near"),
                amount: 5
            })
        );
        assert_eq!(
            class_a_asset("{\"nft_contract\":\"n.near\",\"token_id\":\"a\"}"),
            Ok(ClassAAsset::Nft {
                nft_contract: acc("n.near"),
                token_id: "a".into()
            })
        );
        for bad in [
            "{\"ft_contract\":\"rwa.near\",\"amount\":\"0\"}", "{\"ft_contract\":\"rwa.near\",\"amount\":5}", "{\"ft_contract\":\"rwa.near\"}",
            "{\"ft_contract\":\"Bad Name\",\"amount\":\"5\"}", "{\"ft_contract\":\"rwa.near\",\"amount\":\"-1\"}",
            "{\"ft_contract\":\"rwa.near\",\"amount\":\"5\",\"nft_contract\":\"n.near\",\"token_id\":\"a\"}", "{\"amount\":\"5\"}",
        ] {
            assert!(class_a_asset(bad).is_err(), "{bad}");
        }
    }

    fn completing_ft(c: &mut X207Circle) -> Snapshot {
        let snapshot = registered(c, 10 * CREDIT);
        crate::grant::store_grant_for_test(&grant_ft());
        env::storage_write(&lock_key(1, &snapshot.payer, &snapshot.payee), &[1]);
        Held::Ft(FtEscrow {
            ft_contract: acc("rwa.near"),
            amount: RWA_AMOUNT,
            state: EscrowState::Completing,
            buyer: Some(acc("bob.near")),
            snapshot: Some(snapshot.clone()),
        })
        .save(GRANT);
        snapshot
    }

    // TC-CT-090-U02: a token that refuses the transfer (a whitelist, a frozen account) takes the voucher back exactly, as the NFT does
    #[test]
    fn a_failed_ft_transfer_takes_the_voucher_back_and_keeps_the_tokens_in_the_circle() {
        let mut c = circle();
        let empty = picture(&c);
        completing_ft(&mut c);
        callback(&mut c, PromiseResult::Failed);
        let e = load_ft_escrow(GRANT).unwrap();
        assert_eq!(
            (e.state, e.buyer, e.amount),
            (EscrowState::Held, None, RWA_AMOUNT)
        );
        assert!(e.snapshot.is_none());
        assert_eq!(picture(&c).0, empty.0, "the pair is gone");
        assert_eq!(c.outstanding_debt.get(&acc("bob.near")), Some(&0));
        assert!(!pair_locked(1, &acc("bob.near"), &acc("alice.near")));
        assert_eq!(load_grant(GRANT).status, GrantStatus::Offered);
        assert!(holds_asset(GRANT));
        let event = failed_event();
        assert_eq!(
            (event["reverted"].clone(), event["delta"].as_str()),
            (Value::Bool(true), Some("10000000"))
        );
    }

    // TC-CT-090-U03
    #[test]
    fn a_failed_ft_transfer_that_cannot_be_taken_back_leaves_the_escrow_stuck() {
        let mut c = circle();
        completing_ft(&mut c);
        c.epochs
            .get_mut(&1)
            .unwrap()
            .start_clearing(3_600_000_001_000);
        callback(&mut c, PromiseResult::Failed);
        assert_eq!(
            load_ft_escrow(GRANT).map(|e| e.state),
            Some(EscrowState::Stuck)
        );
        assert!(holds_asset(GRANT) && !pair_locked(1, &acc("bob.near"), &acc("alice.near")));
    }

    // TC-CT-090-U04
    #[test]
    fn a_delivered_ft_activates_the_grant_for_the_buyer() {
        let mut c = circle();
        completing_ft(&mut c);
        callback(&mut c, PromiseResult::Successful(vec![]));
        assert_eq!(
            load_ft_escrow(GRANT).map(|e| e.state),
            Some(EscrowState::Delivered)
        );
        let grant = load_grant(GRANT);
        assert_eq!(
            (grant.status, grant.grantee),
            (GrantStatus::Active, Some(acc("bob.near")))
        );
        assert!(!holds_asset(GRANT) && !pair_locked(1, &acc("bob.near"), &acc("alice.near")));
        assert_eq!(
            c.pairs
                .get(&(1, acc("bob.near"), acc("alice.near")))
                .map(|p| p.cumulative),
            Some(10 * CREDIT)
        );
    }

    // TC-CT-090-U05: the grantor takes the tokens back after expiry; a failed cancel leaves them `held`
    #[test]
    fn cancel_of_an_ft_escrow_returns_the_tokens_after_expiry() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_ft());
        deposit(&mut c, "rwa.near", "alice.near", RWA_AMOUNT, &escrow_msg()).expect("held");
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("alice.near"))
            .block_timestamp(9_000_000_000_001)
            .build());
        let _ = c.cancel_escrow(GRANT);
        assert_eq!(
            load_ft_escrow(GRANT).map(|e| e.state),
            Some(EscrowState::Cancelling)
        );
        testing_env!(
            VMContextBuilder::new()
                .current_account_id(acc("circle.near"))
                .predecessor_account_id(acc("circle.near"))
                .block_timestamp(9_000_000_000_002)
                .build(),
            test_vm_config(),
            RuntimeFeesConfig::test(),
            Default::default(),
            vec![PromiseResult::Failed]
        );
        c.on_escrow_resolved(GRANT, EscrowStep::Cancel);
        assert_eq!(
            load_ft_escrow(GRANT).map(|e| e.state),
            Some(EscrowState::Held),
            "a failed return leaves the tokens held, not lost"
        );
        testing_env!(
            VMContextBuilder::new()
                .current_account_id(acc("circle.near"))
                .predecessor_account_id(acc("circle.near"))
                .block_timestamp(9_000_000_000_003)
                .build(),
            test_vm_config(),
            RuntimeFeesConfig::test(),
            Default::default(),
            vec![PromiseResult::Successful(vec![])]
        );
        c.on_escrow_resolved(GRANT, EscrowStep::Cancel);
        assert_eq!(
            load_ft_escrow(GRANT).map(|e| e.state),
            Some(EscrowState::Cancelled)
        );
        assert_eq!(load_grant(GRANT).status, GrantStatus::Revoked);
    }

    // ---- CT-092: escrow of an exact amount of one NEP-245 multi-token ----

    const MT_TOKEN: &str = "nep141:usdc.near";

    fn grant_mt() -> AccessGrant {
        let mut g = grant_offered();
        g.asset_type = "mt:usdc".into();
        g.limits.scope = format!("{{\"mt_contract\":\"mt.near\",\"token_id\":\"{MT_TOKEN}\",\"amount\":\"{RWA_AMOUNT}\"}}");
        g
    }

    fn deposit_mt(
        c: &mut X207Circle,
        caller: &str,
        owners: &[&str],
        tokens: &[&str],
        amounts: &[u128],
        msg: &str,
    ) -> Result<Vec<u128>, String> {
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc(caller))
            .block_timestamp(5_000)
            .build());
        let owners: Vec<AccountId> = owners.iter().map(|o| acc(o)).collect();
        let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
        let amounts: Vec<U128> = amounts.iter().map(|a| U128(*a)).collect();
        let msg = msg.to_string();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.mt_on_transfer(acc("alice.near"), owners, tokens, amounts, msg)
        }))
        .map(|r| match r {
            PromiseOrValue::Value(v) => v.into_iter().map(|u| u.0).collect(),
            PromiseOrValue::Promise(_) => vec![u128::MAX],
        })
        .map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default()
        })
    }

    /// A refusal of the multi-token escrow is a return value, not a panic: every amount comes back as unused and the reason is in the log.
    fn refused_mt(r: Result<Vec<u128>, String>, amounts: &[u128], code: &str) {
        let unused = r.expect("a refusal is a return value, not a panic");
        assert_eq!(unused, amounts, "{code}: everything must be handed back");
        assert!(
            get_logs()
                .iter()
                .any(|l| l.contains("x207 mt escrow refused") && l.contains(code)),
            "{code}: no reason in {:?}",
            get_logs()
        );
        assert!(
            load_mt_escrow(GRANT).is_none()
                && load_ft_escrow(GRANT).is_none()
                && load_escrow(GRANT).is_none(),
            "{code}: a refused deposit must leave no escrow"
        );
    }

    // TC-CT-092-U01
    #[test]
    fn the_named_multi_token_and_amount_fill_the_mt_escrow() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_mt());
        let unused = deposit_mt(
            &mut c,
            "mt.near",
            &["alice.near"],
            &[MT_TOKEN],
            &[RWA_AMOUNT],
            &escrow_msg(),
        )
        .expect("accepted");
        assert_eq!(unused, vec![0], "all of it stays in the circle");
        let e = load_mt_escrow(GRANT).expect("escrow");
        assert_eq!(
            (
                e.mt_contract,
                e.token_id.as_str(),
                e.amount,
                e.state,
                e.buyer
            ),
            (
                acc("mt.near"),
                MT_TOKEN,
                RWA_AMOUNT,
                EscrowState::Held,
                None
            )
        );
        assert_eq!(
            c.get_mt_escrow(GRANT).map(|v| v.amount),
            Some(U128(RWA_AMOUNT))
        );
        assert!(
            c.get_escrow(GRANT).is_none() && c.get_ft_escrow(GRANT).is_none(),
            "the other views do not show it"
        );
        assert!(holds_asset(GRANT));
        let log = get_logs()
            .into_iter()
            .find(|l| l.contains("mt_escrow_held"))
            .expect("mt_escrow_held");
        let data = serde_json::from_str::<Value>(log.strip_prefix("EVENT_JSON:").unwrap()).unwrap()
            ["data"][0]
            .clone();
        assert_eq!(
            (
                data["mt_contract"].as_str(),
                data["token_id"].as_str(),
                data["amount"].as_str()
            ),
            (Some("mt.near"), Some(MT_TOKEN), Some("1000"))
        );
    }

    // TC-CT-092-S01: attacks on the receiver; each one is a panic, so the multi-token contract gives everything back
    #[test]
    fn a_multi_token_deposit_that_does_not_match_the_grant_is_refused() {
        #[allow(clippy::type_complexity)]
        let cases: [(&str, &[&str], &[&str], &[u128], &str); 9] = [
            (
                "fake.near",
                &["alice.near"],
                &[MT_TOKEN],
                &[RWA_AMOUNT],
                "ERR_ESCROW_MT_MISMATCH",
            ),
            (
                "mt.near",
                &["alice.near"],
                &["nep141:other.near"],
                &[RWA_AMOUNT],
                "ERR_ESCROW_MT_MISMATCH",
            ),
            (
                "mt.near",
                &["alice.near"],
                &[MT_TOKEN],
                &[RWA_AMOUNT - 1],
                "ERR_ESCROW_MT_AMOUNT",
            ),
            (
                "mt.near",
                &["alice.near"],
                &[MT_TOKEN],
                &[RWA_AMOUNT + 1],
                "ERR_ESCROW_MT_AMOUNT",
            ),
            (
                "mt.near",
                &["mallory.near"],
                &[MT_TOKEN],
                &[RWA_AMOUNT],
                "ERR_ESCROW_DEPOSITOR",
            ),
            (
                "mt.near",
                &["alice.near"],
                &[MT_TOKEN],
                &[0],
                "ERR_ZERO_AMOUNT",
            ),
            (
                "mt.near",
                &["alice.near"],
                &[MT_TOKEN, "nep141:other.near"],
                &[RWA_AMOUNT, RWA_AMOUNT],
                "ERR_ESCROW_MT_BATCH",
            ),
            (
                "mt.near",
                &["alice.near", "mallory.near"],
                &[MT_TOKEN],
                &[RWA_AMOUNT],
                "ERR_ESCROW_MT_BATCH",
            ),
            (
                "mt.near",
                &[],
                &[MT_TOKEN],
                &[RWA_AMOUNT],
                "ERR_ESCROW_MT_BATCH",
            ),
        ];
        for (caller, owners, tokens, amounts, code) in cases {
            let mut c = circle();
            crate::grant::store_grant_for_test(&grant_mt());
            refused_mt(
                deposit_mt(&mut c, caller, owners, tokens, amounts, &escrow_msg()),
                amounts,
                code,
            );
        }
    }

    // TC-CT-092-S02
    #[test]
    fn a_second_mt_deposit_and_the_wrong_kind_of_grant_are_refused() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_mt());
        deposit_mt(
            &mut c,
            "mt.near",
            &["alice.near"],
            &[MT_TOKEN],
            &[RWA_AMOUNT],
            &escrow_msg(),
        )
        .expect("first");
        let again = deposit_mt(
            &mut c,
            "mt.near",
            &["alice.near"],
            &[MT_TOKEN],
            &[RWA_AMOUNT],
            &escrow_msg(),
        );
        assert_eq!(again, Ok(vec![RWA_AMOUNT]), "the second deposit comes back");
        assert!(get_logs().iter().any(|l| l.contains("ERR_ESCROW_EXISTS")));
        assert_eq!(
            load_mt_escrow(GRANT).map(|e| e.amount),
            Some(RWA_AMOUNT),
            "the first escrow is untouched"
        );
        // an NFT grant and a NEP-141 grant take no multi-token
        for g in [grant_offered(), grant_ft()] {
            let mut c = circle();
            let mut map = mt_escrows();
            map.remove(&GRANT);
            map.flush();
            crate::grant::store_grant_for_test(&g);
            refused_mt(
                deposit_mt(
                    &mut c,
                    "mt.near",
                    &["alice.near"],
                    &[MT_TOKEN],
                    &[RWA_AMOUNT],
                    &escrow_msg(),
                ),
                &[RWA_AMOUNT],
                "ERR_ESCROW_MT_MISMATCH",
            );
        }
        // a message with no grant, and a grant that does not exist
        let mut c = circle();
        refused_mt(
            deposit_mt(
                &mut c,
                "mt.near",
                &["alice.near"],
                &[MT_TOKEN],
                &[RWA_AMOUNT],
                "{\"action\":\"escrow\"}",
            ),
            &[RWA_AMOUNT],
            "ERR_BAD_MSG",
        );
        refused_mt(
            deposit_mt(
                &mut c,
                "mt.near",
                &["alice.near"],
                &[MT_TOKEN],
                &[RWA_AMOUNT],
                "{\"action\":\"escrow\",\"grant_id\":99}",
            ),
            &[RWA_AMOUNT],
            "ERR_UNKNOWN_GRANT",
        );
    }

    // TC-CT-092-S03: the USDT of the circle held at Intents is not an escrow asset; a settle through mt_on_transfer is not changed
    #[test]
    fn usdt_at_intents_is_not_an_escrow_asset_and_the_settle_path_is_untouched() {
        let mut c = circle();
        let mut g = grant_mt();
        g.limits.scope = format!("{{\"mt_contract\":\"intents.near\",\"token_id\":\"nep141:usdt.near\",\"amount\":\"{RWA_AMOUNT}\"}}");
        crate::grant::store_grant_for_test(&g);
        refused_mt(
            deposit_mt(
                &mut c,
                "intents.near",
                &["alice.near"],
                &["nep141:usdt.near"],
                &[RWA_AMOUNT],
                &escrow_msg(),
            ),
            &[RWA_AMOUNT],
            "ERR_ESCROW_USDT",
        );
        // another token at Intents can be the escrow asset
        let mut g2 = grant_mt();
        g2.limits.scope = format!("{{\"mt_contract\":\"intents.near\",\"token_id\":\"{MT_TOKEN}\",\"amount\":\"{RWA_AMOUNT}\"}}");
        crate::grant::store_grant_for_test(&g2);
        deposit_mt(
            &mut c,
            "intents.near",
            &["alice.near"],
            &[MT_TOKEN],
            &[RWA_AMOUNT],
            &escrow_msg(),
        )
        .expect("another token at Intents");
        // stake and settle through a contract that is not Intents stay refused, as before
        for msg in [
            "{\"action\":\"stake\",\"member_id\":null}",
            "{\"action\":\"settle\",\"epoch\":1,\"member_id\":null}",
            "not json",
        ] {
            let mut c = circle();
            let err = deposit_mt(
                &mut c,
                "mt.near",
                &["alice.near"],
                &["nep141:usdt.near"],
                &[RWA_AMOUNT],
                msg,
            )
            .expect_err(msg);
            assert!(err.contains("ERR_WRONG_TOKEN"), "{msg}: {err}");
        }
    }

    // TC-CT-092-S04: the scope names one multi-token
    #[test]
    fn the_scope_of_a_class_a_grant_may_name_one_multi_token() {
        use crate::grant::{class_a_asset, ClassAAsset};
        assert_eq!(
            class_a_asset("{\"mt_contract\":\"mt.near\",\"token_id\":\"a:b\",\"amount\":\"5\"}"),
            Ok(ClassAAsset::Mt {
                mt_contract: acc("mt.near"),
                token_id: "a:b".into(),
                amount: 5
            })
        );
        for bad in [
            "{\"mt_contract\":\"mt.near\",\"token_id\":\"a\"}", "{\"mt_contract\":\"mt.near\",\"amount\":\"5\"}", "{\"mt_contract\":\"mt.near\",\"token_id\":\"\",\"amount\":\"5\"}",
            "{\"mt_contract\":\"mt.near\",\"token_id\":\"a\",\"amount\":\"0\"}", "{\"mt_contract\":\"Bad Name\",\"token_id\":\"a\",\"amount\":\"5\"}",
            "{\"mt_contract\":\"mt.near\",\"token_id\":\"a\",\"amount\":\"5\",\"nft_contract\":\"n.near\"}", "{\"mt_contract\":\"mt.near\",\"token_id\":\"a\",\"amount\":\"5\",\"ft_contract\":\"f.near\"}",
        ] {
            assert!(class_a_asset(bad).is_err(), "{bad}");
        }
    }

    fn completing_mt(c: &mut X207Circle) -> Snapshot {
        let snapshot = registered(c, 10 * CREDIT);
        crate::grant::store_grant_for_test(&grant_mt());
        env::storage_write(&lock_key(1, &snapshot.payer, &snapshot.payee), &[1]);
        Held::Mt(MtEscrow {
            mt_contract: acc("mt.near"),
            token_id: MT_TOKEN.into(),
            amount: RWA_AMOUNT,
            state: EscrowState::Completing,
            buyer: Some(acc("bob.near")),
            snapshot: Some(snapshot.clone()),
        })
        .save(GRANT);
        snapshot
    }

    // TC-CT-092-U02
    #[test]
    fn a_failed_mt_transfer_takes_the_voucher_back_and_a_delivered_one_activates_the_grant() {
        let mut c = circle();
        let empty = picture(&c);
        completing_mt(&mut c);
        callback(&mut c, PromiseResult::Failed);
        let e = load_mt_escrow(GRANT).unwrap();
        assert_eq!(
            (e.state, e.buyer, e.amount),
            (EscrowState::Held, None, RWA_AMOUNT)
        );
        assert_eq!(picture(&c).0, empty.0);
        assert_eq!(c.outstanding_debt.get(&acc("bob.near")), Some(&0));
        assert!(!pair_locked(1, &acc("bob.near"), &acc("alice.near")) && holds_asset(GRANT));
        let event = failed_event();
        assert_eq!(
            (event["reverted"].clone(), event["delta"].as_str()),
            (Value::Bool(true), Some("10000000"))
        );

        let mut c = circle();
        completing_mt(&mut c);
        callback(&mut c, PromiseResult::Successful(vec![]));
        assert_eq!(
            load_mt_escrow(GRANT).map(|e| e.state),
            Some(EscrowState::Delivered)
        );
        let grant = load_grant(GRANT);
        assert_eq!(
            (grant.status, grant.grantee),
            (GrantStatus::Active, Some(acc("bob.near")))
        );
        assert!(!holds_asset(GRANT));
    }

    // TC-CT-092-U03: a failure that cannot be taken back leaves the escrow stuck
    #[test]
    fn a_failed_mt_transfer_that_cannot_be_taken_back_leaves_the_escrow_stuck() {
        let mut c = circle();
        completing_mt(&mut c);
        c.epochs
            .get_mut(&1)
            .unwrap()
            .start_clearing(3_600_000_001_000);
        callback(&mut c, PromiseResult::Failed);
        assert_eq!(
            load_mt_escrow(GRANT).map(|e| e.state),
            Some(EscrowState::Stuck)
        );
        assert!(holds_asset(GRANT) && !pair_locked(1, &acc("bob.near"), &acc("alice.near")));
    }

    // ---- CT-096: official contracts per asset type ----

    fn set_list(asset_type: &str, contracts: &[&str]) {
        let mut map = crate::asset_contracts::lists();
        map.insert(
            asset_type.to_string(),
            contracts.iter().map(|c| acc(c)).collect(),
        );
        map.flush();
    }

    // TC-CT-096-U03
    #[test]
    fn the_nft_receiver_refuses_a_contract_that_is_not_on_the_list_of_its_asset_type() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_offered());
        set_list("nft:ticket", &["official-tickets.near"]);
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("nft.near"))
            .block_timestamp(5_000)
            .build());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = c.nft_on_transfer(
                acc("alice.near"),
                acc("alice.near"),
                "t1".into(),
                format!("{{\"grant_id\":{GRANT}}}"),
            );
        }));
        let text = r
            .expect_err("refused")
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(text.contains("ERR_CONTRACT_NOT_OFFICIAL"), "{text}");
        assert!(load_escrow(GRANT).is_none());
        set_list("nft:ticket", &["official-tickets.near", "nft.near"]);
        let _ = c.nft_on_transfer(
            acc("alice.near"),
            acc("alice.near"),
            "t1".into(),
            format!("{{\"grant_id\":{GRANT}}}"),
        );
        assert_eq!(load_escrow(GRANT).map(|e| e.state), Some(EscrowState::Held));
    }

    // TC-CT-096-U04
    #[test]
    fn the_token_receivers_refuse_a_contract_that_is_not_on_the_list_the_multi_token_one_by_its_answer(
    ) {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_ft());
        set_list("ft:rwa", &["real-rwa.near"]);
        assert_refused(
            deposit(&mut c, "rwa.near", "alice.near", RWA_AMOUNT, &escrow_msg()),
            "ERR_CONTRACT_NOT_OFFICIAL",
        );
        set_list("ft:rwa", &["real-rwa.near", "rwa.near"]);
        assert_eq!(
            deposit(&mut c, "rwa.near", "alice.near", RWA_AMOUNT, &escrow_msg()),
            Ok(0)
        );
        // the multi-token escrow answers with the whole amount and says why (the storage of this thread still holds the escrow of the grant above)
        let mut map = ft_escrows();
        map.remove(&GRANT);
        map.flush();
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_mt());
        set_list("mt:usdc", &["intents.near"]);
        refused_mt(
            deposit_mt(
                &mut c,
                "mt.near",
                &["alice.near"],
                &[MT_TOKEN],
                &[RWA_AMOUNT],
                &escrow_msg(),
            ),
            &[RWA_AMOUNT],
            "ERR_CONTRACT_NOT_OFFICIAL",
        );
        set_list("mt:usdc", &["intents.near", "mt.near"]);
        assert_eq!(
            deposit_mt(
                &mut c,
                "mt.near",
                &["alice.near"],
                &[MT_TOKEN],
                &[RWA_AMOUNT],
                &escrow_msg()
            ),
            Ok(vec![0])
        );
    }

    // TC-CT-096-U05: a list of one asset type does not touch another type, and an escrow that is held stays held when the list changes
    #[test]
    fn a_list_of_another_type_changes_nothing_and_a_held_escrow_stays() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_ft());
        set_list("nft:ticket", &["official-tickets.near"]);
        assert_eq!(
            deposit(&mut c, "rwa.near", "alice.near", RWA_AMOUNT, &escrow_msg()),
            Ok(0)
        );
        set_list("ft:rwa", &["real-rwa.near"]);
        assert_eq!(
            load_ft_escrow(GRANT).map(|e| e.state),
            Some(EscrowState::Held),
            "the list does not reach back into a held escrow"
        );
    }

    // ---- CT-126: escrow of a NEAR account held by its locker ----

    const SOLD: &str = "sold.near";
    const BUYER_KEY: &str = "ed25519:7Yk4CPJpnV2VAwYTvHdUKRrHSQbCE8SiJBB9UCjSQbVM";
    const OLD_KEY: &str = "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp";

    fn grant_account(asset_type: &str) -> AccessGrant {
        AccessGrant {
            asset_type: asset_type.into(),
            limits: GrantLimits {
                scope: format!("{{\"account_id\":\"{SOLD}\"}}"),
                quota: None,
                expires_at: U64(9_000_000_000_000),
            },
            ..grant_offered()
        }
    }

    fn member(c: &mut X207Circle, id: &str, status: crate::member::MemberStatus) {
        let account_id = acc(id);
        c.members.insert(
            account_id.clone(),
            crate::member::Member {
                account_id: account_id.clone(),
                pubkey_voucher: OLD_KEY.parse().unwrap(),
                terms_hash: "9f".repeat(32),
                stake: 100 * CREDIT,
                limit: 80 * CREDIT,
                status,
                admitted: true,
                joined_at: 1_000,
                settled_history: 0,
                last_active_epoch: 1,
                exit_requested_epoch: None,
                withdraw_pending: false,
                reactivate_epoch: None,
            },
        );
        c.member_index.push(account_id);
        c.member_count += 1;
    }

    fn as_locker(now: u64) {
        testing_env!(VMContextBuilder::new()
            .current_account_id(acc("circle.near"))
            .predecessor_account_id(acc(SOLD))
            .block_timestamp(now)
            .build());
    }

    fn lock_text(c: &mut X207Circle) -> Result<(), String> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.account_on_lock(GRANT))).map_err(
            |e| {
                e.downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default()
            },
        )
    }

    /// The panic text of a call that must fail (the promise that a call returns has no `Debug`, so it is dropped inside).
    fn refusal<T>(f: impl FnOnce() -> T) -> String {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f();
        }));
        let err = result.expect_err("the call must fail");
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    }

    fn refused_lock(c: &mut X207Circle, code: &str) {
        let err = lock_text(c).expect_err(code);
        assert!(err.contains(code), "{code}: {err}");
        assert!(
            crate::account_escrow::load_account_escrow(GRANT).is_none(),
            "{code}: nothing is recorded"
        );
    }

    fn voucher() -> Voucher {
        Voucher {
            circle: acc("circle.near"),
            payer: acc("bob.near"),
            payee: acc("alice.near"),
            epoch: 1,
            seq: 1,
            cumulative_amount: U128(10 * CREDIT),
            unit: "credit".into(),
            resource: "x".into(),
        }
    }

    // TC-CT-126-01: the locker of the right account registers the escrow of an offered class A grant; the escrow is held and the event is out
    #[test]
    fn an_account_locker_registers_a_held_escrow() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_account("near:account"));
        as_locker(5_000);
        c.account_on_lock(GRANT);
        let view = c.get_account_escrow(GRANT).expect("escrow");
        assert_eq!(
            (view.account_id, view.state, view.locked_epoch),
            (acc(SOLD), EscrowState::Held, 1)
        );
        assert!(get_logs()
            .iter()
            .any(|l| l.contains("account_escrow_held") && l.contains(SOLD)));
        assert!(
            holds_asset(GRANT),
            "the held account keeps the grant from being revoked"
        );
    }

    // TC-CT-126-02: a locker on another account, a grant that is not offered or expired, a second escrow, a grantor that sells itself are refused
    #[test]
    fn account_on_lock_refuses_what_does_not_match() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_account("near:account"));
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("other.near"))
            .block_timestamp(5_000)
            .build());
        refused_lock(&mut c, "ERR_ESCROW_ACCOUNT_MISMATCH");
        let mut g = grant_account("near:account");
        g.status = GrantStatus::Active;
        crate::grant::store_grant_for_test(&g);
        as_locker(5_000);
        refused_lock(&mut c, "ERR_GRANT_NOT_OFFERED");
        crate::grant::store_grant_for_test(&grant_account("near:account"));
        as_locker(9_000_000_000_001);
        refused_lock(&mut c, "ERR_GRANT_EXPIRED");
        let mut g = grant_account("near:account");
        g.grantor = acc(SOLD);
        crate::grant::store_grant_for_test(&g);
        as_locker(5_000);
        refused_lock(&mut c, "ERR_ACCOUNT_SELF");
        let mut g = grant_account("near:account");
        g.class = AssetClass::B;
        crate::grant::store_grant_for_test(&g);
        refused_lock(&mut c, "ERR_ESCROW_NOT_CLASS_A");
        crate::grant::store_grant_for_test(&grant_account("near:account"));
        c.account_on_lock(GRANT);
        let again = lock_text(&mut c).expect_err("second lock");
        assert!(again.contains("ERR_ESCROW_EXISTS"), "{again}");
    }

    // TC-CT-129-01: the lock is refused when the grant runs longer than `now + max_escrow_ttl`; exactly at the limit passes
    #[test]
    fn the_lock_is_bounded_by_the_escrow_ttl() {
        let now = 5_000u64;
        let ttl = crate::account_escrow::DEFAULT_MAX_ESCROW_TTL_NS;
        let mut c = circle();
        assert_eq!(c.get_max_escrow_ttl().0, ttl, "default is 72 hours");
        let with_expiry = |expires_at: u64| {
            let mut g = grant_account("near:account");
            g.limits.expires_at = U64(expires_at);
            crate::grant::store_grant_for_test(&g);
        };
        with_expiry(now + ttl + 1);
        as_locker(now);
        refused_lock(&mut c, "ERR_ESCROW_TTL_TOO_LONG");
        with_expiry(now + ttl);
        as_locker(now);
        c.account_on_lock(GRANT);
        assert!(c.get_account_escrow(GRANT).is_some());
    }

    // TC-CT-129-02: `set_max_escrow_ttl` is for the owner, within 1 hour to 30 days, and the new limit is the one that counts
    #[test]
    fn set_max_escrow_ttl_is_owner_only_and_in_range() {
        use crate::account_escrow::{MAX_MAX_ESCROW_TTL_NS, MIN_MAX_ESCROW_TTL_NS};
        let mut c = circle();
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("eve.near"))
            .build());
        let eve = refusal(|| c.set_max_escrow_ttl(U64(MIN_MAX_ESCROW_TTL_NS)));
        assert!(eve.contains("ERR_NOT_OWNER"), "{eve}");
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("dao.near"))
            .build());
        for bad in [MIN_MAX_ESCROW_TTL_NS - 1, MAX_MAX_ESCROW_TTL_NS + 1, 0] {
            let err = refusal(|| c.set_max_escrow_ttl(U64(bad)));
            assert!(err.contains("ERR_BAD_ESCROW_TTL"), "{bad}: {err}");
        }
        c.set_max_escrow_ttl(U64(MIN_MAX_ESCROW_TTL_NS));
        c.set_max_escrow_ttl(U64(MAX_MAX_ESCROW_TTL_NS));
        assert_eq!(c.get_max_escrow_ttl().0, MAX_MAX_ESCROW_TTL_NS);
        c.set_max_escrow_ttl(U64(MIN_MAX_ESCROW_TTL_NS));
        let now = 5_000u64;
        let mut g = grant_account("near:account");
        g.limits.expires_at = U64(now + MIN_MAX_ESCROW_TTL_NS + 1);
        crate::grant::store_grant_for_test(&g);
        as_locker(now);
        refused_lock(&mut c, "ERR_ESCROW_TTL_TOO_LONG");
        g.limits.expires_at = U64(now + MIN_MAX_ESCROW_TTL_NS);
        crate::grant::store_grant_for_test(&g);
        c.account_on_lock(GRANT);
        assert!(c.get_account_escrow(GRANT).is_some());
    }

    // TC-CT-126-03: the asset type follows the account: a member is sold as near:member_account only, anybody else as near:account
    #[test]
    fn the_asset_type_must_match_the_kind_of_account() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_account("near:member_account"));
        as_locker(5_000);
        refused_lock(&mut c, "ERR_ACCOUNT_ASSET_TYPE"); // not a member, sold as a member account
        member(&mut c, SOLD, crate::member::MemberStatus::Active);
        crate::grant::store_grant_for_test(&grant_account("near:account"));
        refused_lock(&mut c, "ERR_ACCOUNT_ASSET_TYPE"); // a member, sold as a plain account
        crate::grant::store_grant_for_test(&grant_account("near:member_account"));
        c.account_on_lock(GRANT);
        assert!(c.get_account_escrow(GRANT).is_some());
    }

    // TC-CT-126-04: a member account that is not active, is leaving, has a bond, is frozen or is an EVM member is not sellable
    #[test]
    fn a_member_account_in_a_bad_state_is_not_sellable() {
        use crate::member::MemberStatus;
        for case in ["suspended", "exited", "exit_requested", "withdraw_pending"] {
            let mut c = circle();
            crate::grant::store_grant_for_test(&grant_account("near:member_account"));
            member(&mut c, SOLD, MemberStatus::Active);
            let m = c.members.get_mut(&acc(SOLD)).unwrap();
            match case {
                "suspended" => m.status = MemberStatus::Suspended,
                "exited" => m.status = MemberStatus::Exited,
                "exit_requested" => m.exit_requested_epoch = Some(1),
                _ => m.withdraw_pending = true,
            }
            as_locker(5_000);
            refused_lock(&mut c, "ERR_ACCOUNT_NOT_SELLABLE");
        }
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_account("near:member_account"));
        let evm = "0x1111111111111111111111111111111111111111";
        let mut g = grant_account("near:member_account");
        g.limits.scope = format!("{{\"account_id\":\"{evm}\"}}");
        crate::grant::store_grant_for_test(&g);
        member(&mut c, evm, MemberStatus::Active);
        let mut signers = crate::evm::evm_signers();
        signers.insert(
            acc(evm),
            crate::evm::EvmSigner {
                address: [0x11; 20],
                sponsor: acc("relayer.near"),
                nonce: 0,
            },
        );
        signers.flush();
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc(evm))
            .block_timestamp(5_000)
            .build());
        refused_lock(&mut c, "ERR_ACCOUNT_NOT_SELLABLE");
    }

    // TC-CT-126-05: complete_escrow on an account escrow and complete_account_escrow on a token escrow are refused before any payment
    #[test]
    fn the_two_complete_methods_refuse_the_wrong_kind_of_escrow() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_account("near:account"));
        as_locker(5_000);
        c.account_on_lock(GRANT);
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("bob.near"))
            .block_timestamp(5_000)
            .build());
        let text = refusal(|| c.complete_escrow(GRANT, voucher(), "sig".into()));
        assert!(text.contains("ERR_ESCROW_NEEDS_PUBKEY"), "{text}");

        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_offered());
        save(
            GRANT,
            Escrow {
                nft_contract: acc("nft.near"),
                token_id: "t1".into(),
                state: EscrowState::Held,
                buyer: None,
                snapshot: None,
            },
        );
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("bob.near"))
            .block_timestamp(5_000)
            .build());
        let text = refusal(|| {
            c.complete_account_escrow(GRANT, voucher(), "sig".into(), BUYER_KEY.parse().unwrap())
        });
        assert!(text.contains("ERR_ESCROW_NOT_ACCOUNT"), "{text}");
    }

    // TC-CT-126-06 (#14): a member account is sold only from the epoch after the lock, and only to an ed25519 key
    #[test]
    fn a_member_account_waits_one_epoch_and_needs_an_ed25519_key() {
        let mut c = circle();
        member(&mut c, SOLD, crate::member::MemberStatus::Active);
        crate::grant::store_grant_for_test(&grant_account("near:member_account"));
        as_locker(5_000);
        c.account_on_lock(GRANT);
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("bob.near"))
            .block_timestamp(5_000)
            .build());
        let text = refusal(|| {
            c.complete_account_escrow(GRANT, voucher(), "sig".into(), BUYER_KEY.parse().unwrap())
        });
        assert!(text.contains("ERR_ESCROW_EPOCH_WAIT"), "{text}");
        let secp = "secp256k1:qMoRgcoXai4mBPsdbHi1wfyxF9TdbPCF4qSDQTRP3TfescSRoUdSx6nmeQoN3aiwGzwMyGXAb1gUjBTv5AY8DXj";
        let text = refusal(|| {
            c.complete_account_escrow(GRANT, voucher(), "sig".into(), secp.parse().unwrap())
        });
        assert!(text.contains("ERR_BAD_PUBKEY"), "{text}");
    }

    // TC-CT-126-12 (#14, coordinator point 2): the epoch of the lock has ended, but the debt is final only when every earlier epoch is finalized
    #[test]
    fn a_member_account_waits_until_the_earlier_epochs_are_finalized() {
        use crate::clearing::{Epoch, EpochPhase};
        let mut c = circle();
        member(&mut c, SOLD, crate::member::MemberStatus::Active);
        crate::grant::store_grant_for_test(&grant_account("near:member_account"));
        as_locker(5_000);
        c.account_on_lock(GRANT);
        let buy = |c: &mut X207Circle| {
            testing_env!(VMContextBuilder::new()
                .predecessor_account_id(acc("bob.near"))
                .block_timestamp(5_000)
                .build());
            refusal(|| {
                c.complete_account_escrow(
                    GRANT,
                    voucher(),
                    "sig".into(),
                    BUYER_KEY.parse().unwrap(),
                )
            })
        };
        // epoch 1 is still the open one: the lock epoch has not ended
        assert!(buy(&mut c).contains("ERR_ESCROW_EPOCH_WAIT"));
        // epoch 2 is open, epoch 1 has closed but is still clearing: the debt can still change
        c.current_epoch = 2;
        let phase = |c: &mut X207Circle, epoch: u64, phase: EpochPhase| {
            if c.epochs.get(&epoch).is_none() {
                c.epochs
                    .insert(epoch, Epoch::open(epoch, 1_000, 2_000, Params::default()));
            }
            c.epochs.get_mut(&epoch).unwrap().phase = phase;
        };
        phase(&mut c, 1, EpochPhase::Clearing);
        assert!(buy(&mut c).contains("ERR_ESCROW_EPOCH_WAIT"));
        phase(&mut c, 1, EpochPhase::PaymentWindow);
        assert!(buy(&mut c).contains("ERR_ESCROW_EPOCH_WAIT"));
        // epoch 1 finalized, but epoch 2 (before the open epoch 3) is not
        phase(&mut c, 1, EpochPhase::Finalized);
        c.current_epoch = 3;
        phase(&mut c, 2, EpochPhase::PaymentWindow);
        assert!(buy(&mut c).contains("ERR_ESCROW_EPOCH_WAIT"));
        // everything before the open epoch is finalized: the next refusal is not about epochs
        phase(&mut c, 2, EpochPhase::Finalized);
        assert!(!buy(&mut c).contains("ERR_ESCROW_EPOCH_WAIT"));
    }

    fn completing_account(c: &mut X207Circle, member_account: bool) -> Snapshot {
        let snapshot = registered(c, 10 * CREDIT);
        let asset = if member_account {
            "near:member_account"
        } else {
            "near:account"
        };
        crate::grant::store_grant_for_test(&grant_account(asset));
        if member_account {
            member(c, SOLD, crate::member::MemberStatus::Active);
        }
        env::storage_write(&lock_key(1, &snapshot.payer, &snapshot.payee), &[1]);
        crate::account_escrow::save_account_escrow(
            GRANT,
            crate::account_escrow::AccountEscrow {
                account_id: acc(SOLD),
                state: EscrowState::Completing,
                buyer: Some(acc("bob.near")),
                snapshot: Some(snapshot.clone()),
                locked_epoch: 1,
                delivery_pubkey: Some(BUYER_KEY.parse().unwrap()),
            },
        );
        snapshot
    }

    // TC-CT-126-07: a delivered account activates the grant with the key of the buyer; the voucher key of a member account is rotated to that key
    #[test]
    fn a_delivered_account_activates_the_grant_and_rotates_the_member_key() {
        let mut c = circle();
        completing_account(&mut c, true);
        callback(&mut c, PromiseResult::Successful(vec![]));
        let escrow = c.get_account_escrow(GRANT).unwrap();
        assert_eq!(
            (escrow.state, escrow.buyer),
            (EscrowState::Delivered, Some(acc("bob.near")))
        );
        let grant = load_grant(GRANT);
        assert_eq!(
            (grant.status, grant.grantee, grant.delivery_pubkey),
            (
                GrantStatus::Active,
                Some(acc("bob.near")),
                Some(BUYER_KEY.parse().unwrap())
            )
        );
        assert_eq!(
            c.members.get(&acc(SOLD)).unwrap().pubkey_voucher,
            BUYER_KEY.parse::<PublicKey>().unwrap(),
            "the seller's old voucher key no longer signs for the member"
        );
        assert!(!pair_locked(1, &acc("bob.near"), &acc("alice.near")));
        // an account without a position changes no member row
        let mut plain = circle();
        completing_account(&mut plain, false);
        callback(&mut plain, PromiseResult::Successful(vec![]));
        assert!(plain.members.get(&acc(SOLD)).is_none());
    }

    // TC-CT-126-08: a failed release takes the voucher back exactly and leaves the escrow held; the member key is not rotated
    #[test]
    fn a_failed_release_takes_the_voucher_back_and_keeps_the_old_key() {
        let mut c = circle();
        let before = picture(&c);
        completing_account(&mut c, true);
        callback(&mut c, PromiseResult::Failed);
        let escrow = c.get_account_escrow(GRANT).unwrap();
        assert_eq!((escrow.state, escrow.buyer), (EscrowState::Held, None));
        assert_eq!(picture(&c), before, "the registration is gone");
        assert_eq!(
            c.members.get(&acc(SOLD)).unwrap().pubkey_voucher,
            OLD_KEY.parse::<PublicKey>().unwrap()
        );
        assert_eq!(failed_event()["reverted"], serde_json::json!(true));
    }

    // TC-CT-126-09: cancel of an account escrow goes through give_back after expiry, only for the grantor
    #[test]
    fn cancel_of_an_account_escrow_gives_the_keys_back_after_expiry() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_account("near:account"));
        as_locker(5_000);
        c.account_on_lock(GRANT);
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("eve.near"))
            .block_timestamp(9_000_000_000_001)
            .build());
        assert!(refusal(|| c.cancel_escrow(GRANT)).contains("ERR_ESCROW_NOT_GRANTOR"));
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("alice.near"))
            .block_timestamp(5_000)
            .build());
        assert!(refusal(|| c.cancel_escrow(GRANT)).contains("ERR_ESCROW_NOT_EXPIRED"));
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("alice.near"))
            .block_timestamp(9_000_000_000_001)
            .build());
        let _promise = c.cancel_escrow(GRANT);
        assert_eq!(
            c.get_account_escrow(GRANT).unwrap().state,
            EscrowState::Cancelling
        );
        testing_env!(
            VMContextBuilder::new()
                .current_account_id(acc("circle.near"))
                .predecessor_account_id(acc("circle.near"))
                .block_timestamp(9_000_000_000_002)
                .build(),
            test_vm_config(),
            RuntimeFeesConfig::test(),
            Default::default(),
            vec![PromiseResult::Successful(vec![])]
        );
        c.on_escrow_resolved(GRANT, EscrowStep::Cancel);
        assert_eq!(
            c.get_account_escrow(GRANT).unwrap().state,
            EscrowState::Cancelled
        );
        assert_eq!(load_grant(GRANT).status, GrantStatus::Revoked);
    }

    // TC-CT-126-13 (coordinator point 3): anyone may cancel an expired account escrow, once, never early, never for a token, never after a sale started
    #[test]
    fn anyone_cancels_an_expired_account_escrow_once() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_account("near:account"));
        as_locker(5_000);
        c.account_on_lock(GRANT);
        let as_eve = |now: u64| {
            testing_env!(VMContextBuilder::new()
                .predecessor_account_id(acc("eve.near"))
                .block_timestamp(now)
                .build());
        };
        as_eve(5_000);
        assert!(
            refusal(|| c.cancel_expired_account_escrow(GRANT)).contains("ERR_ESCROW_NOT_EXPIRED")
        );
        as_eve(9_000_000_000_001);
        drop(c.cancel_expired_account_escrow(GRANT)); // a Promise is scheduled when it is dropped
        assert_eq!(
            c.get_account_escrow(GRANT).unwrap().state,
            EscrowState::Cancelling
        );
        // (b) the only key that comes back is the return key: the circle sends give_back, which carries no key argument
        let receipts = near_sdk::test_utils::get_created_receipts();
        let call = format!("{:?}", receipts.first().expect("a receipt"));
        let bytes = |name: &str| {
            name.bytes()
                .map(|b| b.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        assert!(
            call.contains(&bytes("give_back")) && !call.contains(&bytes("release")),
            "{call}"
        );
        assert_eq!(receipts[0].receiver_id.as_str(), SOLD);
        // (c) a second call is refused; (d) so is a sale that arrives after the cancel started
        assert!(refusal(|| c.cancel_expired_account_escrow(GRANT)).contains("ERR_ESCROW_NOT_HELD"));
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("bob.near"))
            .block_timestamp(9_000_000_000_001)
            .build());
        let text = refusal(|| {
            c.complete_account_escrow(GRANT, voucher(), "sig".into(), BUYER_KEY.parse().unwrap())
        });
        assert!(
            text.contains("ERR_ESCROW_NOT_HELD") || text.contains("ERR_GRANT_EXPIRED"),
            "{text}"
        );
    }

    // TC-CT-126-15: a stranger does not cancel the escrow of a token through the account method
    #[test]
    fn a_stranger_cancels_no_token_escrow() {
        let mut c2 = circle();
        crate::grant::store_grant_for_test(&grant_offered());
        save(
            GRANT,
            Escrow {
                nft_contract: acc("nft.near"),
                token_id: "t1".into(),
                state: EscrowState::Held,
                buyer: None,
                snapshot: None,
            },
        );
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("eve.near"))
            .block_timestamp(9_000_000_000_001)
            .build());
        assert!(
            refusal(|| c2.cancel_expired_account_escrow(GRANT)).contains("ERR_ESCROW_NOT_ACCOUNT")
        );
    }

    // TC-CT-126-14 (coordinator point 1): a member account under an open escrow registers no voucher as payer; the lock ends when the account is delivered or taken back
    #[test]
    fn an_account_under_escrow_adds_no_debt() {
        let mut c = circle();
        member(&mut c, SOLD, crate::member::MemberStatus::Active);
        crate::grant::store_grant_for_test(&grant_account("near:member_account"));
        as_locker(5_000);
        c.account_on_lock(GRANT);
        let text = refusal(|| crate::account_escrow::assert_not_locked(&acc(SOLD)));
        assert!(text.contains("ERR_ACCOUNT_LOCKED"), "{text}");
        crate::account_escrow::assert_not_locked(&acc("bob.near"));
        // a failed delivery leaves the escrow held and the lock in place
        let mut c = circle();
        completing_account(&mut c, true);
        crate::account_escrow::mark_locked(&acc(SOLD), GRANT);
        callback(&mut c, PromiseResult::Failed);
        assert_eq!(
            c.get_account_escrow(GRANT).unwrap().state,
            EscrowState::Held
        );
        assert!(
            refusal(|| crate::account_escrow::assert_not_locked(&acc(SOLD)))
                .contains("ERR_ACCOUNT_LOCKED")
        );
        // a delivery ends it, and the key of the member is the key of the buyer
        let mut c = circle();
        completing_account(&mut c, true);
        crate::account_escrow::mark_locked(&acc(SOLD), GRANT);
        callback(&mut c, PromiseResult::Successful(vec![]));
        crate::account_escrow::assert_not_locked(&acc(SOLD));
        assert_eq!(
            c.members.get(&acc(SOLD)).unwrap().pubkey_voucher,
            BUYER_KEY.parse::<PublicKey>().unwrap()
        );
    }

    // TC-CT-126-10: the list of locker code hashes is the owner's, short, base58 and without duplicates
    #[test]
    fn the_locker_code_hashes_belong_to_the_owner() {
        let mut c = circle();
        let good = "5Qoo9JQ9LSEsWDpeS2SvZkwBTqgQ8Hy1zRmHkq3EJdmE".to_string();
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("eve.near"))
            .build());
        let eve = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.set_locker_code_hashes(vec![good.clone()])
        }))
        .expect_err("owner");
        assert!(eve
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default()
            .contains("ERR_NOT_OWNER"));
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc("dao.near"))
            .build());
        c.set_locker_code_hashes(vec![good.clone()]);
        assert_eq!(c.get_locker_code_hashes(), vec![good.clone()]);
        for (bad, code) in [
            (vec![good.clone(), good.clone()], "ERR_DUPLICATE_HASH"),
            (vec!["not-a-hash".to_string()], "ERR_BAD_HASH"),
            (vec![good.clone(); 9], "ERR_TOO_MANY_HASHES"),
        ] {
            let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                c.set_locker_code_hashes(bad.clone())
            }))
            .expect_err(code);
            assert!(
                err.downcast_ref::<String>()
                    .cloned()
                    .unwrap_or_default()
                    .contains(code),
                "{code}"
            );
        }
        assert_eq!(
            c.get_locker_code_hashes(),
            vec![good],
            "a refused list changes nothing"
        );
        c.set_locker_code_hashes(vec![]);
        assert!(c.get_locker_code_hashes().is_empty());
    }

    // TC-CT-126-11: offer_grant takes the account scope for the two account types and refuses a seller that offers its own account
    #[test]
    fn the_account_scope_is_checked() {
        use crate::account_escrow::account_scope;
        assert_eq!(
            account_scope("{\"account_id\":\"a.near\"}"),
            Ok(acc("a.near"))
        );
        for bad in [
            "{}",
            "[]",
            "{\"account_id\":1}",
            "{\"account_id\":\"A B\"}",
            "{\"account_id\":\"a.near\",\"extra\":1}",
            "nope",
        ] {
            assert!(account_scope(bad).is_err(), "{bad}");
        }
    }
}
