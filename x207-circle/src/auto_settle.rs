//! CT-108: opt-in settlement from the stake. A member that sets `auto_settle` and misses its `due` pays it from its stake
//! at `finalize_epoch` and stays `active`: no default, no suspension. Stake that does not cover the `due` leaves the old
//! default path unchanged. The flag lives in a sidecar map under its own storage prefix, so `Member` keeps its layout.
//! An EVM member sets it with a signed `Action` (`auto_settle` to turn it on, `auto_settle_off` to turn it off, so the
//! signature fixes the choice; epoch 0, receiver "", termsHash "").

use near_sdk::json_types::U128;
use near_sdk::store::LookupMap;
use near_sdk::{env, near, AccountId};

use crate::events::{AutoSettleSet, AutoSettled, X207Event};
use crate::member::MemberStatus;
use crate::settlement::PositionStatus;
use crate::state::{StorageKey, X207Circle, X207CircleExt};
use crate::types::EpochId;

/// Value = block timestamp (ns) of the opt-in. It only counts for an epoch committed at or after it (review M1).
fn flags() -> LookupMap<AccountId, u64> {
    LookupMap::new(StorageKey::AutoSettle)
}

pub fn is_on(account: &AccountId) -> bool {
    flags().contains_key(account)
}

fn on_since(account: &AccountId) -> Option<u64> {
    flags().get(account).copied()
}

impl X207Circle {
    fn set_auto_settle_for(&mut self, account: AccountId, enabled: bool) {
        let Some(member) = self.members.get(&account) else { env::panic_str(&format!("ERR_NOT_MEMBER: {account}")) };
        if member.status == MemberStatus::Exited {
            env::panic_str(&format!("ERR_MEMBER_EXITED: {account}"));
        }
        let mut map = flags();
        if enabled {
            if !map.contains_key(&account) {
                map.insert(account.clone(), env::block_timestamp());
            }
        } else {
            map.remove(&account);
        }
        map.flush();
        AutoSettleSet { account_id: account, enabled }.emit();
    }

    /// Called by `finalize_epoch` for a debtor whose `due` is unpaid. `None` leaves the debtor to the default path:
    /// not opted in, not `active`, or a free stake (stake minus a locked bond) below the `due`.
    pub(crate) fn try_auto_settle(&mut self, epoch: EpochId, account: &AccountId, block: u64) -> Option<AutoSettled> {
        // opt-in must predate the commit of this epoch: no opting in after the due is known (review M1)
        let since = on_since(account)?;
        if since > self.epoch_or_panic(epoch).committed_at? {
            return None;
        }
        let key = (epoch, account.clone());
        let due = self.positions.get(&key)?.amount;
        let member = self.members.get(account)?;
        if member.status != MemberStatus::Active || member.stake.saturating_sub(crate::deal::bond_locked_of(account)) < due {
            return None;
        }
        self.d6_stake_to_pool(epoch, due);
        let member = self.members.get_mut(account).expect("member checked above");
        member.stake -= due;
        // the limit follows the new stake (rule of `compute_limit`) in `recompute_limits`; until then it must not exceed it
        member.limit = crate::deal::bound_limit(member.limit, member.stake, account);
        let stake_left = member.stake;
        self.positions
            .get_mut(&key)
            .expect("position checked above")
            .transition(PositionStatus::Paid, block)
            .unwrap_or_else(|reason| env::panic_str(&reason));
        let debt = self.outstanding_debt.get(account).copied().unwrap_or(0);
        let debt = debt
            .checked_sub(due)
            .unwrap_or_else(|| env::panic_str("ERR_D2_ACCOUNTING: outstanding_debt below the auto-settled due"));
        self.outstanding_debt.insert(account.clone(), debt);
        Some(AutoSettled {
            epoch,
            account_id: account.clone(),
            due: U128(due),
            stake: U128(stake_left),
            status: "active".into(),
            position_status: "paid".into(),
            outstanding_debt: U128(debt),
        })
    }
}

#[near]
impl X207Circle {
    /// The caller turns settlement from its own stake on or off. Needs 1 yocto (a function-call key cannot switch it off). It counts for epochs committed after the opt-in. Checks: `ERR_NOT_MEMBER`, `ERR_MEMBER_EXITED`.
    #[payable]
    pub fn set_auto_settle(&mut self, enabled: bool) {
        near_sdk::assert_one_yocto();
        self.set_auto_settle_for(env::predecessor_account_id(), enabled);
    }

    /// `set_auto_settle` for an EVM member: anyone relays, the signature of the member is the gate (`Action` with
    /// `action` = `auto_settle` when `enabled`, `auto_settle_off` when not).
    pub fn set_auto_settle_evm(&mut self, member: AccountId, enabled: bool, nonce: u64, deadline: u64, signature: String) {
        let action = if enabled { "auto_settle" } else { "auto_settle_off" };
        crate::evm::authorize(&member, action, 0, "", nonce, deadline, 0, &signature);
        self.set_auto_settle_for(member, enabled);
    }

    pub fn is_auto_settle(&self, account_id: AccountId) -> bool {
        is_on(&account_id)
    }
}
