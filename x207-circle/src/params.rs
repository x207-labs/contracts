//! D7 `Params`, ranges, defaults, `validate_params`, `set_params`.

use near_sdk::json_types::U128;
use near_sdk::{env, near};

use crate::events::{ParamsUpdated, X207Event};
use crate::state::{PendingParams, X207Circle, X207CircleExt};

// Ranges (spec 3.4, ADR-20). `payment_window` minimum is 15 minutes (ADR-21).
pub const EPOCH_LENGTH_MIN: u64 = 300;
pub const EPOCH_LENGTH_MAX: u64 = 604_800;
pub const PAYMENT_WINDOW_MIN: u64 = 900;
pub const PAYMENT_WINDOW_MAX: u64 = 604_800;
pub const PROPOSAL_WINDOW_MIN: u64 = 60;
pub const PROPOSAL_WINDOW_MAX: u64 = 86_400;
pub const BASE_LIMIT_RATIO_MAX: u16 = 10_000;
pub const LIMIT_GROWTH_MAX: u16 = 10_000;
pub const LIMIT_CAP_MIN: u128 = 1_000_000;
pub const LIMIT_CAP_MAX: u128 = 100_000_000_000;
pub const LIMIT_DECAY_MAX: u16 = 5_000;
pub const CLEARING_FEE_BPS_MAX: u16 = 100;
pub const SOLVER_FEE_BPS_MAX: u16 = 100;
pub const MAX_SEARCH_SHARE_BPS_MAX: u16 = 10_000;
/// CT-094: the value of a barter from which a bond is required, in the smallest unit of credit (10 credit to 1,000,000 credit), and the bond in bps of that value.
pub const BOND_THRESHOLD_MIN: u128 = 10_000_000;
pub const BOND_THRESHOLD_MAX: u128 = 1_000_000_000_000;
pub const BOND_BPS_MIN: u16 = 500;
pub const BOND_BPS_MAX: u16 = 5_000;

// Defaults (ADR-25 and spec 3.4).
pub const DEFAULT_EPOCH_LENGTH: u64 = 3_600;
pub const DEFAULT_PAYMENT_WINDOW: u64 = 1_800;
pub const DEFAULT_PROPOSAL_WINDOW: u64 = 600;
pub const DEFAULT_BASE_LIMIT_RATIO: u16 = 8_000;
pub const DEFAULT_LIMIT_GROWTH: u16 = 1_000;
pub const DEFAULT_LIMIT_CAP: u128 = 1_000_000_000;
pub const DEFAULT_LIMIT_DECAY: u16 = 0;
/// F9 (CT-082): a declared seeker leg may take at most 5% of the sum of all legs of a bundle.
pub const DEFAULT_MAX_SEARCH_SHARE_BPS: u16 = 500;

/// CT-094: 100 credit, and 20% of the value of the barter.
pub const DEFAULT_BOND_THRESHOLD: u128 = 100_000_000;
pub const DEFAULT_BOND_BPS: u16 = 2_000;

fn default_max_search_share_bps() -> u16 {
    DEFAULT_MAX_SEARCH_SHARE_BPS
}

fn default_bond_threshold() -> U128 {
    U128(DEFAULT_BOND_THRESHOLD)
}

