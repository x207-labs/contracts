//! F6 risk: `compute_limit`, `finalize_epoch` limit recalculation.

use near_sdk::json_types::U128;
use near_sdk::{env, near, AccountId};

use crate::member::{Member, MemberStatus};
use crate::params::Params;
use crate::state::{X207Circle, X207CircleExt};

const BPS: u128 = 10_000;

/// `min(limit_cap, stake * base_limit_ratio / 10000 + settled_history * limit_growth / 10000, stake)`
/// (ADR-08, ADR-18, spec 4.5). Rounds down; saturating arithmetic cannot panic, and the final
/// clamps bound the result by `limit_cap` and `stake` (I-01, I-02).
pub fn compute_limit(stake: u128, settled_history: u128, params: &Params) -> u128 {
    let base = stake.saturating_mul(params.base_limit_ratio as u128) / BPS;
    let growth = settled_history.saturating_mul(params.limit_growth as u128) / BPS;
    base.saturating_add(growth)
        .min(params.limit_cap.0)
        .min(stake)
}

/// P1 `limit_decay` (CT-053, spec 4.5 step 3): a member with no registration since `last_active_epoch` keeps
/// `(1 - limit_decay / 10000)^(epoch - last_active_epoch)` of its limit. Integer arithmetic: the limit is
/// multiplied and rounded down once per idle epoch. `limit_decay` 0, or a member active in `epoch`, changes nothing.
pub fn decay_limit(limit: u128, last_active_epoch: u64, epoch: u64, limit_decay: u16) -> u128 {
    if limit_decay == 0 || last_active_epoch >= epoch {
        return limit;
    }
    let keep = BPS - u128::from(limit_decay);
    let mut limit = limit;
    for _ in 0..(epoch - last_active_epoch) {
        if limit == 0 {
            break;
        }
        limit = limit.saturating_mul(keep) / BPS;
    }
    limit
}

/// Guard for every path that changes stake or limit: panics `ERR_LIMIT_EXCEEDS_STAKE` when
/// `limit > stake` (ADR-18, I-01).
pub fn assert_limit_invariant(member: &Member) {
    if member.limit > member.stake {
        env::panic_str(&format!(
            "ERR_LIMIT_EXCEEDS_STAKE: {} limit {} > stake {}",
            member.account_id, member.limit, member.stake
        ));
    }
}

#[near]
impl X207Circle {
    /// Stored limit of a member (kept equal to `compute_limit` by the stake and finalize paths);
    /// `"0"` for an account that is not a member.
    pub fn limit(&self, account_id: AccountId) -> U128 {
        U128(
            self.members
                .get(&account_id)
                .map_or(0, |m| self.member_now(m).limit),
        )
    }

