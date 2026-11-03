//! `prune_epoch` (spec section 4, ADR-31 c): gives back the storage of a `finalized` epoch.
//!
//! What goes: the pair entries of the epoch (D2/D3) with the edge index, the participant totals with the participant index, and the bundles of the epoch with
//! their reservations, per-participant counters and hash markers (D8, security review F2). What stays: the epoch record with its counts, the clearing
//! proposal, and every D5 position with its index, so `claim`, `net_position` and the defaulted/claimed history work as before. The call is permissionless and
//! counts removed storage entries, so `max_items` bounds its gas; call it again until it returns `0`.

use near_sdk::{env, near, require};

use crate::bundle::{bundle_counts, bundles, epoch_bundle_ids, hash_key, reserves, set_epoch_bundle_ids};
use crate::clearing::EpochPhase;
use crate::state::{X207Circle, X207CircleExt};
use crate::types::EpochId;

/// Most entries one call may remove, whatever `max_items` says: keeps a call far below the gas limit.
pub const MAX_PRUNE_ITEMS_PER_CALL: u32 = 200;

#[near]
impl X207Circle {
    /// Removes up to `max_items` entries of a `finalized` epoch (see the module doc) and returns how many it removed. `ERR_UNKNOWN_EPOCH`, `ERR_NOT_FINALIZED`.
    pub fn prune_epoch(&mut self, epoch: EpochId, max_items: u32) -> u32 {
        let e = self.epoch_or_panic(epoch);
        require!(e.phase == EpochPhase::Finalized, format!("ERR_NOT_FINALIZED: epoch {epoch}"));
        let budget = max_items.min(MAX_PRUNE_ITEMS_PER_CALL);
        let mut removed = 0u32;

        // pairs and the edge index, last edge first
        while removed < budget {
            let Some((payer, payee)) = self.epoch_mut_or_panic(epoch).edges.pop() else { break };
            self.pairs.remove(&(epoch, payer, payee));
            removed += 1;
        }
        // participant totals and the participant index
        while removed < budget {
            let Some(account) = self.epoch_mut_or_panic(epoch).participants.pop() else { break };
            self.totals.remove(&(epoch, account));
            removed += 1;
        }
        // bundles: the record, the hash marker, the reservation of each payer and the counter of each participant
        if removed < budget {
            let mut ids = epoch_bundle_ids(epoch);
            let mut map = bundles();
            let mut reserved = reserves();
            let mut counts = bundle_counts();
            // a bundle costs its record, its hash marker and three entries per leg (reservation and two counters); a call that already
            // removed something leaves a bundle that would not fit for the next call
            while removed < budget {
                let Some(&id) = ids.last() else { break };
                let legs = map.get(&id).map_or(0, |b| b.legs.len() as u32);
                let cost = 2 + 3 * legs;
                if removed > 0 && removed + cost > budget {
                    break;
                }
                ids.pop();
                if let Some(bundle) = map.remove(&id) {
                    env::storage_remove(&hash_key(&bundle.terms_hash));
                    for leg in &bundle.legs {
                        reserved.remove(&(epoch, leg.payer.clone()));
                        counts.remove(&(epoch, leg.payer.clone()));
                        counts.remove(&(epoch, leg.payee.clone()));
                    }
                }
                removed += cost;
            }
            set_epoch_bundle_ids(epoch, &ids);
            map.flush();
            reserved.flush();
            counts.flush();
        }
        self.pairs.flush();
        self.totals.flush();
        self.epochs.flush();
        removed
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::json_types::U128;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{testing_env, AccountId};

    use super::*;
    use crate::bundle::{Bundle, BundleLeg, BundleStatus};
    use crate::params::Params;
    use crate::settlement::{Position, PositionRole, PositionStatus};
    use crate::voucher::{PairState, ParticipantTotals};

    const CREDIT: u128 = 1_000_000;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("expected a panic");
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    fn position(role: PositionRole, status: PositionStatus, amount: u128) -> Position {
        Position { net: amount as i128, role, status, amount, recovered: 0, payment_deadline: 9, updated_at_block: 5 }
    }

    /// Epoch 1 `finalized` with three pairs, three participants, two positions (a claimable creditor and a defaulted debtor) and two bundles.
    fn finalized() -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        let mut c = X207Circle::with_config(acc("dao.near"), "agents-main".into(), acc("usdt.near"), acc("intents.near"), Params::default(), 1_000);
        let names = ["a.near", "b.near", "c.near"];
        let pairs = [("a.near", "b.near"), ("b.near", "c.near"), ("c.near", "a.near")];
        for (payer, payee) in pairs {
            c.pairs.insert((1, acc(payer), acc(payee)), PairState { cumulative: 10 * CREDIT, last_seq: 1, cleared: 0 });
            c.epochs.get_mut(&1).unwrap().edges.push((acc(payer), acc(payee)));
        }
        for name in names {
            c.totals.insert((1, acc(name)), ParticipantTotals { owed_out: 10 * CREDIT, owed_in: 10 * CREDIT });
            c.epochs.get_mut(&1).unwrap().participants.push(acc(name));
        }
        c.positions.insert((1, acc("a.near")), position(PositionRole::Creditor, PositionStatus::Claimable, 7 * CREDIT));
        c.positions.insert((1, acc("b.near")), position(PositionRole::Debtor, PositionStatus::Defaulted, 7 * CREDIT));
        {
            let e = c.epochs.get_mut(&1).unwrap();
            e.position_accounts.push(acc("a.near"));
            e.position_accounts.push(acc("b.near"));
            e.edge_count = 3;
            e.participant_count = 3;
            e.pool = 7 * CREDIT;
            e.pool_final = 7 * CREDIT;
            e.claimable_final = 7 * CREDIT;
        }
        let mut map = crate::bundle::bundles();
        let mut ids: Vec<u64> = Vec::new();
        let mut reserved = crate::bundle::reserves();
        let mut counts = crate::bundle::bundle_counts();
        for id in 1..=2u64 {
            let terms_hash = [id as u8; 32];
            let legs = vec![
                BundleLeg { payer: acc("a.near"), payee: acc("b.near"), max_amount: U128(CREDIT) },
                BundleLeg { payer: acc("b.near"), payee: acc("a.near"), max_amount: U128(CREDIT) },
            ];
            map.insert(id, Bundle { terms_hash, epoch: 1, registered: vec![0; 2], legs, solver: acc("solver.near"), status: BundleStatus::Active });
            ids.push(id);
            env::storage_write(&hash_key(&terms_hash), &id.to_le_bytes());
            for who in ["a.near", "b.near"] {
                reserved.insert((1, acc(who)), CREDIT);
                counts.insert((1, acc(who)), 2);
            }
        }
        map.flush();
        crate::bundle::set_epoch_bundle_ids(1, &ids);
        reserved.flush();
        counts.flush();
        c.epochs.get_mut(&1).unwrap().phase = EpochPhase::Finalized;
        c.epochs.flush();
        c.pairs.flush();
        c.totals.flush();
        c.positions.flush();
        c
    }

