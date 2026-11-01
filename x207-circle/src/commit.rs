//! `commit_epoch` (CT-028, spec 4.4): applies the stored proposal to D2, writes the D5 positions,
//! sets the payment deadline, and emits `clearing_applied` and `epoch_committed`.
//!
//! Fees (P1, CT-051/052) are not charged here yet: `clearing_fee` and `solver_fee` are `"0"`.
//! Event logs: near caps the total log length of one call at 16 KiB, so a commit with more than
//! about 60 edges at 64-character accounts does not fit the chunked events; see the CT-004 note.

use near_sdk::json_types::{I128, U128, U64};
use near_sdk::{env, near, AccountId};

use crate::events::{emit_chunked, emit_clearing_applied, EpochCommitted, PositionEntry};
use crate::settlement::{Position, PositionRole, PositionStatus};
use crate::state::{X207Circle, X207CircleExt};
use crate::types::{ClearingEdge, EpochId};

fn checked_signed(value: u128) -> i128 {
    i128::try_from(value).unwrap_or_else(|_| env::panic_str("ERR_D2_ACCOUNTING: amount exceeds i128"))
}

impl X207Circle {
    /// Applies the stored proposal of `epoch` to the pair map and marks it committed with its
    /// edges dropped. Returns the solver, `cleared_total`, and the edges that were applied.
    fn apply_proposal(&mut self, epoch: EpochId) -> (Option<AccountId>, u128, Vec<ClearingEdge>) {
        let Some(proposal) = self.proposals.get_mut(&epoch) else { return (None, 0, vec![]) };
        let edges = std::mem::take(&mut proposal.edges);
        proposal.committed = true;
        let (solver, cleared_total) = (proposal.solver.clone(), proposal.cleared_total.0);
        for edge in &edges {
            let key = (epoch, edge.debtor.clone(), edge.creditor.clone());
            let pair = self.pairs.get_mut(&key).unwrap_or_else(|| env::panic_str("ERR_EDGE_NOT_IN_SNAPSHOT"));
            pair.cleared += edge.cleared.0;
        }
        (Some(solver), cleared_total, edges)
    }
}

