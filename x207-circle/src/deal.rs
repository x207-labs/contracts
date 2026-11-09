//! Bond of both sides with an arbiter (CT-094, ADR-43). A swap of exactly two participants whose terms name an arbiter locks a part of the stake of each side (the bond).
//! The bond is a lock on the stake, not a new deposit and not headroom: at all times `limit <= stake - bond_locked`, so a default is paid without touching a bond. The terms
//! are signed by both parties, arbiter and window included. Money moves only from the stake of the side that the arbiter found at fault to the stake of the other
//! side, inside the bond that both locked; nobody else, not the DAO and not the owner, can touch a bond (ADR-20).
//!
//! `commit_bundle` opens the deal: at or above `bond_threshold` the arbiter is required, below it the parties may still ask for a bond by naming one. `confirm_deal(P)`
//! releases the bond of the other side (it protects the receiver). `dispute_deal` opens one dispute per deal. `rule_deal` is the arbiter's alone. `settle_deal` is
//! permissionless: after `confirm_by` with no dispute the bonds are released, and an arbiter that is silent for `RULING_WINDOW_NS` releases both (nobody is punished
//! without a ruling).

use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::json_types::{U128, U64};
use near_sdk::store::LookupMap;
use near_sdk::{env, near, require, AccountId};

use crate::events::{
    BondLocked, BondReleased, BondSlashed, DealConfirmed, DealDisputed, DealRuled, LimitChange,
    LimitsUpdated, X207Event,
};
use crate::state::{StorageKey, X207Circle, X207CircleExt};

