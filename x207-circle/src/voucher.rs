//! F2 vouchers: D2/D3, canonical bytes, `register_cumulative`, `register_voucher`.

use near_sdk::json_types::U128;
use near_sdk::{env, near, require, AccountId};

use crate::canonical::{parse_signature, voucher_signing_hash, Voucher};
use crate::clearing::EpochPhase;
use crate::events::{VoucherRegistered, X207Event};
use crate::member::MemberStatus;
use crate::state::{X207Circle, X207CircleExt};
use crate::types::EpochId;

/// D2 and D3 for one `(epoch, payer, payee)` (spec 9.1, prefix `p`):
/// `obligation = cumulative - cleared`; `last_seq` is `0` before the first registration.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PairState {
    pub cumulative: u128,
    pub last_seq: u64,
    pub cleared: u128,
}

/// D2 aggregate per `(epoch, account)` (spec 9.1, prefix `t`).
#[near(serializers = [borsh])]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParticipantTotals {
    pub owed_out: u128,
    pub owed_in: u128,
}

impl PairState {
    /// `cumulative - cleared`; `cleared <= cumulative` always holds (spec 3.6).
    pub fn obligation(&self) -> u128 {
        self.cumulative - self.cleared
    }
}

/// `obligation` and `list_obligations` result (spec section 5).
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObligationView {
    pub epoch: EpochId,
    pub debtor: AccountId,
    pub creditor: AccountId,
    pub cumulative: U128,
    pub last_seq: u64,
    pub cleared: U128,
    pub obligation: U128,
}

impl ObligationView {
    fn new(epoch: EpochId, debtor: AccountId, creditor: AccountId, pair: &PairState) -> Self {
        Self {
            epoch,
            debtor,
            creditor,
            cumulative: U128(pair.cumulative),
            last_seq: pair.last_seq,
            cleared: U128(pair.cleared),
            obligation: U128(pair.obligation()),
        }
    }
}

#[near]
impl X207Circle {
    /// D2 `outstanding_debt`; `0` for an unknown account.
    pub fn outstanding_debt(&self, account_id: AccountId) -> U128 {
        U128(self.outstanding_debt.get(&account_id).copied().unwrap_or(0))
    }

    /// D3 `last_seq` of `(payer, payee, epoch)`; `0` before the first registration.
    pub fn last_seq(&self, payer: AccountId, payee: AccountId, epoch: EpochId) -> u64 {
        self.pairs
            .get(&(epoch, payer, payee))
            .map_or(0, |p| p.last_seq)
    }

    /// D2 and D3 of one pair, or `None` before the first registration.
    pub fn obligation(
        &self,
        epoch: EpochId,
        debtor: AccountId,
        creditor: AccountId,
    ) -> Option<ObligationView> {
        let pair = self.pairs.get(&(epoch, debtor.clone(), creditor.clone()))?;
        Some(ObligationView::new(epoch, debtor, creditor, pair))
    }

    /// Pairs of `epoch` in first-registration order (the D2 snapshot index); `limit` 0, an unknown
    /// epoch, or a `from_index` past the end gives `[]`.
    pub fn list_obligations(
        &self,
        epoch: EpochId,
        from_index: u32,
        limit: u32,
    ) -> Vec<ObligationView> {
        let Some(e) = self.epochs.get(&epoch) else {
            return vec![];
        };
        e.edges
            .iter()
            .skip(from_index as usize)
            .take(limit as usize)
            .filter_map(|(debtor, creditor)| {
                self.obligation(epoch, debtor.clone(), creditor.clone())
            })
            .collect()
    }
}

/// Registration mode (spec 4.2): `cumulative` accepts any `seq > last_seq`; `per_request` (demo)
/// needs `seq = last_seq + 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterMode {
    Cumulative,
    PerRequest,
}

impl RegisterMode {
    pub fn wire(self) -> &'static str {
        match self {
            RegisterMode::Cumulative => "cumulative",
            RegisterMode::PerRequest => "per_request",
        }
    }
}

impl X207Circle {
    /// The member as a registration sees it: a reactivation whose epoch has opened is applied (ADR-30), so the
    /// limit is the reactivated one. Nothing is written here.
    fn active_member(&self, account_id: &AccountId, err: &str) -> crate::member::Member {
        match self.members.get(account_id).map(|m| self.member_now(m)) {
            Some(m) if m.status == MemberStatus::Active => m,
            _ => env::panic_str(&format!("{err}: {account_id}")),
        }
    }

    /// The 12 checks of spec 4.2 in order, then the state change. Nothing is written before the
    /// last check passes (I-06). Shared by `register_cumulative` and `register_voucher`.
    pub fn register(&mut self, voucher: Voucher, signature: String, mode: RegisterMode) -> U128 {
        self.register_with(voucher, signature, mode, None)
    }

