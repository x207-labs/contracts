//! Upgrade by the DAO through a timelock (CT-055, ADR-20, spec 4): `propose_upgrade(code_hash)` fixes the hash and an
//! `eta` 24 hours later (the delay is a constant the DAO cannot change); `apply_upgrade()` takes the new wasm as raw
//! input and, once `now >= eta` and `sha256(input) = code_hash`, deploys it over the contract and calls `migrate`.
//! The pending upgrade lives under its own storage key, so the contract state layout does not change.

use near_sdk::borsh::{self, BorshDeserialize, BorshSerialize};
use near_sdk::json_types::{Base58CryptoHash, U64};
use near_sdk::{env, near, Gas, NearToken, Promise};

use crate::events::{UpgradeApplied, UpgradeProposed, X207Event};
use crate::state::{X207Circle, X207CircleExt};

/// Timelock between `propose_upgrade` and `apply_upgrade`: 24 hours in nanoseconds.
pub const UPGRADE_TIMELOCK_NS: u64 = 24 * 3600 * 1_000_000_000;
const STORAGE_KEY: &[u8] = b"x207_upgrade";
const MIGRATE_GAS: Gas = Gas::from_tgas(50);

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
#[borsh(crate = "near_sdk::borsh")]
struct PendingUpgrade {
    code_hash: [u8; 32],
    eta: u64,
}

/// `get_upgrade` (spec 12): the pending upgrade.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeView {
    pub code_hash: Base58CryptoHash,
    pub eta: U64,
}

fn pending() -> Option<PendingUpgrade> {
    env::storage_read(STORAGE_KEY)
        .map(|bytes| borsh::from_slice(&bytes).expect("pending upgrade is valid borsh"))
}

#[near]
impl X207Circle {
    /// Owner only. A new proposal replaces a pending one and restarts the timelock.
    pub fn propose_upgrade(&mut self, code_hash: Base58CryptoHash) {
        self.assert_owner();
        let eta = env::block_timestamp() + UPGRADE_TIMELOCK_NS;
        let pending = PendingUpgrade {
            code_hash: code_hash.into(),
            eta,
        };
        env::storage_write(STORAGE_KEY, &borsh::to_vec(&pending).expect("borsh"));
        UpgradeProposed {
            code_hash,
            eta: U64(eta),
        }
        .emit();
    }

    /// Owner only; the input is the raw wasm. Order of checks: `ERR_NOT_OWNER`, `ERR_NO_UPGRADE`,
    /// `ERR_TIMELOCK_ACTIVE`, `ERR_CODE_HASH_MISMATCH`. The new code must keep the rules of ADR-20 and define `migrate`.
    pub fn apply_upgrade(&mut self) -> Promise {
        self.assert_owner();
        let Some(upgrade) = pending() else {
            env::panic_str("ERR_NO_UPGRADE: no upgrade is pending")
        };
        let now = env::block_timestamp();
        if now < upgrade.eta {
            env::panic_str(&format!(
                "ERR_TIMELOCK_ACTIVE: eta {} now {now}",
                upgrade.eta
            ));
        }
        let code = env::input().unwrap_or_default();
        if env::sha256_array(&code) != upgrade.code_hash {
            env::panic_str(
                "ERR_CODE_HASH_MISMATCH: sha256 of the input differs from the proposed code_hash",
            );
        }
        env::storage_remove(STORAGE_KEY);
        UpgradeApplied {
            code_hash: upgrade.code_hash.into(),
            applied_at: U64(now),
        }
        .emit();
        Promise::new(env::current_account_id())
            .deploy_contract(code)
            .function_call(
                "migrate".to_string(),
                b"{}".to_vec(),
                NearToken::from_yoctonear(0),
                MIGRATE_GAS,
            )
    }

    pub fn get_upgrade(&self) -> Option<UpgradeView> {
        pending().map(|p| UpgradeView {
            code_hash: p.code_hash.into(),
            eta: U64(p.eta),
        })
    }

