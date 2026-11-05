//! EVM agents (CT-104, CT-105): a member whose id is a lowercase `0x` address and who signs with EIP-712.
//! Design: `vault/14 x207/04 Arsitektur/x207 - Desain Agent EVM.md`. The sidecar map `evm_signers` sits under its own
//! storage prefix, so neither `Member` nor the contract root changes layout.
//!
//! Hashing is plain EIP-712: `keccak256(0x19 0x01 || domainSeparator || hashStruct(message))`. The domain is
//! `{ name: "x207", version: "1", salt }` (no `chainId`, no `verifyingContract`); the `circle` field of every message
//! separates circles. Since the audit C55 the domain also has `salt = keccak256("near:<network>:<contract id>")`. Signature string: `eip712:0x<130 hex>` = r || s || v. The test vectors in
//! `tests/fixtures/evm-eip712.json` come from viem and win over this code.

use near_sdk::json_types::{U128, U64};
use near_sdk::store::LookupMap;
use near_sdk::{env, near, require, AccountId, NearToken, Promise, PromiseOrValue, PublicKey};

use crate::canonical::Voucher;
use crate::events::{MemberJoined, X207Event};
use crate::member::{Member, MemberStatus, MemberView, JOIN_STORAGE_DEPOSIT, MAX_MEMBERS};

use crate::state::{StorageKey, X207Circle, X207CircleExt};
use crate::types::{ClaimDestination, EpochId};

pub const EIP712_PREFIX: &str = "eip712:0x";

const DOMAIN_TYPE: &str = "EIP712Domain(string name,string version,bytes32 salt)";
const VOUCHER_TYPE: &str =
    "Voucher(string circle,string payer,string payee,uint64 epoch,uint64 seq,uint256 cumulativeAmount,string unit,string resource)";
const BUNDLE_TYPE: &str = "Bundle(string circle,string message,string termsHash)";
const ACTION_TYPE: &str =
    "Action(string circle,string member,string action,uint64 epoch,string receiver,string termsHash,uint64 nonce,uint64 deadline,uint256 maxFee)";

/// CT-117 (R1a): an `Action` may not carry a `deadline` further than this after the block time (24 hours).
pub const MAX_EVM_DEADLINE_MS: u64 = 86_400_000;

/// Placeholder in `Member.pubkey_voucher` of an EVM member. It is never used to verify anything, and it must not decode to a
/// curve point: 32 zero bytes is a small-order point that verifies forged signatures (security review M2).
pub const EVM_PLACEHOLDER_KEY: &str = "ed25519:8opHzTAnfzRpPEx21XtnrVTX28YQuCpAjcn1PczScKh";

/// Sidecar record (design decision 2).
#[near(serializers = [borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvmSigner {
    pub address: [u8; 20],
    /// The account that paid the join deposit; the deposit goes back to it when the member exits.
    pub sponsor: AccountId,
    /// Next nonce an `*_evm` action must carry. Starts at 0 after `join_evm`.
    pub nonce: u64,
}

/// View of the sidecar record.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvmSignerView {
    pub address: String,
    pub sponsor: AccountId,
    pub nonce: U64,
}

pub(crate) fn evm_signers() -> LookupMap<AccountId, EvmSigner> {
    LookupMap::new(StorageKey::EvmSigners)
}

/// `0x` + 40 lowercase hex characters, decoded. `None` for anything else (uppercase included).
pub fn parse_address(id: &str) -> Option<[u8; 20]> {
    let hex = id.strip_prefix("0x")?;
    if hex.len() != 40 || hex.bytes().any(|b| b.is_ascii_uppercase()) {
        return None;
    }
    decode_hex(hex)?.try_into().ok()
}

pub fn address_string(address: &[u8; 20]) -> String {
    format!("0x{}", encode_hex(address))
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    s.as_bytes().chunks(2).map(|p| Some(nibble(p[0])? << 4 | nibble(p[1])?)).collect()
}

/// Cheap shape test before the storage read: only `0x` + 40 characters can be an EVM member id.
fn looks_evm(id: &AccountId) -> bool {
    id.as_str().len() == 42 && id.as_str().starts_with("0x")
}

pub fn is_evm_member(id: &AccountId) -> bool {
    looks_evm(id) && evm_signers().contains_key(id)
}

fn keccak(bytes: &[u8]) -> [u8; 32] {
    env::keccak256_array(bytes)
}

fn word_str(s: &str) -> [u8; 32] {
    keccak(s.as_bytes())
}