    /// `register` for a leg of bundle `bundle` (CT-064): the credit check is the reserved amount of the leg, not the
    /// free headroom, and the registered obligation is booked on the leg.
    pub fn register_with(
        &mut self,
        voucher: Voucher,
        signature: String,
        mode: RegisterMode,
        bundle: Option<u64>,
    ) -> U128 {
        let now = env::block_timestamp();
        // 1-3
        require!(
            voucher.circle == env::current_account_id(),
            format!("ERR_WRONG_CIRCLE: {}", voucher.circle)
        );
        require!(
            voucher.unit == "credit",
            format!("ERR_BAD_UNIT: {}", voucher.unit)
        );
        require!(
            voucher.resource.is_ascii(),
            "ERR_RESOURCE_NOT_ASCII: resource must be ASCII"
        );
        require!(
            voucher.payer != voucher.payee,
            "ERR_SELF_PAYMENT: payer equals payee"
        );
        // 4-5
        let epoch_id = voucher.epoch;
        let epoch = self.epochs.get(&epoch_id);
        let cutoff_at = match epoch {
            Some(e) if epoch_id == self.current_epoch && e.phase == EpochPhase::Open => e.cutoff_at,
            _ => env::panic_str(&format!(
                "ERR_EPOCH_MISMATCH: voucher epoch {epoch_id}, current {}",
                self.current_epoch
            )),
        };
        require!(
            now < cutoff_at,
            format!("ERR_PAST_CUTOFF: cutoff_at {cutoff_at}")
        );
        // 6-8
        let payer = self.active_member(&voucher.payer, "ERR_PAYER_NOT_ACTIVE");
        let payee = self.active_member(&voucher.payee, "ERR_PAYEE_NOT_ACTIVE");
        // CT-095: a freeze stops vouchers only from the epoch after the one in which it took effect
        crate::freeze::assert_not_frozen_for_voucher(&voucher.payer, epoch_id);
        crate::freeze::assert_not_frozen_for_voucher(&voucher.payee, epoch_id);
        // CT-126 (0.0.18): a member account under an open account escrow adds no debt for the buyer to inherit
        crate::account_escrow::assert_not_locked(&voucher.payer);
        for m in [&payer, &payee] {
            if let Some(exit) = m.exit_requested_epoch {
                require!(
                    exit >= epoch_id,
                    format!("ERR_EXITING: {} exit_requested_epoch {exit}", m.account_id)
                );
            }
        }
        let (limit, pubkey) = (payer.limit, payer.pubkey_voucher.clone());
        // 9-10
        let key = (epoch_id, voucher.payer.clone(), voucher.payee.clone());
        require!(
            !crate::escrow::pair_locked(epoch_id, &voucher.payer, &voucher.payee),
            "ERR_ESCROW_PENDING: an escrow payment of this pair is in flight"
        );
        let pair = self.pairs.get(&key).cloned();
        let is_new_pair = pair.is_none();
        let pair = pair.unwrap_or_default();
        require!(
            voucher.seq > pair.last_seq,
            format!(
                "ERR_SEQ_NOT_INCREASING: seq {} last_seq {}",
                voucher.seq, pair.last_seq
            )
        );
        if mode == RegisterMode::PerRequest {
            require!(
                voucher.seq == pair.last_seq + 1,
                format!(
                    "ERR_SEQ_GAP: seq {} last_seq {}",
                    voucher.seq, pair.last_seq
                )
            );
        }
        let cumulative = voucher.cumulative_amount.0;
        require!(
            cumulative >= pair.cumulative,
            format!(
                "ERR_CUMULATIVE_DECREASED: {cumulative} < {}",
                pair.cumulative
            )
        );
        // 11
        let delta = cumulative - pair.cumulative;
        let debt = self
            .outstanding_debt
            .get(&voucher.payer)
            .copied()
            .unwrap_or(0);
        let mut leg_index = None;
        if let Some(bundle_id) = bundle {
            let record = crate::bundle::load_bundle(bundle_id);
            require!(
                record.epoch == epoch_id,
                format!(
                    "ERR_EPOCH_MISMATCH: bundle epoch {}, voucher epoch {epoch_id}",
                    record.epoch
                )
            );
            let Some((index, remaining)) =
                crate::bundle::leg_remaining(&record, &voucher.payer, &voucher.payee)
            else {
                env::panic_str(&format!(
                    "ERR_BUNDLE_LEG: {} -> {} is not a leg of bundle {bundle_id}",
                    voucher.payer, voucher.payee
                ))
            };
            require!(
                delta <= remaining,
                format!("ERR_BUNDLE_LEG_EXCEEDED: delta {delta} remaining on the leg {remaining}")
            );
            leg_index = Some(index);
        } else {
            // headroom held for bundle legs of this epoch is not free
            let headroom = limit
                .saturating_sub(debt)
                .saturating_sub(crate::bundle::reserved(epoch_id, &voucher.payer));
            require!(
                delta <= headroom,
                format!("ERR_INSUFFICIENT_CREDIT: delta {delta} headroom {headroom}")
            );
        }
        // 12
        if crate::evm::is_evm_member(&voucher.payer) {
            // CT-104: an EVM payer signs EIP-712; the recovered address must be the payer id
            require!(
                crate::evm::verify_voucher(&voucher, &signature),
                "ERR_BAD_EVM_SIG: eip712 verification failed"
            );
        } else {
            let verified = parse_signature(&signature)
                .zip(<[u8; 32]>::try_from(&pubkey.as_bytes()[1..]).ok())
                .is_some_and(|(sig, pk)| {
                    env::ed25519_verify(&sig, &voucher_signing_hash(&voucher), &pk)
                });
            require!(verified, "ERR_BAD_SIGNATURE: ed25519 verification failed");
        }

        // State change.
        if let (Some(bundle_id), Some(index)) = (bundle, leg_index) {
            crate::bundle::consume_leg(bundle_id, index, &voucher.payer, delta);
        }
        let new_debt = debt + delta;
        let obligation = cumulative - pair.cleared;
        self.pairs.insert(
            key,
            crate::voucher::PairState {
                cumulative,
                last_seq: voucher.seq,
                cleared: pair.cleared,
            },
        );
        self.outstanding_debt
            .insert(voucher.payer.clone(), new_debt);
        let mut new_participants = 0;
        for (account, out, inn) in [(&voucher.payer, delta, 0), (&voucher.payee, 0, delta)] {
            let tkey = (epoch_id, account.clone());
            let is_new = self.totals.get(&tkey).is_none();
            let mut t = self.totals.get(&tkey).cloned().unwrap_or_default();
            t.owed_out += out;
            t.owed_in += inn;
            self.totals.insert(tkey, t);
            if is_new {
                new_participants += 1;
                self.epochs
                    .get_mut(&epoch_id)
                    .unwrap()
                    .participants
                    .push(account.clone());
            }
        }
        let e = self.epochs.get_mut(&epoch_id).unwrap();
        e.participant_count += new_participants;
        e.total_obligation += delta;
        if is_new_pair {
            e.edges.push((voucher.payer.clone(), voucher.payee.clone()));
            e.edge_count += 1;
        }
        let current = self.current_epoch;
        let params = self.epoch_or_panic(current).params.clone();
        for id in [&voucher.payer, &voucher.payee] {
            let member = self.members.get_mut(id).unwrap();
            member.settle_reactivation(current, &params);
            member.last_active_epoch = epoch_id;
        }
        VoucherRegistered {
            payer: voucher.payer,
            payee: voucher.payee,
            epoch: epoch_id,
            seq: voucher.seq,
            cumulative_amount: U128(cumulative),
            delta: U128(delta),
            obligation: U128(obligation),
            outstanding_debt: U128(new_debt),
            mode: mode.wire().to_string(),
        }
        .emit();
        U128(obligation)
    }
}

