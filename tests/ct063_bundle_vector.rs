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
    check("bundle_abca");
}

/// CT-094 / SD-073: the terms of a bundle with an arbiter (`arbiter`, `agreement_hash`, `confirm_by` after the legs), with and without the agreement hash.
#[test]
fn contract_matches_the_sdk_deal_vectors() {
    check("bundle_deal");
    check("bundle_deal_no_agreement");
}

fn check(name: &str) {
    testing_env!(VMContextBuilder::new().build());
    let raw = std::fs::read_to_string(format!("{}/tests/vectors/barter/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap();
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
    let terms = &v["terms"];
    let arbiter: Option<near_sdk::AccountId> = terms["arbiter"].as_str().map(|a| a.parse().unwrap());
    let confirm_by: Option<u64> = terms["confirm_by"].as_str().map(|c| c.parse().unwrap());
    let deal = arbiter.as_ref().map(|a| (a, terms["agreement_hash"].as_str(), confirm_by.expect("a deal has confirm_by")));
    let message = terms_message(&circle, terms["epoch"].as_u64().unwrap(), &legs, None, deal);
    assert_eq!(message, v["message"].as_str().unwrap(), "{name}: canonical message, legs sorted by (payer, payee)");
    let terms_hash = near_sdk::env::sha256_array(message.as_bytes());
    assert_eq!(hex(&terms_hash), v["termsHash"].as_str().unwrap(), "{name}");
    let signing_hash = nep413_signing_hash(&message, terms_hash, circle.as_str());
    assert_eq!(hex(&signing_hash), v["signingHash"].as_str().unwrap(), "{name}");

    for s in v["signatures"].as_array().unwrap() {
        let account = s["account"].as_str().unwrap();
        let pubkey = v["participants"][account].as_str().unwrap();
        let key: near_sdk::PublicKey = pubkey.parse().unwrap();
        let sig = parse_signature(s["signature"].as_str().unwrap()).expect("64 byte signature");
        let pk = <[u8; 32]>::try_from(&key.as_bytes()[1..]).unwrap();
        assert!(near_sdk::env::ed25519_verify(&sig, &signing_hash, &pk), "{account}");
    }
}