    /// `max(0, limit - outstanding_debt - headroom reserved for bundle legs of the running epoch)` for an `active` member; `"0"` for anyone else (spec section 5).
    pub fn headroom(&self, account_id: AccountId) -> U128 {
        let limit = match self.members.get(&account_id).map(|m| self.member_now(m)) {
            Some(m) if m.status == MemberStatus::Active => m.limit,
            _ => return U128(0),
        };
        let debt = self.outstanding_debt.get(&account_id).copied().unwrap_or(0);
        U128(
            limit
                .saturating_sub(debt)
                .saturating_sub(crate::bundle::reserved(self.current_epoch, &account_id)),
        )
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{testing_env, PublicKey};

    use super::*;
    use crate::params::{Admission, Fees};

    const CREDIT: u128 = 1_000_000;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn p(f: impl FnOnce(&mut Params)) -> Params {
        let mut params = Params::default();
        f(&mut params);
        params
    }

    fn member(stake: u128, limit: u128) -> Member {
        Member {
            account_id: acc("translator.near"),
            pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp"
                .parse::<PublicKey>()
                .unwrap(),
            terms_hash: "a".repeat(64),
            stake,
            limit,
            status: MemberStatus::Active,
            admitted: false,
            joined_at: 1,
            settled_history: 0,
            last_active_epoch: 1,
            exit_requested_epoch: None,
            withdraw_pending: false,
            reactivate_epoch: None,
        }
    }

    // TC-CT-015-01
    #[test]
    fn tc_ct_015_01_base_formula() {
        assert_eq!(
            compute_limit(100 * CREDIT, 0, &Params::default()),
            80 * CREDIT
        );
    }

    // TC-CT-015-02
    #[test]
    fn tc_ct_015_02_growth_from_settlement() {
        assert_eq!(
            compute_limit(100 * CREDIT, 35 * CREDIT, &Params::default()),
            83_500_000
        );
    }

    // TC-CT-015-03
    #[test]
    fn tc_ct_015_03_clamped_to_stake() {
        let params = p(|x| x.base_limit_ratio = 10_000);
        assert_eq!(
            compute_limit(100 * CREDIT, 50 * CREDIT, &params),
            100 * CREDIT
        );
    }

    // TC-CT-015-04
    #[test]
    fn tc_ct_015_04_clamped_to_limit_cap() {
        assert_eq!(
            compute_limit(2_000 * CREDIT, 0, &Params::default()),
            1_000 * CREDIT
        );
    }

    // TC-CT-015-05
    #[test]
    fn tc_ct_015_05_minimum_limit_cap() {
        let params = p(|x| x.limit_cap = U128(1_000_000));
        assert_eq!(compute_limit(100 * CREDIT, 0, &params), CREDIT);
    }

    // TC-CT-015-06
    #[test]
    fn tc_ct_015_06_zero_stake() {
        assert_eq!(compute_limit(0, 0, &Params::default()), 0);
    }

    // TC-CT-015-07
    #[test]
    fn tc_ct_015_07_zero_ratios() {
        let params = p(|x| {
            x.base_limit_ratio = 0;
            x.limit_growth = 0;
        });
        assert_eq!(compute_limit(100 * CREDIT, 35 * CREDIT, &params), 0);
    }

    // TC-CT-015-08
    #[test]
    fn tc_ct_015_08_rounds_down() {
        assert_eq!(compute_limit(1, 0, &Params::default()), 0);
    }

    // TC-CT-015-09
    #[test]
    fn tc_ct_015_09_no_overflow() {
        let stake = 10u128.pow(30);
        assert_eq!(compute_limit(stake, 0, &Params::default()), 1_000_000_000);
        assert_eq!(
            compute_limit(u128::MAX, u128::MAX, &Params::default()),
            1_000_000_000
        );
    }

    // TC-CT-015-10: deterministic pseudo-random sweep over the ADR-20 ranges (no proptest dependency).
    #[test]
    fn tc_ct_015_10_invariants_hold_for_random_inputs() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move |max: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % max.saturating_add(1).max(1)
        };
        for _ in 0..20_000 {
            let params = Params {
                epoch_length: 3600,
                payment_window: 1800,
                proposal_window: 600,
                base_limit_ratio: next(10_000) as u16,
                limit_growth: next(10_000) as u16,
                limit_cap: U128(1_000_000 + next(99_999_000_000) as u128),
                limit_decay: 0,
                fees: Fees {
                    clearing_fee_bps: 0,
                    solver_fee_bps: 0,
                },
                admission: Admission::Open,
                max_search_share_bps: 500,
                bond_threshold: near_sdk::json_types::U128(100_000_000),
                bond_bps: 2000,
                prune_delay_epochs: 2,
                relayer_fee: near_sdk::json_types::U128(0),
            };
            let stake = (next(u64::MAX - 1) as u128) * (next(1_000_000) as u128);
            let history = (next(u64::MAX - 1) as u128) * (next(1_000_000) as u128);
            let limit = compute_limit(stake, history, &params);
            assert!(limit <= stake, "limit {limit} > stake {stake}");
            assert!(limit <= params.limit_cap.0, "limit {limit} > cap");
        }
    }

    #[test]
    fn guard_accepts_limit_up_to_stake_and_rejects_above() {
        assert_limit_invariant(&member(100, 100));
        assert_limit_invariant(&member(100, 0));
        let err =
            std::panic::catch_unwind(|| assert_limit_invariant(&member(100, 101))).unwrap_err();
        let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(msg.contains("ERR_LIMIT_EXCEEDS_STAKE"), "{msg}");
    }

    #[test]
    fn limit_view_member_and_outsider() {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        let mut c = X207Circle::with_config(
            acc("dao.near"),
            "agents-main".to_string(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            1_000,
        );
        c.members
            .insert(acc("translator.near"), member(100 * CREDIT, 80 * CREDIT));
        assert_eq!(c.limit(acc("translator.near")), U128(80_000_000));
        assert_eq!(c.limit(acc("outsider.near")), U128(0));
    }

    fn circle_with(members: &[(&str, MemberStatus, u128)], debts: &[(&str, u128)]) -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        let mut c = X207Circle::with_config(
            acc("dao.near"),
            "agents-main".to_string(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            1_000,
        );
        for (id, status, limit) in members {
            let mut m = member(100 * CREDIT, *limit);
            m.account_id = acc(id);
            m.status = *status;
            c.members.insert(acc(id), m);
        }
        for (id, debt) in debts {
            c.outstanding_debt.insert(acc(id), *debt);
        }
        c
    }

    // TC-CT-021-01 (value check: limit 80 minus debt 25 = 55)
    #[test]
    fn tc_ct_021_01_headroom_is_limit_minus_debt() {
        let c = circle_with(
            &[("translator.near", MemberStatus::Active, 80 * CREDIT)],
            &[("translator.near", 25 * CREDIT)],
        );
        assert_eq!(c.headroom(acc("translator.near")), U128(55 * CREDIT));
        assert_eq!(
            c.headroom(acc("translator.near")).0 + c.outstanding_debt(acc("translator.near")).0,
            c.limit(acc("translator.near")).0
        );
    }

    // TC-CT-021-03
    #[test]
    fn tc_ct_021_03_not_active_gives_zero() {
        let c = circle_with(
            &[
                ("pending.near", MemberStatus::Pending, 80 * CREDIT),
                ("suspended.near", MemberStatus::Suspended, 80 * CREDIT),
            ],
            &[],
        );
        for id in ["outsider.near", "pending.near", "suspended.near"] {
            assert_eq!(c.headroom(acc(id)), U128(0), "{id}");
        }
    }

    // TC-CT-021-04
    #[test]
    fn tc_ct_021_04_never_negative() {
        let c = circle_with(
            &[("translator.near", MemberStatus::Active, 20 * CREDIT)],
            &[("translator.near", 55 * CREDIT)],
        );
        assert_eq!(c.headroom(acc("translator.near")), U128(0));
    }

    #[test]
    fn headroom_without_debt_equals_limit() {
        let c = circle_with(
            &[("translator.near", MemberStatus::Active, 80 * CREDIT)],
            &[],
        );
        assert_eq!(c.headroom(acc("translator.near")), U128(80 * CREDIT));
    }

    #[test]
    fn ct_053_decay_limit_arithmetic() {
        use super::decay_limit;
        assert_eq!(
            decay_limit(80 * CREDIT, 5, 5, 1_000),
            80 * CREDIT,
            "active in the epoch"
        );
        assert_eq!(
            decay_limit(80 * CREDIT, 9, 5, 1_000),
            80 * CREDIT,
            "last_active_epoch after the epoch"
        );
        assert_eq!(
            decay_limit(80 * CREDIT, 4, 5, 0),
            80 * CREDIT,
            "limit_decay 0"
        );
        assert_eq!(decay_limit(80 * CREDIT, 4, 5, 1_000), 72 * CREDIT);
        assert_eq!(
            decay_limit(80 * CREDIT, 3, 5, 1_000),
            64_800_000,
            "80 * 0.9 * 0.9"
        );
        assert_eq!(
            decay_limit(100, 0, 3, 5_000),
            12,
            "each step rounds down: 50, 25, 12"
        );
        assert_eq!(
            decay_limit(u128::MAX, 0, 1_000_000, 5_000),
            0,
            "long idle ends at zero without overflow"
        );
        assert_eq!(decay_limit(0, 0, 9, 1_000), 0);
    }
}