#[near]
impl X207Circle {
    /// Default registration (ADR-17): returns the pair obligation after registration.
    pub fn register_cumulative(&mut self, voucher: Voucher, signature: String) -> U128 {
        self.register(voucher, signature, RegisterMode::Cumulative)
    }

    /// F5 (CT-064): registers a voucher that settles a leg of bundle `bundle_id`. The voucher bytes are the usual ones
    /// (cumulative mode); the leg fixes the most the pair may owe in the bundle epoch. Returns the pair obligation.
    pub fn register_in_bundle(
        &mut self,
        bundle_id: u64,
        voucher: Voucher,
        signature: String,
    ) -> U128 {
        self.register_with(
            voucher,
            signature,
            RegisterMode::Cumulative,
            Some(bundle_id),
        )
    }

    /// Demo mode (ADR-26): one voucher per request, `seq` must be `last_seq + 1`. Same checks and
    /// state as `register_cumulative`; returns the pair obligation after registration.
    pub fn register_voucher(&mut self, voucher: Voucher, signature: String) -> U128 {
        self.register(voucher, signature, RegisterMode::PerRequest)
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::serde_json::{self, json};
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{env, testing_env};

    use super::*;
    use crate::params::Params;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn circle() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        X207Circle::with_config(
            acc("dao.near"),
            "agents-main".to_string(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            env::block_timestamp(),
        )
    }

    /// Stands in for registration: one entry in the pair map and one in the epoch edge index.
    fn put(c: &mut X207Circle, epoch: EpochId, payer: &str, payee: &str, pair: PairState) {
        c.pairs.insert((epoch, acc(payer), acc(payee)), pair);
        c.epochs
            .get_mut(&epoch)
            .unwrap()
            .edges
            .push((acc(payer), acc(payee)));
    }

    // TC-CT-011-05
    #[test]
    fn tc_ct_011_05_missing_pair() {
        let c = circle();
        assert_eq!(c.last_seq(acc("gpu.near"), acc("scraper.near"), 1), 0);
        assert_eq!(
            serde_json::to_value(c.obligation(1, acc("gpu.near"), acc("scraper.near"))).unwrap(),
            json!(null)
        );
        assert_eq!(c.outstanding_debt(acc("gpu.near")), U128(0));
    }

    // TC-CT-011-01 (view shape), TC-CT-011-06 (obligation = cumulative - cleared)
    #[test]
    fn tc_ct_011_06_obligation_is_cumulative_minus_cleared() {
        let mut c = circle();
        put(
            &mut c,
            1,
            "scraper.near",
            "gpu.near",
            PairState {
                cumulative: 60_000_000,
                last_seq: 3,
                cleared: 25_000_000,
            },
        );
        let v =
            serde_json::to_value(c.obligation(1, acc("scraper.near"), acc("gpu.near"))).unwrap();
        assert_eq!(
            v,
            json!({"epoch":1,"debtor":"scraper.near","creditor":"gpu.near","cumulative":"60000000",
                   "last_seq":3,"cleared":"25000000","obligation":"35000000"})
        );
        assert_eq!(c.last_seq(acc("scraper.near"), acc("gpu.near"), 1), 3);
    }

    // TC-CT-011-02, TC-CT-011-03: one entry per pair per epoch, epochs independent
    #[test]
    fn tc_ct_011_03_epochs_are_separate_entries() {
        let mut c = circle();
        c.epochs.insert(
            2,
            crate::clearing::Epoch::open(2, 2_000, 9_000, Params::default()),
        );
        put(
            &mut c,
            1,
            "translator.near",
            "scraper.near",
            PairState {
                cumulative: 25_000_000,
                last_seq: 2,
                cleared: 0,
            },
        );
        put(
            &mut c,
            2,
            "translator.near",
            "scraper.near",
            PairState {
                cumulative: 5_000_000,
                last_seq: 1,
                cleared: 0,
            },
        );
        assert_eq!(c.list_obligations(1, 0, 10).len(), 1);
        assert_eq!(c.list_obligations(2, 0, 10).len(), 1);
        assert_eq!(
            c.obligation(1, acc("translator.near"), acc("scraper.near"))
                .unwrap()
                .cumulative,
            U128(25_000_000)
        );
        assert_eq!(
            c.obligation(2, acc("translator.near"), acc("scraper.near"))
                .unwrap()
                .cumulative,
            U128(5_000_000)
        );
        assert_eq!(
            c.last_seq(acc("translator.near"), acc("scraper.near"), 2),
            1
        );
    }

    // TC-CT-011-04 (view reads the stored per-account total)
    #[test]
    fn tc_ct_011_04_outstanding_debt_view() {
        let mut c = circle();
        c.outstanding_debt
            .insert(acc("translator.near"), 55_000_000);
        assert_eq!(c.outstanding_debt(acc("translator.near")), U128(55_000_000));
    }

    // TC-CT-011-07
    #[test]
    fn tc_ct_011_07_list_obligations_pages() {
        let mut c = circle();
        for i in 0..6u64 {
            put(
                &mut c,
                1,
                &format!("p{i}.near"),
                "q.near",
                PairState {
                    cumulative: 1,
                    last_seq: i + 1,
                    cleared: 0,
                },
            );
        }
        assert_eq!(c.list_obligations(1, 0, 4).len(), 4);
        let tail = c.list_obligations(1, 4, 4);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].debtor, acc("p4.near"));
        assert!(c.list_obligations(1, 6, 4).is_empty());
        assert!(c.list_obligations(1, 0, 0).is_empty());
        assert!(c.list_obligations(9, 0, 4).is_empty());
    }
}