fn default_bond_bps() -> u16 {
    DEFAULT_BOND_BPS
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fees {
    /// To the reserve fund (ADR-18, CT-051).
    pub clearing_fee_bps: u16,
    /// To the winning solver (CT-052).
    pub solver_fee_bps: u16,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    Open,
    Dao,
}

/// D7 circle parameters. Durations in seconds, ratios in bps.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    pub epoch_length: u64,
    pub payment_window: u64,
    pub proposal_window: u64,
    pub base_limit_ratio: u16,
    pub limit_growth: u16,
    pub limit_cap: U128,
    pub limit_decay: u16,
    pub fees: Fees,
    pub admission: Admission,
    /// F9 (CT-082): cap of the seeker leg of a bundle in bps of the sum of its legs. Absent in JSON means the default.
    #[serde(default = "default_max_search_share_bps")]
    pub max_search_share_bps: u16,
    /// CT-094. NOT in the Borsh of `Params`: every `Epoch` already stores a `Params`, and those records cannot be rewritten. The two values live in their own keys
    /// (`x207_rules`, `x207_rules_pending`) and are laid over the stored `Params` by `get_params`, `get_pending_params` and `set_params`.
    #[borsh(skip)]
    #[serde(default = "default_bond_threshold")]
    pub bond_threshold: U128,
    #[borsh(skip)]
    #[serde(default = "default_bond_bps")]
    pub bond_bps: u16,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            epoch_length: DEFAULT_EPOCH_LENGTH,
            payment_window: DEFAULT_PAYMENT_WINDOW,
            proposal_window: DEFAULT_PROPOSAL_WINDOW,
            base_limit_ratio: DEFAULT_BASE_LIMIT_RATIO,
            limit_growth: DEFAULT_LIMIT_GROWTH,
            limit_cap: U128(DEFAULT_LIMIT_CAP),
            limit_decay: DEFAULT_LIMIT_DECAY,
            fees: Fees { clearing_fee_bps: 0, solver_fee_bps: 0 },
            admission: Admission::Open,
            max_search_share_bps: DEFAULT_MAX_SEARCH_SHARE_BPS,
            bond_threshold: U128(DEFAULT_BOND_THRESHOLD),
            bond_bps: DEFAULT_BOND_BPS,
        }
    }
}

fn require_range<T: PartialOrd>(value: T, min: T, max: T, field: &str) {
    if value < min || value > max {
        env::panic_str(&format!("ERR_PARAM_OUT_OF_RANGE: {field}"));
    }
}

/// Panics `ERR_PARAM_OUT_OF_RANGE: <field>` on the first field (struct order) outside its range.
/// Nested fields are written with a dot, e.g. `fees.clearing_fee_bps` (K-17).
pub fn validate_params(p: &Params) {
    require_range(p.epoch_length, EPOCH_LENGTH_MIN, EPOCH_LENGTH_MAX, "epoch_length");
    require_range(p.payment_window, PAYMENT_WINDOW_MIN, PAYMENT_WINDOW_MAX, "payment_window");
    require_range(p.proposal_window, PROPOSAL_WINDOW_MIN, PROPOSAL_WINDOW_MAX, "proposal_window");
    require_range(p.base_limit_ratio, 0, BASE_LIMIT_RATIO_MAX, "base_limit_ratio");
    require_range(p.limit_growth, 0, LIMIT_GROWTH_MAX, "limit_growth");
    require_range(p.limit_cap.0, LIMIT_CAP_MIN, LIMIT_CAP_MAX, "limit_cap");
    require_range(p.limit_decay, 0, LIMIT_DECAY_MAX, "limit_decay");
    require_range(p.fees.clearing_fee_bps, 0, CLEARING_FEE_BPS_MAX, "fees.clearing_fee_bps");
    require_range(p.fees.solver_fee_bps, 0, SOLVER_FEE_BPS_MAX, "fees.solver_fee_bps");
    require_range(p.max_search_share_bps, 0, MAX_SEARCH_SHARE_BPS_MAX, "max_search_share_bps");
    require_range(p.bond_threshold.0, BOND_THRESHOLD_MIN, BOND_THRESHOLD_MAX, "bond_threshold");
    require_range(p.bond_bps, BOND_BPS_MIN, BOND_BPS_MAX, "bond_bps");
}

const RULES_KEY: &[u8] = b"x207_rules";
const PENDING_RULES_KEY: &[u8] = b"x207_rules_pending";

/// `(bond_threshold, bond_bps)` in force now: the stored value, or the defaults for a circle that never set them (also a circle upgraded from 0.0.11).
pub fn current_rules() -> (u128, u16) {
    env::storage_read(RULES_KEY).map_or((DEFAULT_BOND_THRESHOLD, DEFAULT_BOND_BPS), |b| {
        let (t, bps): (u128, u16) = near_sdk::borsh::from_slice(&b).expect("rules are valid borsh");
        (t, bps)
    })
}

