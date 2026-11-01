//! CT-023: voucher test vectors from `sdk/spec/test-vectors/voucher/`, shared with the TypeScript CI.
//! Set `X207_TEST_VECTORS` to read another folder. A missing folder fails the test (TC-CT-023-04).

use std::fs;
use std::path::{Path, PathBuf};

use near_sdk::json_types::U128;
use near_sdk::serde_json::{self, Value};
use near_sdk::test_utils::VMContextBuilder;
use near_sdk::{env, AccountId, MockedBlockchain, PublicKey};
use x207_circle::canonical::{parse_signature, voucher_message, voucher_nonce, voucher_payload, voucher_signing_hash, Voucher};
use x207_circle::clearing::Epoch;
use x207_circle::member::{Member, MemberStatus};
use x207_circle::params::Params;
use x207_circle::X207Circle;

const CREDIT: u128 = 1_000_000;

fn vectors_dir() -> PathBuf {
    let dir = std::env::var("X207_TEST_VECTORS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/spec/test-vectors"));
    voucher_folder(dir)
}

fn voucher_folder(dir: PathBuf) -> PathBuf {
    assert!(dir.join("voucher").is_dir(), "test vector folder missing: {}", dir.join("voucher").display());
    dir.join("voucher")
}

fn load(dir: &Path) -> Vec<Value> {
    let mut files: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    files.sort();
    assert!(!files.is_empty(), "no vectors in {}", dir.display());
    files.iter().map(|p| serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn acc(s: &str) -> AccountId {
    s.parse().unwrap()
}

fn voucher_of(v: &Value) -> Voucher {
    let mut v = v.clone();
    v.as_object_mut().unwrap().remove("signature");
    serde_json::from_value(v).expect("vector voucher fits the Voucher struct")
}

fn mock(current: &str, now_ns: u64) {
    near_sdk::env::set_blockchain_interface(MockedBlockchain::new(
        VMContextBuilder::new().current_account_id(acc(current)).block_timestamp(now_ns).build(),
        near_sdk::test_vm_config(),
        near_sdk::RuntimeFeesConfig::test(),
        Default::default(),
        Default::default(),
        Default::default(),
        None,
    ));
}

/// Signature check as the contract performs it, without state (`parse_signature` + `ed25519_verify`).
fn verifies(voucher: &Voucher, signature: &str, key: &PublicKey) -> bool {
    mock(voucher.circle.as_str(), 0);
    let pk: [u8; 32] = key.as_bytes()[1..].try_into().unwrap();
    parse_signature(signature).is_some_and(|sig| env::ed25519_verify(&sig, &voucher_signing_hash(voucher), &pk))
}

/// A circle at `voucher.circle` whose epoch `voucher.epoch` is open, with payer and payee active
/// under `key`. `last_seq` and the cutoff come from the vector `context`.
fn circle_for(voucher: &Voucher, key: &PublicKey, context: Option<&Value>) -> (X207Circle, u64) {
    let cutoff_ns = context.and_then(|c| c["cutoffAtMs"].as_u64()).map_or(2_000_000_000_000_000_000, |ms| ms * 1_000_000);
    let now_ns = context.and_then(|c| c["nowMs"].as_u64()).map_or(1_000_000_000_000_000_000, |ms| ms * 1_000_000);
    mock(voucher.circle.as_str(), now_ns);
    let mut c = X207Circle::with_config(acc("dao.near"), "agents-main".into(), acc("usdt.near"), acc("intents.near"), Params::default(), 1);
    c.epochs.insert(voucher.epoch, Epoch::open(voucher.epoch, 1, cutoff_ns, Params::default()));
    c.current_epoch = voucher.epoch;
    for id in [&voucher.payer, &voucher.payee] {
        c.members.insert(
            id.clone(),
            Member {
                account_id: id.clone(),
                pubkey_voucher: key.clone(),
                terms_hash: "a".repeat(64),
                stake: 100 * CREDIT,
                limit: 80 * CREDIT,
                status: MemberStatus::Active,
                admitted: false,
                joined_at: 0,
                settled_history: 0,
                last_active_epoch: 0,
                exit_requested_epoch: None,
                withdraw_pending: false,
                reactivate_epoch: None,
            },
        );
    }
    if let Some(last_seq) = context.and_then(|c| c["lastSeq"].as_u64()) {
        c.pairs.insert(
            (voucher.epoch, voucher.payer.clone(), voucher.payee.clone()),
            x207_circle::voucher::PairState { cumulative: voucher.cumulative_amount.0, last_seq, cleared: 0 },
        );
    }
    (c, now_ns)
}

fn panic_text(f: impl FnOnce()) -> Option<String> {
    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).err()?;
    Some(err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default())
}

/// Spec 3.3 written out byte by byte, with `amount` as the literal text of `cumulativeAmount`.
fn payload_for(voucher: &Voucher, amount: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let message = format!(
        r#"{{"circle":"{}","payer":"{}","payee":"{}","epoch":{},"seq":{},"cumulativeAmount":"{amount}","unit":"credit","resource":"{}"}}"#,
        voucher.circle, voucher.payer, voucher.payee, voucher.epoch, voucher.seq, voucher.resource
    );
    let nonce: [u8; 32] = Sha256::digest(format!("x207:{}:{}:{}:{}", voucher.payer, voucher.payee, voucher.epoch, voucher.seq)).into();
    let mut payload = 2_147_484_061u32.to_le_bytes().to_vec();
    payload.extend((message.len() as u32).to_le_bytes());
    payload.extend(message.as_bytes());
    payload.extend(nonce);
    payload.extend((voucher.circle.as_str().len() as u32).to_le_bytes());
    payload.extend(voucher.circle.as_str().as_bytes());
    payload.push(0);
    payload
}

// TC-CT-023-01: message, nonce, payload and signing hash equal the vector byte for byte.
#[test]
fn valid_vector_bytes_match() {
    let vectors = load(&vectors_dir());
    let valid = vectors.iter().find(|v| v["name"] == "valid").expect("valid vector");
    let voucher = voucher_of(&valid["input"]["voucher"]);
    mock(voucher.circle.as_str(), 0);
    let expected = &valid["expected"];
    assert_eq!(voucher_message(&voucher), expected["message"].as_str().unwrap());
    assert_eq!(hex(&voucher_nonce(&voucher)), expected["nonce"].as_str().unwrap());
    assert_eq!(hex(&voucher_payload(&voucher)), expected["payload"].as_str().unwrap());
    assert_eq!(hex(&voucher_signing_hash(&voucher)), expected["signingHash"].as_str().unwrap());
}

// TC-CT-023-01..03: verification result equals `expected.valid` for every vector.
#[test]
fn verification_matches_expected_for_every_vector() {
    for vector in load(&vectors_dir()) {
        let input = &vector["input"];
        let voucher = voucher_of(&input["voucher"]);
        let key: PublicKey = input["publicKey"].as_str().unwrap().parse().unwrap();
        let signature = input["voucher"]["signature"].as_str().unwrap();
        assert_eq!(verifies(&voucher, signature, &key), vector["expected"]["valid"].as_bool().unwrap(), "{}", vector["name"]);
    }
}

// TC-CT-023-02/03: through `register_cumulative` every vector ends with its `contractError`, and the
// accepted one registers. A tampered `unit` is stopped by the cheaper unit check before the signature.
#[test]
fn register_cumulative_matches_expected_for_every_vector() {
    for vector in load(&vectors_dir()) {
        let name = vector["name"].as_str().unwrap().to_string();
        let input = &vector["input"];
        let voucher = voucher_of(&input["voucher"]);
        let key: PublicKey = input["publicKey"].as_str().unwrap().parse().unwrap();
        let signature = input["voucher"]["signature"].as_str().unwrap().to_string();
        let (mut c, now_ns) = circle_for(&voucher, &key, input.get("context"));
        let outcome = panic_text(|| {
            c.register_cumulative(voucher.clone(), signature.clone());
        });
        let expected_error = match name.as_str() {
            "valid" => None,
            "tampered_unit" => Some("ERR_BAD_UNIT"),
            _ => vector["expected"]["contractError"].as_str(),
        };
        match (expected_error, outcome) {
            (None, None) => assert_eq!(c.last_seq(voucher.payer.clone(), voucher.payee.clone(), voucher.epoch), voucher.seq, "{name}"),
            (Some(code), Some(text)) => assert!(text.contains(code), "{name}: expected {code}, got {text} (now {now_ns})"),
            (want, got) => panic!("{name}: expected {want:?}, got {got:?}"),
        }
    }
}

// TC-CT-023-04
#[test]
#[should_panic(expected = "test vector folder missing")]
fn missing_folder_fails_loudly() {
    let _ = voucher_folder(PathBuf::from("/nonexistent/x207-vectors"));
}

// TC-CT-023-06: a signature over `"cumulativeAmount":"007"` fails, because the contract rebuilds "7".
#[test]
fn leading_zero_amount_does_not_verify() {
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    let valid = load(&vectors_dir()).into_iter().find(|v| v["name"] == "valid").unwrap();
    let mut json = valid["input"]["voucher"].clone();
    json["cumulativeAmount"] = Value::from("007");
    json.as_object_mut().unwrap().remove("signature");
    let voucher: Voucher = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(voucher.cumulative_amount, U128(7));
    // Spec 3.3 byte by byte, with the literal "007" in the message.
    let payload = payload_for(&voucher, "007");
    let signing_key = SigningKey::from_bytes(&[9u8; 32]);
    let signature = format!("ed25519:{}", near_sdk::bs58::encode(signing_key.sign(&Sha256::digest(&payload)).to_bytes()).into_string());
    let key = PublicKey::from_parts(near_sdk::CurveType::ED25519, signing_key.verifying_key().to_bytes().to_vec()).unwrap();
    assert!(!verifies(&voucher, &signature, &key));
    // Control: the same builder with "7" verifies, so the "007" failure comes from the leading zeros.
    let canonical = payload_for(&voucher, "7");
    let good = format!("ed25519:{}", near_sdk::bs58::encode(signing_key.sign(&Sha256::digest(&canonical)).to_bytes()).into_string());
    assert!(verifies(&voucher, &good, &key));
}
