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
use near_sdk::{env, near, require, AccountId, Gas, NearToken, Promise, PromiseError, PromiseOrValue};

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
        // `TooLong` means the call succeeded and returned more than 64 bytes (a non-standard NFT contract); it is not a failure
        let ok = !matches!(env::promise_result_checked(0, 64), Err(PromiseError::Failed));
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
        X207Circle::with_config(acc("dao.near"), "agents-main".into(), acc("usdt.near"), acc("intents.near"), Params::default(), 1_000)
    }

    /// The state that `complete_escrow` leaves after `register_with` for a new pair `bob -> alice` of `delta`, and the snapshot it keeps.
    fn registered(c: &mut X207Circle, delta: u128) -> Snapshot {
        let (payer, payee) = (acc("bob.near"), acc("alice.near"));
        c.pairs.insert((1, payer.clone(), payee.clone()), PairState { cumulative: delta, last_seq: 1, cleared: 0 });
        c.outstanding_debt.insert(payer.clone(), delta);
        c.totals.insert((1, payer.clone()), ParticipantTotals { owed_out: delta, owed_in: 0 });
        c.totals.insert((1, payee.clone()), ParticipantTotals { owed_out: 0, owed_in: delta });
        let epoch = c.epochs.get_mut(&1).unwrap();
        epoch.participants.push(payer.clone());
        epoch.participants.push(payee.clone());
        epoch.participant_count = 2;
        epoch.total_obligation = delta;
        epoch.edges.push((payer.clone(), payee.clone()));
        epoch.edge_count = 1;
        Snapshot { epoch: 1, payer, payee, pair_before: None, delta, payer_new: true, payee_new: true, seq: 1, cumulative: delta }
    }

    /// Everything `take_back` may touch, to compare before and after.
    fn picture(c: &X207Circle) -> (Option<PairState>, u128, Option<ParticipantTotals>, Option<ParticipantTotals>, u32, u32, u128, usize, usize) {
        let e = c.epochs.get(&1).unwrap();
        (
            c.pairs.get(&(1, acc("bob.near"), acc("alice.near"))).cloned(),
            c.outstanding_debt.get(&acc("bob.near")).copied().unwrap_or(0),
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
        let before = PairState { cumulative: 4 * CREDIT, last_seq: 1, cleared: 0 };
        c.pairs.insert((1, acc("bob.near"), acc("alice.near")), before.clone());
        c.outstanding_debt.insert(acc("bob.near"), 4 * CREDIT);
        c.totals.insert((1, acc("bob.near")), ParticipantTotals { owed_out: 4 * CREDIT, owed_in: 0 });
        c.totals.insert((1, acc("alice.near")), ParticipantTotals { owed_out: 0, owed_in: 4 * CREDIT });
        // the escrow voucher raises the pair from 4 to 14
        c.pairs.insert((1, acc("bob.near"), acc("alice.near")), PairState { cumulative: 14 * CREDIT, last_seq: 2, cleared: 0 });
        c.outstanding_debt.insert(acc("bob.near"), 14 * CREDIT);
        c.totals.insert((1, acc("bob.near")), ParticipantTotals { owed_out: 14 * CREDIT, owed_in: 0 });
        c.totals.insert((1, acc("alice.near")), ParticipantTotals { owed_out: 0, owed_in: 14 * CREDIT });
        let snapshot = Snapshot {
            epoch: 1, payer: acc("bob.near"), payee: acc("alice.near"), pair_before: Some(before.clone()), delta: 10 * CREDIT,
            payer_new: false, payee_new: false, seq: 2, cumulative: 14 * CREDIT,
        };
        assert!(c.take_back(&snapshot));
        assert_eq!(c.pairs.get(&(1, acc("bob.near"), acc("alice.near"))), Some(&before));
        assert_eq!(c.outstanding_debt.get(&acc("bob.near")), Some(&(4 * CREDIT)));
        assert_eq!(c.totals.get(&(1, acc("bob.near"))), Some(&ParticipantTotals { owed_out: 4 * CREDIT, owed_in: 0 }));
    }

    // TC-CT-075-U03: the epoch is not `open` any more between the registration and the callback
    #[test]
    fn take_back_refuses_when_the_epoch_is_no_longer_open() {
        let mut c = circle();
        let snapshot = registered(&mut c, 10 * CREDIT);
        c.epochs.get_mut(&1).unwrap().start_clearing(3_600_000_001_000);
        let before = picture(&c);
        assert!(!c.take_back(&snapshot));
        assert_eq!(picture(&c), before, "nothing may change when the take back is refused");
    }

    // TC-CT-075-U04: the pair moved on (another voucher raised it)
    #[test]
    fn take_back_refuses_when_the_pair_changed() {
        for (seq, cumulative) in [(2, 10 * CREDIT), (1, 12 * CREDIT), (3, 25 * CREDIT)] {
            let mut c = circle();
            let snapshot = registered(&mut c, 10 * CREDIT);
            c.pairs.insert((1, acc("bob.near"), acc("alice.near")), PairState { cumulative, last_seq: seq, cleared: 0 });
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
            grant_id: GRANT, asset_type: "nft:ticket".into(), grantor: acc("alice.near"), grantee: None, class: AssetClass::A,
            limits: GrantLimits { scope: "{}".into(), quota: None, expires_at: U64(9_000_000_000_000) }, unit: "once".into(), unit_price: U128(10 * CREDIT),
            operator: None, delivery_pubkey: None, status: GrantStatus::Offered, proof_hash: None, proof_kind: None,
        }
    }

    /// `complete_escrow` has run: state `Completing`, the voucher registered, the pair locked, the grant still `offered`.
    fn completing(c: &mut X207Circle) -> Snapshot {
        let snapshot = registered(c, 10 * CREDIT);
        crate::grant::store_grant_for_test(&grant_offered());
        env::storage_write(&lock_key(1, &snapshot.payer, &snapshot.payee), &[1]);
        save(GRANT, Escrow {
            nft_contract: acc("nft.near"), token_id: "t1".into(), state: EscrowState::Completing, buyer: Some(acc("bob.near")), snapshot: Some(snapshot.clone()),
        });
        snapshot
    }

    fn callback(c: &mut X207Circle, result: PromiseResult) {
        testing_env!(
            VMContextBuilder::new().current_account_id(acc("circle.near")).predecessor_account_id(acc("circle.near")).block_timestamp(5_000).build(),
            test_vm_config(), RuntimeFeesConfig::test(), Default::default(), vec![result]
        );
        c.on_escrow_resolved(GRANT, EscrowStep::Complete);
    }

    fn failed_event() -> Value {
        let log = get_logs().into_iter().find(|l| l.contains("escrow_failed")).expect("escrow_failed event");
        serde_json::from_str::<Value>(log.strip_prefix("EVENT_JSON:").unwrap()).unwrap()["data"][0].clone()
    }

    // TC-CT-075-U06: a failed transfer that cannot be taken back leaves the escrow `stuck`, the payment in place and the NFT in the circle
    #[test]
    fn a_failed_transfer_that_cannot_be_taken_back_leaves_the_escrow_stuck() {
        for breakage in ["epoch", "pair", "debt"] {
            let mut c = circle();
            completing(&mut c);
            match breakage {
                "epoch" => c.epochs.get_mut(&1).unwrap().start_clearing(3_600_000_001_000),
                "pair" => {
                    c.pairs.insert((1, acc("bob.near"), acc("alice.near")), PairState { cumulative: 12 * CREDIT, last_seq: 2, cleared: 0 });
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
            assert!(!pair_locked(1, &acc("bob.near"), &acc("alice.near")), "{breakage}");
            assert_eq!(load_grant(GRANT).status, GrantStatus::Offered, "{breakage}");
            assert!(holds_asset(GRANT), "{breakage}: a stuck escrow still blocks revoke_grant");
            let event = failed_event();
            assert_eq!((event["reverted"].clone(), event["delta"].as_str(), event["step"].as_str()), (Value::Bool(false), Some("0"), Some("complete")), "{breakage}");
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
        assert_eq!((event["reverted"].clone(), event["delta"].as_str()), (Value::Bool(true), Some("10000000")));
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
        assert_eq!((grant.status, grant.grantee), (GrantStatus::Active, Some(acc("bob.near"))));
        assert!(!holds_asset(GRANT));
        assert_eq!(c.pairs.get(&(1, acc("bob.near"), acc("alice.near"))).map(|p| p.cumulative), Some(10 * CREDIT));
    }

    // TC-CT-075-S01 (security review): a transfer that succeeded and returned more than 64 bytes is a success, not a failure
    #[test]
    fn a_nft_contract_that_returns_a_long_value_still_delivers() {
        let mut c = circle();
        completing(&mut c);
        callback(&mut c, PromiseResult::Successful(vec![7u8; 200]));
        let escrow = load_escrow(GRANT).unwrap();
        assert_eq!(escrow.state, EscrowState::Delivered, "the NFT moved, so the payment must stay and the grant must be active");
        assert_eq!(load_grant(GRANT).status, GrantStatus::Active);
        assert_eq!(c.pairs.get(&(1, acc("bob.near"), acc("alice.near"))).map(|p| p.cumulative), Some(10 * CREDIT));
    }

    // TC-CT-075-S02 (security review, needs a decision, see docs/security-review.md F1): nft_on_transfer trusts its caller. Run with --ignored.
    #[test]
    #[ignore = "finding F1 needs a spec decision: the grant must name the NFT contract that may fill its escrow"]
    fn an_account_that_is_not_the_nft_of_the_grant_cannot_fill_the_escrow() {
        let mut c = circle();
        crate::grant::store_grant_for_test(&grant_offered());
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("mallory.near")).block_timestamp(5_000).build());
        // mallory is a plain account, not a NFT contract, and calls the receiver directly with the grantor as previous owner
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = c.nft_on_transfer(acc("alice.near"), acc("alice.near"), "fake".into(), format!("{{\"grant_id\":{GRANT}}}"));
        }));
        assert!(result.is_err(), "the escrow of the grant was filled by an account that never held a token");
        assert!(load_escrow(GRANT).is_none());
    }
}
