//! CT-003 gas probe. `probe_measure` splits the cost of one registration signature check into
//! stages so the cheap-checks-first order of spec 4.2 can be justified (TC-CT-003-07).

use near_sdk::{env, near, PublicKey};

#[path = "../../../x207-circle/src/canonical.rs"]
pub mod canonical;

use canonical::{parse_signature, voucher_message, voucher_signing_hash, Voucher};

#[near(serializers = [json])]
#[serde(rename_all = "snake_case")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Serialize,
    SerializeHash,
    Full,
}

#[near(contract_state)]
#[derive(Default)]
pub struct Probe {}

fn key_bytes(pubkey: &PublicKey) -> [u8; 32] {
    pubkey.as_bytes()[1..]
        .try_into()
        .unwrap_or_else(|_| env::panic_str("pubkey must be 32 bytes"))
}

fn signature_bytes(signature: &str) -> [u8; 64] {
    parse_signature(signature).unwrap_or_else(|| env::panic_str("signature must be 64 bytes"))
}

fn verify(voucher: &Voucher, signature: &[u8; 64], pubkey: &[u8; 32]) -> bool {
    env::ed25519_verify(signature, voucher_signing_hash(voucher), pubkey)
}

#[near]
impl Probe {
    pub fn probe_verify(&self, voucher: Voucher, signature: String, pubkey: PublicKey) -> bool {
        verify(&voucher, &signature_bytes(&signature), &key_bytes(&pubkey))
    }

    /// Runs `stage` `count` times. Returns the number of successful rounds, so no stage is
    /// optimized away.
    pub fn probe_measure(
        &self,
        voucher: Voucher,
        signature: String,
        pubkey: PublicKey,
        count: u32,
        stage: Stage,
    ) -> u32 {
        let signature = signature_bytes(&signature);
        let pubkey = key_bytes(&pubkey);
        let mut passed = 0;
        for _ in 0..count {
            let ok = match stage {
                Stage::Serialize => !voucher_message(&voucher).is_empty(),
                Stage::SerializeHash => voucher_signing_hash(&voucher) != [0u8; 32],
                Stage::Full => verify(&voucher, &signature, &pubkey),
            };
            passed += u32::from(ok);
        }
        passed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::json_types::U128;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    fn voucher() -> Voucher {
        Voucher {
            circle: "x207-circle.near".parse().unwrap(),
            payer: "a.near".parse().unwrap(),
            payee: "b.near".parse().unwrap(),
            epoch: 42,
            seq: 137,
            cumulative_amount: U128(2_740_000),
            unit: "credit".to_string(),
            resource: "https://example.test/api".to_string(),
        }
    }

    fn pubkey() -> PublicKey {
        format!(
            "ed25519:{}",
            near_sdk::bs58::encode([7u8; 32]).into_string()
        )
        .parse()
        .unwrap()
    }

    // TC-CT-003-05
    #[test]
    #[should_panic(expected = "signature must be 64 bytes")]
    fn short_signature_panics_before_verify() {
        testing_env!(VMContextBuilder::new().build());
        let signature = format!(
            "ed25519:{}",
            near_sdk::bs58::encode([1u8; 63]).into_string()
        );
        Probe::default().probe_verify(voucher(), signature, pubkey());
    }

    #[test]
    fn canonical_message_key_order() {
        testing_env!(VMContextBuilder::new().build());
        assert_eq!(
            voucher_message(&voucher()),
            r#"{"circle":"x207-circle.near","payer":"a.near","payee":"b.near","epoch":42,"seq":137,"cumulativeAmount":"2740000","unit":"credit","resource":"https://example.test/api"}"#
        );
    }
}
