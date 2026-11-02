//! `commit_epoch` (CT-028, spec 4.4): applies the stored proposal to D2, writes the D5 positions,
//! sets the payment deadline, and emits `clearing_applied` and `epoch_committed`.
//!
//! Fees (P1, CT-051/052, spec 4.4 step 4): `clearing_fee_bps` and `solver_fee_bps` of the epoch snapshot are
//! charged on `cleared_total`, pro rata to each debtor's cleared amount, out of its stake. The clearing part goes
//! to the reserve and the solver part to the balance of the winning solver. Both default to 0.
//! Event logs: near caps the total log length of one call at 16 KiB, so a commit with more than
//! about 60 edges at 64-character accounts does not fit the chunked events; see the CT-004 note.

use near_sdk::json_types::{I128, U128, U64};
use near_sdk::json_types::U128 as WireU128;
use near_sdk::serde_json::json;
use near_sdk::{assert_one_yocto, env, near, AccountId, NearToken, Promise};

use crate::events::{emit_chunked, emit_clearing_applied, EpochCommitted, PositionEntry, SolverFeeClaimFailed, SolverFeeClaimed, X207Event};
use crate::settlement::{Position, PositionRole, PositionStatus};
use crate::state::{X207Circle, X207CircleExt};
use crate::types::{ClearingEdge, EpochId};

fn checked_signed(value: u128) -> i128 {
    i128::try_from(value).unwrap_or_else(|_| env::panic_str("ERR_D2_ACCOUNTING: amount exceeds i128"))
}

/// Splits `fee_total` over the debtors of `edges` pro rata to their cleared amount: floor shares, then the
/// leftover units one by one in account order. Returns `(account, share)` for every debtor with a share.
pub fn pro_rata_fee(edges: &[ClearingEdge], cleared_total: u128, fee_total: u128) -> Vec<(AccountId, u128)> {
    if fee_total == 0 || cleared_total == 0 {
        return vec![];
    }
    let mut out: std::collections::BTreeMap<AccountId, u128> = std::collections::BTreeMap::new();
    for edge in edges {
        *out.entry(edge.debtor.clone()).or_default() += edge.cleared.0;
    }
    let mut shares: Vec<(AccountId, u128)> = out
        .into_iter()
        .map(|(account, cleared)| {
            let product = fee_total.checked_mul(cleared).unwrap_or_else(|| env::panic_str("ERR_D2_ACCOUNTING: fee share overflows u128"));
            let share = product / cleared_total;
            (account, share)
        })
        .collect();
    let mut leftover = fee_total - shares.iter().map(|(_, s)| s).sum::<u128>();
    for (_, share) in shares.iter_mut() {
        if leftover == 0 {
            break;
        }
        *share += 1;
        leftover -= 1;
    }
    shares.retain(|(_, s)| *s > 0);
    shares
}

impl X207Circle {
    /// Charges the P1 fees of `epoch` from the stake of the debtors (what a stake cannot cover is not charged) and
    /// credits the reserve and `solver`. Returns `(clearing_fee, solver_fee)` actually collected.
    fn charge_fees(&mut self, epoch: EpochId, solver: &Option<AccountId>, cleared_total: u128, edges: &[ClearingEdge]) -> (u128, u128) {
        let fees = self.epoch_or_panic(epoch).params.fees.clone();
        let (clearing_bps, solver_bps) = (u128::from(fees.clearing_fee_bps), u128::from(fees.solver_fee_bps));
        let Some(solver) = solver else { return (0, 0) };
        if cleared_total == 0 || (clearing_bps == 0 && solver_bps == 0) {
            return (0, 0);
        }
        let clearing_fee = cleared_total.saturating_mul(clearing_bps) / 10_000;
        let solver_fee = cleared_total.saturating_mul(solver_bps) / 10_000;
        let mut charged = 0u128;
        for (account, share) in pro_rata_fee(edges, cleared_total, clearing_fee + solver_fee) {
            let member = self.members.get_mut(&account).expect("a debtor of a cleared edge is a member");
            let take = share.min(member.stake);
            member.stake -= take;
            member.limit = member.limit.min(member.stake);
            charged += take;
        }
        let to_reserve = clearing_fee.min(charged);
        let to_solver = charged - to_reserve;
        self.d6_stake_to_fees(to_reserve, to_solver);
        if to_solver > 0 {
            let balance = self.solver_fees.get(solver).copied().unwrap_or(0);
            self.solver_fees.insert(solver.clone(), balance + to_solver);
        }
        (to_reserve, to_solver)
    }

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
    /// Balance of the fee the solver earned at `commit_epoch` (CT-052); `"0"` for anyone else.
    pub fn solver_fee_balance(&self, account_id: AccountId) -> WireU128 {
        WireU128(self.solver_fees.get(&account_id).copied().unwrap_or(0))
    }

