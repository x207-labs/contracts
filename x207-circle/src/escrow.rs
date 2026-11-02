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
use near_sdk::{env, near, require, AccountId, Gas, NearToken, Promise, PromiseOrValue};

use crate::clearing::EpochPhase;
use crate::events::{EscrowFailed, EscrowHeld, GrantActivated, GrantRevoked, X207Event};
use crate::grant::{load_grant, AssetClass, GrantId, GrantStatus, RevokeReason};
use crate::state::{StorageKey, X207Circle, X207CircleExt};
use crate::types::EpochId;
use crate::canonical::Voucher;
use crate::voucher::{PairState, ParticipantTotals, RegisterMode};

/// `complete_escrow` is refused this close to the cutoff of the epoch, so that the callback cannot meet a closed epoch.
pub const ESCROW_CUTOFF_MARGIN_NS: u64 = 120_000_000_000;
const NFT_TRANSFER_GAS: Gas = Gas::from_tgas(30);
const RESOLVE_GAS: Gas = Gas::from_tgas(30);
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

fn lock_key(epoch: EpochId, payer: &AccountId, payee: &AccountId) -> Vec<u8> {
    [LOCK_PREFIX, epoch.to_le_bytes().as_slice(), payer.as_bytes(), b">", payee.as_bytes()].concat()
}

/// `register_with` asks this before it touches a pair: a pair with an escrow payment in flight takes no other voucher.
pub fn pair_locked(epoch: EpochId, payer: &AccountId, payee: &AccountId) -> bool {
    env::storage_has_key(&lock_key(epoch, payer, payee))
}

/// An escrow that holds an asset keeps its grant from being revoked: `cancel_escrow` is the way out.
pub fn holds_asset(grant_id: GrantId) -> bool {
    load_escrow(grant_id).is_some_and(|e| matches!(e.state, EscrowState::Held | EscrowState::Completing | EscrowState::Cancelling | EscrowState::Stuck))
}

#[near]
impl X207Circle {
    /// NEP-171 receiver. `msg` is `{"grant_id": N}`. Only a class A grant that is `offered`, whose grantor sends the token itself and has no
    /// escrow yet. Returns `false`: the token stays. A panic gives the token back to the sender (the NFT contract resolves it).
    pub fn nft_on_transfer(&mut self, sender_id: AccountId, previous_owner_id: AccountId, token_id: String, msg: String) -> PromiseOrValue<bool> {
        let _ = sender_id;
        let nft_contract = env::predecessor_account_id();
        let grant_id = near_sdk::serde_json::from_str::<near_sdk::serde_json::Value>(&msg)
            .ok()
            .and_then(|v| v.get("grant_id").and_then(|g| g.as_u64()))
            .unwrap_or_else(|| env::panic_str("ERR_BAD_MSG: msg must be {\"grant_id\": N}"));
        let grant = load_grant(grant_id);
        require!(grant.class == AssetClass::A, format!("ERR_ESCROW_NOT_CLASS_A: grant {grant_id} is class {:?}", grant.class));
        require!(grant.status == GrantStatus::Offered, format!("ERR_GRANT_NOT_OFFERED: {:?}", grant.status));
        require!(previous_owner_id == grant.grantor, format!("ERR_ESCROW_DEPOSITOR: only {} may put the asset of grant {grant_id}", grant.grantor));
        require!(load_escrow(grant_id).is_none(), format!("ERR_ESCROW_EXISTS: grant {grant_id}"));
        require!(env::block_timestamp() < grant.limits.expires_at.0, "ERR_GRANT_EXPIRED: the grant ran out");
        save(grant_id, Escrow { nft_contract: nft_contract.clone(), token_id: token_id.clone(), state: EscrowState::Held, buyer: None, snapshot: None });
        EscrowHeld { grant_id, nft_contract, token_id }.emit();
        PromiseOrValue::Value(false)
    }

    pub fn get_escrow(&self, grant_id: GrantId) -> Option<EscrowView> {
        load_escrow(grant_id).map(|e| EscrowView { grant_id, nft_contract: e.nft_contract, token_id: e.token_id, state: e.state, buyer: e.buyer })
    }