#[cfg(test)]
mod register_tests {
    use ed25519_dalek::{Signer, SigningKey};
    use near_sdk::json_types::U128;
    use near_sdk::serde_json::{self, json};
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::{testing_env, AccountId, CurveType, PublicKey};

    use super::*;
    use crate::member::Member;
    use crate::params::Params;

    const CREDIT: u128 = 1_000_000;
    const SEC: u64 = 1_000_000_000;
    const CIRCLE: &str = "x207-circle.near";

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn signing_key(name: &str) -> SigningKey {
        let mut seed = [7u8; 32];
        seed[..name.len().min(32)].copy_from_slice(&name.as_bytes()[..name.len().min(32)]);
        SigningKey::from_bytes(&seed)
    }

    fn at(ns: u64) {
        testing_env!(VMContextBuilder::new()
            .current_account_id(acc(CIRCLE))
            .block_timestamp(ns)
            .build());
    }

    fn add(c: &mut X207Circle, name: &str, status: MemberStatus, limit: u128) {
        let id = acc(&format!("{name}.near"));
        let pk = PublicKey::from_parts(
            CurveType::ED25519,
            signing_key(name).verifying_key().to_bytes().to_vec(),
        )
        .unwrap();
        c.members.insert(
            id.clone(),
            Member {
                account_id: id.clone(),
                pubkey_voucher: pk,
                terms_hash: "a".repeat(64),
                stake: 100 * CREDIT,
                limit,
                status,
                admitted: false,
                joined_at: 0,
                settled_history: 0,
                last_active_epoch: 0,
                exit_requested_epoch: None,
                withdraw_pending: false,
                reactivate_epoch: None,
            },
        );
        c.member_index.push(id);
    }