fn word_u64(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

fn word_u128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

/// `salt = keccak256("near:" + network + ":" + contract account id)`. The network comes from the account suffix:
/// `*.testnet` is `testnet`, anything else is `mainnet`. So a signature made for one contract, or for the same
/// account name on the other network, never verifies on another (audit C55).
pub fn domain_salt() -> [u8; 32] {
    let contract = env::current_account_id();
    let network = if contract.as_str().ends_with(".testnet") { "testnet" } else { "mainnet" };
    keccak(format!("near:{network}:{contract}").as_bytes())
}

fn domain_separator() -> [u8; 32] {
    keccak(&[keccak(DOMAIN_TYPE.as_bytes()), word_str("x207"), word_str("1"), domain_salt()].concat())
}

fn digest(struct_hash: [u8; 32]) -> [u8; 32] {
    keccak(&[&[0x19u8, 0x01][..], &domain_separator(), &struct_hash].concat())
}

pub fn voucher_digest(v: &Voucher) -> [u8; 32] {
    digest(keccak(
        &[
            keccak(VOUCHER_TYPE.as_bytes()),
            word_str(v.circle.as_str()),
            word_str(v.payer.as_str()),
            word_str(v.payee.as_str()),
            word_u64(v.epoch),
            word_u64(v.seq),
            word_u128(v.cumulative_amount.0),
            word_str(&v.unit),
            word_str(&v.resource),
        ]
        .concat(),
    ))
}

/// `message` is the canonical terms JSON and `terms_hash_hex` its sha256 in lowercase hex (barter-format.md).
pub fn bundle_digest(circle: &str, message: &str, terms_hash_hex: &str) -> [u8; 32] {
    digest(keccak(&[keccak(BUNDLE_TYPE.as_bytes()), word_str(circle), word_str(message), word_str(terms_hash_hex)].concat()))
}

pub struct ActionMsg<'a> {
    pub circle: &'a str,
    pub member: &'a str,
    pub action: &'a str,
    pub epoch: u64,
    pub receiver: &'a str,
    pub terms_hash: &'a str,
    pub nonce: u64,
    pub deadline: u64,
    /// The most the member lets the relayer take (CT-117). 0 for every action but `claim` and `withdraw_stake`.
    pub max_fee: u128,
}

pub fn action_digest(a: &ActionMsg) -> [u8; 32] {
    digest(keccak(
        &[
            keccak(ACTION_TYPE.as_bytes()),
            word_str(a.circle),
            word_str(a.member),
            word_str(a.action),
            word_u64(a.epoch),
            word_str(a.receiver),
            word_str(a.terms_hash),
            word_u64(a.nonce),
            word_u64(a.deadline),
            word_u128(a.max_fee),
        ]
        .concat(),
    ))
}

/// Address that signed `digest`, or `None` for a malformed, high-s (malleable) or unrecoverable signature.
/// `v` may be 27/28 (viem) or 0/1.
pub fn recover(digest: &[u8; 32], signature: &str) -> Option<[u8; 20]> {
    let hex = signature.strip_prefix(EIP712_PREFIX)?;
    if hex.len() != 130 {
        return None;
    }
    let bytes = decode_hex(hex)?;
    let v = match bytes[64] {
        27 | 28 => bytes[64] - 27,
        0 | 1 => bytes[64],
        _ => return None,
    };
    let public_key = env::ecrecover(digest, &bytes[..64], v, true)?;
    keccak(&public_key)[12..].try_into().ok()
}

/// Voucher check 12 for an EVM payer: the recovered address must be the payer id.
pub fn verify_voucher(voucher: &Voucher, signature: &str) -> bool {
    recover(&voucher_digest(voucher), signature).is_some_and(|a| address_string(&a) == voucher.payer.as_str())
}

/// Bundle signature of an EVM participant.
pub fn verify_bundle(signer: &AccountId, circle: &str, message: &str, terms_hash_hex: &str, signature: &str) -> bool {
    recover(&bundle_digest(circle, message, terms_hash_hex), signature).is_some_and(|a| address_string(&a) == signer.as_str())
}

/// Transfers the join deposit back to the sponsor of an EVM member that has exited. Only what the contract holds above
/// its storage stake leaves, so a refund can never make the contract unable to pay for its own storage.
pub(crate) fn refund_sponsor(account: &AccountId) {
    if !looks_evm(account) {
        return;
    }
    let Some(signer) = evm_signers().get(account).cloned() else { return };
    let storage = NearToken::from_yoctonear(env::storage_byte_cost().as_yoctonear() * u128::from(env::storage_usage()));
    let spare = env::account_balance().saturating_sub(storage);
    let amount = spare.min(JOIN_STORAGE_DEPOSIT);
    if !amount.is_zero() {
        Promise::new(signer.sponsor).transfer(amount).detach();
    }
}

/// `ERR_EVM_EXPIRED` when the deadline has passed, `ERR_EVM_DEADLINE_TOO_FAR` when it is more than `MAX_EVM_DEADLINE_MS` ahead (CT-117, R1a).
fn check_deadline(deadline: u64) {
    let now_ms = env::block_timestamp_ms();
    require!(now_ms <= deadline, format!("ERR_EVM_EXPIRED: deadline {deadline} now {now_ms}"));
    require!(
        deadline - now_ms <= MAX_EVM_DEADLINE_MS,
        format!("ERR_EVM_DEADLINE_TOO_FAR: deadline {deadline} now {now_ms} limit {MAX_EVM_DEADLINE_MS} ms")
    );
}

/// The fee that the relayer takes from `base` (a payout or a stake): the DAO `relayer_fee`, at most what the member signed (`max_fee`),
/// at most half of `base` so the member always receives something and the payout leg is never 0 (CT-117, D4).
pub(crate) fn relayer_fee_for(base: u128, max_fee: u128) -> u128 {
    crate::params::current_relayer_fee().min(max_fee).min(base / 2)
}