    /// The buyer pays and receives the asset. The caller is the payer of `voucher`, the payee is the grantor, and the voucher raises the
    /// pair by at least `unit_price`. Registers like `register_cumulative`, then sends the NFT; the grant becomes `active` with the buyer as
    /// grantee in `on_escrow_resolved`. Checks in order: `ERR_UNKNOWN_GRANT`, `ERR_ESCROW_NOT_CLASS_A`, `ERR_GRANT_NOT_OFFERED`,
    /// `ERR_GRANT_EXPIRED`, `ERR_ESCROW_UNKNOWN`, `ERR_ESCROW_NOT_HELD`, `ERR_GRANT_SELF`, `ERR_ESCROW_VOUCHER`, `ERR_ESCROW_PRICE`,
    /// `ERR_ESCROW_TOO_LATE`, `ERR_ESCROW_PENDING`, then the checks of the voucher itself.
    pub fn complete_escrow(&mut self, grant_id: GrantId, voucher: Voucher, signature: String) -> Promise {
        let grant = load_grant(grant_id);
        require!(grant.class == AssetClass::A, format!("ERR_ESCROW_NOT_CLASS_A: grant {grant_id}"));
        require!(grant.status == GrantStatus::Offered, format!("ERR_GRANT_NOT_OFFERED: {:?}", grant.status));
        let now = env::block_timestamp();
        require!(now < grant.limits.expires_at.0, "ERR_GRANT_EXPIRED: the grant ran out");
        let mut escrow = load_escrow(grant_id).unwrap_or_else(|| env::panic_str(&format!("ERR_ESCROW_UNKNOWN: grant {grant_id}")));
        require!(escrow.state == EscrowState::Held, format!("ERR_ESCROW_NOT_HELD: {:?}", escrow.state));
        let buyer = env::predecessor_account_id();
        require!(buyer != grant.grantor, format!("ERR_GRANT_SELF: {buyer}"));
        require!(voucher.payer == buyer && voucher.payee == grant.grantor, "ERR_ESCROW_VOUCHER: the caller pays the grantor");

        let epoch_id = voucher.epoch;
        let key = (epoch_id, voucher.payer.clone(), voucher.payee.clone());
        let pair_before = self.pairs.get(&key).cloned();
        let delta = voucher.cumulative_amount.0.saturating_sub(pair_before.as_ref().map_or(0, |p| p.cumulative));
        require!(delta >= grant.unit_price.0, format!("ERR_ESCROW_PRICE: the voucher raises the pair by {delta}, the price is {}", grant.unit_price.0));
        if let Some(epoch) = self.epochs.get(&epoch_id) {
            require!(now + ESCROW_CUTOFF_MARGIN_NS < epoch.cutoff_at, format!("ERR_ESCROW_TOO_LATE: cutoff_at {}", epoch.cutoff_at));
        }
        require!(!pair_locked(epoch_id, &voucher.payer, &voucher.payee), "ERR_ESCROW_PENDING: another escrow payment of this pair is in flight");
        let payer_new = self.totals.get(&(epoch_id, voucher.payer.clone())).is_none();
        let payee_new = self.totals.get(&(epoch_id, voucher.payee.clone())).is_none();
        let (seq, cumulative) = (voucher.seq, voucher.cumulative_amount.0);

        self.register_with(voucher.clone(), signature, RegisterMode::Cumulative, None);

        env::storage_write(&lock_key(epoch_id, &voucher.payer, &voucher.payee), &[1]);
        escrow.state = EscrowState::Completing;
        escrow.buyer = Some(buyer.clone());
        escrow.snapshot = Some(Snapshot { epoch: epoch_id, payer: voucher.payer, payee: voucher.payee, pair_before, delta, payer_new, payee_new, seq, cumulative });
        let (nft, token) = (escrow.nft_contract.clone(), escrow.token_id.clone());
        save(grant_id, escrow);
        self.send_and_resolve(nft, token, buyer, grant_id, EscrowStep::Complete)
    }

    /// The grantor takes the asset back after `expires_at` of the grant, when nobody bought it.
    pub fn cancel_escrow(&mut self, grant_id: GrantId) -> Promise {
        let grant = load_grant(grant_id);
        require!(env::predecessor_account_id() == grant.grantor, format!("ERR_ESCROW_NOT_GRANTOR: only {} cancels", grant.grantor));
        let mut escrow = load_escrow(grant_id).unwrap_or_else(|| env::panic_str(&format!("ERR_ESCROW_UNKNOWN: grant {grant_id}")));
        require!(escrow.state == EscrowState::Held, format!("ERR_ESCROW_NOT_HELD: {:?}", escrow.state));
        require!(grant.status == GrantStatus::Offered, format!("ERR_GRANT_NOT_OFFERED: {:?}", grant.status));
        require!(env::block_timestamp() >= grant.limits.expires_at.0, format!("ERR_ESCROW_NOT_EXPIRED: expires_at {}", grant.limits.expires_at.0));
        escrow.state = EscrowState::Cancelling;
        let (nft, token) = (escrow.nft_contract.clone(), escrow.token_id.clone());
        save(grant_id, escrow);
        self.send_and_resolve(nft, token, grant.grantor, grant_id, EscrowStep::Cancel)
    }