    /// The solver takes its fee balance as USDT (CT-052, spec 4.7). `assert_one_yocto`, `ERR_GAS_TOO_LOW`,
    /// `ERR_NOTHING_TO_CLAIM`. The balance goes in flight; `on_solver_fee_resolved` settles the outcome.
    #[payable]
    pub fn claim_solver_fee(&mut self) -> Promise {
        assert_one_yocto();
        if env::prepaid_gas() < crate::settlement::CLAIM_MIN_GAS {
            env::panic_str(&format!("ERR_GAS_TOO_LOW: need {} TGas", crate::settlement::CLAIM_MIN_GAS.as_tgas()));
        }
        let account = env::predecessor_account_id();
        let amount = self.solver_fees.get(&account).copied().unwrap_or(0);
        if amount == 0 {
            env::panic_str(&format!("ERR_NOTHING_TO_CLAIM: {account}"));
        }
        self.solver_fees.insert(account.clone(), 0);
        self.d6_solver_fee_to_in_flight(amount);
        Promise::new(self.usdt_token_id.clone())
            .function_call(
                "ft_transfer".to_string(),
                json!({"receiver_id": account, "amount": WireU128(amount), "memo": null}).to_string().into_bytes(),
                NearToken::from_yoctonear(1),
                crate::settlement::FT_TRANSFER_GAS,
            )
            .then(Self::ext(env::current_account_id()).with_static_gas(crate::settlement::CLAIM_CALLBACK_GAS).on_solver_fee_resolved(account, WireU128(amount)))
    }

    /// Callback of the solver fee transfer. Never panics. Returns the amount that arrived.
    #[private]
    pub fn on_solver_fee_resolved(&mut self, account_id: AccountId, amount: WireU128) -> WireU128 {
        let sent = amount.0;
        if env::promise_result_checked(0, 64).is_ok() {
            self.d6_in_flight_out(sent);
            SolverFeeClaimed { account_id, amount }.emit();
            return amount;
        }
        self.d6_in_flight_to_solver_fee(sent);
        let balance = self.solver_fees.get(&account_id).copied().unwrap_or(0);
        self.solver_fees.insert(account_id.clone(), balance + sent);
        SolverFeeClaimFailed { account_id, amount }.emit();
        WireU128(0)
    }

