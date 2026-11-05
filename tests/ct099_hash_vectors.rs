//! CT-099: the hashes of the contract against the golden vector `tests/vectors/clearing_hashes.json` (written and checked by `scripts/clearing_hashes.py`): the same
//! canonical JSON, the same sha256, for `edges_hash` (the edges with `cleared > 0`, sorted by debtor and creditor) and `positions_hash` (all positions sorted by account id).

use near_sdk::json_types::{I128, U128};
use near_sdk::test_utils::VMContextBuilder;
use near_sdk::testing_env;
use serde_json::Value;
use x207_circle::events::{edges_hash, positions_hash, PositionEntry};
use x207_circle::types::ClearingEdge;

fn vector() -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/vectors/clearing_hashes.json"
        ))
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn edges_hash_matches_the_golden_vector() {
    testing_env!(VMContextBuilder::new().build());
    for case in vector()["edges"].as_array().unwrap() {
        let edges: Vec<ClearingEdge> = case["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| ClearingEdge {
                debtor: e["debtor"].as_str().unwrap().parse().unwrap(),
                creditor: e["creditor"].as_str().unwrap().parse().unwrap(),
                cleared: U128(e["cleared"].as_str().unwrap().parse().unwrap()),
            })
            .collect();
        assert_eq!(
            edges_hash(&edges),
            case["edges_hash"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn positions_hash_matches_the_golden_vector() {
    testing_env!(VMContextBuilder::new().build());
    for case in vector()["positions"].as_array().unwrap() {
        let positions: Vec<PositionEntry> = case["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| PositionEntry {
                account_id: p["account_id"].as_str().unwrap().parse().unwrap(),
                net: I128(p["net"].as_str().unwrap().parse().unwrap()),
                role: p["role"].as_str().unwrap().to_string(),
                status: p["status"].as_str().map(str::to_string),
                amount: U128(p["amount"].as_str().unwrap().parse().unwrap()),
                outstanding_debt: U128(p["outstanding_debt"].as_str().unwrap().parse().unwrap()),
            })
            .collect();
        assert_eq!(
            positions_hash(&positions),
            case["positions_hash"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
}
