//! F5 barter bundle (CT-061..064, ADR-38): D8. A bundle is a set of legs `(payer, payee, max_amount)` that all
//! participants signed through its `terms_hash` (`sdk/spec/barter-format.md`). `commit_bundle` reserves headroom per
//! leg for the epoch; `register_in_bundle` registers the voucher of a leg against that reservation. Intents are only
//! hashes. Everything lives under its own storage keys (`StorageKey::Intents`, `Bundles`, `BundleReserves`, and two raw
//! keys), so the contract root does not change layout.

use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::json_types::{Base58CryptoHash, U128, U64};
use near_sdk::store::LookupMap;
use near_sdk::{env, near, require, AccountId};

use crate::canonical::{nep413_signing_hash, parse_signature};
use crate::clearing::EpochPhase;
use crate::events::{BundleCommitted, IntentPosted, X207Event};
use crate::member::MemberStatus;
use crate::state::{StorageKey, X207Circle, X207CircleExt};
use crate::types::EpochId;

const SEQ_KEY: &[u8] = b"x207_bundle_seq";
/// Intents one member may have open at the same time (security review F2). `remove_intent` frees a slot.
pub const MAX_OPEN_INTENTS_PER_MEMBER: u32 = 64;
/// Bundles one participant may be in per epoch (security review F2). The limit is per participant, so two accounts cannot use up the slots of a third.
pub const MAX_BUNDLES_PER_MEMBER_PER_EPOCH: u32 = 16;
/// Legs one bundle may have. Measured in near-sandbox (`ct086_gas_limits`): 64 legs cost 75 Tgas at most (a cycle through 64 members); without a cap, about 240 legs
/// fill the 16 KiB of a log message with `bundle_committed` and the transaction fails with a host error instead of `ERR_BAD_BUNDLE`.
pub const MAX_BUNDLE_LEGS: usize = 64;
const HASH_KEY_PREFIX: &[u8] = b"x207_bundle_hash:";

/// One leg: `payer` may owe `payee` at most `max_amount` in the epoch of the bundle.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleLeg {
    pub payer: AccountId,
    pub payee: AccountId,
    pub max_amount: U128,
}

/// Argument of `commit_bundle`. `terms_hash` is computed by the contract from the circle, the epoch and the legs.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleInput {
    pub epoch: EpochId,
    pub legs: Vec<BundleLeg>,
    /// F9 (CT-082): the leg that is the seeker's share. It is part of the signed terms, so every participant
    /// agrees to it. Absent: no leg is a seeker share and `max_search_share_bps` does not apply.
    #[serde(default)]
    pub search_leg: Option<SearchLeg>,
}

/// Names the seeker leg by its pair; the pair is unique inside a bundle.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchLeg {
    pub payer: AccountId,
    pub payee: AccountId,
}

/// Signature of one participant over the `terms_hash`: `ed25519:<base58 64 bytes>` with the voucher key.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleSignature {
    pub account_id: AccountId,
    pub signature: String,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BundleStatus {
    Active,
}