pub(crate) fn write_rules(threshold: u128, bps: u16) {
    if (threshold, bps) == (DEFAULT_BOND_THRESHOLD, DEFAULT_BOND_BPS) {
        env::storage_remove(RULES_KEY);
    } else {
        env::storage_write(RULES_KEY, &near_sdk::borsh::to_vec(&(threshold, bps)).expect("borsh"));
    }
}

/// `Params` as stored, with the bond rules in force laid over it.
pub fn with_rules(mut p: Params) -> Params {
    let (threshold, bps) = current_rules();
    p.bond_threshold = U128(threshold);
    p.bond_bps = bps;
    p
}

/// Called when a pending `Params` becomes the params of the next epoch: the pending bond rules come with it.
pub(crate) fn apply_pending_rules(effective_epoch: u64) {
    if let Some(bytes) = env::storage_read(PENDING_RULES_KEY) {
        let (t, bps, epoch): (u128, u16, u64) = near_sdk::borsh::from_slice(&bytes).expect("pending rules are valid borsh");
        if epoch == effective_epoch {
            write_rules(t, bps);
        }
        env::storage_remove(PENDING_RULES_KEY);
    }
}

impl Params {
    /// Names of the fields that differ from `other`, in struct order; nested fields use a dot (K-17).
    pub fn changed_fields(&self, other: &Params) -> Vec<String> {
        let checks = [
            ("epoch_length", self.epoch_length != other.epoch_length),
            ("payment_window", self.payment_window != other.payment_window),
            ("proposal_window", self.proposal_window != other.proposal_window),
            ("base_limit_ratio", self.base_limit_ratio != other.base_limit_ratio),
            ("limit_growth", self.limit_growth != other.limit_growth),
            ("limit_cap", self.limit_cap != other.limit_cap),
            ("limit_decay", self.limit_decay != other.limit_decay),
            ("fees.clearing_fee_bps", self.fees.clearing_fee_bps != other.fees.clearing_fee_bps),
            ("fees.solver_fee_bps", self.fees.solver_fee_bps != other.fees.solver_fee_bps),
            ("admission", self.admission != other.admission),
            ("max_search_share_bps", self.max_search_share_bps != other.max_search_share_bps),
            ("bond_threshold", self.bond_threshold != other.bond_threshold),
            ("bond_bps", self.bond_bps != other.bond_bps),
        ];
        checks.into_iter().filter(|(_, differs)| *differs).map(|(name, _)| name.to_string()).collect()
    }
}

#[near]
impl X207Circle {
    /// Owner only (the Sputnik DAO). Validates, then stores `params` as pending for the next epoch; the
    /// running epoch keeps its snapshot, and `close_epoch` applies the pending value. A second call in
    /// the same epoch overwrites the first. `old` in the event is the active params, never the pending ones.
    pub fn set_params(&mut self, params: Params) {
        self.assert_owner();
        validate_params(&params);
        let old = with_rules(self.epoch_or_panic(self.current_epoch).params.clone());
        let effective_epoch = self.current_epoch + 1;
        env::storage_write(PENDING_RULES_KEY, &near_sdk::borsh::to_vec(&(params.bond_threshold.0, params.bond_bps, effective_epoch)).expect("borsh"));
        let changed = old.changed_fields(&params);
        self.pending_params = Some(PendingParams { params: params.clone(), effective_epoch });
        ParamsUpdated { old, new: params, changed, effective_epoch }.emit();
    }

    pub fn get_pending_params(&self) -> Option<PendingParams> {
        self.pending_params.clone().map(|mut pending| {
            if let Some(bytes) = env::storage_read(PENDING_RULES_KEY) {
                let (t, bps, _): (u128, u16, u64) = near_sdk::borsh::from_slice(&bytes).expect("pending rules are valid borsh");
                pending.params.bond_threshold = U128(t);
                pending.params.bond_bps = bps;
            } else {
                let (t, bps) = current_rules();
                pending.params.bond_threshold = U128(t);
                pending.params.bond_bps = bps;
            }
            pending
        })
    }