pub const MAX_OPEN_DEALS_PER_MEMBER: u32 = 8;
pub const MIN_CONFIRM_WINDOW_NS: u64 = 3600 * 1_000_000_000;
pub const MAX_CONFIRM_WINDOW_NS: u64 = 14 * 24 * 3600 * 1_000_000_000;
pub const RULING_WINDOW_NS: u64 = 7 * 24 * 3600 * 1_000_000_000;

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The accused failed: its bond goes to the stake of the complainant, the bond of the complainant is released.
    AccusedGuilty,
    /// The complaint was false: half of the bond of the complainant goes to the stake of the accused, the rest is released, and the bond of the accused is released.
    ComplaintFalse,
    /// Both bonds are released.
    NoFault,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    Confirmed,
    Elapsed,
    ArbiterSilent,
    Ruling,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DealState {
    Open,
    Disputed,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
#[borsh(crate = "near_sdk::borsh")]
pub struct Deal {
    pub parties: [AccountId; 2],
    pub arbiter: AccountId,
    pub agreement_hash: Option<String>,
    pub confirm_by: u64,
    pub bond_each: u128,
    pub locked: [bool; 2],
    pub state: DealState,
    pub complainant: Option<u8>,
    pub disputed_at: u64,
    pub evidence_hash: Option<String>,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DealView {
    pub bundle_id: u64,
    pub parties: Vec<AccountId>,
    pub arbiter: AccountId,
    pub agreement_hash: Option<String>,
    pub confirm_by: U64,
    pub bond_each: U128,
    /// per party, in the order of `parties`: whether its bond is still locked
    pub locked: Vec<bool>,
    pub state: DealState,
    pub complainant: Option<AccountId>,
    pub accused: Option<AccountId>,
    pub disputed_at: Option<U64>,
    pub evidence_hash: Option<String>,
}

fn deals() -> LookupMap<u64, Deal> {
    LookupMap::new(StorageKey::Deals)
}

pub fn load_deal(bundle_id: u64) -> Option<Deal> {
    deals().get(&bundle_id).cloned()
}

fn save(bundle_id: u64, deal: &Deal) {
    let mut map = deals();
    map.insert(bundle_id, deal.clone());
    map.flush();
}

pub(crate) fn locked_key(account: &AccountId) -> Vec<u8> {
    [b"x207_bond_locked:".as_slice(), account.as_bytes()].concat()
}

fn count_key(account: &AccountId) -> Vec<u8> {
    [b"x207_deal_count:".as_slice(), account.as_bytes()].concat()
}

fn read_u128(key: &[u8]) -> u128 {
    env::storage_read(key).map_or(0, |b| u128::from_le_bytes(b.try_into().expect("16 bytes")))
}

pub(crate) fn write_u128(key: &[u8], value: u128) {
    if value == 0 {
        env::storage_remove(key);
    } else {
        env::storage_write(key, &value.to_le_bytes());
    }
}

/// The part of the stake of `account` that is locked as bonds.
pub fn bond_locked_of(account: &AccountId) -> u128 {
    read_u128(&locked_key(account))
}

fn deal_count_of(account: &AccountId) -> u32 {
    env::storage_read(&count_key(account))
        .map_or(0, |b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
}

fn set_deal_count(account: &AccountId, n: u32) {
    if n == 0 {
        env::storage_remove(&count_key(account));
    } else {
        env::storage_write(&count_key(account), &n.to_le_bytes());
    }
}

/// `limit` cannot be above `stake - bond_locked`.
pub fn bound_limit(limit: u128, stake: u128, account: &AccountId) -> u128 {
    limit.min(stake.saturating_sub(bond_locked_of(account)))
}

/// `ERR_BOND_LOCKED` while any bond of `account` is locked (leaving the circle with the stake waits for the end of the deals).
pub fn assert_no_bond(account: &AccountId) {
    let locked = bond_locked_of(account);
    require!(
        locked == 0,
        format!("ERR_BOND_LOCKED: {account} has {locked} locked as a bond")
    );
}

fn is_hash(h: &str) -> bool {
    h.len() == 64
        && h.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn index_of(deal: &Deal, who: &AccountId) -> Option<usize> {
    deal.parties.iter().position(|p| p == who)
}

impl X207Circle {
    /// Called by `commit_bundle` for a swap that names an arbiter. The caller checked the shape of the terms; this checks the stake, the window and the limits, then locks
    /// the bonds. Nothing is written before every check passed.
    pub(crate) fn open_deal(
        &mut self,
        bundle_id: u64,
        parties: [AccountId; 2],
        arbiter: AccountId,
        agreement_hash: Option<String>,
        confirm_by: u64,
        bond_each: u128,
    ) {
        let now = env::block_timestamp();
        require!(
            arbiter != self.owner_id && arbiter != env::current_account_id(),
            "ERR_ARBITER_INVALID: the arbiter cannot be the owner of the circle or the circle"
        );
        require!(
            !parties.contains(&arbiter),
            "ERR_ARBITER_INVALID: the arbiter cannot be a party of the deal"
        );
        require!(
            confirm_by >= now + MIN_CONFIRM_WINDOW_NS && confirm_by <= now + MAX_CONFIRM_WINDOW_NS,
            format!("ERR_DEAL_WINDOW: confirm_by must be between 1 hour and 14 days after the commit, got {confirm_by} at {now}")
        );
        if let Some(h) = &agreement_hash {
            require!(
                is_hash(h),
                "ERR_BAD_BUNDLE: agreement_hash is 64 lowercase hex"
            );
        }
        for who in &parties {
            let member = self.members.get(who).expect("participants are members");
            let locked = bond_locked_of(who);
            let slack = member
                .stake
                .saturating_sub(member.limit)
                .saturating_sub(locked);
            require!(bond_each <= slack, format!("ERR_BOND_INSUFFICIENT_STAKE: {who} needs {bond_each} above its limit, has {slack}"));
            require!(deal_count_of(who) < MAX_OPEN_DEALS_PER_MEMBER, format!("ERR_DEAL_LIMIT: {who} has {} open deals, the limit is {MAX_OPEN_DEALS_PER_MEMBER}", deal_count_of(who)));
        }
        for who in &parties {
            write_u128(&locked_key(who), bond_locked_of(who) + bond_each);
            set_deal_count(who, deal_count_of(who) + 1);
        }
        save(
            bundle_id,
            &Deal {
                parties: parties.clone(),
                arbiter,
                agreement_hash,
                confirm_by,
                bond_each,
                locked: [true, true],
                state: DealState::Open,
                complainant: None,
                disputed_at: 0,
                evidence_hash: None,
            },
        );
        for who in parties {
            BondLocked {
                bundle_id,
                account_id: who,
                amount: U128(bond_each),
            }
            .emit();
        }
    }

    fn release_bond(&self, bundle_id: u64, deal: &mut Deal, index: usize, reason: ReleaseReason) {
        if !deal.locked[index] {
            return;
        }
        deal.locked[index] = false;
        let who = deal.parties[index].clone();
        write_u128(
            &locked_key(&who),
            bond_locked_of(&who).saturating_sub(deal.bond_each),
        );
        BondReleased {
            bundle_id,
            account_id: who,
            amount: U128(deal.bond_each),
            reason,
        }
        .emit();
    }

    /// Moves `amount` of the bond of `from` to the stake of the other party. The lock itself is lifted by the caller.
    fn slash_bond(
        &mut self,
        bundle_id: u64,
        from_account: &AccountId,
        to_account: &AccountId,
        amount: u128,
    ) {
        // review C-7: never panics; what the stake cannot cover (a default took it) is not moved
        let from_stake_before = self
            .members
            .get(from_account)
            .expect("party is a member")
            .stake;
        let amount = amount.min(from_stake_before);
        let from_stake = {
            let f = self
                .members
                .get_mut(from_account)
                .expect("party is a member");
            f.stake -= amount;
            f.stake
        };
        let to_stake = {
            let t = self.members.get_mut(to_account).expect("party is a member");
            t.stake = t.stake.checked_add(amount).expect("stake overflow");
            t.stake
        };
        BondSlashed {
            bundle_id,
            from: from_account.clone(),
            to: to_account.clone(),
            amount: U128(amount),
            from_stake: U128(from_stake),
            to_stake: U128(to_stake),
        }
        .emit();
    }

    /// After a bond left the stake of `who`, the limit must still be at most `stake - bond_locked`.
    fn bound_member_limit(&mut self, who: &AccountId) {
        let locked = bond_locked_of(who);
        if let Some(m) = self.members.get_mut(who) {
            m.limit = m.limit.min(m.stake.saturating_sub(locked));
        }
    }

    fn close_deal(&mut self, bundle_id: u64, deal: &Deal) {
        let mut map = deals();
        map.remove(&bundle_id);
        map.flush();
        for who in &deal.parties {
            set_deal_count(who, deal_count_of(who).saturating_sub(1));
        }
    }

    fn party_index(&self, deal: &Deal, bundle_id: u64) -> usize {
        let caller = env::predecessor_account_id();
        index_of(deal, &caller).unwrap_or_else(|| {
            env::panic_str(&format!(
                "ERR_DEAL_NOT_PARTY: {caller} is not a party of deal {bundle_id}"
            ))
        })
    }
}

#[near]
impl X207Circle {
    /// A party says that the other party delivered: the bond of the other party is released. `ERR_DEAL_UNKNOWN`, `ERR_DEAL_NOT_PARTY`, `ERR_DEAL_ALREADY_DISPUTED`,
    /// `ERR_DEAL_CLOSED` (the bond of the other party is released already).
    pub fn confirm_deal(&mut self, bundle_id: u64) {
        let mut deal = load_deal(bundle_id)
            .unwrap_or_else(|| env::panic_str(&format!("ERR_DEAL_UNKNOWN: {bundle_id}")));
        let me = self.party_index(&deal, bundle_id);
        require!(
            deal.state == DealState::Open,
            format!("ERR_DEAL_ALREADY_DISPUTED: deal {bundle_id} has a dispute, the arbiter rules")
        );
        let other = 1 - me;
        require!(
            deal.locked[other],
            format!(
                "ERR_DEAL_CLOSED: the bond of {} is released already",
                deal.parties[other]
            )
        );
        DealConfirmed {
            bundle_id,
            account_id: deal.parties[me].clone(),
            of: deal.parties[other].clone(),
        }
        .emit();
        self.release_bond(bundle_id, &mut deal, other, ReleaseReason::Confirmed);
        if deal.locked == [false, false] {
            self.close_deal(bundle_id, &deal);
        } else {
            save(bundle_id, &deal);
        }
    }

    /// A party says that the other party did not deliver, before `confirm_by`, while the bond of the other party is still locked. One dispute per deal.
    /// `ERR_DEAL_UNKNOWN`, `ERR_DEAL_NOT_PARTY`, `ERR_DEAL_ALREADY_DISPUTED`, `ERR_DEAL_WINDOW_OVER`, `ERR_DEAL_CLOSED`, `ERR_DEAL_NO_STAKE_AT_RISK`, `ERR_BAD_HASH`.
    pub fn dispute_deal(&mut self, bundle_id: u64, evidence_hash: String) {
        let mut deal = load_deal(bundle_id)
            .unwrap_or_else(|| env::panic_str(&format!("ERR_DEAL_UNKNOWN: {bundle_id}")));
        let me = self.party_index(&deal, bundle_id);
        require!(
            deal.state == DealState::Open,
            format!("ERR_DEAL_ALREADY_DISPUTED: deal {bundle_id}")
        );
        let now = env::block_timestamp();
        // `<`, not `<=`: at `confirm_by` the deal is due for `settle_deal` (`>=`), so the two never overlap (review C-10)
        require!(
            now < deal.confirm_by,
            format!(
                "ERR_DEAL_WINDOW_OVER: confirm_by {} now {now}",
                deal.confirm_by
            )
        );
        require!(
            deal.locked[1 - me],
            format!(
                "ERR_DEAL_CLOSED: {} has nothing locked any more, there is nothing to dispute",
                deal.parties[1 - me]
            )
        );
        // review C-8: a complainant whose own bond was released (it confirmed) risks nothing by accusing
        require!(deal.locked[me], format!("ERR_DEAL_NO_STAKE_AT_RISK: the bond of {} is released, so {} cannot accuse the other party", deal.parties[me], deal.parties[me]));
        require!(
            is_hash(&evidence_hash),
            "ERR_BAD_HASH: evidence_hash is 64 lowercase hex"
        );
        deal.state = DealState::Disputed;
        deal.complainant = Some(me as u8);
        deal.disputed_at = now;
        deal.evidence_hash = Some(evidence_hash.clone());
        save(bundle_id, &deal);
        DealDisputed {
            bundle_id,
            complainant: deal.parties[me].clone(),
            accused: deal.parties[1 - me].clone(),
            evidence_hash,
            disputed_at: U64(now),
        }
        .emit();
    }

    /// The arbiter alone, on a disputed deal, within `RULING_WINDOW_NS` of the dispute. `ERR_DEAL_UNKNOWN`, `ERR_NOT_ARBITER`, `ERR_DEAL_NOT_DISPUTED`, `ERR_RULING_WINDOW`.
    pub fn rule_deal(&mut self, bundle_id: u64, verdict: Verdict) {
        let mut deal = load_deal(bundle_id)
            .unwrap_or_else(|| env::panic_str(&format!("ERR_DEAL_UNKNOWN: {bundle_id}")));
        let caller = env::predecessor_account_id();
        require!(
            caller == deal.arbiter,
            format!(
                "ERR_NOT_ARBITER: only {} rules deal {bundle_id}",
                deal.arbiter
            )
        );
        require!(
            deal.state == DealState::Disputed,
            format!("ERR_DEAL_NOT_DISPUTED: deal {bundle_id}")
        );
        let now = env::block_timestamp();
        require!(
            now < deal.disputed_at + RULING_WINDOW_NS,
            format!(
                "ERR_RULING_WINDOW: the dispute is from {}, the window of the arbiter is over",
                deal.disputed_at
            )
        );
        let c = deal.complainant.expect("a disputed deal has a complainant") as usize;
        let a = 1 - c;
        DealRuled {
            bundle_id,
            arbiter: caller,
            verdict,
        }
        .emit();
        match verdict {
            Verdict::AccusedGuilty => {
                // the bond of the accused goes to the complainant; the rest of the locks are released
                if deal.locked[a] {
                    let bond = deal.bond_each;
                    self.slash_then_release(bundle_id, &mut deal, a, bond);
                }
                self.release_bond(bundle_id, &mut deal, c, ReleaseReason::Ruling);
            }
            Verdict::ComplaintFalse => {
                if deal.locked[c] {
                    let half = deal.bond_each / 2;
                    self.slash_then_release(bundle_id, &mut deal, c, half);
                }
                self.release_bond(bundle_id, &mut deal, a, ReleaseReason::Ruling);
            }
            Verdict::NoFault => {
                self.release_bond(bundle_id, &mut deal, c, ReleaseReason::Ruling);
                self.release_bond(bundle_id, &mut deal, a, ReleaseReason::Ruling);
            }
        }
        let mut changes = Vec::new();
        for who in deal.parties.clone() {
            let old = self.members.get(&who).map_or(0, |m| m.limit);
            self.bound_member_limit(&who);
            if let Some(m) = self.members.get(&who) {
                if m.limit != old {
                    changes.push(LimitChange {
                        account_id: who.clone(),
                        old_limit: U128(old),
                        new_limit: U128(m.limit),
                        status: "active".into(),
                    });
                }
            }
        }
        if !changes.is_empty() {
            LimitsUpdated {
                epoch: self.current_epoch,
                changes,
                chunk: 0,
                chunks: 1,
            }
            .emit();
        }
        self.close_deal(bundle_id, &deal);
    }

    /// Anybody, once the deal is due: open and past `confirm_by` (the bonds still locked are released), or disputed and past the window of the arbiter, who was silent
    /// (both bonds are released). `ERR_DEAL_UNKNOWN`, `ERR_DEAL_NOT_DUE`.
    pub fn settle_deal(&mut self, bundle_id: u64) {
        let mut deal = load_deal(bundle_id)
            .unwrap_or_else(|| env::panic_str(&format!("ERR_DEAL_UNKNOWN: {bundle_id}")));
        let now = env::block_timestamp();
        let reason = match deal.state {
            DealState::Open if now >= deal.confirm_by => ReleaseReason::Elapsed,
            DealState::Disputed if now >= deal.disputed_at + RULING_WINDOW_NS => {
                ReleaseReason::ArbiterSilent
            }
            _ => env::panic_str(&format!(
                "ERR_DEAL_NOT_DUE: deal {bundle_id}, state {:?}, confirm_by {}, now {now}",
                deal.state, deal.confirm_by
            )),
        };
        for index in 0..2 {
            self.release_bond(bundle_id, &mut deal, index, reason);
        }
        self.close_deal(bundle_id, &deal);
    }

    pub fn get_deal(&self, bundle_id: u64) -> Option<DealView> {
        load_deal(bundle_id).map(|d| DealView {
            bundle_id,
            parties: d.parties.to_vec(),
            arbiter: d.arbiter.clone(),
            agreement_hash: d.agreement_hash.clone(),
            confirm_by: U64(d.confirm_by),
            bond_each: U128(d.bond_each),
            locked: d.locked.to_vec(),
            state: d.state,
            complainant: d.complainant.map(|c| d.parties[c as usize].clone()),
            accused: d.complainant.map(|c| d.parties[1 - c as usize].clone()),
            disputed_at: (d.state == DealState::Disputed).then_some(U64(d.disputed_at)),
            evidence_hash: d.evidence_hash.clone(),
        })
    }

    pub fn bond_locked(&self, account_id: AccountId) -> U128 {
        U128(bond_locked_of(&account_id))
    }
}

impl X207Circle {
    /// Slashes `amount` of the bond of `index` to the other party and releases the rest of that bond.
    fn slash_then_release(&mut self, bundle_id: u64, deal: &mut Deal, index: usize, amount: u128) {
        let (from, to) = (deal.parties[index].clone(), deal.parties[1 - index].clone());
        self.slash_bond(bundle_id, &from, &to, amount);
        // the whole bond leaves the lock: `amount` went to the other stake, the remainder is released
        let who = deal.parties[index].clone();
        deal.locked[index] = false;
        write_u128(
            &locked_key(&who),
            bond_locked_of(&who).saturating_sub(deal.bond_each),
        );
        if deal.bond_each > amount {
            BondReleased {
                bundle_id,
                account_id: who,
                amount: U128(deal.bond_each - amount),
                reason: ReleaseReason::Ruling,
            }
            .emit();
        }
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::{testing_env, PublicKey};

    use super::*;
    use crate::member::{Member, MemberStatus};
    use crate::params::Params;

    const T0: u64 = 1_000_000_000_000;
    const STAKE: u128 = 1_000;
    const LIMIT: u128 = 800;
    const BOND: u128 = 100;
    const H: &str = "abababababababababababababababababababababababababababababababab";

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn at(caller: &str, ts: u64) {
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc(caller))
            .block_timestamp(ts)
            .build());
    }

    fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("expected a panic");
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    }

    fn circle() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(T0).build());
        let mut c = X207Circle::with_config(
            acc("dao.near"),
            "agents-main".into(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            T0,
        );
        for id in ["alice.near", "bob.near"] {
            let member = Member {
                account_id: acc(id),
                pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp"
                    .parse::<PublicKey>()
                    .unwrap(),
                terms_hash: "a".repeat(64),
                stake: STAKE,
                limit: LIMIT,
                status: MemberStatus::Active,
                admitted: false,
                joined_at: T0,
                settled_history: 0,
                last_active_epoch: 1,
                exit_requested_epoch: None,
                withdraw_pending: false,
                reactivate_epoch: None,
            };
            c.members.insert(acc(id), member);
            c.member_index.push(acc(id));
            c.member_count += 1;
            c.total_stake += STAKE;
            write_u128(&locked_key(&acc(id)), 0);
            set_deal_count(&acc(id), 0);
        }
        c
    }

    /// Deal `id` between alice and bob, bond 100 each, window of one day, arbiter `arb.near`.
    fn open(c: &mut X207Circle, id: u64) {
        at("solver.near", T0);
        c.open_deal(
            id,
            [acc("alice.near"), acc("bob.near")],
            acc("arb.near"),
            Some(H.into()),
            T0 + 24 * 3600 * 1_000_000_000,
            BOND,
        );
    }

    fn stakes(c: &X207Circle) -> (u128, u128) {
        (
            c.members.get(&acc("alice.near")).unwrap().stake,
            c.members.get(&acc("bob.near")).unwrap().stake,
        )
    }

    fn events() -> Vec<String> {
        get_logs()
            .iter()
            .filter_map(|l| l.strip_prefix("EVENT_JSON:"))
            .map(|b| {
                near_sdk::serde_json::from_str::<near_sdk::serde_json::Value>(b).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    // TC-CT-094-U01
    #[test]
    fn opening_a_deal_locks_the_bond_of_both_sides_inside_the_slack_between_stake_and_limit() {
        let mut c = circle();
        open(&mut c, 1);
        assert_eq!(
            (
                bond_locked_of(&acc("alice.near")),
                bond_locked_of(&acc("bob.near"))
            ),
            (BOND, BOND)
        );
        assert_eq!(deal_count_of(&acc("alice.near")), 1);
        assert_eq!(events(), vec!["bond_locked", "bond_locked"]);
        let view = c.get_deal(1).unwrap();
        assert_eq!(
            (view.state, view.locked, view.bond_each, view.arbiter),
            (
                DealState::Open,
                vec![true, true],
                U128(BOND),
                acc("arb.near")
            )
        );
        // the slack was 200 (stake 1000, limit 800): a second deal of 100 fits, a third does not
        open(&mut c, 2);
        assert_eq!(bond_locked_of(&acc("alice.near")), 2 * BOND);
        assert!(panic_text(std::panic::AssertUnwindSafe(|| open(&mut c, 3)))
            .contains("ERR_BOND_INSUFFICIENT_STAKE"));
        assert_eq!(
            bond_locked_of(&acc("bob.near")),
            2 * BOND,
            "a refused deal locks nothing"
        );
        // the limit can no longer grow into the locked part
        assert_eq!(
            bound_limit(950, STAKE, &acc("alice.near")),
            STAKE - 2 * BOND
        );
        assert_eq!(bound_limit(700, STAKE, &acc("alice.near")), 700);
    }

    // TC-CT-094-U02
    #[test]
    fn the_arbiter_the_window_the_hash_and_the_number_of_open_deals_are_checked() {
        let mut c = circle();
        at("solver.near", T0);
        let day = 24 * 3600 * 1_000_000_000;
        let open_with =
            |c: &mut X207Circle, arbiter: &str, confirm_by: u64, hash: Option<String>| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    c.open_deal(
                        9,
                        [acc("alice.near"), acc("bob.near")],
                        acc(arbiter),
                        hash,
                        confirm_by,
                        10,
                    )
                }))
            };
        let text = |r: Result<(), Box<dyn std::any::Any + Send>>| {
            r.expect_err("refused")
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_default()
        };
        assert!(
            text(open_with(&mut c, "dao.near", T0 + day, None)).contains("ERR_ARBITER_INVALID"),
            "the owner is not an arbiter"
        );
        assert!(
            text(open_with(&mut c, "alice.near", T0 + day, None)).contains("ERR_ARBITER_INVALID"),
            "a party is not the arbiter"
        );
        assert!(text(open_with(
            &mut c,
            "arb.near",
            T0 + 3599 * 1_000_000_000,
            None
        ))
        .contains("ERR_DEAL_WINDOW"));
        assert!(
            text(open_with(&mut c, "arb.near", T0 + 15 * day, None)).contains("ERR_DEAL_WINDOW")
        );
        assert!(
            text(open_with(&mut c, "arb.near", T0 + day, Some("XYZ".into())))
                .contains("agreement_hash")
        );
        assert!(
            load_deal(9).is_none() && bond_locked_of(&acc("alice.near")) == 0,
            "a refusal leaves nothing"
        );
        for i in 0..MAX_OPEN_DEALS_PER_MEMBER as u64 {
            at("solver.near", T0);
            c.open_deal(
                100 + i,
                [acc("alice.near"), acc("bob.near")],
                acc("arb.near"),
                None,
                T0 + day,
                1,
            );
        }
        assert!(text(open_with(&mut c, "arb.near", T0 + day, None)).contains("ERR_DEAL_LIMIT"));
    }

    // TC-CT-094-U03: confirmation releases the bond of the OTHER side; both confirm: the deal is gone
    #[test]
    fn a_confirmation_releases_the_bond_of_the_other_party_and_two_confirmations_close_the_deal() {
        let mut c = circle();
        open(&mut c, 1);
        at("mallory.near", T0 + 5);
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| c.confirm_deal(1)))
                .contains("ERR_DEAL_NOT_PARTY")
        );
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| c.confirm_deal(99)))
                .contains("ERR_DEAL_UNKNOWN")
        );
        at("alice.near", T0 + 5);
        c.confirm_deal(1);
        assert_eq!(
            (
                bond_locked_of(&acc("alice.near")),
                bond_locked_of(&acc("bob.near"))
            ),
            (BOND, 0),
            "alice said bob delivered: bob's bond is free, hers is not"
        );
        assert_eq!(c.get_deal(1).unwrap().locked, vec![true, false]);
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| c.confirm_deal(1)))
                .contains("ERR_DEAL_CLOSED"),
            "twice"
        );
        at("bob.near", T0 + 6);
        c.confirm_deal(1);
        assert_eq!(
            (
                bond_locked_of(&acc("alice.near")),
                bond_locked_of(&acc("bob.near"))
            ),
            (0, 0)
        );
        assert!(c.get_deal(1).is_none());
        assert_eq!(
            (
                deal_count_of(&acc("alice.near")),
                deal_count_of(&acc("bob.near"))
            ),
            (0, 0)
        );
        assert!(events().iter().filter(|e| *e == "bond_released").count() >= 1);
        assert_eq!(stakes(&c), (STAKE, STAKE), "a confirmation moves nothing");
    }

    // TC-CT-094-U04
    #[test]
    fn a_dispute_is_one_per_deal_inside_the_window_and_only_while_the_other_bond_is_locked() {
        let mut c = circle();
        open(&mut c, 1);
        let confirm_by = c.get_deal(1).unwrap().confirm_by.0;
        at("alice.near", confirm_by + 1);
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| c.dispute_deal(1, H.into())))
                .contains("ERR_DEAL_WINDOW_OVER")
        );
        at("alice.near", T0 + 10);
        assert!(panic_text(std::panic::AssertUnwindSafe(
            || c.dispute_deal(1, "nothex".into())
        ))
        .contains("ERR_BAD_HASH"));
        at("mallory.near", T0 + 10);
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| c.dispute_deal(1, H.into())))
                .contains("ERR_DEAL_NOT_PARTY")
        );
        at("alice.near", T0 + 10);
        c.dispute_deal(1, H.into());
        let view = c.get_deal(1).unwrap();
        assert_eq!(
            (view.state, view.complainant, view.accused, view.disputed_at),
            (
                DealState::Disputed,
                Some(acc("alice.near")),
                Some(acc("bob.near")),
                Some(U64(T0 + 10))
            )
        );
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| c.dispute_deal(1, H.into())))
                .contains("ERR_DEAL_ALREADY_DISPUTED")
        );
        at("bob.near", T0 + 11);
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| c.dispute_deal(1, H.into())))
                .contains("ERR_DEAL_ALREADY_DISPUTED"),
            "one dispute per deal"
        );
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| c.confirm_deal(1)))
                .contains("ERR_DEAL_ALREADY_DISPUTED"),
            "no confirmation after a dispute"
        );
        // a party that confirmed cannot dispute afterwards
        let mut d = circle();
        open(&mut d, 1);
        at("alice.near", T0 + 5);
        d.confirm_deal(1);
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| d.dispute_deal(1, H.into())))
                .contains("ERR_DEAL_CLOSED")
        );
    }

    fn disputed(c: &mut X207Circle, complainant: &str) {
        open(c, 1);
        at(complainant, T0 + 10);
        c.dispute_deal(1, H.into());
    }

    // TC-CT-094-U05: accused guilty: the bond of the accused goes to the stake of the complainant
    #[test]
    fn a_guilty_accused_pays_its_bond_to_the_complainant_and_the_complainant_gets_its_own_back() {
        let mut c = circle();
        disputed(&mut c, "alice.near");
        let total = stakes(&c).0 + stakes(&c).1;
        at("arb.near", T0 + 20);
        c.rule_deal(1, Verdict::AccusedGuilty);
        assert_eq!(
            stakes(&c),
            (STAKE + BOND, STAKE - BOND),
            "bob's bond moved to alice"
        );
        assert_eq!(
            (
                bond_locked_of(&acc("alice.near")),
                bond_locked_of(&acc("bob.near"))
            ),
            (0, 0)
        );
        assert!(c.get_deal(1).is_none() && deal_count_of(&acc("alice.near")) == 0);
        assert_eq!(
            stakes(&c).0 + stakes(&c).1,
            total,
            "the stakes of the circle did not change in sum"
        );
        let e = events();
        assert!(
            e.contains(&"deal_ruled".to_string()) && e.contains(&"bond_slashed".to_string()),
            "{e:?}"
        );
        // bob's limit was 800 and his stake is 900 now: still inside, so unchanged; the invariant holds
        let bob = c.members.get(&acc("bob.near")).unwrap();
        assert!(bob.limit <= bob.stake - bond_locked_of(&acc("bob.near")));
    }

    // TC-CT-094-U06: complaint false: HALF of the bond of the complainant goes to the accused, the rest is released
    #[test]
    fn a_false_complaint_costs_half_of_the_bond_of_the_complainant() {
        let mut c = circle();
        disputed(&mut c, "alice.near");
        at("arb.near", T0 + 20);
        c.rule_deal(1, Verdict::ComplaintFalse);
        assert_eq!(stakes(&c), (STAKE - BOND / 2, STAKE + BOND / 2));
        assert_eq!(
            (
                bond_locked_of(&acc("alice.near")),
                bond_locked_of(&acc("bob.near"))
            ),
            (0, 0)
        );
        let released: Vec<_> = get_logs()
            .iter()
            .filter(|l| l.contains("bond_released"))
            .cloned()
            .collect();
        assert!(
            released.iter().any(|l| l.contains("\"amount\":\"50\"")),
            "the other half of her bond is released: {released:?}"
        );
    }

    // TC-CT-094-U07
    #[test]
    fn no_fault_releases_both_and_a_ruling_that_lowers_a_stake_lowers_the_limit_with_it() {
        let mut c = circle();
        disputed(&mut c, "bob.near");
        at("arb.near", T0 + 20);
        c.rule_deal(1, Verdict::NoFault);
        assert_eq!(stakes(&c), (STAKE, STAKE));
        assert_eq!(
            (
                bond_locked_of(&acc("alice.near")),
                bond_locked_of(&acc("bob.near"))
            ),
            (0, 0)
        );
        // alice at the edge: limit 950 on a stake of 1000, a deal with a bond of 50 would not fit; with a lower limit it does and a ruling keeps the invariant
        let mut d = circle();
        d.members.get_mut(&acc("alice.near")).unwrap().limit = 900;
        at("solver.near", T0);
        d.open_deal(
            1,
            [acc("alice.near"), acc("bob.near")],
            acc("arb.near"),
            None,
            T0 + 24 * 3600 * 1_000_000_000,
            100,
        );
        at("bob.near", T0 + 10);
        d.dispute_deal(1, H.into());
        at("arb.near", T0 + 20);
        d.rule_deal(1, Verdict::ComplaintFalse); // bob complained and lost half of his bond (50) to alice
        let bob = d.members.get(&acc("bob.near")).unwrap();
        assert_eq!(bob.stake, STAKE - 50);
        assert!(
            bob.limit <= bob.stake,
            "limit {} stake {}",
            bob.limit,
            bob.stake
        );
    }

    // TC-CT-094-U12 (review C-8, C-10): a party whose own bond is released cannot accuse; `confirm_by` belongs to `settle_deal`, not to `dispute_deal`
    #[test]
    fn a_party_that_confirmed_cannot_accuse_and_the_dispute_window_ends_before_settle_begins() {
        let mut c = circle();
        open(&mut c, 1);
        at("alice.near", T0 + 5);
        c.confirm_deal(1); // the bond of bob is released, the bond of alice is still locked
        at("bob.near", T0 + 6);
        let err = panic_text(std::panic::AssertUnwindSafe(|| c.dispute_deal(1, H.into())));
        assert!(err.contains("ERR_DEAL_NO_STAKE_AT_RISK"), "{err}");
        let mut d = circle();
        open(&mut d, 1);
        let confirm_by = d.get_deal(1).unwrap().confirm_by.0;
        at("alice.near", confirm_by);
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| d.dispute_deal(1, H.into())))
                .contains("ERR_DEAL_WINDOW_OVER"),
            "at confirm_by the window is over"
        );
        at("anybody.near", confirm_by);
        d.settle_deal(1);
    }

    // TC-CT-094-U13 (review C-7): the accused defaulted and the pool took its free stake; the ruling does not panic and moves what is left of the stake
    #[test]
    fn a_ruling_after_a_default_moves_only_what_the_stake_still_holds() {
        let mut c = circle();
        disputed(&mut c, "alice.near");
        // a default took everything above the bond of bob, and the fees took 30 of the bond itself
        c.members.get_mut(&acc("bob.near")).unwrap().stake = BOND - 30;
        c.members.get_mut(&acc("bob.near")).unwrap().limit = 0;
        at("arb.near", T0 + 20);
        c.rule_deal(1, Verdict::AccusedGuilty);
        assert_eq!(stakes(&c), (STAKE + BOND - 30, 0));
        assert_eq!(bond_locked_of(&acc("bob.near")), 0);
        assert!(c.get_deal(1).is_none());
    }

    // TC-CT-094-U14 (review C-9): a ruling that lowers a limit says so with `limits_updated`
    #[test]
    fn a_ruling_that_lowers_a_limit_emits_limits_updated() {
        let mut c = circle();
        disputed(&mut c, "bob.near");
        c.members.get_mut(&acc("bob.near")).unwrap().limit = 980;
        at("arb.near", T0 + 20);
        c.rule_deal(1, Verdict::ComplaintFalse); // bob loses 50 of his bond: stake 950
        assert_eq!(c.members.get(&acc("bob.near")).unwrap().limit, 950);
        assert!(events().contains(&"limits_updated".to_string()));
    }

    // TC-CT-094-U08
    #[test]
    fn only_the_arbiter_rules_only_a_disputed_deal_only_inside_the_window() {
        let mut c = circle();
        open(&mut c, 1);
        at("arb.near", T0 + 20);
        assert!(panic_text(std::panic::AssertUnwindSafe(
            || c.rule_deal(1, Verdict::NoFault)
        ))
        .contains("ERR_DEAL_NOT_DISPUTED"));
        at("alice.near", T0 + 10);
        c.dispute_deal(1, H.into());
        for who in ["alice.near", "bob.near", "dao.near", "mallory.near"] {
            at(who, T0 + 20);
            assert!(
                panic_text(std::panic::AssertUnwindSafe(
                    || c.rule_deal(1, Verdict::AccusedGuilty)
                ))
                .contains("ERR_NOT_ARBITER"),
                "{who}"
            );
        }
        at("arb.near", T0 + 10 + RULING_WINDOW_NS);
        assert!(panic_text(std::panic::AssertUnwindSafe(
            || c.rule_deal(1, Verdict::AccusedGuilty)
        ))
        .contains("ERR_RULING_WINDOW"));
        assert!(panic_text(std::panic::AssertUnwindSafe(
            || c.rule_deal(99, Verdict::NoFault)
        ))
        .contains("ERR_DEAL_UNKNOWN"));
        assert_eq!(stakes(&c), (STAKE, STAKE), "nothing moved");
    }

    // TC-CT-094-U09: settle
    #[test]
    fn settle_releases_after_the_window_and_after_a_silent_arbiter_and_not_before() {
        let mut c = circle();
        open(&mut c, 1);
        at("anybody.near", T0 + 100);
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| c.settle_deal(1)))
                .contains("ERR_DEAL_NOT_DUE")
        );
        let confirm_by = c.get_deal(1).unwrap().confirm_by.0;
        at("anybody.near", confirm_by);
        c.settle_deal(1);
        assert_eq!(
            (
                bond_locked_of(&acc("alice.near")),
                bond_locked_of(&acc("bob.near"))
            ),
            (0, 0)
        );
        assert!(c.get_deal(1).is_none());
        assert!(get_logs()
            .iter()
            .any(|l| l.contains("\"reason\":\"elapsed\"")));
        assert_eq!(stakes(&c), (STAKE, STAKE));
        // a dispute and a silent arbiter
        let mut d = circle();
        disputed(&mut d, "alice.near");
        at("anybody.near", T0 + 10 + RULING_WINDOW_NS - 1);
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| d.settle_deal(1)))
                .contains("ERR_DEAL_NOT_DUE")
        );
        at("anybody.near", T0 + 10 + RULING_WINDOW_NS);
        d.settle_deal(1);
        assert_eq!(
            stakes(&d),
            (STAKE, STAKE),
            "a silent arbiter punishes nobody"
        );
        assert!(get_logs()
            .iter()
            .any(|l| l.contains("\"reason\":\"arbiter_silent\"")));
        assert!(d.get_deal(1).is_none());
    }

    // TC-CT-094-U10: a member with a locked bond cannot leave
    #[test]
    fn a_locked_bond_stops_the_exit() {
        let mut c = circle();
        open(&mut c, 1);
        assert!(panic_text(|| assert_no_bond(&acc("alice.near"))).contains("ERR_BOND_LOCKED"));
        at("anybody.near", c.get_deal(1).unwrap().confirm_by.0);
        c.settle_deal(1);
        assert_no_bond(&acc("alice.near"));
    }
}