/// D8 record.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
#[borsh(crate = "near_sdk::borsh")]
pub struct Bundle {
    pub terms_hash: [u8; 32],
    pub epoch: EpochId,
    pub legs: Vec<BundleLeg>,
    /// Per leg: obligation registered against it so far.
    pub registered: Vec<u128>,
    pub solver: AccountId,
    pub status: BundleStatus,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
#[borsh(crate = "near_sdk::borsh")]
pub struct IntentRecord {
    pub account_id: AccountId,
    pub posted_at: u64,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleLegView {
    pub payer: AccountId,
    pub payee: AccountId,
    pub max_amount: U128,
    /// Obligation registered against this leg (CT-064).
    pub registered: U128,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleView {
    pub bundle_id: u64,
    pub terms_hash: String,
    pub epoch: EpochId,
    pub legs: Vec<BundleLegView>,
    pub status: BundleStatus,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentView {
    pub intent_hash: Base58CryptoHash,
    pub account_id: AccountId,
    pub posted_at: U64,
}

pub(crate) fn bundles() -> LookupMap<u64, Bundle> {
    LookupMap::new(StorageKey::Bundles)
}

fn intents() -> LookupMap<[u8; 32], IntentRecord> {
    LookupMap::new(StorageKey::Intents)
}

fn intent_counts() -> LookupMap<AccountId, u32> {
    LookupMap::new(StorageKey::IntentCounts)
}

pub(crate) fn bundle_counts() -> LookupMap<(EpochId, AccountId), u32> {
    LookupMap::new(StorageKey::BundleCounts)
}

pub(crate) fn reserves() -> LookupMap<(EpochId, AccountId), u128> {
    LookupMap::new(StorageKey::BundleReserves)
}

/// Headroom reserved for bundle legs of `account` in `epoch`.
pub fn reserved(epoch: EpochId, account: &AccountId) -> u128 {
    reserves().get(&(epoch, account.clone())).copied().unwrap_or(0)
}

fn set_reserved(epoch: EpochId, account: &AccountId, value: u128) {
    let mut map = reserves();
    map.insert((epoch, account.clone()), value);
    map.flush();
}

fn epoch_ids_key(epoch: EpochId) -> Vec<u8> {
    [b"x207_bundle_ids:".as_slice(), &epoch.to_le_bytes()].concat()
}

/// Ids of the bundles committed in `epoch`, oldest first: raw storage, little-endian `u64`s, so `prune_epoch` finds the bundles of an epoch.
pub(crate) fn epoch_bundle_ids(epoch: EpochId) -> Vec<u64> {
    env::storage_read(&epoch_ids_key(epoch)).map_or_else(Vec::new, |b| b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes"))).collect())
}

pub(crate) fn set_epoch_bundle_ids(epoch: EpochId, ids: &[u64]) {
    if ids.is_empty() {
        env::storage_remove(&epoch_ids_key(epoch));
    } else {
        env::storage_write(&epoch_ids_key(epoch), &ids.iter().flat_map(|i| i.to_le_bytes()).collect::<Vec<u8>>());
    }
}

pub(crate) fn hash_key(terms_hash: &[u8; 32]) -> Vec<u8> {
    [HASH_KEY_PREFIX, terms_hash.as_slice()].concat()
}

/// Canonical JSON of the terms (legs sorted by `(payer, payee)`), spec `sdk/spec/barter-format.md`.
pub fn terms_message(circle: &AccountId, epoch: EpochId, legs: &[BundleLeg], search_leg: Option<&SearchLeg>) -> String {
    let mut sorted: Vec<&BundleLeg> = legs.iter().collect();
    sorted.sort_by(|a, b| (a.payer.as_str(), a.payee.as_str()).cmp(&(b.payer.as_str(), b.payee.as_str())));
    let legs: Vec<String> = sorted
        .iter()
        .map(|l| format!(r#"{{"payer":"{}","payee":"{}","max_amount":"{}"}}"#, l.payer, l.payee, l.max_amount.0))
        .collect();
    let search = search_leg.map(|l| format!(r#","search_leg":{{"payer":"{}","payee":"{}"}}"#, l.payer, l.payee)).unwrap_or_default();
    format!(r#"{{"circle":"{circle}","epoch":{epoch},"legs":[{}]{search}}}"#, legs.join(","))
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Bundle {
    fn view(&self, bundle_id: u64) -> BundleView {
        BundleView {
            bundle_id,
            terms_hash: hex(&self.terms_hash),
            epoch: self.epoch,
            legs: self
                .legs
                .iter()
                .zip(&self.registered)
                .map(|(l, r)| BundleLegView { payer: l.payer.clone(), payee: l.payee.clone(), max_amount: l.max_amount, registered: U128(*r) })
                .collect(),
            status: self.status,
        }
    }
}

/// Leg of bundle `bundle_id` for `(payer, payee)`: `(index, remaining reserved amount)`.
pub fn leg_remaining(bundle: &Bundle, payer: &AccountId, payee: &AccountId) -> Option<(usize, u128)> {
    bundle.legs.iter().position(|l| &l.payer == payer && &l.payee == payee).map(|i| (i, bundle.legs[i].max_amount.0 - bundle.registered[i]))
}

pub fn load_bundle(bundle_id: u64) -> Bundle {
    bundles().get(&bundle_id).cloned().unwrap_or_else(|| env::panic_str(&format!("ERR_UNKNOWN_BUNDLE: {bundle_id}")))
}

/// Records `delta` as registered against leg `index` and releases the same amount of reservation.
pub fn consume_leg(bundle_id: u64, index: usize, payer: &AccountId, delta: u128) {
    let mut map = bundles();
    let bundle = map.get_mut(&bundle_id).expect("bundle loaded before");
    bundle.registered[index] += delta;
    let epoch = bundle.epoch;
    map.flush();
    set_reserved(epoch, payer, reserved(epoch, payer).saturating_sub(delta));
}

#[near]
impl X207Circle {
    /// F5 (CT-062): an `active` member posts the hash of its barter intent. `ERR_PAYER_NOT_ACTIVE`, `ERR_INTENT_EXISTS`.
    pub fn post_intent(&mut self, intent_hash: Base58CryptoHash) {
        let account = env::predecessor_account_id();
        let active = self.members.get(&account).map(|m| self.member_now(m)).is_some_and(|m| m.status == MemberStatus::Active);
        require!(active, format!("ERR_PAYER_NOT_ACTIVE: {account}"));
        let key: [u8; 32] = intent_hash.into();
        let mut map = intents();
        require!(map.get(&key).is_none(), format!("ERR_INTENT_EXISTS: {}", hex(&key)));
        let mut counts = intent_counts();
        let open = counts.get(&account).copied().unwrap_or(0);
        require!(open < MAX_OPEN_INTENTS_PER_MEMBER, format!("ERR_INTENT_LIMIT: {account} has {open} open intents, the limit is {MAX_OPEN_INTENTS_PER_MEMBER}"));
        let posted_at = env::block_timestamp();
        map.insert(key, IntentRecord { account_id: account.clone(), posted_at });
        map.flush();
        counts.insert(account.clone(), open + 1);
        counts.flush();
        IntentPosted { intent_hash, account_id: account, posted_at: U64(posted_at) }.emit();
    }

    /// F5 (security review F2): the owner of an intent takes it away and gets the slot back. `ERR_UNKNOWN_INTENT`, `ERR_NOT_INTENT_OWNER`.
    pub fn remove_intent(&mut self, intent_hash: Base58CryptoHash) {
        let account = env::predecessor_account_id();
        let key: [u8; 32] = intent_hash.into();
        let mut map = intents();
        let Some(record) = map.get(&key) else { env::panic_str(&format!("ERR_UNKNOWN_INTENT: {}", hex(&key))) };
        require!(record.account_id == account, format!("ERR_NOT_INTENT_OWNER: {account}"));
        map.remove(&key);
        map.flush();
        let mut counts = intent_counts();
        let open = counts.get(&account).copied().unwrap_or(1);
        counts.insert(account, open.saturating_sub(1));
        counts.flush();
    }

    /// F5 (CT-063): permissionless. Checks in order: epoch (`ERR_UNKNOWN_EPOCH`, `ERR_WRONG_PHASE`), past cutoff
    /// (`ERR_PAST_CUTOFF`), shape of the legs (`ERR_BAD_BUNDLE`), participants active (`ERR_PAYER_NOT_ACTIVE`,
    /// `ERR_PAYEE_NOT_ACTIVE`), `terms_hash` new (`ERR_BUNDLE_EXISTS`), one valid signature per participant
    /// (`ERR_BUNDLE_SIGNATURES`), headroom of every payer for the sum of its legs (`ERR_BUNDLE_HEADROOM`). Nothing is
    /// written before the last check passes. Returns the `bundle_id`.
    pub fn commit_bundle(&mut self, bundle: BundleInput, signatures: Vec<BundleSignature>) -> u64 {
        let epoch_id = bundle.epoch;
        let Some(epoch) = self.epochs.get(&epoch_id) else { env::panic_str(&format!("ERR_UNKNOWN_EPOCH: {epoch_id}")) };
        if epoch_id != self.current_epoch || epoch.phase != EpochPhase::Open {
            env::panic_str(&format!("ERR_WRONG_PHASE: epoch {epoch_id} is not the running open epoch"));
        }
        let now = env::block_timestamp();
        require!(now < epoch.cutoff_at, format!("ERR_PAST_CUTOFF: cutoff_at {}", epoch.cutoff_at));

        // shape: at least two legs, payer != payee, unique pairs, amounts > 0
        require!(bundle.legs.len() >= 2, "ERR_BAD_BUNDLE: a bundle needs at least two legs");
        require!(bundle.legs.len() <= MAX_BUNDLE_LEGS, format!("ERR_BAD_BUNDLE: {} legs, the limit is {MAX_BUNDLE_LEGS}", bundle.legs.len()));
        let mut pairs = std::collections::BTreeSet::new();
        for leg in &bundle.legs {
            require!(leg.payer != leg.payee, format!("ERR_BAD_BUNDLE: leg pays itself {}", leg.payer));
            require!(leg.max_amount.0 > 0, "ERR_BAD_BUNDLE: max_amount must be above 0");
            require!(pairs.insert((leg.payer.clone(), leg.payee.clone())), format!("ERR_BAD_BUNDLE: duplicate leg {} -> {}", leg.payer, leg.payee));
        }
        if let Some(search) = &bundle.search_leg {
            let leg = bundle.legs.iter().find(|l| l.payer == search.payer && l.payee == search.payee);
            let leg = leg.unwrap_or_else(|| env::panic_str("ERR_BAD_BUNDLE: search_leg is not a leg of the bundle"));
            let sum = bundle.legs.iter().try_fold(0u128, |a, l| a.checked_add(l.max_amount.0)).unwrap_or_else(|| env::panic_str("ERR_BAD_BUNDLE: legs overflow"));
            let cap_bps = epoch.params.max_search_share_bps as u128;
            let over = leg.max_amount.0.checked_mul(10_000).map_or(true, |scaled| scaled > cap_bps.saturating_mul(sum));
            require!(!over, format!("ERR_SEARCH_SHARE_EXCEEDED: {} of {} exceeds {} bps", leg.max_amount.0, sum, cap_bps));
        }
        let mut participants: Vec<AccountId> = Vec::new();
        let mut owed: std::collections::BTreeMap<AccountId, u128> = std::collections::BTreeMap::new();
        for leg in &bundle.legs {
            for who in [&leg.payer, &leg.payee] {
                if !participants.contains(who) {
                    participants.push(who.clone());
                }
            }
            let total = owed.entry(leg.payer.clone()).or_default();
            *total = total.checked_add(leg.max_amount.0).unwrap_or_else(|| env::panic_str("ERR_BAD_BUNDLE: legs overflow"));
        }
        for leg in &bundle.legs {
            let payer = self.members.get(&leg.payer).map(|m| self.member_now(m));
            require!(payer.is_some_and(|m| m.status == MemberStatus::Active), format!("ERR_PAYER_NOT_ACTIVE: {}", leg.payer));
            let payee = self.members.get(&leg.payee).map(|m| self.member_now(m));
            require!(payee.is_some_and(|m| m.status == MemberStatus::Active), format!("ERR_PAYEE_NOT_ACTIVE: {}", leg.payee));
        }

        // security review F2: a participant is in at most MAX_BUNDLES_PER_MEMBER_PER_EPOCH bundles of an epoch
        let counts = bundle_counts();
        for who in &participants {
            let n = counts.get(&(epoch_id, who.clone())).copied().unwrap_or(0);
            require!(n < MAX_BUNDLES_PER_MEMBER_PER_EPOCH, format!("ERR_BUNDLE_LIMIT: {who} is in {n} bundles of epoch {epoch_id}, the limit is {MAX_BUNDLES_PER_MEMBER_PER_EPOCH}"));
        }

        let circle = env::current_account_id();
        let message = terms_message(&circle, epoch_id, &bundle.legs, bundle.search_leg.as_ref());
        let terms_hash = env::sha256_array(message.as_bytes());
        require!(!env::storage_has_key(&hash_key(&terms_hash)), format!("ERR_BUNDLE_EXISTS: {}", hex(&terms_hash)));

        // exactly one valid signature per participant, nobody else
        require!(signatures.len() == participants.len(), "ERR_BUNDLE_SIGNATURES: every participant signs exactly once");
        let signing_hash = nep413_signing_hash(&message, terms_hash, circle.as_str());
        for who in &participants {
            let mut mine = signatures.iter().filter(|s| &s.account_id == who);
            let signature = mine.next();
            require!(signature.is_some() && mine.next().is_none(), format!("ERR_BUNDLE_SIGNATURES: {who} must sign exactly once"));
            let pubkey = self.members.get(who).expect("participants are members").pubkey_voucher.clone();
            let ok = parse_signature(&signature.expect("checked").signature)
                .zip(<[u8; 32]>::try_from(&pubkey.as_bytes()[1..]).ok())
                .is_some_and(|(sig, pk)| env::ed25519_verify(&sig, &signing_hash, &pk));
            require!(ok, format!("ERR_BUNDLE_SIGNATURES: bad signature of {who}"));
        }

        // headroom of each payer: limit - outstanding debt - what other bundles of this epoch already reserve
        for (payer, total) in &owed {
            let member = self.members.get(payer).map(|m| self.member_now(m)).expect("checked above");
            let debt = self.outstanding_debt.get(payer).copied().unwrap_or(0);
            let free = member.limit.saturating_sub(debt).saturating_sub(reserved(epoch_id, payer));
            require!(*total <= free, format!("ERR_BUNDLE_HEADROOM: {payer} needs {total} headroom {free}"));
        }

        // state change
        let mut map = bundles();
        let bundle_id = env::storage_read(SEQ_KEY).map_or(1, |b| u64::from_le_bytes(b.try_into().expect("8 bytes")) + 1);
        env::storage_write(SEQ_KEY, &bundle_id.to_le_bytes());
        env::storage_write(&hash_key(&terms_hash), &bundle_id.to_le_bytes());
        let solver = env::predecessor_account_id();
        let record = Bundle {
            terms_hash,
            epoch: epoch_id,
            registered: vec![0; bundle.legs.len()],
            legs: bundle.legs.clone(),
            solver: solver.clone(),
            status: BundleStatus::Active,
        };
        map.insert(bundle_id, record);
        map.flush();
        let mut ids = epoch_bundle_ids(epoch_id);
        ids.push(bundle_id);
        set_epoch_bundle_ids(epoch_id, &ids);
        for (payer, total) in &owed {
            set_reserved(epoch_id, payer, reserved(epoch_id, payer) + total);
        }
        let mut counts = bundle_counts();
        for who in &participants {
            let n = counts.get(&(epoch_id, who.clone())).copied().unwrap_or(0);
            counts.insert((epoch_id, who.clone()), n + 1);
        }
        counts.flush();
        BundleCommitted { bundle_id, terms_hash: hex(&terms_hash), epoch: epoch_id, legs: bundle.legs, solver }.emit();
        bundle_id
    }

    pub fn get_bundle(&self, bundle_id: u64) -> Option<BundleView> {
        bundles().get(&bundle_id).map(|b| b.view(bundle_id))
    }

    pub fn get_intent(&self, intent_hash: Base58CryptoHash) -> Option<IntentView> {
        let key: [u8; 32] = intent_hash.into();
        intents().get(&key).map(|r| IntentView { intent_hash, account_id: r.account_id.clone(), posted_at: U64(r.posted_at) })
    }
}

#[cfg(test)]
mod security_tests {
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    use super::*;
    use crate::member::Member;
    use crate::params::Params;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn circle_with_member(name: &str) -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        let mut c = X207Circle::with_config(acc("dao.near"), "agents-main".into(), acc("usdt.near"), acc("intents.near"), Params::default(), 1_000);
        c.members.insert(acc(name), Member {
            account_id: acc(name), pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse().unwrap(), terms_hash: "a".repeat(64),
            stake: 100, limit: 80, status: MemberStatus::Active, admitted: false, joined_at: 1, settled_history: 0, last_active_epoch: 1, exit_requested_epoch: None,
            withdraw_pending: false, reactivate_epoch: None,
        });
        c
    }

    fn post(c: &mut X207Circle, who: &str, n: u32) {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc(who)).block_timestamp(2_000).build());
        let mut hash = [0u8; 32];
        hash[..4].copy_from_slice(&n.to_le_bytes());
        c.post_intent(Base58CryptoHash::from(hash));
    }

    // TC-CT-062-S02 (security review F2): a member has at most MAX_OPEN_INTENTS_PER_MEMBER intents open; remove_intent frees a slot.
    #[test]
    fn one_member_cannot_fill_the_storage_of_the_contract_with_intents() {
        let mut c = circle_with_member("gina.near");
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for n in 0..5_000 {
                post(&mut c, "gina.near", n);
            }
        }));
        assert!(refused.is_err(), "5000 intents of one member were all accepted, paid by the balance of the contract");
    }

    fn hash_of(n: u32) -> Base58CryptoHash {
        let mut hash = [0u8; 32];
        hash[..4].copy_from_slice(&n.to_le_bytes());
        Base58CryptoHash::from(hash)
    }

    fn panics_with(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("expected a panic");
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    #[test]
    fn a_bundle_with_more_than_64_legs_is_refused_with_err_bad_bundle() {
        let mut c = circle_with_member("gina.near");
        let legs = |n: usize| -> Vec<BundleLeg> {
            (0..n).map(|i| BundleLeg { payer: acc("gina.near"), payee: acc(&format!("p{i}.near")), max_amount: U128(1) }).collect()
        };
        let input = |n: usize| BundleInput { epoch: 1, legs: legs(n), search_leg: None };
        let msg = panics_with(std::panic::AssertUnwindSafe(|| {
            c.commit_bundle(input(MAX_BUNDLE_LEGS + 1), vec![]);
        }));
        assert!(msg.contains("ERR_BAD_BUNDLE") && msg.contains("limit is 64"), "{msg}");
        // 64 legs pass the shape check and stop later (the payees are not members)
        let msg = panics_with(std::panic::AssertUnwindSafe(|| {
            c.commit_bundle(input(MAX_BUNDLE_LEGS), vec![]);
        }));
        assert!(msg.contains("ERR_PAYEE_NOT_ACTIVE"), "{msg}");
    }

    #[test]
    fn the_65th_open_intent_of_a_member_is_refused_with_err_intent_limit() {
        let mut c = circle_with_member("gina.near");
        for n in 0..MAX_OPEN_INTENTS_PER_MEMBER {
            post(&mut c, "gina.near", n);
        }
        let msg = panics_with(std::panic::AssertUnwindSafe(|| post(&mut c, "gina.near", 1_000)));
        assert!(msg.contains("ERR_INTENT_LIMIT"), "{msg}");
    }

    #[test]
    fn remove_intent_frees_a_slot_and_only_the_owner_can_remove() {
        let mut c = circle_with_member("gina.near");
        for n in 0..MAX_OPEN_INTENTS_PER_MEMBER {
            post(&mut c, "gina.near", n);
        }
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("mallory.near")).block_timestamp(2_000).build());
        let msg = panics_with(std::panic::AssertUnwindSafe(|| c.remove_intent(hash_of(3))));
        assert!(msg.contains("ERR_NOT_INTENT_OWNER"), "{msg}");
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("gina.near")).block_timestamp(2_000).build());
        c.remove_intent(hash_of(3));
        let msg = panics_with(std::panic::AssertUnwindSafe(|| c.remove_intent(hash_of(3))));
        assert!(msg.contains("ERR_UNKNOWN_INTENT"), "{msg}");
        post(&mut c, "gina.near", 1_000);
    }
}

/// Property test of the bookkeeping of partly registered legs (security review, "Not checked"): whatever is registered through the legs of several bundles, in any order and
/// in parts, the headroom reserved for a payer is exactly what its legs have not registered yet, a registration moves value from the reservation to the debt and never creates
/// or loses any, and no leg registers more than its maximum.
#[cfg(test)]
mod leg_properties {
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{MockedBlockchain, RuntimeFeesConfig};
    use proptest::prelude::*;

