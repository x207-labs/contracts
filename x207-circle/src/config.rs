//! Init and owner (CT-008): `new`, `assert_owner`, `get_owner`, `get_circle`. The owner is the
//! Sputnik DAO account. No method here or elsewhere lets the owner move D6 (ADR-20, I-15).

use near_sdk::{env, near, AccountId};

use crate::params::{validate_params, Params};
use crate::state::{X207Circle, X207CircleExt};

pub const CREDIT_UNIT: &str = "credit";
pub const CREDIT_DECIMALS: u8 = 6;

/// `circleId` of the x402 network `x207:<circleId>`: `[-_a-zA-Z0-9]{1,32}` (CAIP-2 reference).
pub fn is_valid_circle_id(circle_id: &str) -> bool {
    (1..=32).contains(&circle_id.len())
        && circle_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `get_circle` result (spec section 5).
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CircleConfig {
    pub circle_id: String,
    pub contract_id: AccountId,
    pub network: String,
    pub owner_id: AccountId,
    pub usdt_token_id: AccountId,
    pub intents_id: AccountId,
    pub unit: String,
    pub decimals: u8,
    pub member_count: u32,
}

impl X207Circle {
    /// Panics `ERR_NOT_OWNER` unless the predecessor is the owner account.
    pub fn assert_owner(&self) {
        let caller = env::predecessor_account_id();
        if caller != self.owner_id {
            env::panic_str(&format!("ERR_NOT_OWNER: {caller} is not the owner"));
        }
    }

    pub fn circle_config(&self) -> CircleConfig {
        CircleConfig {
            circle_id: self.circle_id.clone(),
            contract_id: env::current_account_id(),
            network: format!("x207:{}", self.circle_id),
            owner_id: self.owner_id.clone(),
            usdt_token_id: self.usdt_token_id.clone(),
            intents_id: self.intents_id.clone(),
            unit: CREDIT_UNIT.to_string(),
            decimals: CREDIT_DECIMALS,
            member_count: self.member_count,
        }
    }
}

#[near]
impl X207Circle {
    /// Runs once. Epoch `1` opens now with `cutoff_at = now + epoch_length`; `params` `None` means
    /// the ADR-25 defaults.
    #[init]
    pub fn new(
        owner_id: AccountId,
        circle_id: String,
        usdt_token_id: AccountId,
        intents_id: AccountId,
        params: Option<Params>,
    ) -> Self {
        if !is_valid_circle_id(&circle_id) {
            env::panic_str(&format!("ERR_BAD_CIRCLE_ID: {circle_id:?}"));
        }
        let params = params.unwrap_or_default();
        validate_params(&params);
        Self::with_config(owner_id, circle_id, usdt_token_id, intents_id, params, env::block_timestamp())
    }

    pub fn get_owner(&self) -> AccountId {
        self.owner_id.clone()
    }

    pub fn get_circle(&self) -> CircleConfig {
        self.circle_config()
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    use super::*;
    use crate::clearing::EpochPhase;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn context(predecessor: &str) {
        testing_env!(VMContextBuilder::new()
            .current_account_id(acc("x207-circle.near"))
            .predecessor_account_id(acc(predecessor))
            .block_timestamp(5_000_000_000)
            .build());
    }

    fn init(circle_id: &str, params: Option<Params>) -> X207Circle {
        X207Circle::new(acc("dao.near"), circle_id.to_string(), acc("usdt.near"), acc("intents.near"), params)
    }

    fn panic_message(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).unwrap_err();
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    // TC-CT-008-01 (value part; the sandbox test asserts the same through the views)
    #[test]
    fn init_stores_config_and_opens_epoch_one() {
        context("deployer.near");
        let c = init("agents-main", None);
        assert_eq!(c.get_owner(), acc("dao.near"));
        let config = c.get_circle();
        assert_eq!(
            near_sdk::serde_json::to_value(&config).unwrap(),
            near_sdk::serde_json::json!({
                "circle_id": "agents-main", "contract_id": "x207-circle.near", "network": "x207:agents-main",
                "owner_id": "dao.near", "usdt_token_id": "usdt.near", "intents_id": "intents.near",
                "unit": "credit", "decimals": 6, "member_count": 0
            })
        );
        let epoch = c.get_epoch(None).unwrap();
        assert_eq!((epoch.epoch, epoch.cutoff_at.0), (1, 5_000_000_000 + 3600 * 1_000_000_000));
        assert_eq!(c.epochs.get(&1).unwrap().phase, EpochPhase::Open);
        assert_eq!(c.get_params(), Params::default());
        assert_eq!(c.get_pool().accounted.0, 0);
        assert_eq!(c.interface_version(), "0.0.1");
    }

    #[test]
    fn init_accepts_explicit_params() {
        context("deployer.near");
        let params = Params { epoch_length: 900, ..Params::default() };
        let c = init("agents-main", Some(params.clone()));
        assert_eq!(c.get_params(), params);
        assert_eq!(c.get_epoch(None).unwrap().cutoff_at.0, 5_000_000_000 + 900 * 1_000_000_000);
    }

    // TC-CT-008-04
    #[test]
    fn circle_id_with_dot_is_rejected() {
        context("deployer.near");
        assert!(panic_message(|| drop(init("agents.main", None))).contains("ERR_BAD_CIRCLE_ID"));
    }

    // TC-CT-008-05
    #[test]
    fn empty_circle_id_is_rejected() {
        context("deployer.near");
        assert!(panic_message(|| drop(init("", None))).contains("ERR_BAD_CIRCLE_ID"));
    }

    // TC-CT-008-06
    #[test]
    fn circle_id_length_bounds() {
        context("deployer.near");
        init("a", None);
        init(&"a".repeat(32), None);
        assert!(panic_message(|| drop(init(&"a".repeat(33), None))).contains("ERR_BAD_CIRCLE_ID"));
    }

    #[test]
    fn circle_id_alphabet() {
        for ok in ["agents-main", "a_b", "A9", "0"] {
            assert!(is_valid_circle_id(ok), "{ok}");
        }
        for bad in ["", "a b", "a.b", "a:b", "é", "a/b", "a\n"] {
            assert!(!is_valid_circle_id(bad), "{bad:?}");
        }
    }

    // TC-CT-007-10 (unit side): params outside the range stop `new`
    #[test]
    fn out_of_range_params_stop_init() {
        context("deployer.near");
        let params = Params { epoch_length: 100, ..Params::default() };
        assert!(panic_message(|| drop(init("agents-main", Some(params)))).contains("ERR_PARAM_OUT_OF_RANGE: epoch_length"));
    }

    // TC-CT-008-07
    #[test]
    fn assert_owner_accepts_only_the_dao() {
        context("deployer.near");
        let c = init("agents-main", None);
        context("dao.near");
        c.assert_owner();
        context("outsider.near");
        assert!(panic_message(std::panic::AssertUnwindSafe(|| c.assert_owner())).contains("ERR_NOT_OWNER"));
    }

    #[test]
    fn member_count_shows_in_circle_view() {
        context("deployer.near");
        let mut c = init("agents-main", None);
        c.member_count = 6;
        assert_eq!(c.get_circle().member_count, 6);
    }
}