/// Checks the signer, deadline, nonce and signature of one `*_evm` action, then bumps the nonce. Order of checks:
/// `ERR_NOT_EVM_MEMBER`, `ERR_EVM_EXPIRED`, `ERR_EVM_DEADLINE_TOO_FAR`, `ERR_EVM_NONCE`, `ERR_BAD_EVM_SIG`. A later panic of the same
/// transaction reverts the bump.
#[allow(clippy::too_many_arguments)]
pub(crate) fn authorize(
    account: &AccountId,
    action: &str,
    epoch: u64,
    receiver: &str,
    nonce: u64,
    deadline: u64,
    max_fee: u128,
    signature: &str,
) {
    let mut signers = evm_signers();
    let Some(mut signer) = looks_evm(account).then(|| signers.get(account).cloned()).flatten() else {
        env::panic_str(&format!("ERR_NOT_EVM_MEMBER: {account}"))
    };
    check_deadline(deadline);
    require!(nonce == signer.nonce, format!("ERR_EVM_NONCE: nonce {nonce} expected {}", signer.nonce));
    let circle = env::current_account_id();
    let message = ActionMsg { circle: circle.as_str(), member: account.as_str(), action, epoch, receiver, terms_hash: "", nonce, deadline, max_fee };
    let ok = recover(&action_digest(&message), signature) == Some(signer.address);
    require!(ok, "ERR_BAD_EVM_SIG: eip712 verification failed");
    signer.nonce += 1;
    signers.insert(account.clone(), signer);
    signers.flush();
}

#[near]
impl X207Circle {
    /// Registers the EVM address `address` (lowercase `0x`, 42 characters) as a `pending` member. Anyone may call it;
    /// the signature of the address over `Action{action:"join"}` is the gate and the caller becomes the `sponsor`
    /// that pays the deposit and gets it back on exit. Checks: `ERR_INSUFFICIENT_STORAGE_DEPOSIT`,
    /// `ERR_BAD_EVM_ADDRESS`, `ERR_BAD_TERMS_HASH`, `ERR_ALREADY_MEMBER`, `ERR_MAX_MEMBERS`, `ERR_EVM_EXPIRED`,
    /// `ERR_BAD_EVM_SIG`. The first action after a join carries nonce 0.
    #[payable]
    pub fn join_evm(&mut self, address: String, terms_hash: String, deadline: u64, signature: String) -> MemberView {
        let deposit = env::attached_deposit();
        require!(
            deposit >= JOIN_STORAGE_DEPOSIT,
            format!("ERR_INSUFFICIENT_STORAGE_DEPOSIT: attached {deposit}, need {JOIN_STORAGE_DEPOSIT}")
        );
        let Some(raw) = parse_address(&address) else {
            env::panic_str("ERR_BAD_EVM_ADDRESS: address must be 0x and 40 lowercase hex characters")
        };
        if terms_hash.len() != 64 || !terms_hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            env::panic_str("ERR_BAD_TERMS_HASH: terms_hash must be 64 lowercase hex characters");
        }
        let account_id: AccountId = address.parse().unwrap_or_else(|_| env::panic_str("ERR_BAD_EVM_ADDRESS: not an account id"));
        if self.members.contains_key(&account_id) {
            env::panic_str(&format!("ERR_ALREADY_MEMBER: {account_id}"));
        }
        if self.member_count >= MAX_MEMBERS {
            env::panic_str(&format!("ERR_MAX_MEMBERS: {MAX_MEMBERS}"));
        }
        check_deadline(deadline);
        let circle = env::current_account_id();
        let message = ActionMsg {
            circle: circle.as_str(),
            member: &address,
            action: "join",
            epoch: 0,
            receiver: "",
            terms_hash: &terms_hash,
            nonce: 0,
            deadline,
            max_fee: 0,
        };
        require!(recover(&action_digest(&message), &signature) == Some(raw), "ERR_BAD_EVM_SIG: eip712 verification failed");