    use super::*;

    fn acc(i: usize) -> AccountId {
        format!("m{i}.near").parse().unwrap()
    }

    /// Fresh storage for every case.
    fn fresh() {
        near_sdk::env::set_blockchain_interface(MockedBlockchain::new(
            VMContextBuilder::new().block_timestamp(1_000).build(),
            near_sdk::test_vm_config(),
            RuntimeFeesConfig::test(),
            Default::default(),
            Default::default(),
            Default::default(),
            None,
        ));
    }

    fn legs() -> impl Strategy<Value = Vec<(usize, usize, u128)>> {
        proptest::collection::vec((0usize..6, 0usize..6, 1u128..=1_000), 2..=8).prop_map(|raw| {
            let mut seen = std::collections::BTreeSet::new();
            raw.into_iter().filter(|(a, b, _)| a != b && seen.insert((*a, *b))).collect::<Vec<_>>()
        })
    }

    fn commit(id: u64, legs: &[(usize, usize, u128)]) {
        let record = Bundle {
            terms_hash: [id as u8; 32],
            epoch: 1,
            registered: vec![0; legs.len()],
            legs: legs.iter().map(|&(a, b, m)| BundleLeg { payer: acc(a), payee: acc(b), max_amount: U128(m) }).collect(),
            solver: acc(9),
            status: BundleStatus::Active,
        };
        let mut map = bundles();
        map.insert(id, record);
        map.flush();
        for &(a, _, m) in legs {
            set_reserved(1, &acc(a), reserved(1, &acc(a)) + m);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        #[test]
        fn reservation_equals_what_the_legs_have_not_registered(
            first in legs(), second in legs(),
            steps in proptest::collection::vec((any::<bool>(), any::<prop::sample::Index>(), 0u128..=1_200), 0..=60),
        ) {
            prop_assume!(first.len() >= 2 && second.len() >= 2);
            fresh();
            let mut debt: std::collections::BTreeMap<usize, u128> = std::collections::BTreeMap::new();
            commit(1, &first);
            commit(2, &second);
            let initial: std::collections::BTreeMap<usize, u128> = (0..6).map(|i| (i, reserved(1, &acc(i)))).collect();
            for (use_second, pick, raw) in steps {
                let (id, set) = if use_second { (2u64, &second) } else { (1u64, &first) };
                let index = pick.index(set.len());
                let bundle = load_bundle(id);
                let payer = acc(set[index].0);
                let (_, remaining) = leg_remaining(&bundle, &payer, &acc(set[index].1)).expect("a leg of the bundle");
                // the contract refuses a delta above the remaining part of the leg (ERR_BUNDLE_LEG_EXCEEDED); a registration is never above it
                let delta = raw.min(remaining);
                consume_leg(id, index, &payer, delta);
                *debt.entry(set[index].0).or_default() += delta;
            }
            for i in 0..6usize {
                let open: u128 = [(1u64, &first), (2u64, &second)]
                    .iter()
                    .map(|(id, set)| {
                        let bundle = load_bundle(*id);
                        set.iter().enumerate().filter(|(_, l)| l.0 == i).map(|(k, l)| l.2 - bundle.registered[k]).sum::<u128>()
                    })
                    .sum();
                prop_assert_eq!(reserved(1, &acc(i)), open, "reservation of member {}", i);
                prop_assert_eq!(reserved(1, &acc(i)) + debt.get(&i).copied().unwrap_or(0), initial[&i], "value of member {} is neither created nor lost", i);
            }
            for (id, set) in [(1u64, &first), (2u64, &second)] {
                let bundle = load_bundle(id);
                for (k, l) in set.iter().enumerate() {
                    prop_assert!(bundle.registered[k] <= l.2);
                }
            }
        }
    }
}
