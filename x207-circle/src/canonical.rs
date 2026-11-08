//! Voucher struct and canonical signing bytes (spec 3.2, 3.3). Self-contained: gas probes include
//! this file by path. The test vectors in `sdk/spec/test-vectors/` win over this code (spec 1.5).

use near_sdk::borsh::{self, BorshSerialize};
use near_sdk::json_types::U128;
use near_sdk::{env, near, serde_json, AccountId};

/// NEP-413 tag: 2^31 + 413.
pub const NEP413_TAG: u32 = 2_147_484_061;

/// Field order here is the canonical key order of the signed JSON message.
#[near(serializers = [json])]
#[serde(rename_all = "camelCase")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Voucher {
    pub circle: AccountId,
    pub payer: AccountId,
    pub payee: AccountId,
    pub epoch: u64,
    pub seq: u64,
    pub cumulative_amount: U128,
    pub unit: String,
    pub resource: String,
}

#[derive(BorshSerialize)]
#[borsh(crate = "near_sdk::borsh")]
struct Nep413Payload {
    message: String,
    nonce: [u8; 32],
    recipient: String,
    callback_url: Option<String>,
}

/// Canonical JSON without `signature`: fixed key order, no spaces.
pub fn voucher_message(voucher: &Voucher) -> String {
    serde_json::to_string(voucher).expect("voucher serializes")
}

/// `sha256("x207:" + payer + ":" + payee + ":" + epoch + ":" + seq)`.
pub fn voucher_nonce(voucher: &Voucher) -> [u8; 32] {
    let seed = format!(
        "x207:{}:{}:{}:{}",
        voucher.payer, voucher.payee, voucher.epoch, voucher.seq
    );
    env::sha256_array(seed.as_bytes())
}

/// `borsh(tag) || borsh({ message, nonce, recipient, callback_url })`.
pub fn voucher_payload(voucher: &Voucher) -> Vec<u8> {
    let payload = Nep413Payload {
        message: voucher_message(voucher),
        nonce: voucher_nonce(voucher),
        recipient: voucher.circle.to_string(),
        callback_url: None,
    };
    let mut bytes = borsh::to_vec(&NEP413_TAG).expect("tag serializes");
    bytes.extend(borsh::to_vec(&payload).expect("payload serializes"));
    bytes
}

/// The bytes that `ed25519_verify` checks: `sha256(payload)`.
pub fn voucher_signing_hash(voucher: &Voucher) -> [u8; 32] {
    env::sha256_array(voucher_payload(voucher))
}

/// Decodes `"ed25519:<base58 64 bytes>"`; `None` for a malformed string or a wrong length.
pub fn parse_signature(signature: &str) -> Option<[u8; 64]> {
    let encoded = signature.strip_prefix("ed25519:")?;
    near_sdk::bs58::decode(encoded)
        .into_vec()
        .ok()?
        .try_into()
        .ok()
}

/// `sha256` of the NEP-413 payload for an arbitrary `message` and 32 byte `nonce` (barter bundles: `message` is the
/// canonical terms JSON and `nonce` is the `terms_hash`; `sdk/spec/barter-format.md`).
pub fn nep413_signing_hash(message: &str, nonce: [u8; 32], recipient: &str) -> [u8; 32] {
    let payload = Nep413Payload {
        message: message.to_string(),
        nonce,
        recipient: recipient.to_string(),
        callback_url: None,
    };
    let mut bytes = borsh::to_vec(&NEP413_TAG).expect("tag serializes");
    bytes.extend(borsh::to_vec(&payload).expect("payload serializes"));
    env::sha256_array(bytes)
}
