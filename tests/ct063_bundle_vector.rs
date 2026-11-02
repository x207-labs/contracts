//! CT-063 against the SDK vector (`sdk/spec/test-vectors/barter/bundle_abca.json`, copy in `tests/vectors/barter/`):
//! the contract builds the same canonical terms, the same `terms_hash` and the same NEP-413 signing hash, and the
//! participant signatures of the vector verify with the voucher keys of the vector.

use near_sdk::test_utils::VMContextBuilder;
use near_sdk::testing_env;
use serde_json::Value;
use x207_circle::bundle::{terms_message, BundleLeg};
use x207_circle::canonical::{nep413_signing_hash, parse_signature};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn contract_matches_the_sdk_bundle_vector() {
    testing_env!(VMContextBuilder::new().build());
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/barter/bundle_abca.json")).unwrap();
    let v: Value = serde_json::from_str(&raw).unwrap();
    let circle = v["circle"].as_str().unwrap().parse().unwrap();
    let legs: Vec<BundleLeg> = v["terms"]["legs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| BundleLeg {
            payer: l["payer"].as_str().unwrap().parse().unwrap(),
            payee: l["payee"].as_str().unwrap().parse().unwrap(),
            max_amount: near_sdk::json_types::U128(l["max_amount"].as_str().unwrap().parse().unwrap()),
        })
        .collect();
    let message = terms_message(&circle, v["terms"]["epoch"].as_u64().unwrap(), &legs);
    assert_eq!(message, v["message"].as_str().unwrap(), "canonical message, legs sorted by (payer, payee)");
    let terms_hash = near_sdk::env::sha256_array(message.as_bytes());
    assert_eq!(hex(&terms_hash), v["termsHash"].as_str().unwrap());
    let signing_hash = nep413_signing_hash(&message, terms_hash, circle.as_str());
    assert_eq!(hex(&signing_hash), v["signingHash"].as_str().unwrap());

    for s in v["signatures"].as_array().unwrap() {
        let account = s["account"].as_str().unwrap();
        let pubkey = v["participants"][account].as_str().unwrap();
        let key: near_sdk::PublicKey = pubkey.parse().unwrap();
        let sig = parse_signature(s["signature"].as_str().unwrap()).expect("64 byte signature");
        let pk = <[u8; 32]>::try_from(&key.as_bytes()[1..]).unwrap();
        assert!(near_sdk::env::ed25519_verify(&sig, &signing_hash, &pk), "{account}");
    }
}