#[near]
impl X207Circle {
    /// Permissionless after the proposal window. Net positions do not change by clearing: every
    /// member is a debtor `due`, a creditor `claimable`, or has no D5 record (net zero).
    pub fn commit_epoch(&mut self, epoch: EpochId) {
        let now = env::block_timestamp();
        let block = env::block_height();
        self.epoch_mut_or_panic(epoch).start_payment_window(now);

        let (solver, cleared_total, edges) = self.apply_proposal(epoch);
        let e = self.epoch_or_panic(epoch);
        let payment_deadline = e.payment_deadline.expect("payment_window epoch has payment_deadline");
        let committed_at = e.committed_at.expect("committed epoch has committed_at");
        let participants: Vec<AccountId> = e.participants.iter().cloned().collect();

        let (mut total_due, mut total_claimable) = (0u128, 0u128);
        let mut entries = Vec::with_capacity(participants.len());
        for account in participants {
            let totals = self.totals.get(&(epoch, account.clone())).cloned().expect("participant has totals");
            let net = checked_signed(totals.owed_in) - checked_signed(totals.owed_out);
            let (role, status, amount) = match net {
                n if n < 0 => (PositionRole::Debtor, PositionStatus::Due, n.unsigned_abs()),
                n if n > 0 => (PositionRole::Creditor, PositionStatus::Claimable, n.unsigned_abs()),
                _ => (PositionRole::Debtor, PositionStatus::Due, 0),
            };
            let due = if net < 0 { amount } else { 0 };
            // Gross obligations of this epoch leave the debt, only the residual stays (spec 4.4, step 3).
            let debt = self.outstanding_debt.get(&account).copied().unwrap_or(0);
            let debt = debt
                .checked_sub(totals.owed_out)
                .unwrap_or_else(|| env::panic_str("ERR_D2_ACCOUNTING: outstanding_debt below epoch obligations"))
                + due;
            self.outstanding_debt.insert(account.clone(), debt);

            if net == 0 {
                entries.push(PositionEntry {
                    account_id: account,
                    net: I128(0),
                    role: "none".into(),
                    status: None,
                    amount: U128(0),
                    outstanding_debt: U128(debt),
                });
                continue;
            }
            let (wire_role, wire_status) = match status {
                PositionStatus::Due => ("debtor", "due"),
                _ => ("creditor", "claimable"),
            };
            if net < 0 {
                total_due += amount;
            } else {
                total_claimable += amount;
            }
            entries.push(PositionEntry {
                account_id: account.clone(),
                net: I128(net),
                role: wire_role.into(),
                status: Some(wire_status.into()),
                amount: U128(amount),
                outstanding_debt: U128(debt),
            });
            self.put_position(
                epoch,
                account,
                Position { net, role, status, amount, recovered: 0, payment_deadline, updated_at_block: block },
            );
        }
        assert_eq!(total_due, total_claimable, "I-09: sum of due must equal sum of claimable");
        self.epoch_mut_or_panic(epoch).total_cleared = cleared_total;

        emit_clearing_applied(epoch, solver, U128(cleared_total), &edges, U128(0), U128(0));
        emit_chunked(&entries, |positions, chunk, chunks| EpochCommitted {
            epoch,
            committed_at: U64(committed_at),
            payment_deadline: U64(payment_deadline),
            total_due: U128(total_due),
            total_claimable: U128(total_claimable),
            positions,
            chunk,
            chunks,
        });
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::serde_json::{self, Value};
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::testing_env;

    use super::*;
    use crate::params::Params;
    use crate::voucher::{PairState, ParticipantTotals};

    const CREDIT: u128 = 1_000_000;
    const SEC: u64 = 1_000_000_000;
    const T0: u64 = 1_000;
    const T_CUT: u64 = T0 + 3600 * SEC;
    const T_PROP: u64 = T_CUT + 600 * SEC;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn at(ns: u64, predecessor: &str) {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc(predecessor)).block_timestamp(ns).block_height(500).build());
    }

    fn edge(d: &str, c: &str, cleared: u128) -> ClearingEdge {
        ClearingEdge { debtor: acc(d), creditor: acc(c), cleared: U128(cleared) }
    }

    /// Stands in for `register_cumulative`: the same D2 bookkeeping without signatures.
    fn register(c: &mut X207Circle, epoch: EpochId, payer: &str, payee: &str, amount: u128) {
        c.pairs.insert((epoch, acc(payer), acc(payee)), PairState { cumulative: amount, last_seq: 1, cleared: 0 });
        let debt = c.outstanding_debt.get(&acc(payer)).copied().unwrap_or(0);
        c.outstanding_debt.insert(acc(payer), debt + amount);
        for (account, out, inn) in [(payer, amount, 0), (payee, 0, amount)] {
            let key = (epoch, acc(account));
            let is_new = c.totals.get(&key).is_none();
            let mut totals = c.totals.get(&key).cloned().unwrap_or_else(ParticipantTotals::default);
            totals.owed_out += out;
            totals.owed_in += inn;
            c.totals.insert(key, totals);
            if is_new {
                let e = c.epochs.get_mut(&epoch).unwrap();
                e.participants.push(acc(account));
                e.participant_count += 1;
            }
        }
        let e = c.epochs.get_mut(&epoch).unwrap();
        e.edges.push((acc(payer), acc(payee)));
        e.edge_count += 1;
        e.total_obligation += amount;
    }

    fn circle() -> X207Circle {
        at(T0, "deployer.near");
        X207Circle::with_config(acc("dao.near"), "agents-main".into(), acc("usdt.near"), acc("intents.near"), Params::default(), T0)
    }

    /// S1 of the test case file, registered but not yet closed.
    fn s1() -> X207Circle {
        let mut c = circle();
        register(&mut c, 1, "scraper.near", "gpu.near", 60 * CREDIT);
        register(&mut c, 1, "gpu.near", "translator.near", 25 * CREDIT);
        register(&mut c, 1, "translator.near", "scraper.near", 25 * CREDIT);
        c
    }

    fn p_s1() -> Vec<ClearingEdge> {
        vec![
            edge("scraper.near", "gpu.near", 25 * CREDIT),
            edge("gpu.near", "translator.near", 25 * CREDIT),
            edge("translator.near", "scraper.near", 25 * CREDIT),
        ]
    }

    /// `S1.close`, then optionally `S1.sub`; leaves the clock at `proposal_deadline`.
    fn closed(mut c: X207Circle, proposal: Option<Vec<ClearingEdge>>) -> X207Circle {
        at(T_CUT, "keeper.near");
        c.close_epoch(1);
        if let Some(edges) = proposal {
            at(T_CUT + SEC, "solver1.near");
            c.submit_clearing(1, edges);
        }
        at(T_PROP, "keeper.near");
        c
    }

    fn net(c: &X207Circle, epoch: EpochId, account: &str) -> i128 {
        let t = c.totals.get(&(epoch, acc(account))).cloned().unwrap_or_default();
        t.owed_in as i128 - t.owed_out as i128
    }

    fn position(c: &X207Circle, account: &str) -> Option<crate::settlement::PositionView> {
        c.list_positions(1, 0, 100).into_iter().find(|p| p.account_id == acc(account))
    }

    fn log_events() -> Vec<Value> {
        get_logs()
            .iter()
            .filter_map(|l| l.strip_prefix("EVENT_JSON:"))
            .map(|body| serde_json::from_str::<Value>(body).unwrap())
            .collect()
    }

    fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).unwrap_err();
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    // TC-CT-028-01
    #[test]
    fn valid_commit_applies_clearing_and_writes_positions() {
        let mut c = closed(s1(), Some(p_s1()));
        c.commit_epoch(1);

        let ob = |d: &str, cr: &str| c.obligation(1, acc(d), acc(cr)).unwrap();
        assert_eq!(ob("scraper.near", "gpu.near").obligation.0, 35 * CREDIT);
        assert_eq!(ob("gpu.near", "translator.near").obligation.0, 0);
        assert_eq!(ob("translator.near", "scraper.near").obligation.0, 0);
        assert_eq!(ob("gpu.near", "translator.near").cleared.0, 25 * CREDIT);

        let proposal = c.get_clearing_proposal(1).unwrap();
        assert!(proposal.committed && proposal.edges.is_empty());
        assert_eq!((proposal.edge_count, proposal.cleared_total.0), (3, 75 * CREDIT));

        let scraper = position(&c, "scraper.near").unwrap();
        assert_eq!((scraper.role, scraper.status, scraper.amount.0, scraper.net.0), (PositionRole::Debtor, PositionStatus::Due, 35 * CREDIT, -35 * CREDIT as i128));
        let gpu = position(&c, "gpu.near").unwrap();
        assert_eq!((gpu.role, gpu.status, gpu.amount.0), (PositionRole::Creditor, PositionStatus::Claimable, 35 * CREDIT));
        assert!(position(&c, "translator.near").is_none());

        let e = c.get_epoch(Some(1)).unwrap();
        assert_eq!(e.committed_at.unwrap().0, T_PROP);
        assert_eq!(e.payment_deadline.unwrap().0, T_PROP + 1800 * SEC);
        assert_eq!(scraper.payment_deadline.0, T_PROP + 1800 * SEC);
        assert_eq!(scraper.updated_at_block, 500);
        assert_eq!(e.total_cleared.0, 75 * CREDIT);
        assert_eq!(serde_json::to_value(e.phase).unwrap(), "payment_window");

        assert_eq!(c.outstanding_debt(acc("scraper.near")).0, 35 * CREDIT);
        assert_eq!(c.outstanding_debt(acc("gpu.near")).0, 0);
        assert_eq!(c.outstanding_debt(acc("translator.near")).0, 0);
    }

    // TC-CT-028-02, TC-CT-028-11 (single chunk), K-19, K-35
    #[test]
    fn commit_events() {
        let mut c = closed(s1(), Some(p_s1()));
        let _ = get_logs();
        c.commit_epoch(1);
        let events = log_events();
        assert_eq!(events.iter().map(|e| e["event"].as_str().unwrap()).collect::<Vec<_>>(), ["clearing_applied", "epoch_committed"]);

        let applied = &events[0]["data"][0];
        assert_eq!(applied["solver"], "solver1.near");
        assert_eq!(applied["cleared_total"], "75000000");
        assert_eq!(applied["edges"].as_array().unwrap().len(), 3);
        assert_eq!((applied["clearing_fee"].as_str(), applied["solver_fee"].as_str()), (Some("0"), Some("0")));
        assert_eq!((applied["chunk"].as_u64(), applied["chunks"].as_u64()), (Some(0), Some(1)));

        let committed = &events[1]["data"][0];
        assert_eq!(committed["total_due"], "35000000");
        assert_eq!(committed["total_claimable"], "35000000");
        assert_eq!(committed["committed_at"], T_PROP.to_string());
        assert_eq!(committed["payment_deadline"], (T_PROP + 1800 * SEC).to_string());
        assert_eq!((committed["chunk"].as_u64(), committed["chunks"].as_u64()), (Some(0), Some(1)));
        let positions = committed["positions"].as_array().unwrap();
        assert_eq!(positions.len(), 3);
        let by = |name: &str| positions.iter().find(|p| p["account_id"] == name).unwrap();
        assert_eq!((by("scraper.near")["role"].as_str(), by("scraper.near")["status"].as_str(), by("scraper.near")["outstanding_debt"].as_str()), (Some("debtor"), Some("due"), Some("35000000")));
        assert_eq!((by("gpu.near")["role"].as_str(), by("gpu.near")["status"].as_str(), by("gpu.near")["amount"].as_str()), (Some("creditor"), Some("claimable"), Some("35000000")));
        let zero = by("translator.near");
        assert_eq!(zero["role"], "none");
        assert!(zero["status"].is_null());
        assert_eq!((zero["amount"].as_str(), zero["outstanding_debt"].as_str()), (Some("0"), Some("0")));
    }

    // TC-CT-028-03
    #[test]
    fn net_positions_are_unchanged_by_clearing() {
        let c0 = s1();
        let before: Vec<i128> = ["scraper.near", "gpu.near", "translator.near"].iter().map(|a| net(&c0, 1, a)).collect();
        let mut c = closed(c0, Some(p_s1()));
        c.commit_epoch(1);
        // D2 after the commit: net from the remaining obligations of the pair map.
        let after: Vec<i128> = ["scraper.near", "gpu.near", "translator.near"]
            .iter()
            .map(|a| {
                let mut n = 0i128;
                for (d, cr) in [("scraper.near", "gpu.near"), ("gpu.near", "translator.near"), ("translator.near", "scraper.near")] {
                    let o = c.obligation(1, acc(d), acc(cr)).unwrap().obligation.0 as i128;
                    if cr == *a { n += o }
                    if d == *a { n -= o }
                }
                n
            })
            .collect();
        assert_eq!(before, after);
        assert_eq!(before, vec![-35 * CREDIT as i128, 35 * CREDIT as i128, 0]);
    }

    // TC-CT-028-04 (S2)
    #[test]
    fn sum_due_equals_sum_claimable() {
        let mut c = circle();
        register(&mut c, 1, "summarizer.near", "analyst.near", 15 * CREDIT);
        register(&mut c, 1, "analyst.near", "summarizer.near", 15 * CREDIT);
        register(&mut c, 1, "analyst.near", "coder.near", 10 * CREDIT);
        register(&mut c, 1, "coder.near", "summarizer.near", 10 * CREDIT);
        let p_opt = vec![
            edge("summarizer.near", "analyst.near", 15 * CREDIT),
            edge("analyst.near", "summarizer.near", 5 * CREDIT),
            edge("analyst.near", "coder.near", 10 * CREDIT),
            edge("coder.near", "summarizer.near", 10 * CREDIT),
        ];
        let mut c = closed(c, Some(p_opt));
        c.commit_epoch(1);
        let all = c.list_positions(1, 0, 100);
        let due: u128 = all.iter().filter(|p| p.status == PositionStatus::Due).map(|p| p.amount.0).sum();
        let claimable: u128 = all.iter().filter(|p| p.status == PositionStatus::Claimable).map(|p| p.amount.0).sum();
        assert_eq!((due, claimable), (10 * CREDIT, 10 * CREDIT));
        assert_eq!(position(&c, "analyst.near").unwrap().status, PositionStatus::Due);
        assert_eq!(position(&c, "summarizer.near").unwrap().status, PositionStatus::Claimable);
        assert!(position(&c, "coder.near").is_none());
    }

    // TC-CT-028-05
    #[test]
    fn commit_needs_the_proposal_window_to_end() {
        let mut c = closed(s1(), Some(p_s1()));
        at(T_PROP - 1, "keeper.near");
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.commit_epoch(1))).contains("ERR_PROPOSAL_WINDOW_OPEN"));
        at(T_PROP, "keeper.near");
        c.commit_epoch(1);
        assert!(position(&c, "scraper.near").is_some());
    }

    // TC-CT-028-06
    #[test]
    fn commit_in_the_wrong_phase() {
        let mut c = s1();
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.commit_epoch(1))).contains("ERR_WRONG_PHASE"), "open epoch");
        let mut c = closed(s1(), Some(p_s1()));
        c.commit_epoch(1);
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.commit_epoch(1))).contains("ERR_WRONG_PHASE"), "second commit");
    }

    // TC-CT-028-07
    #[test]
    fn commit_unknown_epoch() {
        let mut c = s1();
        assert!(panic_text(std::panic::AssertUnwindSafe(|| c.commit_epoch(99))).contains("ERR_UNKNOWN_EPOCH"));
    }

    // TC-CT-028-08
    #[test]
    fn commit_without_a_proposal() {
        let mut c = closed(s1(), None);
        let _ = get_logs();
        c.commit_epoch(1);
        let events = log_events();
        let applied = &events[0]["data"][0];
        assert!(applied["solver"].is_null());
        assert_eq!(applied["cleared_total"], "0");
        assert_eq!(applied["edges"].as_array().unwrap().len(), 0);
        assert_eq!(position(&c, "scraper.near").unwrap().status, PositionStatus::Due);
        assert_eq!(position(&c, "scraper.near").unwrap().amount.0, 35 * CREDIT);
        assert_eq!(position(&c, "gpu.near").unwrap().status, PositionStatus::Claimable);
        assert_eq!(c.outstanding_debt(acc("scraper.near")).0, 35 * CREDIT);
        assert_eq!(c.get_clearing_proposal(1), None);
    }

    // TC-CT-028-09
    #[test]
    fn next_epoch_debt_is_kept() {
        let mut c = s1();
        at(T_CUT, "keeper.near");
        c.close_epoch(1);
        register(&mut c, 2, "scraper.near", "gpu.near", 10 * CREDIT);
        assert_eq!(c.outstanding_debt(acc("scraper.near")).0, 70 * CREDIT);
        at(T_CUT + SEC, "solver1.near");
        c.submit_clearing(1, p_s1());
        at(T_PROP, "keeper.near");
        c.commit_epoch(1);
        assert_eq!(c.outstanding_debt(acc("scraper.near")).0, 45 * CREDIT);
        assert_eq!(c.outstanding_debt(acc("gpu.near")).0, 25 * CREDIT - 25 * CREDIT);
    }

    // TC-CT-028-10
    #[test]
    fn all_net_zero_writes_no_positions() {
        let mut c = circle();
        register(&mut c, 1, "translator.near", "gpu.near", 50 * CREDIT);
        register(&mut c, 1, "gpu.near", "translator.near", 50 * CREDIT);
        let edges = vec![edge("translator.near", "gpu.near", 50 * CREDIT), edge("gpu.near", "translator.near", 50 * CREDIT)];
        let mut c = closed(c, Some(edges));
        let _ = get_logs();
        c.commit_epoch(1);
        assert!(c.list_positions(1, 0, 100).is_empty());
        let committed = log_events().into_iter().find(|e| e["event"] == "epoch_committed").unwrap();
        let data = &committed["data"][0];
        assert_eq!((data["total_due"].as_str(), data["total_claimable"].as_str()), (Some("0"), Some("0")));
        assert_eq!(c.outstanding_debt(acc("translator.near")).0, 0);
    }

    // TC-CT-028-11
    #[test]
    fn positions_event_is_chunked_at_fifty() {
        let mut c = circle();
        for i in 0..32 {
            register(&mut c, 1, &format!("d{i:02}.near"), &format!("c{i:02}.near"), 10 * CREDIT);
        }
        let mut c = closed(c, None);
        let _ = get_logs();
        c.commit_epoch(1);
        let chunks: Vec<Value> = log_events().into_iter().filter(|e| e["event"] == "epoch_committed").collect();
        assert_eq!(chunks.len(), 2);
        for (index, chunk) in chunks.iter().enumerate() {
            let data = &chunk["data"][0];
            assert_eq!((data["chunk"].as_u64(), data["chunks"].as_u64()), (Some(index as u64), Some(2)));
            assert_eq!(data["total_due"], "320000000");
        }
        assert_eq!(chunks[0]["data"][0]["positions"].as_array().unwrap().len(), 50);
        assert_eq!(chunks[1]["data"][0]["positions"].as_array().unwrap().len(), 14);
    }

    // TC-CT-028-12
    #[test]
    fn settled_history_is_untouched() {
        let mut c = closed(s1(), Some(p_s1()));
        c.members.insert(acc("scraper.near"), member("scraper.near"));
        c.commit_epoch(1);
        assert_eq!(c.members.get(&acc("scraper.near")).unwrap().settled_history, 0);
    }

    fn member(id: &str) -> crate::member::Member {
        crate::member::Member {
            account_id: acc(id),
            pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse().unwrap(),
            terms_hash: "a".repeat(64),
            stake: 100 * CREDIT,
            limit: 80 * CREDIT,
            status: crate::member::MemberStatus::Active,
            admitted: false,
            joined_at: T0,
            settled_history: 0,
            last_active_epoch: 1,
            exit_requested_epoch: None,
            withdraw_pending: false,
            reactivate_epoch: None,
        }
    }

    // TC-CT-028-14 (unit side)
    #[test]
    fn anyone_can_commit() {
        let mut c = closed(s1(), Some(p_s1()));
        at(T_PROP, "outsider.near");
        c.commit_epoch(1);
        assert!(position(&c, "scraper.near").is_some());
    }

    #[test]
    fn params_snapshot_of_the_epoch_sets_the_deadline() {
        // The epoch keeps its own params snapshot; a different `payment_window` in force now must not matter.
        let mut c = s1();
        c.pending_params = Some(crate::state::PendingParams { params: Params { payment_window: 2700, ..Params::default() }, effective_epoch: 2 });
        let mut c = closed(c, Some(p_s1()));
        c.commit_epoch(1);
        assert_eq!(c.get_epoch(Some(1)).unwrap().payment_deadline.unwrap().0, T_PROP + 1800 * SEC);
    }
}