    fn entries(c: &X207Circle) -> (usize, usize, usize, usize) {
        let pairs = ["a.near", "b.near", "c.near"].iter().flat_map(|p| ["a.near", "b.near", "c.near"].iter().map(move |q| (p, q))).filter(|(p, q)| c.pairs.get(&(1, acc(p), acc(q))).is_some()).count();
        let totals = ["a.near", "b.near", "c.near"].iter().filter(|n| c.totals.get(&(1, acc(n))).is_some()).count();
        let bundle_rows = (1..=2u64).filter(|id| bundles().get(id).is_some()).count();
        let counters = ["a.near", "b.near"].iter().filter(|n| bundle_counts().get(&(1, acc(n))).is_some()).count();
        (pairs, totals, bundle_rows, counters)
    }

    // TC-CT-085-01 (ADR-31 c): only a finalized epoch is pruned
    #[test]
    fn an_epoch_that_is_not_finalized_is_refused() {
        for phase in [EpochPhase::Open, EpochPhase::Clearing, EpochPhase::PaymentWindow] {
            let mut c = finalized();
            c.epochs.get_mut(&1).unwrap().phase = phase;
            let text = panic_text(std::panic::AssertUnwindSafe(|| {
                c.prune_epoch(1, 10);
            }));
            assert!(text.contains("ERR_NOT_FINALIZED"), "{phase:?}: {text}");
        }
        let mut c = finalized();
        assert!(panic_text(std::panic::AssertUnwindSafe(|| {
            c.prune_epoch(9, 10);
        }))
        .contains("ERR_UNKNOWN_EPOCH"));
    }