    /// Params active in the running epoch: the snapshot of `current_epoch` (spec section 5).
    pub fn get_params(&self) -> Params {
        with_rules(self.epoch_or_panic(self.current_epoch).params.clone())
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    use super::*;

    fn with(f: impl FnOnce(&mut Params)) -> Params {
        let mut p = Params::default();
        f(&mut p);
        p
    }

    fn panic_message(p: &Params) -> String {
        let p = p.clone();
        let err = std::panic::catch_unwind(move || validate_params(&p)).unwrap_err();
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    }

    // TC-CT-007-01 (value part; the sandbox part asserts it through `new` and `get_params`)
    #[test]
    fn tc_ct_007_01_default_json() {
        let json = serde_json::to_value(Params::default()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "epoch_length": 3600, "payment_window": 1800, "proposal_window": 600,
                "base_limit_ratio": 8000, "limit_growth": 1000, "limit_cap": "1000000000",
                "limit_decay": 0,
                "fees": {"clearing_fee_bps": 0, "solver_fee_bps": 0},
                "admission": "open", "max_search_share_bps": 500,
                "bond_threshold": "100000000", "bond_bps": 2000
            })
        );
        validate_params(&Params::default());
    }

    // TC-CT-007-02
    #[test]
    fn tc_ct_007_02_all_fields_present() {
        let json = serde_json::to_value(Params::default()).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 12);
        assert_eq!(json["fees"].as_object().unwrap().len(), 2);
    }

    // TC-CT-007-03
    #[test]
    fn tc_ct_007_03_minimum_values_accepted() {
        validate_params(&with(|p| p.epoch_length = 300));
        validate_params(&with(|p| p.payment_window = 900));
        validate_params(&with(|p| p.proposal_window = 60));
        validate_params(&with(|p| p.base_limit_ratio = 0));
        validate_params(&with(|p| p.limit_growth = 0));
        validate_params(&with(|p| p.limit_cap = U128(1_000_000)));
        validate_params(&with(|p| p.limit_decay = 0));
        validate_params(&with(|p| p.fees.clearing_fee_bps = 0));
        validate_params(&with(|p| p.fees.solver_fee_bps = 0));
    }

    // TC-CT-007-04
    #[test]
    fn tc_ct_007_04_maximum_values_accepted() {
        validate_params(&with(|p| p.epoch_length = 604_800));
        validate_params(&with(|p| p.payment_window = 604_800));
        validate_params(&with(|p| p.proposal_window = 86_400));
        validate_params(&with(|p| p.base_limit_ratio = 10_000));
        validate_params(&with(|p| p.limit_growth = 10_000));
        validate_params(&with(|p| p.limit_cap = U128(100_000_000_000)));
        validate_params(&with(|p| p.limit_decay = 5_000));
        validate_params(&with(|p| p.fees.clearing_fee_bps = 100));
        validate_params(&with(|p| p.fees.solver_fee_bps = 100));
    }

    // TC-CT-007-05
    #[test]
    fn tc_ct_007_05_payment_window_below_15_minutes() {
        let msg = panic_message(&with(|p| p.payment_window = 899));
        assert!(msg.contains("ERR_PARAM_OUT_OF_RANGE: payment_window"), "{msg}");
    }

    // TC-CT-007-06
    #[test]
    fn tc_ct_007_06_below_minimum() {
        for (p, field) in [
            (with(|p| p.epoch_length = 299), "epoch_length"),
            (with(|p| p.proposal_window = 59), "proposal_window"),
            (with(|p| p.limit_cap = U128(999_999)), "limit_cap"),
        ] {
            let msg = panic_message(&p);
            assert!(msg.contains(&format!("ERR_PARAM_OUT_OF_RANGE: {field}")), "{msg}");
        }
    }

    // TC-CT-007-07
    #[test]
    fn tc_ct_007_07_above_maximum() {
        let cases: Vec<(Params, &str)> = vec![
            (with(|p| p.epoch_length = 604_801), "epoch_length"),
            (with(|p| p.payment_window = 604_801), "payment_window"),
            (with(|p| p.proposal_window = 86_401), "proposal_window"),
            (with(|p| p.base_limit_ratio = 10_001), "base_limit_ratio"),
            (with(|p| p.limit_growth = 10_001), "limit_growth"),
            (with(|p| p.limit_cap = U128(100_000_000_001)), "limit_cap"),
            (with(|p| p.limit_decay = 5_001), "limit_decay"),
            (with(|p| p.fees.clearing_fee_bps = 101), "fees.clearing_fee_bps"),
            (with(|p| p.fees.solver_fee_bps = 101), "fees.solver_fee_bps"),
        ];
        assert_eq!(cases.len(), 9);
        for (p, field) in cases {
            let msg = panic_message(&p);
            assert!(msg.contains(&format!("ERR_PARAM_OUT_OF_RANGE: {field}")), "{msg}");
        }
    }

    // TC-CT-007-08
    #[test]
    fn tc_ct_007_08_first_bad_field_in_struct_order() {
        let msg = panic_message(&with(|p| {
            p.epoch_length = 1;
            p.limit_decay = 9_999;
        }));
        assert!(msg.contains("ERR_PARAM_OUT_OF_RANGE: epoch_length"), "{msg}");
    }

    // TC-CT-007-09
    #[test]
    fn tc_ct_007_09_admission_values() {
        assert_eq!(serde_json::from_str::<Admission>("\"open\"").unwrap(), Admission::Open);
        assert_eq!(serde_json::from_str::<Admission>("\"dao\"").unwrap(), Admission::Dao);
        let err = serde_json::from_str::<Admission>("\"closed\"").unwrap_err().to_string();
        assert!(!err.contains("ERR_"), "{err}");
    }

    fn circle(params: Params) -> X207Circle {
        testing_env!(VMContextBuilder::new().block_timestamp(1_000).build());
        X207Circle::with_config(
            "dao.near".parse().unwrap(),
            "agents-main".to_string(),
            "usdt.near".parse().unwrap(),
            "intents.near".parse().unwrap(),
            params,
            env::block_timestamp(),
        )
    }

    // TC-CT-007-01 (default via root) and TC-CT-007-02
    #[test]
    fn tc_ct_007_01_get_params_returns_default_and_all_fields() {
        let c = circle(Params::default());
        assert_eq!(c.get_params(), Params::default());
        let json = serde_json::to_value(c.get_params()).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 12);
        assert_eq!(json["fees"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn get_params_reports_active_epoch_snapshot() {
        let custom = with(|p| p.epoch_length = 7_200);
        assert_eq!(circle(custom.clone()).get_params(), custom);
    }

    fn as_caller(who: &str, now: u64) {
        testing_env!(VMContextBuilder::new()
            .block_timestamp(now)
            .predecessor_account_id(who.parse().unwrap())
            .build());
    }

    fn params_events() -> Vec<serde_json::Value> {
        near_sdk::test_utils::get_logs()
            .iter()
            .filter(|l| l.contains("params_updated"))
            .map(|l| serde_json::from_str::<serde_json::Value>(l.trim_start_matches("EVENT_JSON:")).unwrap()["data"][0].clone())
            .collect()
    }

    fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("expected a panic");
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    // TC-CT-040-01
    #[test]
    fn tc_ct_040_01_owner_change_is_pending_for_next_epoch() {
        let mut c = circle(Params::default());
        as_caller("dao.near", 2_000);
        let new = with(|p| p.payment_window = 2_700);
        c.set_params(new.clone());
        assert_eq!(c.get_params(), Params::default());
        let pending = c.get_pending_params().unwrap();
        assert_eq!((pending.params, pending.effective_epoch), (new.clone(), 2));
        let e = params_events();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0]["changed"], serde_json::json!(["payment_window"]));
        assert_eq!(e[0]["effective_epoch"], serde_json::json!(2));
        assert_eq!(e[0]["old"]["payment_window"], serde_json::json!(1800));
        assert_eq!(e[0]["new"]["payment_window"], serde_json::json!(2700));
    }

    // TC-CT-040-02 (unit approximation of the sandbox case): close_epoch applies the pending params
    #[test]
    fn tc_ct_040_02_applied_by_close_epoch() {
        let mut c = circle(Params::default());
        as_caller("dao.near", 2_000);
        let new = with(|p| p.payment_window = 2_700);
        c.set_params(new.clone());
        as_caller("keeper.near", 1_000 + 3600 * 1_000_000_000);
        c.close_epoch(1);
        assert_eq!(c.get_params(), new);
        assert!(c.get_pending_params().is_none());
        assert_eq!(c.get_epoch(Some(1)).unwrap().params, Params::default());
    }

    // TC-CT-040-03
    #[test]
    fn tc_ct_040_03_not_owner() {
        let mut c = circle(Params::default());
        as_caller("translator.near", 2_000);
        let msg = panic_text(std::panic::AssertUnwindSafe(|| c.set_params(with(|p| p.payment_window = 2_700))));
        assert!(msg.contains("ERR_NOT_OWNER"), "{msg}");
        assert!(c.get_pending_params().is_none());
    }

    // TC-CT-040-04
    #[test]
    fn tc_ct_040_04_out_of_range() {
        let mut c = circle(Params::default());
        as_caller("dao.near", 2_000);
        let msg = panic_text(std::panic::AssertUnwindSafe(|| c.set_params(with(|p| p.payment_window = 899))));
        assert!(msg.contains("ERR_PARAM_OUT_OF_RANGE: payment_window"), "{msg}");
        assert!(c.get_pending_params().is_none());
    }

    // TC-CT-040-05
    #[test]
    fn tc_ct_040_05_second_call_overwrites() {
        let mut c = circle(Params::default());
        as_caller("dao.near", 2_000);
        c.set_params(with(|p| p.payment_window = 2_700));
        let second = with(|p| p.proposal_window = 300);
        c.set_params(second.clone());
        assert_eq!(c.get_pending_params().unwrap().params, second);
        let e = params_events();
        assert_eq!(e.len(), 2);
        assert_eq!(e[1]["old"]["payment_window"], serde_json::json!(1800));
        assert_eq!(e[1]["changed"], serde_json::json!(["proposal_window"]));
    }

    // TC-CT-040-07
    #[test]
    fn tc_ct_040_07_several_fields_changed() {
        let mut c = circle(Params::default());
        as_caller("dao.near", 2_000);
        c.set_params(with(|p| {
            p.epoch_length = 7_200;
            p.limit_cap = U128(2_000_000_000);
        }));
        assert_eq!(params_events()[0]["changed"], serde_json::json!(["epoch_length", "limit_cap"]));
    }

    #[test]
    fn nested_and_enum_fields_are_named() {
        let a = Params::default();
        let b = with(|p| {
            p.fees.solver_fee_bps = 10;
            p.admission = Admission::Dao;
        });
        assert_eq!(a.changed_fields(&b), ["fees.solver_fee_bps", "admission"]);
    }

    // TC-CT-040-08
    #[test]
    fn tc_ct_040_08_identical_params_accepted() {
        let mut c = circle(Params::default());
        as_caller("dao.near", 2_000);
        c.set_params(Params::default());
        assert_eq!(params_events()[0]["changed"], serde_json::json!([]));
        assert!(c.get_pending_params().is_some());
    }

    // TC-CT-040-09
    #[test]
    fn tc_ct_040_09_does_not_touch_d6() {
        let mut c = circle(Params::default());
        as_caller("dao.near", 2_000);
        let before = c.get_pool();
        c.set_params(with(|p| p.base_limit_ratio = 5_000));
        assert_eq!(c.get_pool(), before);
    }

    // TC-CT-040-null: nothing pending right after init
    #[test]
    fn no_pending_params_after_init() {
        assert!(circle(Params::default()).get_pending_params().is_none());
    }

    // ---- CT-094: the bond rules ride next to Params, not inside its Borsh ----

    fn as_owner() {
        testing_env!(VMContextBuilder::new().predecessor_account_id("dao.near".parse().unwrap()).block_timestamp(2_000).build());
    }

    // TC-CT-094-P01
    #[test]
    fn the_bond_rules_have_defaults_ranges_and_a_json_that_older_clients_may_leave_out() {
        let c = circle(Params::default());
        assert_eq!(current_rules(), (DEFAULT_BOND_THRESHOLD, DEFAULT_BOND_BPS));
        assert_eq!((c.get_params().bond_threshold, c.get_params().bond_bps), (U128(100_000_000), 2_000));
        // a set_params call that does not know the two fields
        let old_client = r#"{"epoch_length":3600,"payment_window":1800,"proposal_window":600,"base_limit_ratio":8000,"limit_growth":1000,"limit_cap":"1000000000","limit_decay":0,"fees":{"clearing_fee_bps":0,"solver_fee_bps":0},"admission":"open"}"#;
        let parsed: Params = serde_json::from_str(old_client).unwrap();
        assert_eq!((parsed.bond_threshold, parsed.bond_bps, parsed.max_search_share_bps), (U128(100_000_000), 2_000, 500));
        validate_params(&parsed);
        for (name, p) in [
            ("bond_threshold", with(|p| p.bond_threshold = U128(BOND_THRESHOLD_MIN - 1))),
            ("bond_threshold", with(|p| p.bond_threshold = U128(BOND_THRESHOLD_MAX + 1))),
            ("bond_bps", with(|p| p.bond_bps = BOND_BPS_MIN - 1)),
            ("bond_bps", with(|p| p.bond_bps = BOND_BPS_MAX + 1)),
        ] {
            let msg = panic_text(std::panic::AssertUnwindSafe(|| validate_params(&p)));
            assert!(msg.contains(&format!("ERR_PARAM_OUT_OF_RANGE: {name}")), "{msg}");
        }
        validate_params(&with(|p| (p.bond_threshold, p.bond_bps) = (U128(BOND_THRESHOLD_MIN), BOND_BPS_MIN)));
        validate_params(&with(|p| (p.bond_threshold, p.bond_bps) = (U128(BOND_THRESHOLD_MAX), BOND_BPS_MAX)));
    }

    // TC-CT-094-P02: pending until the next epoch, then in force; the event says what changed
    #[test]
    fn new_bond_rules_wait_for_the_next_epoch_and_the_event_lists_them() {
        let mut c = circle(Params::default());
        as_owner();
        let new = with(|p| (p.bond_threshold, p.bond_bps) = (U128(50_000_000), 1_000));
        c.set_params(new.clone());
        assert!(near_sdk::test_utils::get_logs().iter().any(|l| l.contains("params_updated") && l.contains("\"bond_threshold\"") && l.contains("bond_bps")));
        assert_eq!(current_rules(), (DEFAULT_BOND_THRESHOLD, DEFAULT_BOND_BPS), "not in force yet");
        assert_eq!(c.get_params().bond_bps, 2_000);
        let pending = c.get_pending_params().unwrap();
        assert_eq!((pending.params.bond_threshold, pending.params.bond_bps, pending.effective_epoch), (U128(50_000_000), 1_000, 2));
        // the epoch turns: the pending params and the rules come in together
        apply_pending_rules(2);
        assert_eq!(current_rules(), (50_000_000, 1_000));
        assert_eq!((c.get_params().bond_threshold, c.get_params().bond_bps), (U128(50_000_000), 1_000));
        // back to the defaults removes the key
        c.set_params(Params::default());
        apply_pending_rules(2);
        assert_eq!(current_rules(), (DEFAULT_BOND_THRESHOLD, DEFAULT_BOND_BPS));
        assert!(!env::storage_has_key(RULES_KEY));
    }

    // TC-CT-094-P03: the Borsh of Params is the one of 0.0.11, so the stored epochs still read
    #[test]
    fn the_borsh_of_params_does_not_carry_the_bond_rules() {
        let a = near_sdk::borsh::to_vec(&Params::default()).unwrap();
        let b = near_sdk::borsh::to_vec(&with(|p| (p.bond_threshold, p.bond_bps) = (U128(50_000_000), 1_000))).unwrap();
        assert_eq!(a, b, "the two fields are skipped");
        let back: Params = near_sdk::borsh::from_slice(&b).unwrap();
        assert_eq!(back.epoch_length, 3_600);
    }
}