        let sponsor = env::predecessor_account_id();
        let joined_at = env::block_timestamp();
        let placeholder: PublicKey = EVM_PLACEHOLDER_KEY.parse().expect("placeholder key");
        let member = Member {
            account_id: account_id.clone(),
            pubkey_voucher: placeholder.clone(),
            terms_hash: terms_hash.clone(),
            stake: 0,
            limit: 0,
            status: MemberStatus::Pending,
            admitted: false,
            joined_at,
            settled_history: 0,
            last_active_epoch: self.current_epoch,
            exit_requested_epoch: None,
            withdraw_pending: false,
            reactivate_epoch: None,
        };
        let view = member.to_view();
        self.members.insert(account_id.clone(), member);
        self.member_index.push(account_id.clone());
        self.member_count += 1;
        let mut signers = evm_signers();
        signers.insert(account_id.clone(), EvmSigner { address: raw, sponsor: sponsor.clone(), nonce: 0 });
        signers.flush();
        MemberJoined {
            account_id,
            pubkey_voucher: String::from(&placeholder),
            terms_hash,
            status: "pending".to_string(),
            joined_at: U64(joined_at),
        }
        .emit();
        let excess = deposit.saturating_sub(JOIN_STORAGE_DEPOSIT);
        if !excess.is_zero() {
            Promise::new(sponsor).transfer(excess).detach();
        }
        view
    }

    /// The sidecar record of an EVM member, `null` for any other account.
    pub fn evm_signer(&self, account_id: AccountId) -> Option<EvmSignerView> {
        if !looks_evm(&account_id) {
            return None;
        }
        evm_signers().get(&account_id).map(|s| EvmSignerView { address: address_string(&s.address), sponsor: s.sponsor.clone(), nonce: U64(s.nonce) })
    }

    /// `claim` for an EVM member (CT-105). Anyone may relay it. `member` is the `0x` id that holds the position; the
    /// payout goes to `receiver`, which the member signed. When `receiver` is the Intents contract the payout is a
    /// `ft_transfer_call` that credits the `0x` id at Intents (destination `intents`); otherwise it is a plain
    /// `ft_transfer` to `receiver`.
    ///
    /// CT-117: the relayer (the caller) may take `min(relayer_fee, max_fee, payout / 2)` of the payout; `max_fee` is signed. The fee is a
    /// separate transfer: if it fails, the member keeps that part as a claimable balance and the payout is not affected.
    #[allow(clippy::too_many_arguments)]
    pub fn claim_evm(
        &mut self,
        member: AccountId,
        epoch: EpochId,
        receiver: AccountId,
        nonce: u64,
        deadline: u64,
        max_fee: U128,
        signature: String,
    ) -> Promise {
        authorize(&member, "claim", epoch, receiver.as_str(), nonce, deadline, max_fee.0, &signature);
        let relayer = Some((env::predecessor_account_id(), max_fee.0));
        if receiver == self.intents_id {
            self.claim_for(member, epoch, ClaimDestination::Intents, None, relayer)
        } else {
            self.claim_for(member, epoch, ClaimDestination::Near, Some(receiver), relayer)
        }
    }

    /// `request_exit` for an EVM member (CT-105).
    pub fn request_exit_evm(&mut self, member: AccountId, nonce: u64, deadline: u64, signature: String) {
        authorize(&member, "request_exit", 0, "", nonce, deadline, 0, &signature);
        self.request_exit_for(member);
    }

    /// `withdraw_stake` for an EVM member (CT-105): the stake goes to `receiver` with a plain `ft_transfer`. The Intents
    /// contract is refused as receiver (`ERR_BAD_EVM_RECEIVER`): a plain transfer there would credit nobody. The join
    /// deposit goes back to the sponsor once the exit has completed.
    ///
    /// CT-117: the relayer may take `min(relayer_fee, max_fee, stake / 2)` of the stake, as a separate transfer (see `claim_evm`).
    pub fn withdraw_stake_evm(
        &mut self,
        member: AccountId,
        receiver: AccountId,
        nonce: u64,
        deadline: u64,
        max_fee: U128,
        signature: String,
    ) -> PromiseOrValue<bool> {
        require!(receiver != self.intents_id, "ERR_BAD_EVM_RECEIVER: the Intents contract cannot receive a stake withdrawal");
        authorize(&member, "withdraw_stake", 0, receiver.as_str(), nonce, deadline, max_fee.0, &signature);
        self.withdraw_stake_for(member, receiver, Some((env::predecessor_account_id(), max_fee.0)))
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::serde_json::Value;
    use near_sdk::test_utils::{get_created_receipts, VMContextBuilder};
    use near_sdk::{testing_env, NearToken};

    use super::*;
    use crate::clearing::EpochPhase;
    use crate::params::Params;

    const NOW_NS: u64 = 1_899_996_400_000_000_000; // one hour before every deadline of the fixture (1.9e12 ms)

    fn fixture() -> Value {
        near_sdk::serde_json::from_str(include_str!("../../tests/fixtures/evm-eip712.json")).unwrap()
    }

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn ctx(caller: &str, deposit: NearToken, now_ns: u64) {
        testing_env!(VMContextBuilder::new()
            .current_account_id(acc("x207-v014.testnet"))
            .predecessor_account_id(acc(caller))
            .attached_deposit(deposit)
            .block_timestamp(now_ns)
            .build());
    }

    fn circle() -> X207Circle {
        ctx("relayer.testnet", NearToken::from_yoctonear(0), NOW_NS);
        X207Circle::with_config(acc("dao.near"), "agents-main".into(), acc("usdt.near"), acc("intents.near"), Params::default(), NOW_NS)
    }

    fn v(name: &str) -> Value {
        fixture()["vectors"][name].clone()
    }

    fn sig(name: &str) -> String {
        format!("eip712:{}", v(name)["signature"].as_str().unwrap())
    }

    fn address() -> String {
        fixture()["address"].as_str().unwrap().to_string()
    }

    fn hex32(s: &str) -> [u8; 32] {
        decode_hex(s.trim_start_matches("0x")).unwrap().try_into().unwrap()
    }

    fn joined(c: &mut X207Circle) {
        ctx("relayer.testnet", JOIN_STORAGE_DEPOSIT, NOW_NS);
        c.join_evm(address(), "a".repeat(64), 1_900_000_000_000, sig("join"));
    }

    /// Active member with an exit request already made at epoch 1, stake 0, and the signer nonce set.
    fn active(c: &mut X207Circle, nonce: u64) {
        joined(c);
        let id = acc(&address());
        c.members.get_mut(&id).unwrap().status = MemberStatus::Active;
        let mut signers = evm_signers();
        let mut s = signers.get(&id).cloned().unwrap();
        s.nonce = nonce;
        signers.insert(id, s);
        signers.flush();
    }

    fn panic_text(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("must panic");
        err.downcast_ref::<String>().cloned().or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    }

    // TC-CT-104-01: hashes and signatures of viem
    #[test]
    fn tc_ct_104_01_digests_match_viem() {
        let _c = circle();
        let f = fixture();
        assert_eq!(domain_separator(), hex32(f["domainSeparator"].as_str().unwrap()));
        let m = &v("voucher")["message"];
        let voucher = Voucher {
            circle: acc(m["circle"].as_str().unwrap()),
            payer: acc(m["payer"].as_str().unwrap()),
            payee: acc(m["payee"].as_str().unwrap()),
            epoch: 4,
            seq: 2,
            cumulative_amount: near_sdk::json_types::U128(m["cumulativeAmount"].as_str().unwrap().parse().unwrap()),
            unit: "credit".into(),
            resource: m["resource"].as_str().unwrap().into(),
        };
        assert_eq!(voucher_digest(&voucher), hex32(v("voucher")["digest"].as_str().unwrap()));
        assert!(verify_voucher(&voucher, &sig("voucher")));
        let b = v("bundle");
        assert_eq!(bundle_digest("x207-v014.testnet", b["termsMessage"].as_str().unwrap(), b["termsHash"].as_str().unwrap()), hex32(b["digest"].as_str().unwrap()));
        assert!(verify_bundle(&acc(&address()), "x207-v014.testnet", b["termsMessage"].as_str().unwrap(), b["termsHash"].as_str().unwrap(), &sig("bundle")));
    }

    #[test]
    fn tc_ct_104_02_action_digests_match_viem() {
        let _c = circle();
        let a = address();
        let mk = |action: &'static str, epoch, receiver: &'static str, terms: &'static str, nonce, max_fee| {
            action_digest(&ActionMsg { circle: "x207-v014.testnet", member: Box::leak(a.clone().into_boxed_str()), action, epoch, receiver, terms_hash: terms, nonce, deadline: 1_900_000_000_000, max_fee })
        };
        let join_terms: &'static str = Box::leak("a".repeat(64).into_boxed_str());
        assert_eq!(mk("join", 0, "", join_terms, 0, 0), hex32(v("join")["digest"].as_str().unwrap()));
        assert_eq!(mk("claim", 5, "agent.testnet", "", 0, 250_000), hex32(v("claim")["digest"].as_str().unwrap()));
        assert_eq!(mk("request_exit", 0, "", "", 1, 0), hex32(v("request_exit")["digest"].as_str().unwrap()));
        assert_eq!(mk("withdraw_stake", 0, "agent.testnet", "", 2, 100_000), hex32(v("withdraw_stake")["digest"].as_str().unwrap()));
        // the signed fee ceiling is part of the digest: another maxFee is another message
        assert_ne!(mk("claim", 5, "agent.testnet", "", 0, 250_001), hex32(v("claim")["digest"].as_str().unwrap()));
    }

    // a changed field, a high-s twin, a wrong prefix and a bad v are all refused
    #[test]
    fn tc_ct_104_03_malformed_and_malleable_signatures() {
        let _c = circle();
        let d = hex32(v("voucher")["digest"].as_str().unwrap());
        let good = sig("voucher");
        assert_eq!(recover(&d, &good).map(|a| address_string(&a)), Some(address()));
        let mut other = d;
        other[0] ^= 1;
        assert_ne!(recover(&other, &good).map(|a| address_string(&a)), Some(address()));
        let high = format!("eip712:{}", v("voucher")["highSSignature"].as_str().unwrap());
        assert_eq!(recover(&d, &high), None);
        assert_eq!(recover(&d, &good.replace("eip712:", "ed25519:")), None);
        assert_eq!(recover(&d, &good[..good.len() - 2]), None);
        let bad_v = format!("{}ff", &good[..good.len() - 2]);
        assert_eq!(recover(&d, &bad_v), None);
        // v as 0/1 also works
        let v01 = format!("{}{:02x}", &good[..good.len() - 2], u8::from_str_radix(&good[good.len() - 2..], 16).unwrap() - 27);
        assert_eq!(recover(&d, &v01).map(|a| address_string(&a)), Some(address()));
    }

    #[test]
    fn tc_ct_104_04_address_must_be_lowercase_0x_40() {
        assert!(parse_address(&address()).is_some());
        assert!(parse_address(&address().to_uppercase().replace("0X", "0x")).is_none());
        assert!(parse_address("0x70997970C51812dc3A010C7d01b50e0d17dc79C8").is_none());
        assert!(parse_address(&address()[2..]).is_none());
        assert!(parse_address(&format!("{}00", address())).is_none());
        assert!(parse_address("0xzz997970c51812dc3a010c7d01b50e0d17dc79c8").is_none());
    }

    // TC-CT-104-05: join_evm
    #[test]
    fn tc_ct_104_05_join_evm_creates_member_and_sidecar() {
        let mut c = circle();
        ctx("relayer.testnet", NearToken::from_near(1), NOW_NS);
        let view = c.join_evm(address(), "a".repeat(64), 1_900_000_000_000, sig("join"));
        assert_eq!(view.account_id.as_str(), address());
        assert_eq!(view.status, MemberStatus::Pending);
        let signer = c.evm_signer(acc(&address())).unwrap();
        assert_eq!((signer.address.as_str(), signer.sponsor.as_str(), signer.nonce.0), (address().as_str(), "relayer.testnet", 0));
        assert!(is_evm_member(&acc(&address())));
        assert!(!is_evm_member(&acc("gpu.testnet")));
        assert_eq!(c.member_count, 1);
        // the excess over the join deposit goes back to the sponsor
        let receipts = get_created_receipts();
        assert!(receipts.iter().any(|r| r.receiver_id.as_str() == "relayer.testnet" && format!("{:?}", r.actions).contains(&(NearToken::from_near(1).as_yoctonear() - JOIN_STORAGE_DEPOSIT.as_yoctonear()).to_string())));
    }

    #[test]
    fn tc_ct_104_06_join_evm_refusals() {
        let mut c = circle();
        let t = "a".repeat(64);
        ctx("relayer.testnet", JOIN_STORAGE_DEPOSIT, NOW_NS);
        // wrong signer: the signature of another action
        let text = panic_text(std::panic::AssertUnwindSafe(|| { c.join_evm(address(), t.clone(), 1_900_000_000_000, sig("claim")); }));
        assert!(text.contains("ERR_BAD_EVM_SIG"), "{text}");
        // changed terms_hash
        let text = panic_text(std::panic::AssertUnwindSafe(|| { c.join_evm(address(), "b".repeat(64), 1_900_000_000_000, sig("join")); }));
        assert!(text.contains("ERR_BAD_EVM_SIG"), "{text}");
        // changed deadline
        let text = panic_text(std::panic::AssertUnwindSafe(|| { c.join_evm(address(), t.clone(), 1_900_000_000_001, sig("join")); }));
        assert!(text.contains("ERR_BAD_EVM_SIG"), "{text}");
        // uppercase letters in the address
        let text = panic_text(std::panic::AssertUnwindSafe(|| { c.join_evm("0x70997970C51812dc3A010C7d01b50e0d17dc79C8".into(), t.clone(), 1_900_000_000_000, sig("join")); }));
        assert!(text.contains("ERR_BAD_EVM_ADDRESS"), "{text}");
        // short deposit
        ctx("relayer.testnet", NearToken::from_millinear(349), NOW_NS);
        let text = panic_text(std::panic::AssertUnwindSafe(|| { c.join_evm(address(), t.clone(), 1_900_000_000_000, sig("join")); }));
        assert!(text.contains("ERR_INSUFFICIENT_STORAGE_DEPOSIT"), "{text}");
        // past the deadline
        ctx("relayer.testnet", JOIN_STORAGE_DEPOSIT, 1_900_000_001_000_000_000);
        let text = panic_text(std::panic::AssertUnwindSafe(|| { c.join_evm(address(), t.clone(), 1_900_000_000_000, sig("join")); }));
        assert!(text.contains("ERR_EVM_EXPIRED"), "{text}");
        // replay of a join
        ctx("relayer.testnet", JOIN_STORAGE_DEPOSIT, NOW_NS);
        c.join_evm(address(), t.clone(), 1_900_000_000_000, sig("join"));
        let text = panic_text(std::panic::AssertUnwindSafe(|| { c.join_evm(address(), t.clone(), 1_900_000_000_000, sig("join")); }));
        assert!(text.contains("ERR_ALREADY_MEMBER"), "{text}");
    }

    // TC-CT-105-01: nonce, replay, expiry, receiver
    #[test]
    fn tc_ct_105_01_request_exit_evm_replay_and_nonce() {
        let mut c = circle();
        active(&mut c, 1);
        let id = acc(&address());
        ctx("anyone.testnet", NearToken::from_yoctonear(0), NOW_NS);
        // a nonce that is not the next one
        let text = panic_text(std::panic::AssertUnwindSafe(|| c.request_exit_evm(id.clone(), 2, 1_900_000_000_000, sig("request_exit"))));
        assert!(text.contains("ERR_EVM_NONCE"), "{text}");
        c.request_exit_evm(id.clone(), 1, 1_900_000_000_000, sig("request_exit"));
        assert_eq!(c.members.get(&id).unwrap().exit_requested_epoch, Some(1));
        assert_eq!(c.evm_signer(id.clone()).unwrap().nonce.0, 2);
        // replay of the same signature
        let text = panic_text(std::panic::AssertUnwindSafe(|| c.request_exit_evm(id.clone(), 1, 1_900_000_000_000, sig("request_exit"))));
        assert!(text.contains("ERR_EVM_NONCE"), "{text}");
    }

    #[test]
    fn tc_ct_105_02_expired_and_not_evm_member() {
        let mut c = circle();
        active(&mut c, 1);
        let id = acc(&address());
        ctx("anyone.testnet", NearToken::from_yoctonear(0), 1_900_000_001_000_000_000);
        let text = panic_text(std::panic::AssertUnwindSafe(|| c.request_exit_evm(id.clone(), 1, 1_900_000_000_000, sig("request_exit"))));
        assert!(text.contains("ERR_EVM_EXPIRED"), "{text}");
        // a NEAR-key member cannot be driven by an EVM signature
        ctx("anyone.testnet", NearToken::from_yoctonear(0), NOW_NS);
        let text = panic_text(std::panic::AssertUnwindSafe(|| c.request_exit_evm(acc("gpu.testnet"), 1, 1_900_000_000_000, sig("request_exit"))));
        assert!(text.contains("ERR_NOT_EVM_MEMBER"), "{text}");
        // and a 0x id that never joined
        let text = panic_text(std::panic::AssertUnwindSafe(|| c.request_exit_evm(acc("0x0000000000000000000000000000000000000001"), 1, 1_900_000_000_000, sig("request_exit"))));
        assert!(text.contains("ERR_NOT_EVM_MEMBER"), "{text}");
    }

    #[test]
    fn tc_ct_105_03_wrong_receiver_or_member_or_action_is_refused() {
        let mut c = circle();
        active(&mut c, 0);
        let id = acc(&address());
        ctx("anyone.testnet", NearToken::from_yoctonear(0), NOW_NS);
        // claim signed for agent.testnet cannot pay thief.testnet
        let text = panic_text(std::panic::AssertUnwindSafe(|| { let _ = c.claim_evm(id.clone(), 5, acc("thief.testnet"), 0, 1_900_000_000_000, U128(250_000), sig("claim")); }));
        assert!(text.contains("ERR_BAD_EVM_SIG"), "{text}");
        // another epoch
        let text = panic_text(std::panic::AssertUnwindSafe(|| { let _ = c.claim_evm(id.clone(), 6, acc("agent.testnet"), 0, 1_900_000_000_000, U128(250_000), sig("claim")); }));
        assert!(text.contains("ERR_BAD_EVM_SIG"), "{text}");
        // the claim signature is not a withdraw signature
        let text = panic_text(std::panic::AssertUnwindSafe(|| { let _ = c.withdraw_stake_evm(id.clone(), acc("agent.testnet"), 0, 1_900_000_000_000, U128(100_000), sig("claim")); }));
        assert!(text.contains("ERR_BAD_EVM_SIG"), "{text}");
        // nothing moved the nonce
        assert_eq!(c.evm_signer(id.clone()).unwrap().nonce.0, 0);
        // the Intents contract is no receiver of a stake withdrawal
        let text = panic_text(std::panic::AssertUnwindSafe(|| { let _ = c.withdraw_stake_evm(id.clone(), acc("intents.near"), 0, 1_900_000_000_000, U128(100_000), sig("claim")); }));
        assert!(text.contains("ERR_BAD_EVM_RECEIVER"), "{text}");
    }

    // a claim with the right signature gets past the signature and meets the next rule (no finalized epoch 5 here)
    #[test]
    fn tc_ct_105_04_claim_evm_signature_ok_reaches_claim_rules() {
        let mut c = circle();
        active(&mut c, 0);
        ctx("anyone.testnet", NearToken::from_yoctonear(0), NOW_NS);
        let id = acc(&address());
        let text = panic_text(std::panic::AssertUnwindSafe(|| { let _ = c.claim_evm(id.clone(), 5, acc("agent.testnet"), 0, 1_900_000_000_000, U128(250_000), sig("claim")); }));
        assert!(!text.contains("ERR_BAD_EVM_SIG") && !text.contains("ERR_EVM_NONCE"), "{text}");
    }

    // withdraw with stake 0: exits at once, the nonce moves, the deposit goes to the sponsor
    #[test]
    fn tc_ct_105_05_withdraw_stake_evm_refunds_sponsor() {
        let mut c = circle();
        active(&mut c, 2);
        let id = acc(&address());
        c.members.get_mut(&id).unwrap().exit_requested_epoch = Some(1);
        c.epochs.get_mut(&1).unwrap().phase = EpochPhase::Finalized;
        ctx("anyone.testnet", NearToken::from_yoctonear(0), NOW_NS);
        let out = c.withdraw_stake_evm(id.clone(), acc("agent.testnet"), 2, 1_900_000_000_000, U128(100_000), sig("withdraw_stake"));
        assert!(matches!(out, PromiseOrValue::Value(true)));
        assert_eq!(c.members.get(&id).unwrap().status, MemberStatus::Exited);
        assert_eq!(c.evm_signer(id).unwrap().nonce.0, 3);
        let receipts = get_created_receipts();
        assert!(receipts.iter().any(|r| r.receiver_id.as_str() == "relayer.testnet"
            && format!("{:?}", r.actions).contains(&JOIN_STORAGE_DEPOSIT.as_yoctonear().to_string())));
    }

    // audit C55: the domain salt binds a signature to one contract and one network
    #[test]
    fn tc_ct_104_09_signature_for_another_contract_or_network_is_refused() {
        let f = fixture();
        let _c = circle();
        assert_eq!(domain_salt(), hex32(f["salt"].as_str().unwrap()));
        let d = hex32(v("voucher")["digest"].as_str().unwrap());
        let own = sig("voucher");
        assert_eq!(recover(&d, &own).map(|a| address_string(&a)), Some(address()));
        for (contract, expect_network_word) in [("other-circle.testnet", "testnet"), ("x207-v014.near", "mainnet")] {
            let cross = &f["crossContract"][contract];
            let cross_sig = format!("eip712:{}", cross["signature"].as_str().unwrap());
            // the signature made for `contract` is refused here ...
            let voucher = |c: &Value| Voucher {
                circle: acc("x207-v014.testnet"),
                payer: acc(&address()),
                payee: acc("gpu.testnet"),
                epoch: 4,
                seq: 2,
                cumulative_amount: near_sdk::json_types::U128(c.as_str().unwrap().parse().unwrap()),
                unit: "credit".into(),
                resource: "/api/translate?lang=id".into(),
            };
            let amount = &v("voucher")["message"]["cumulativeAmount"];
            assert!(!verify_voucher(&voucher(amount), &cross_sig), "{contract}");
            // ... and ours is refused there
            testing_env!(VMContextBuilder::new().current_account_id(acc(contract)).block_timestamp(NOW_NS).build());
            assert_eq!(hex32(cross["salt"].as_str().unwrap()), domain_salt(), "{expect_network_word}");
            assert!(!verify_voucher(&voucher(amount), &own), "{contract}");
        }
        // the testnet and mainnet names of one contract have different salts
        let testnet_salt = hex32(f["salt"].as_str().unwrap());
        assert_ne!(testnet_salt, hex32(f["crossContract"]["x207-v014.near"]["salt"].as_str().unwrap()));
    }

    // security review M2: the placeholder key is not a valid ed25519 point, so it verifies nothing
    #[test]
    fn tc_ct_104_08_placeholder_key_is_not_a_curve_point() {
        let key: PublicKey = EVM_PLACEHOLDER_KEY.parse().unwrap();
        let bytes: [u8; 32] = key.as_bytes()[1..].try_into().unwrap();
        assert!(ed25519_dalek::VerifyingKey::from_bytes(&bytes).is_err());
    }

    // TC-CT-104-07: a voucher of an EVM payer goes through register_cumulative; the ed25519 path is closed for it
    #[test]
    fn tc_ct_104_07_register_cumulative_with_evm_payer() {
        let mut c = circle();
        active(&mut c, 0);
        let payer = acc(&address());
        {
            let m = c.members.get_mut(&payer).unwrap();
            m.limit = 100_000_000_000_000_000_000_000_000;
            m.stake = 1;
        }
        c.members.insert(
            acc("gpu.testnet"),
            Member {
                account_id: acc("gpu.testnet"),
                pubkey_voucher: "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse().unwrap(),
                terms_hash: "a".repeat(64),
                stake: 1,
                limit: 1,
                status: MemberStatus::Active,
                admitted: false,
                joined_at: 1,
                settled_history: 0,
                last_active_epoch: 1,
                exit_requested_epoch: None,
                withdraw_pending: false,
                reactivate_epoch: None,
            },
        );
        c.current_epoch = 4;
        c.epochs.insert(4, crate::clearing::Epoch::open(4, NOW_NS, NOW_NS + 600_000_000_000, Params::default()));
        ctx("relayer.testnet", NearToken::from_yoctonear(0), NOW_NS);
        let m = &v("voucher")["message"];
        let voucher = Voucher {
            circle: acc("x207-v014.testnet"),
            payer: payer.clone(),
            payee: acc("gpu.testnet"),
            epoch: 4,
            seq: 2,
            cumulative_amount: near_sdk::json_types::U128(m["cumulativeAmount"].as_str().unwrap().parse().unwrap()),
            unit: "credit".into(),
            resource: m["resource"].as_str().unwrap().into(),
        };
        let text = panic_text(std::panic::AssertUnwindSafe(|| { c.register_cumulative(voucher.clone(), "ed25519:3tDmsQ7pDmCnn3P9yTDPZ8uS6rqGKWdnh5FZK6Wcb1YZ9k7eQy1VhYYtJWvn5Wv7cqJL8Zyb6WAkJgNiXEcxRSkR".into()); }));
        assert!(text.contains("ERR_BAD_EVM_SIG"), "{text}");
        let mut tampered = voucher.clone();
        tampered.seq = 3;
        let text = panic_text(std::panic::AssertUnwindSafe(|| { c.register_cumulative(tampered, sig("voucher")); }));
        assert!(text.contains("ERR_BAD_EVM_SIG"), "{text}");
        let obligation = c.register_cumulative(voucher, sig("voucher"));
        assert_eq!(obligation.0.to_string(), m["cumulativeAmount"].as_str().unwrap());
    }

    // TC-CT-108-05: set_auto_settle_evm is a signed Action; the choice is part of the signed action name
    #[test]
    fn tc_ct_108_05_set_auto_settle_evm() {
        let mut c = circle();
        active(&mut c, 3);
        let id = acc(&address());
        ctx("anyone.testnet", NearToken::from_yoctonear(0), NOW_NS);
        // the "off" signature cannot switch it on, and the other way round
        let text = panic_text(std::panic::AssertUnwindSafe(|| c.set_auto_settle_evm(id.clone(), true, 3, 1_900_000_000_000, sig("auto_settle_off"))));
        assert!(text.contains("ERR_BAD_EVM_SIG"), "{text}");
        let text = panic_text(std::panic::AssertUnwindSafe(|| c.set_auto_settle_evm(id.clone(), false, 3, 1_900_000_000_000, sig("auto_settle"))));
        assert!(text.contains("ERR_BAD_EVM_SIG"), "{text}");
        c.set_auto_settle_evm(id.clone(), true, 3, 1_900_000_000_000, sig("auto_settle"));
        assert!(c.is_auto_settle(id.clone()));
        assert_eq!(c.evm_signer(id.clone()).unwrap().nonce.0, 4);
        // replay
        let text = panic_text(std::panic::AssertUnwindSafe(|| c.set_auto_settle_evm(id.clone(), true, 3, 1_900_000_000_000, sig("auto_settle"))));
        assert!(text.contains("ERR_EVM_NONCE"), "{text}");
        c.set_auto_settle_evm(id.clone(), false, 4, 1_900_000_000_000, sig("auto_settle_off"));
        assert!(!c.is_auto_settle(id.clone()));
        // expired
        ctx("anyone.testnet", NearToken::from_yoctonear(0), 1_900_000_001_000_000_000);
        let text = panic_text(std::panic::AssertUnwindSafe(|| c.set_auto_settle_evm(id.clone(), true, 5, 1_900_000_000_000, sig("auto_settle"))));
        assert!(text.contains("ERR_EVM_NONCE") || text.contains("ERR_EVM_EXPIRED"), "{text}");
        // a NEAR-key member is not driven by an EVM signature
        ctx("anyone.testnet", NearToken::from_yoctonear(0), NOW_NS);
        let text = panic_text(std::panic::AssertUnwindSafe(|| c.set_auto_settle_evm(acc("gpu.testnet"), true, 3, 1_900_000_000_000, sig("auto_settle"))));
        assert!(text.contains("ERR_NOT_EVM_MEMBER"), "{text}");
    }
}