    /// Callback of the `nft_transfer` of `complete_escrow` or `cancel_escrow`.
    #[private]
    pub fn on_escrow_resolved(&mut self, grant_id: GrantId, step: EscrowStep) {
        let ok = env::promise_result_checked(0, 64).is_ok();
        let mut escrow = load_escrow(grant_id).unwrap_or_else(|| env::panic_str("ERR_ESCROW_UNKNOWN"));
        let mut grant = load_grant(grant_id);
        match (step, ok) {
            (EscrowStep::Complete, true) => {
                let snapshot = escrow.snapshot.take().expect("snapshot of a completing escrow");
                env::storage_remove(&lock_key(snapshot.epoch, &snapshot.payer, &snapshot.payee));
                let buyer = escrow.buyer.clone().expect("buyer of a completing escrow");
                escrow.state = EscrowState::Delivered;
                crate::grant::activate(&mut grant, buyer.clone(), None);
                save(grant_id, escrow);
                GrantActivated { grant_id, grantee: buyer, delivery_pubkey: None }.emit();
            }
            (EscrowStep::Complete, false) => {
                let snapshot = escrow.snapshot.take().expect("snapshot of a completing escrow");
                env::storage_remove(&lock_key(snapshot.epoch, &snapshot.payer, &snapshot.payee));
                let reverted = self.take_back(&snapshot);
                escrow.buyer = None;
                escrow.state = if reverted { EscrowState::Held } else { EscrowState::Stuck };
                save(grant_id, escrow);
                EscrowFailed {
                    grant_id, step: EscrowStep::Complete, epoch: snapshot.epoch, payer: snapshot.payer, payee: snapshot.payee,
                    delta: U128(if reverted { snapshot.delta } else { 0 }), reverted,
                }
                .emit();
            }
            (EscrowStep::Cancel, true) => {
                escrow.state = EscrowState::Cancelled;
                save(grant_id, escrow);
                crate::grant::close(&mut grant, GrantStatus::Revoked);
                GrantRevoked { grant_id, reason: RevokeReason::Grantor }.emit();
            }
            (EscrowStep::Cancel, false) => {
                escrow.state = EscrowState::Held;
                let grantor = grant.grantor.clone();
                save(grant_id, escrow);
                EscrowFailed { grant_id, step: EscrowStep::Cancel, epoch: 0, payer: grantor.clone(), payee: grantor, delta: U128(0), reverted: false }.emit();
            }
        }
    }
}

impl X207Circle {
    fn send_and_resolve(&self, nft: AccountId, token_id: String, receiver: AccountId, grant_id: GrantId, step: EscrowStep) -> Promise {
        Promise::new(nft)
            .function_call(
                "nft_transfer".to_string(),
                json!({"receiver_id": receiver, "token_id": token_id, "memo": format!("x207 escrow grant {grant_id}")}).to_string().into_bytes(),
                NearToken::from_yoctonear(1),
                NFT_TRANSFER_GAS,
            )
            .then(Self::ext(env::current_account_id()).with_static_gas(RESOLVE_GAS).on_escrow_resolved(grant_id, step))
    }

    /// Takes back exactly what `complete_escrow` registered. Only when the epoch is still `open` and the pair is as the voucher left it.
    fn take_back(&mut self, s: &Snapshot) -> bool {
        let key = (s.epoch, s.payer.clone(), s.payee.clone());
        let open = self.epochs.get(&s.epoch).is_some_and(|e| e.phase == EpochPhase::Open) && s.epoch == self.current_epoch;
        let unchanged = self.pairs.get(&key).is_some_and(|p| p.last_seq == s.seq && p.cumulative == s.cumulative);
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
        self.outstanding_debt.insert(s.payer.clone(), debt - s.delta);
        for (account, out, inn, is_new) in [(&s.payer, s.delta, 0, s.payer_new), (&s.payee, 0, s.delta, s.payee_new)] {
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
            if let Some(i) = epoch.edges.iter().position(|(a, b)| a == &s.payer && b == &s.payee) {
                epoch.edges.swap_remove(i as u32);
                epoch.edge_count -= 1;
            }
        }
        true
    }
}

#[allow(dead_code)]
fn _assert_borsh<T: BorshDeserialize>() {}