    /// translator, scraper, gpu active with limit 80; `pending` is pending.
    fn circle() -> X207Circle {
        at(1_000);
        let mut c = X207Circle::with_config(
            acc("dao.near"),
            "agents-main".into(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            1_000,
        );
        for n in ["translator", "scraper", "gpu"] {
            add(&mut c, n, MemberStatus::Active, 80 * CREDIT);
        }
        add(&mut c, "pending", MemberStatus::Pending, 0);
        c
    }

    fn v(payer: &str, payee: &str, epoch: u64, seq: u64, cumulative: u128) -> Voucher {
        Voucher {
            circle: acc(CIRCLE),
            payer: acc(&format!("{payer}.near")),
            payee: acc(&format!("{payee}.near")),
            epoch,
            seq,
            cumulative_amount: U128(cumulative),
            unit: "credit".into(),
            resource: "https://scraper.example/api".into(),
        }
    }

    fn sig_with(key_of: &str, voucher: &Voucher) -> String {
        let s = signing_key(key_of)
            .sign(&voucher_signing_hash(voucher))
            .to_bytes();
        format!("ed25519:{}", near_sdk::bs58::encode(s).into_string())
    }

    fn reg(c: &mut X207Circle, voucher: Voucher) -> U128 {
        let sig = sig_with(voucher.payer.as_str().trim_end_matches(".near"), &voucher);
        c.register_cumulative(voucher, sig)
    }

    fn last_event() -> serde_json::Value {
        let line = get_logs()
            .into_iter()
            .rev()
            .find(|l| l.contains("voucher_registered"))
            .unwrap();
        serde_json::from_str::<serde_json::Value>(line.trim_start_matches("EVENT_JSON:")).unwrap()
            ["data"][0]
            .clone()
    }

    #[test]
    fn tc_ct_020_01_first_registration() {
        let mut c = circle();
        assert_eq!(
            reg(&mut c, v("translator", "scraper", 1, 1, 10 * CREDIT)),
            U128(10 * CREDIT)
        );
        let ob = c
            .obligation(1, acc("translator.near"), acc("scraper.near"))
            .unwrap();
        assert_eq!((ob.obligation, ob.last_seq), (U128(10 * CREDIT), 1));
        assert_eq!(
            c.outstanding_debt(acc("translator.near")),
            U128(10 * CREDIT)
        );
        assert_eq!(
            last_event(),
            json!({"payer":"translator.near","payee":"scraper.near","epoch":1,"seq":1,"cumulative_amount":"10000000",
                   "delta":"10000000","obligation":"10000000","outstanding_debt":"10000000","mode":"cumulative"})
        );
        assert_eq!(
            c.member(acc("translator.near")).unwrap().last_active_epoch,
            1
        );
        assert_eq!(c.member(acc("scraper.near")).unwrap().last_active_epoch, 1);
        let e = c.get_epoch(Some(1)).unwrap();
        assert_eq!(
            (e.edge_count, e.participant_count, e.total_obligation),
            (1, 2, U128(10 * CREDIT))
        );
    }

    #[test]
    fn tc_ct_020_02_to_04_increase_zero_delta_and_retry() {
        let mut c = circle();
        reg(&mut c, v("translator", "scraper", 1, 1, 10 * CREDIT));
        assert_eq!(
            reg(&mut c, v("translator", "scraper", 1, 5, 25 * CREDIT)),
            U128(25 * CREDIT)
        );
        assert_eq!(last_event()["delta"], json!("15000000"));
        assert_eq!(
            c.last_seq(acc("translator.near"), acc("scraper.near"), 1),
            5
        );
        assert_eq!(
            c.outstanding_debt(acc("translator.near")),
            U128(25 * CREDIT)
        );
        reg(&mut c, v("translator", "scraper", 1, 6, 25 * CREDIT));
        assert_eq!(last_event()["delta"], json!("0"));
        assert_eq!(
            c.last_seq(acc("translator.near"), acc("scraper.near"), 1),
            6
        );
        assert_eq!(
            c.obligation(1, acc("translator.near"), acc("scraper.near"))
                .unwrap()
                .obligation,
            U128(25 * CREDIT)
        );
        // still one edge and two participants
        let e = c.get_epoch(Some(1)).unwrap();
        assert_eq!((e.edge_count, e.participant_count), (1, 2));
    }

    fn setup_25() -> X207Circle {
        let mut c = circle();
        reg(&mut c, v("translator", "scraper", 1, 6, 25 * CREDIT));
        c
    }

    #[test]
    #[should_panic(expected = "ERR_SEQ_NOT_INCREASING")]
    fn tc_ct_020_04_retry_same_seq_rejected() {
        reg(
            &mut setup_25(),
            v("translator", "scraper", 1, 6, 25 * CREDIT),
        );
    }

    #[test]
    #[should_panic(expected = "ERR_SEQ_NOT_INCREASING")]
    fn tc_ct_020_05_lower_seq_rejected() {
        reg(
            &mut setup_25(),
            v("translator", "scraper", 1, 4, 30 * CREDIT),
        );
    }

    #[test]
    #[should_panic(expected = "ERR_CUMULATIVE_DECREASED")]
    fn tc_ct_020_06_cumulative_decrease_rejected() {
        reg(
            &mut setup_25(),
            v("translator", "scraper", 1, 7, 24 * CREDIT),
        );
    }

    #[test]
    fn tc_ct_020_07_exactly_at_headroom() {
        let mut c = circle();
        reg(&mut c, v("translator", "scraper", 1, 1, 80 * CREDIT));
        assert_eq!(
            c.outstanding_debt(acc("translator.near")),
            U128(80 * CREDIT)
        );
    }

    #[test]
    #[should_panic(expected = "ERR_INSUFFICIENT_CREDIT")]
    fn tc_ct_020_08_headroom_plus_one_rejected() {
        let mut c = circle();
        reg(&mut c, v("translator", "scraper", 1, 1, 80 * CREDIT));
        reg(&mut c, v("translator", "gpu", 1, 1, 1));
    }

    #[test]
    #[should_panic(expected = "ERR_INSUFFICIENT_CREDIT")]
    fn tc_ct_020_09_headroom_across_payees() {
        let mut c = circle();
        reg(&mut c, v("translator", "scraper", 1, 1, 50 * CREDIT));
        reg(&mut c, v("translator", "gpu", 1, 1, 30 * CREDIT));
        reg(&mut c, v("translator", "gpu", 1, 2, 30 * CREDIT + 1));
    }

    #[test]
    #[should_panic(expected = "ERR_WRONG_CIRCLE")]
    fn tc_ct_020_10_wrong_circle() {
        let mut x = v("translator", "scraper", 1, 1, CREDIT);
        x.circle = acc("other.test.near");
        reg(&mut circle(), x);
    }

    #[test]
    #[should_panic(expected = "ERR_BAD_UNIT")]
    fn tc_ct_020_11_wrong_unit() {
        let mut x = v("translator", "scraper", 1, 1, CREDIT);
        x.unit = "usd".into();
        reg(&mut circle(), x);
    }

    #[test]
    #[should_panic(expected = "ERR_RESOURCE_NOT_ASCII")]
    fn tc_ct_020_12_resource_not_ascii() {
        let mut x = v("translator", "scraper", 1, 1, CREDIT);
        x.resource = "https://x.example/caf\u{e9}".into();
        reg(&mut circle(), x);
    }

    #[test]
    #[should_panic(expected = "ERR_SELF_PAYMENT")]
    fn tc_ct_020_13_self_payment() {
        reg(&mut circle(), v("translator", "translator", 1, 1, CREDIT));
    }

    #[test]
    #[should_panic(expected = "ERR_EPOCH_MISMATCH")]
    fn tc_ct_020_14_epoch_ahead() {
        reg(&mut circle(), v("translator", "scraper", 2, 1, CREDIT));
    }

    #[test]
    #[should_panic(expected = "ERR_EPOCH_MISMATCH")]
    fn tc_ct_020_14_epoch_zero() {
        reg(&mut circle(), v("translator", "scraper", 0, 1, CREDIT));
    }

    #[test]
    fn tc_ct_020_15_one_ns_before_cutoff_ok() {
        let mut c = circle();
        at(1_000 + 3600 * SEC - 1);
        reg(&mut c, v("translator", "scraper", 1, 1, CREDIT));
    }

    #[test]
    #[should_panic(expected = "ERR_PAST_CUTOFF")]
    fn tc_ct_020_15_at_cutoff_rejected() {
        let mut c = circle();
        at(1_000 + 3600 * SEC);
        reg(&mut c, v("translator", "scraper", 1, 1, CREDIT));
    }

    #[test]
    #[should_panic(expected = "ERR_PAYER_NOT_ACTIVE")]
    fn tc_ct_020_16_payer_pending() {
        reg(&mut circle(), v("pending", "scraper", 1, 1, 0));
    }

    #[test]
    #[should_panic(expected = "ERR_PAYER_NOT_ACTIVE")]
    fn tc_ct_020_16_payer_not_member() {
        let x = v("outsider", "scraper", 1, 1, 0);
        let sig = sig_with("translator", &x);
        circle().register_cumulative(x, sig);
    }

    #[test]
    #[should_panic(expected = "ERR_PAYEE_NOT_ACTIVE")]
    fn tc_ct_020_17_payee_pending() {
        reg(&mut circle(), v("translator", "pending", 1, 1, CREDIT));
    }

    #[test]
    #[should_panic(expected = "ERR_PAYER_NOT_ACTIVE")]
    fn tc_ct_020_18_payer_suspended() {
        let mut c = circle();
        c.members.get_mut(&acc("scraper.near")).unwrap().status = MemberStatus::Suspended;
        reg(&mut c, v("scraper", "gpu", 1, 1, CREDIT));
    }

    #[test]
    #[should_panic(expected = "ERR_BAD_SIGNATURE")]
    fn tc_ct_020_19_signed_with_wrong_key() {
        let x = v("translator", "scraper", 1, 1, CREDIT);
        let sig = sig_with("account-access-key", &x);
        circle().register_cumulative(x, sig);
    }

    #[test]
    fn tc_ct_020_20_tampered_fields_rejected() {
        let signed = v("translator", "scraper", 1, 1, 10 * CREDIT);
        let sig = sig_with("translator", &signed);
        let tampered = [
            Voucher {
                cumulative_amount: U128(11 * CREDIT),
                ..signed.clone()
            },
            Voucher {
                payee: acc("gpu.near"),
                ..signed.clone()
            },
            Voucher {
                resource: "https://evil.example".into(),
                ..signed.clone()
            },
            Voucher {
                seq: 2,
                ..signed.clone()
            },
        ];
        for t in tampered {
            let mut c = circle();
            let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                c.register_cumulative(t, sig.clone())
            }));
            let msg = *err
                .unwrap_err()
                .downcast::<String>()
                .unwrap_or_else(|e| Box::new(e.downcast_ref::<&str>().unwrap().to_string()));
            assert!(msg.contains("ERR_BAD_SIGNATURE"), "{msg}");
            assert_eq!(c.outstanding_debt(acc("translator.near")), U128(0));
        }
    }

    #[test]
    #[should_panic(expected = "ERR_BAD_SIGNATURE")]
    fn tc_ct_020_21_malformed_signature_string() {
        circle().register_cumulative(
            v("translator", "scraper", 1, 1, CREDIT),
            "ed25519:abc".into(),
        );
    }

    #[test]
    #[should_panic(expected = "ERR_WRONG_CIRCLE")]
    fn tc_ct_020_22_cheap_checks_before_signature_wrong_circle() {
        let mut x = v("translator", "scraper", 1, 1, CREDIT);
        x.circle = acc("other.test.near");
        circle().register_cumulative(x, "ed25519:abc".into());
    }

    #[test]
    #[should_panic(expected = "ERR_INSUFFICIENT_CREDIT")]
    fn tc_ct_020_22_credit_before_signature() {
        circle().register_cumulative(
            v("translator", "scraper", 1, 1, 81 * CREDIT),
            "ed25519:abc".into(),
        );
    }

    #[test]
    #[should_panic(expected = "ERR_EXITING")]
    fn exiting_member_rejected_for_later_epoch() {
        let mut c = circle();
        c.members
            .get_mut(&acc("scraper.near"))
            .unwrap()
            .exit_requested_epoch = Some(0);
        reg(&mut c, v("translator", "scraper", 1, 1, CREDIT));
    }

    #[test]
    fn exiting_member_still_registers_in_exit_epoch() {
        let mut c = circle();
        c.members
            .get_mut(&acc("scraper.near"))
            .unwrap()
            .exit_requested_epoch = Some(1);
        reg(&mut c, v("translator", "scraper", 1, 1, CREDIT));
    }

    #[test]
    fn state_unchanged_after_rejected_registration() {
        let mut c = setup_25();
        let before = (
            c.get_epoch(Some(1)).unwrap(),
            c.outstanding_debt(acc("translator.near")),
        );
        let x = v("translator", "scraper", 1, 7, 24 * CREDIT);
        let sig = sig_with("translator", &x);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.register_cumulative(x, sig)
        }));
        assert!(r.is_err());
        assert_eq!(
            before,
            (
                c.get_epoch(Some(1)).unwrap(),
                c.outstanding_debt(acc("translator.near"))
            )
        );
    }

    fn reg_demo(c: &mut X207Circle, voucher: Voucher) -> U128 {
        let sig = sig_with(voucher.payer.as_str().trim_end_matches(".near"), &voucher);
        c.register_voucher(voucher, sig)
    }

    fn panic_code(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("expected a panic");
        let msg = err
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        // The mocked host wraps `env::panic_str` text in a `HostError`, so pick the code out of the message.
        let start = msg
            .find("ERR_")
            .unwrap_or_else(|| panic!("no ERR_ code in: {msg}"));
        msg[start..]
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || *c == '_')
            .collect()
    }

    // TC-CT-022-01
    #[test]
    fn tc_ct_022_01_first_per_request_registration() {
        let mut c = circle();
        assert_eq!(
            reg_demo(&mut c, v("translator", "scraper", 1, 1, 20_000)),
            U128(20_000)
        );
        let e = last_event();
        assert_eq!(e["mode"], json!("per_request"));
        assert_eq!(e["delta"], json!("20000"));
        assert_eq!(c.outstanding_debt(acc("translator.near")), U128(20_000));
    }

    // TC-CT-022-02
    #[test]
    fn tc_ct_022_02_next_seq() {
        let mut c = circle();
        reg_demo(&mut c, v("translator", "scraper", 1, 1, 20_000));
        assert_eq!(
            reg_demo(&mut c, v("translator", "scraper", 1, 2, 40_000)),
            U128(40_000)
        );
        assert_eq!(last_event()["delta"], json!("20000"));
    }

    // TC-CT-022-03
    #[test]
    fn tc_ct_022_03_seq_gap() {
        let mut c = circle();
        reg_demo(&mut c, v("translator", "scraper", 1, 1, 20_000));
        reg_demo(&mut c, v("translator", "scraper", 1, 2, 40_000));
        let code = panic_code(std::panic::AssertUnwindSafe(|| {
            reg_demo(&mut c, v("translator", "scraper", 1, 4, 80_000));
        }));
        assert_eq!(code, "ERR_SEQ_GAP");
        assert_eq!(
            c.last_seq(acc("translator.near"), acc("scraper.near"), 1),
            2
        );
    }

    // TC-CT-022-04
    #[test]
    fn tc_ct_022_04_same_seq_not_increasing() {
        let mut c = circle();
        reg_demo(&mut c, v("translator", "scraper", 1, 1, 20_000));
        reg_demo(&mut c, v("translator", "scraper", 1, 2, 40_000));
        let code = panic_code(std::panic::AssertUnwindSafe(|| {
            reg_demo(&mut c, v("translator", "scraper", 1, 2, 40_000));
        }));
        assert_eq!(code, "ERR_SEQ_NOT_INCREASING");
    }

    /// TC-CT-022-05: one test per negative fixture and mode, so each gets its own mocked storage
    /// (`testing_env!` keeps storage inside one test). Both modes must give the same error code.
    fn negative_case(
        demo: bool,
        prior: Option<(u64, u128)>,
        mutate: fn(&mut Voucher),
        signer: &str,
        now: u64,
        expected: &str,
    ) {
        let mut c = circle();
        if let Some((seq, cumulative)) = prior {
            reg(&mut c, v("translator", "scraper", 1, seq, cumulative));
        }
        at(now);
        let mut voucher = v(
            "translator",
            "scraper",
            1,
            if prior.is_some() { 2 } else { 1 },
            CREDIT,
        );
        mutate(&mut voucher);
        let sig = sig_with(signer, &voucher);
        let code = panic_code(std::panic::AssertUnwindSafe(|| {
            if demo {
                c.register_voucher(voucher, sig);
            } else {
                c.register_cumulative(voucher, sig);
            }
        }));
        assert_eq!(code, expected);
    }

    macro_rules! negative_cases {
        ($($name:ident: $prior:expr, $mutate:expr, $signer:expr, $now:expr => $code:expr;)*) => {
            mod tc_ct_022_05 {
                use super::*;
                $(
                    mod $name {
                        use super::*;
                        #[test]
                        fn register_cumulative() { negative_case(false, $prior, $mutate, $signer, $now, $code); }
                        #[test]
                        fn register_voucher() { negative_case(true, $prior, $mutate, $signer, $now, $code); }
                    }
                )*
            }
        };
    }

    negative_cases! {
        wrong_circle: None, |x| x.circle = acc("other.test.near"), "translator", 1_000 => "ERR_WRONG_CIRCLE";
        bad_unit: None, |x| x.unit = "usd".into(), "translator", 1_000 => "ERR_BAD_UNIT";
        non_ascii_resource: None, |x| x.resource = "caf\u{e9}".into(), "translator", 1_000 => "ERR_RESOURCE_NOT_ASCII";
        self_payment: None, |x| x.payee = x.payer.clone(), "translator", 1_000 => "ERR_SELF_PAYMENT";
        epoch_ahead: None, |x| x.epoch = 2, "translator", 1_000 => "ERR_EPOCH_MISMATCH";
        past_cutoff: None, |_| {}, "translator", 1_000 + 3600 * SEC => "ERR_PAST_CUTOFF";
        payer_pending: None, |x| x.payer = acc("pending.near"), "pending", 1_000 => "ERR_PAYER_NOT_ACTIVE";
        payee_pending: None, |x| x.payee = acc("pending.near"), "translator", 1_000 => "ERR_PAYEE_NOT_ACTIVE";
        lower_seq: Some((5, 25 * CREDIT)), |x| x.seq = 4, "translator", 1_000 => "ERR_SEQ_NOT_INCREASING";
        same_seq: Some((1, 25 * CREDIT)), |x| x.seq = 1, "translator", 1_000 => "ERR_SEQ_NOT_INCREASING";
        cumulative_decrease: Some((1, 25 * CREDIT)), |x| x.cumulative_amount = U128(24 * CREDIT), "translator", 1_000 => "ERR_CUMULATIVE_DECREASED";
        headroom_exceeded: None, |x| x.cumulative_amount = U128(81 * CREDIT), "translator", 1_000 => "ERR_INSUFFICIENT_CREDIT";
        wrong_signer: None, |_| {}, "gpu", 1_000 => "ERR_BAD_SIGNATURE";
    }

    // TC-CT-022-06 (K-23): both modes share the pair state
    #[test]
    fn tc_ct_022_06_mixed_modes_share_pair_state() {
        let mut c = circle();
        reg(&mut c, v("translator", "scraper", 1, 3, 30_000));
        assert_eq!(last_event()["mode"], json!("cumulative"));
        assert_eq!(
            reg_demo(&mut c, v("translator", "scraper", 1, 4, 50_000)),
            U128(50_000)
        );
        assert_eq!(last_event()["mode"], json!("per_request"));
        assert_eq!(last_event()["delta"], json!("20000"));
        assert_eq!(
            c.last_seq(acc("translator.near"), acc("scraper.near"), 1),
            4
        );
    }

    // TC-CT-021-07
    #[test]
    fn tc_ct_021_07_last_seq_other_epoch_is_zero() {
        let mut c = circle();
        reg(&mut c, v("translator", "scraper", 1, 5, CREDIT));
        assert_eq!(
            c.last_seq(acc("translator.near"), acc("scraper.near"), 1),
            5
        );
        assert_eq!(
            c.last_seq(acc("translator.near"), acc("scraper.near"), 2),
            0
        );
    }
    type Snapshot = (u128, u64, u128, u128);

    fn snapshot(c: &X207Circle, payer: &str, payee: &str) -> Snapshot {
        let (payer, payee) = (acc(&format!("{payer}.near")), acc(&format!("{payee}.near")));
        let pair = c.obligation(1, payer.clone(), payee.clone());
        let e = c.get_epoch(Some(1)).unwrap();
        (
            pair.as_ref().map_or(0, |p| p.obligation.0),
            c.last_seq(payer.clone(), payee, 1),
            c.outstanding_debt(payer).0,
            e.total_obligation.0,
        )
    }

    // TC-CT-020-25: every negative case of TC-CT-020-04 to TC-CT-020-21 leaves the state as it was (I-06)
    #[test]
    fn tc_ct_020_25_failed_registration_leaves_no_trace() {
        let good = v("translator", "scraper", 1, 6, 25 * CREDIT);
        let bad_sig = sig_with("account-access-key", &good);
        let cases: Vec<(Voucher, Option<String>)> = vec![
            (v("translator", "scraper", 1, 6, 25 * CREDIT), None),
            (v("translator", "scraper", 1, 4, 30 * CREDIT), None),
            (v("translator", "scraper", 1, 7, 24 * CREDIT), None),
            (v("translator", "scraper", 1, 7, 81 * CREDIT), None),
            (v("translator", "gpu", 1, 1, 56 * CREDIT), None),
            (
                Voucher {
                    circle: acc("other.test.near"),
                    ..v("translator", "scraper", 1, 7, 26 * CREDIT)
                },
                None,
            ),
            (
                Voucher {
                    unit: "eur".into(),
                    ..v("translator", "scraper", 1, 7, 26 * CREDIT)
                },
                None,
            ),
            (
                Voucher {
                    resource: "caf\u{e9}".into(),
                    ..v("translator", "scraper", 1, 7, 26 * CREDIT)
                },
                None,
            ),
            (v("translator", "translator", 1, 7, 26 * CREDIT), None),
            (v("translator", "scraper", 2, 7, 26 * CREDIT), None),
            (v("pending", "scraper", 1, 1, CREDIT), None),
            (v("translator", "pending", 1, 1, CREDIT), None),
            (good.clone(), Some(bad_sig)),
            (good.clone(), Some("ed25519:abc".to_string())),
        ];
        let mut c = setup_25();
        for (i, (voucher, sig)) in cases.into_iter().enumerate() {
            let before = (
                snapshot(&c, "translator", "scraper"),
                snapshot(&c, "translator", "gpu"),
                c.get_epoch(Some(1)).unwrap().edge_count,
            );
            let payer = voucher.payer.as_str().trim_end_matches(".near").to_string();
            let sig = sig.unwrap_or_else(|| sig_with(&payer, &voucher));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                c.register_cumulative(voucher, sig)
            }));
            assert!(result.is_err(), "case {i} was accepted");
            let after = (
                snapshot(&c, "translator", "scraper"),
                snapshot(&c, "translator", "gpu"),
                c.get_epoch(Some(1)).unwrap().edge_count,
            );
            assert_eq!(before, after, "case {i} left a trace");
        }
    }

    // TC-CT-020-26: a pseudo-random mix of valid and invalid vouchers over several pairs keeps I-03 and I-05
    #[test]
    fn tc_ct_020_26_random_voucher_order_keeps_the_invariants() {
        use std::collections::BTreeMap;
        let names = ["translator", "scraper", "gpu"];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let mut c = circle();
        let mut model: BTreeMap<(usize, usize), (u64, u128)> = BTreeMap::new();
        let mut accepted = 0;
        for _ in 0..400 {
            let (a, b) = (next(3) as usize, next(3) as usize);
            let (seq, cumulative) = (1 + next(12), u128::from(next(30_000_001)));
            let voucher = v(names[a], names[b], 1, seq, cumulative);
            let sig = sig_with(names[a], &voucher);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                c.register_cumulative(voucher, sig)
            }));
            let (last_seq, last_cum) = model.get(&(a, b)).copied().unwrap_or((0, 0));
            if result.is_ok() {
                accepted += 1;
                assert!(
                    a != b && seq > last_seq && cumulative >= last_cum,
                    "accepted a bad voucher {a}->{b} seq {seq} cum {cumulative}"
                );
                model.insert((a, b), (seq, cumulative));
            }
            for (&(x, y), &(s, cum)) in &model {
                let pair = c
                    .obligation(
                        1,
                        acc(&format!("{}.near", names[x])),
                        acc(&format!("{}.near", names[y])),
                    )
                    .unwrap();
                assert_eq!(
                    (pair.last_seq, pair.obligation.0),
                    (s, cum),
                    "I-05 {x}->{y}"
                );
            }
            for (i, n) in names.iter().enumerate() {
                let owed: u128 = model
                    .iter()
                    .filter(|((x, _), _)| *x == i)
                    .map(|(_, (_, cum))| cum)
                    .sum();
                assert_eq!(c.outstanding_debt(acc(&format!("{n}.near"))).0, owed);
                assert!(owed <= 80 * CREDIT, "I-03 {n} owes {owed}");
            }
        }
        assert!(accepted > 10, "the mix accepted only {accepted} vouchers");
    }
}