    // TC-CT-085-02: max_items bounds one call; calls add up to everything; the last call returns 0
    #[test]
    fn max_items_bounds_a_call_and_repeated_calls_clear_the_epoch() {
        let mut c = finalized();
        assert_eq!(entries(&c), (3, 3, 2, 2));
        assert_eq!(c.prune_epoch(1, 0), 0);
        assert_eq!(c.prune_epoch(1, 2), 2);
        assert_eq!(entries(&c).0, 1);
        let mut total = 2;
        loop {
            let n = c.prune_epoch(1, 2);
            assert!(n <= 2 || n == 8, "a bundle of two legs costs 8 and goes alone");
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, 3 + 3 + 2 * 8, "pairs, participants, two bundles of two legs");
        assert_eq!(entries(&c), (0, 0, 0, 0));
        let e = c.epochs.get(&1).unwrap();
        assert_eq!((e.edges.len(), e.participants.len()), (0, 0));
        assert_eq!(crate::bundle::reserved(1, &acc("a.near")), 0);
        assert!(env::storage_read(&hash_key(&[1u8; 32])).is_none());
        assert_eq!(c.prune_epoch(1, 50), 0, "nothing left");
    }

    // TC-CT-085-03 (required): a claim that was not taken stays possible. Positions, their index and the epoch figures `claim` reads are untouched.
    #[test]
    fn positions_and_the_claim_inputs_survive_a_full_prune() {
        let mut c = finalized();
        let before = (c.positions.get(&(1, acc("a.near"))).cloned(), c.positions.get(&(1, acc("b.near"))).cloned());
        let epoch = c.epochs.get(&1).unwrap();
        let figures = (epoch.phase, epoch.pool, epoch.pool_final, epoch.claimable_final, epoch.shortfall, epoch.edge_count, epoch.participant_count, epoch.position_accounts.len());
        while c.prune_epoch(1, 200) > 0 {}
        assert_eq!((c.positions.get(&(1, acc("a.near"))).cloned(), c.positions.get(&(1, acc("b.near"))).cloned()), before);
        assert_eq!(before.0.as_ref().map(|p| (p.status, p.amount)), Some((PositionStatus::Claimable, 7 * CREDIT)));
        let epoch = c.epochs.get(&1).unwrap();
        assert_eq!((epoch.phase, epoch.pool, epoch.pool_final, epoch.claimable_final, epoch.shortfall, epoch.edge_count, epoch.participant_count, epoch.position_accounts.len()), figures);
        assert_eq!(c.net_position(1, acc("a.near")).map(|p| p.amount.0), Some(7 * CREDIT));
    }

    // TC-CT-085-04: another epoch keeps all its entries
    #[test]
    fn other_epochs_are_not_touched() {
        let mut c = finalized();
        c.pairs.insert((2, acc("a.near"), acc("b.near")), PairState { cumulative: CREDIT, last_seq: 1, cleared: 0 });
        c.totals.insert((2, acc("a.near")), ParticipantTotals { owed_out: CREDIT, owed_in: 0 });
        c.pairs.flush();
        c.totals.flush();
        while c.prune_epoch(1, 200) > 0 {}
        assert!(c.pairs.get(&(2, acc("a.near"), acc("b.near"))).is_some());
        assert!(c.totals.get(&(2, acc("a.near"))).is_some());
    }

    // TC-CT-085-05: anybody may call it
    #[test]
    fn anybody_can_prune() {
        let mut c = finalized();
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("stranger.near")).block_timestamp(5_000).build());
        assert!(c.prune_epoch(1, 1) > 0);
    }
}