    /// Runs on the new code right after the deploy: keeps the stored state as it is. A future version with a changed
    /// layout replaces this body with the conversion.
    #[private]
    #[init(ignore_state)]
    pub fn migrate() -> Self {
        env::state_read().unwrap_or_else(|| env::panic_str("ERR_NO_STATE: nothing to migrate"))
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::{testing_env, AccountId};

    use super::*;
    use crate::params::Params;

    const T0: u64 = 1_000;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn as_caller(caller: &str, ts: u64, input: Vec<u8>) {
        let mut context = VMContextBuilder::new()
            .predecessor_account_id(acc(caller))
            .block_timestamp(ts)
            .build();
        context.input = input.into();
        testing_env!(context);
    }

    fn circle() -> X207Circle {
        as_caller("deployer.near", T0, vec![]);
        X207Circle::with_config(
            acc("dao.near"),
            "agents-main".into(),
            acc("usdt.near"),
            acc("intents.near"),
            Params::default(),
            T0,
        )
    }

    fn hash_of(code: &[u8]) -> Base58CryptoHash {
        env::sha256_array(code).into()
    }

    fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).unwrap_err();
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    }

    // CT-055: only the owner proposes; the proposal shows in get_upgrade with eta = now + 24 h and emits upgrade_proposed
    #[test]
    fn propose_is_owner_only_and_sets_the_eta() {
        let mut c = circle();
        assert_eq!(c.get_upgrade(), None);
        as_caller("outsider.near", T0, vec![]);
        assert!(panic_text(std::panic::AssertUnwindSafe(
            || c.propose_upgrade(hash_of(b"v2"))
        ))
        .contains("ERR_NOT_OWNER"));
        as_caller("dao.near", T0, vec![]);
        let _ = get_logs();
        c.propose_upgrade(hash_of(b"v2"));
        let view = c.get_upgrade().unwrap();
        assert_eq!(
            (view.code_hash, view.eta),
            (hash_of(b"v2"), U64(T0 + UPGRADE_TIMELOCK_NS))
        );
        let log = get_logs().join("\n");
        assert!(
            log.contains(r#""event":"upgrade_proposed""#)
                && log.contains(&format!(r#""eta":"{}""#, T0 + UPGRADE_TIMELOCK_NS)),
            "{log}"
        );
    }

    // CT-055: a second proposal replaces the first and restarts the timelock
    #[test]
    fn a_new_proposal_replaces_the_pending_one() {
        let mut c = circle();
        as_caller("dao.near", T0, vec![]);
        c.propose_upgrade(hash_of(b"v2"));
        as_caller("dao.near", T0 + 5_000, vec![]);
        c.propose_upgrade(hash_of(b"v3"));
        let view = c.get_upgrade().unwrap();
        assert_eq!(
            (view.code_hash, view.eta),
            (hash_of(b"v3"), U64(T0 + 5_000 + UPGRADE_TIMELOCK_NS))
        );
    }

    // CT-055: apply_upgrade refuses in this order: not owner, nothing pending, timelock, hash
    #[test]
    fn apply_refusals_in_order() {
        let mut c = circle();
        as_caller("outsider.near", T0, b"v2".to_vec());
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| drop(c.apply_upgrade())))
                .contains("ERR_NOT_OWNER")
        );
        as_caller("dao.near", T0, b"v2".to_vec());
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| drop(c.apply_upgrade())))
                .contains("ERR_NO_UPGRADE")
        );
        c.propose_upgrade(hash_of(b"v2"));
        let eta = T0 + UPGRADE_TIMELOCK_NS;
        as_caller("dao.near", eta - 1, b"v2".to_vec());
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| drop(c.apply_upgrade())))
                .contains("ERR_TIMELOCK_ACTIVE")
        );
        as_caller("dao.near", eta, b"other code".to_vec());
        assert!(
            panic_text(std::panic::AssertUnwindSafe(|| drop(c.apply_upgrade())))
                .contains("ERR_CODE_HASH_MISMATCH")
        );
        assert!(
            c.get_upgrade().is_some(),
            "a refused apply leaves the proposal"
        );
    }

    // CT-055: at the eta, with the right code, the proposal is consumed and upgrade_applied is emitted
    #[test]
    fn apply_at_the_eta_consumes_the_proposal() {
        let mut c = circle();
        as_caller("dao.near", T0, vec![]);
        c.propose_upgrade(hash_of(b"v2"));
        let eta = T0 + UPGRADE_TIMELOCK_NS;
        as_caller("dao.near", eta, b"v2".to_vec());
        let _ = get_logs();
        drop(c.apply_upgrade());
        assert_eq!(c.get_upgrade(), None);
        let log = get_logs().join("\n");
        assert!(
            log.contains(r#""event":"upgrade_applied""#)
                && log.contains(&format!(r#""applied_at":"{eta}""#)),
            "{log}"
        );
    }
}