    /// Permissionless after the proposal window. Net positions do not change by clearing: every
    /// member is a debtor `due`, a creditor `claimable`, or has no D5 record (net zero).
    pub fn commit_epoch(&mut self, epoch: EpochId) {
        let now = env::block_timestamp();
        let block = env::block_height();
        self.epoch_mut_or_panic(epoch).start_payment_window(now);

        let (solver, cleared_total, edges) = self.apply_proposal(epoch);
        let (clearing_fee, solver_fee) = self.charge_fees(epoch, &solver, cleared_total, &edges);
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

        emit_clearing_applied(epoch, solver, U128(cleared_total), &edges, U128(clearing_fee), U128(solver_fee));
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

    fn s1_with_fees(clearing_bps: u16, solver_bps: u16, stake: u128) -> X207Circle {
        let mut c = s1();
        for name in ["scraper.near", "gpu.near", "translator.near"] {
            let mut m = member(name);
            m.stake = stake;
            m.limit = stake.min(80 * CREDIT);
            c.members.insert(acc(name), m);
            c.member_index.push(acc(name));
        }
        c.total_stake = 3 * stake;
        let fees = &mut c.epochs.get_mut(&1).unwrap().params.fees;
        fees.clearing_fee_bps = clearing_bps;
        fees.solver_fee_bps = solver_bps;
        closed(c, Some(p_s1()))
    }

    // CT-051, CT-052: fees come out of the stake of the debtors of cleared edges, pro rata, and move to the reserve
    // and to the winning solver; D6 stays balanced
    #[test]
    fn ct_051_052_fees_move_stake_to_reserve_and_solver() {
        let mut c = s1_with_fees(100, 50, 100 * CREDIT);
        let before = c.accounted();
        let _ = get_logs();
        c.commit_epoch(1);
        // cleared_total 75: clearing 0.75, solver 0.375, three debtors of 25 each pay a third of 1.125
        assert_eq!((c.reserve, c.solver_fees_total), (750_000, 375_000));
        assert_eq!(c.solver_fees.get(&acc("solver1.near")).copied(), Some(375_000));
        for name in ["scraper.near", "gpu.near", "translator.near"] {
            assert_eq!(c.members.get(&acc(name)).unwrap().stake, 100 * CREDIT - 375_000, "{name}");
        }
        assert_eq!(c.total_stake, 3 * (100 * CREDIT - 375_000));
        assert_eq!(c.accounted(), before, "I-04: stake plus reserve plus solver fees is unchanged");
        let applied = log_events().into_iter().find(|e| e["event"] == "clearing_applied").unwrap();
        assert_eq!((applied["data"][0]["clearing_fee"].as_str(), applied["data"][0]["solver_fee"].as_str()), (Some("750000"), Some("375000")));
    }

    // CT-051: default fees 0 charge nothing
    #[test]
    fn ct_051_default_fees_charge_nothing() {
        let mut c = s1_with_fees(0, 0, 100 * CREDIT);
        c.commit_epoch(1);
        assert_eq!((c.reserve, c.solver_fees_total, c.total_stake), (0, 0, 300 * CREDIT));
        assert_eq!(c.solver_fees.get(&acc("solver1.near")), None);
    }

    // CT-051: a stake that cannot cover its share pays what it has, and the limit is clamped to the stake (I-01)
    #[test]
    fn ct_051_stake_below_the_share_pays_what_it_has() {
        let mut c = s1_with_fees(100, 0, 100_000);
        let before = c.accounted();
        c.commit_epoch(1);
        // share per debtor is 250_000 but the stake is 100_000
        assert_eq!(c.reserve, 300_000);
        for name in ["scraper.near", "gpu.near", "translator.near"] {
            let m = c.members.get(&acc(name)).unwrap();
            assert_eq!((m.stake, m.limit), (0, 0), "{name}");
        }
        assert_eq!(c.accounted(), before);
    }

    #[test]
    fn ct_051_pro_rata_fee_hands_out_every_unit() {
        let edges = vec![edge("a.near", "b.near", 10), edge("b.near", "c.near", 10), edge("c.near", "a.near", 10)];
        let shares = pro_rata_fee(&edges, 30, 10);
        assert_eq!(shares.iter().map(|(_, s)| s).sum::<u128>(), 10);
        assert_eq!(shares.iter().map(|(_, s)| *s).collect::<Vec<_>>(), vec![4, 3, 3]);
        assert!(pro_rata_fee(&edges, 30, 0).is_empty());
        let skew = vec![edge("a.near", "b.near", 90), edge("b.near", "a.near", 10)];
        assert_eq!(pro_rata_fee(&skew, 100, 7), vec![(acc("a.near"), 7)], "floor shares 6 and 0, the leftover unit goes to the first account");
    }

    fn as_solver(who: &str) {
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc(who))
            .block_timestamp(T_PROP)
            .attached_deposit(near_sdk::NearToken::from_yoctonear(1))
            .prepaid_gas(near_sdk::Gas::from_tgas(300))
            .build());
    }

    // TC-CT-052-01
    #[test]
    fn ct_052_claim_without_a_balance() {
        let mut c = s1_with_fees(0, 50, 100 * CREDIT);
        c.commit_epoch(1);
        as_solver("solver2.near");
        let text = panic_text(std::panic::AssertUnwindSafe(|| drop(c.claim_solver_fee())));
        assert!(text.contains("ERR_NOTHING_TO_CLAIM"), "{text}");
    }

    // CT-052: the winning solver sees its balance and `claim_solver_fee` moves it in flight (D6 stays balanced)
    #[test]
    fn ct_052_solver_claim_moves_the_balance_in_flight() {
        let mut c = s1_with_fees(0, 100, 100 * CREDIT);
        let before = c.accounted();
        c.commit_epoch(1);
        assert_eq!(c.solver_fee_balance(acc("solver1.near")).0, 750_000, "75 cleared at 100 bps");
        as_solver("solver1.near");
        drop(c.claim_solver_fee());
        assert_eq!(c.solver_fee_balance(acc("solver1.near")).0, 0);
        assert_eq!((c.solver_fees_total, c.in_flight), (0, 750_000));
        assert_eq!(c.accounted(), before);
        let again = panic_text(std::panic::AssertUnwindSafe(|| drop(c.claim_solver_fee())));
        assert!(again.contains("ERR_NOTHING_TO_CLAIM"), "{again}");
    }
}
