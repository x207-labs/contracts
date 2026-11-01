//! CT-011 ABI checks. Needs `cargo near build non-reproducible-wasm` first.

const ABI: &str = "target/near/x207_circle_abi.json";

fn function_names() -> Vec<String> {
    let abi: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(ABI).expect("run cargo near build first")).unwrap();
    abi["body"]["functions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect()
}

// TC-CT-011-08
#[test]
fn credit_ledger_is_not_a_token() {
    let names = function_names();
    for forbidden in ["ft_balance_of", "mt_balance_of", "ft_total_supply", "mt_transfer", "mt_batch_balance_of"] {
        assert!(!names.iter().any(|n| n == forbidden), "{forbidden} must not exist");
    }
    for view in ["obligation", "list_obligations", "last_seq", "outstanding_debt"] {
        assert!(names.iter().any(|n| n == view), "{view} view missing");
    }
}
