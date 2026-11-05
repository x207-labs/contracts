//! CT-113: replay of EVM `Action` signatures across circles and networks (read-only review of 0.0.15, PoC tests, nothing deployed).
//! No sandbox: the digests and `recover` of `x207_circle::evm` run under the mocked blockchain of near-sdk. A fixed test key is
//! used (not a Hardhat key, never funded). Each test says what a replay would need and shows that the contract refuses it.

use near_sdk::test_utils::VMContextBuilder;
use near_sdk::{testing_env, AccountId};
use secp256k1::{Message, Secp256k1, SecretKey};
use sha3::{Digest, Keccak256};
use x207_circle::evm::{action_digest, address_string, domain_salt, recover, ActionMsg};

const NOW_NS: u64 = 1_800_000_000_000_000_000;
const DEADLINE: u64 = 1_900_000_000_000;

fn acc(s: &str) -> AccountId {
    s.parse().unwrap()
}

fn at(contract: &str) {
    testing_env!(VMContextBuilder::new()
        .current_account_id(acc(contract))
        .block_timestamp(NOW_NS)
        .build());
}

fn key() -> SecretKey {
    SecretKey::from_slice(&[0x42u8; 32]).unwrap()
}

fn address() -> String {
    let public = key().public_key(&Secp256k1::new()).serialize_uncompressed();
    address_string(&Keccak256::digest(&public[1..])[12..].try_into().unwrap())
}

fn sign(digest: &[u8; 32]) -> String {
    let (id, compact) = Secp256k1::new()
        .sign_ecdsa_recoverable(&Message::from_slice(digest).unwrap(), &key())
        .serialize_compact();
    let mut bytes = compact.to_vec();
    bytes.push(27 + id.to_i32() as u8);
    format!(
        "eip712:0x{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

fn msg<'a>(circle: &'a str, member: &'a str, action: &'a str, nonce: u64) -> ActionMsg<'a> {
    ActionMsg {
        circle,
        member,
        action,
        epoch: 0,
        receiver: "",
        terms_hash: "",
        nonce,
        deadline: DEADLINE,
        max_fee: 0,
    }
}

fn recovered(digest: &[u8; 32], signature: &str) -> Option<String> {
    recover(digest, signature).map(|a| address_string(&a))
}

/// TC-CT-113-01: an Action that the member signed for `x207-v014.testnet` (same network, same member id, same nonce 0) does not
/// verify at `x207-v015.testnet`, whatever `circle` the replayer writes in the message.
#[test]
fn tc_ct_113_01_v014_action_does_not_verify_at_v015() {
    let me = address();
    at("x207-v014.testnet");
    let salt_v014 = domain_salt();
    let signature = sign(&action_digest(&msg(
        "x207-v014.testnet",
        &me,
        "request_exit",
        0,
    )));
    assert_eq!(
        recovered(
            &action_digest(&msg("x207-v014.testnet", &me, "request_exit", 0)),
            &signature
        ),
        Some(me.clone()),
        "control: verifies at v014"
    );

    at("x207-v015.testnet");
    assert_ne!(domain_salt(), salt_v014, "the salt names the contract");
    // the replayer keeps the signed message (circle = v014): the domain differs
    assert_ne!(
        recovered(
            &action_digest(&msg("x207-v014.testnet", &me, "request_exit", 0)),
            &signature
        ),
        Some(me.clone())
    );
    // the replayer writes the circle of v015 into the message: the message differs
    assert_ne!(
        recovered(
            &action_digest(&msg("x207-v015.testnet", &me, "request_exit", 0)),
            &signature
        ),
        Some(me.clone())
    );
}

/// TC-CT-113-02: the domain does not hide behind the `circle` field alone. A signature over (domain of v014, circle = v015)
/// verifies nowhere: not at v014 (circle differs from the account) and not at v015 (salt differs).
#[test]
fn tc_ct_113_02_domain_and_circle_field_are_both_needed() {
    let me = address();
    at("x207-v014.testnet");
    let mixed = sign(&action_digest(&msg("x207-v015.testnet", &me, "claim", 3)));
    assert_ne!(
        recovered(
            &action_digest(&msg("x207-v014.testnet", &me, "claim", 3)),
            &mixed
        ),
        Some(me.clone())
    );
    at("x207-v015.testnet");
    assert_ne!(
        recovered(
            &action_digest(&msg("x207-v015.testnet", &me, "claim", 3)),
            &mixed
        ),
        Some(me.clone())
    );
}

/// TC-CT-113-03: `auto_settle` and `auto_settle_off` are different signed messages, so an "off" held by a relayer cannot be
/// replayed as "on" (and the other way round), and a consumed nonce is a different message from the next nonce.
#[test]
fn tc_ct_113_03_auto_settle_on_and_off_are_not_interchangeable() {
    let me = address();
    at("x207-v015.testnet");
    let off = sign(&action_digest(&msg(
        "x207-v015.testnet",
        &me,
        "auto_settle_off",
        4,
    )));
    assert_eq!(
        recovered(
            &action_digest(&msg("x207-v015.testnet", &me, "auto_settle_off", 4)),
            &off
        ),
        Some(me.clone()),
        "control"
    );
    assert_ne!(
        recovered(
            &action_digest(&msg("x207-v015.testnet", &me, "auto_settle", 4)),
            &off
        ),
        Some(me.clone()),
        "off as on"
    );
    assert_ne!(
        recovered(
            &action_digest(&msg("x207-v015.testnet", &me, "auto_settle_off", 5)),
            &off
        ),
        Some(me.clone()),
        "off at the next nonce"
    );
}

/// TC-CT-113-04 (documents the property that CAN be abused): the signature has no network word other than the salt, and the salt
/// takes `mainnet` for every account that does not end in `.testnet`. The same account name on mainnet, on a sandbox and on a
/// private fork gets one salt, so only the account name separates them.
#[test]
fn tc_ct_113_04_every_non_testnet_name_shares_the_mainnet_label() {
    at("x207-circle.near");
    let on_mainnet = domain_salt();
    // the contract cannot tell mainnet from a sandbox or localnet that holds the same account name: the salt is a function of the name only
    for name in ["x207-circle.near", "x207-circle.near"] {
        at(name);
        assert_eq!(domain_salt(), on_mainnet);
    }
    // a different name or the testnet suffix does separate
    at("x207-circle.testnet");
    assert_ne!(domain_salt(), on_mainnet);
    at("x207-circle2.near");
    assert_ne!(domain_salt(), on_mainnet);
}
