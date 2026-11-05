# Security review of the EVM agent feature (CT-104, CT-105, interface 0.0.14)

Scope: the uncommitted working tree of `contracts` on 2026-11-05: new `x207-circle/src/evm.rs`, and the changes to `voucher.rs` (check 12),
`bundle.rs` (`commit_bundle` signatures), `stake.rs` (`request_exit_for`, `withdraw_stake_for`, sponsor refund in `on_withdraw_resolved`),
`settlement.rs` (`claim_for`), `state.rs` (`StorageKey::EvmSigners`) and `Cargo.toml` (near-sdk feature `unstable` for `env::ecrecover`).
Design source: `vault/14 x207/04 Arsitektur/x207 - Desain Agent EVM.md`. Test vectors: `tests/fixtures/evm-eip712.json`.

Method: I read the code as an attacker. No source file was changed and the test suite was not run. Three small probes ran outside the
repository, in a scratch directory:

- **ed25519-dalek 2.2.0.** This crate is the one nearcore `near-vm-runner` 0.37.4 calls in `ed25519_verify`, through `VerifyingKey::from_bytes` and then the non-strict `Verifier::verify` (I read `logic.rs:1815-1866`). The probe uses the same calls.
- **@noble/curves 1.9.7.** This is the version installed in the x207 workspace, which `sdk/packages/core/src/grantee-proof.ts` and `license.ts` use.
- **Node 26.11.1 `crypto.verify` (OpenSSL 3.5.9).** This is what `sdk/packages/core/src/voucher.ts` `verifyVoucher` uses.

This report lists what I tried and what I found. It does not prove the code is safe.

## Summary

| # | Severity | Finding |
| --- | --- | --- |
| M1 | Medium | The sponsor refund makes it almost free to fill all 64 member slots for good (Open admission) |
| M2 | Medium | The placeholder `pubkey_voucher` of an EVM member is a small-order ed25519 point: any verifier that does not branch on member type accepts forged signatures for any message |
| M3 | Medium | A signed `request_exit` is bound to no epoch, cannot be cancelled and may have an unbounded deadline: anyone who saw a failed one can force an irreversible exit later |
| L1 | Low | The refund leaves zero storage margin and is computed before buffered writes flush; exited EVM records then live on the shared storage float |
| L2 | Low | Nothing in the signed data marks the network or the deployment; nonces restart at 0 if a circle is redeployed at the same account |
| L3 | Low | `claim_evm` and `withdraw_stake_evm` accept the circle itself or the token contract as `receiver` |
| L4 | Low | EVM members still reach every predecessor-gated method through their NEP-518 eth-implicit account, which feeds M2 |
| L5 | Low | `join` accepts small-order ed25519 keys for NEAR members (already in 0.0.13) |
| L6 | Low | `on_claim_resolved` over-credits after a partial use in a shortfall epoch (already in 0.0.13) |
| I1-I8 | Info | Signature encodings that are not canonical, live fixture signatures, facilitator branch, withdraw route, and others (below) |

No High finding. The cryptographic core holds: EIP-712 hashing, `ecrecover` use, the address compare, and the separation of circles and action types.

---

## Medium

### M1. The sponsor refund makes it almost free to fill all 64 member slots for good

- **Where:**
  - `x207-circle/src/evm.rs:203-214` (`refund_sponsor`)
  - `x207-circle/src/stake.rs:229` and `:283` (calls)
  - `x207-circle/src/evm.rs:260` and `x207-circle/src/member.rs:142` (cap check)
  - `x207-circle/src/member.rs:16` (`MAX_MEMBERS = 64`)
- **Why:**
  - `member_count` only grows, at `member.rs:165` and `evm.rs:299`. No code path removes a member: `prune_epoch` keeps members, and `Exited` is terminal.
  - So a circle accepts at most 64 joins in its whole life, and exited members still count.
  - Under 0.0.13, filling the circle cost 64 x 0.35 = 22.4 NEAR, which never came back.
  - With `join_evm` plus the refund, the attacker gets all of it back.
- **Scenario (Open admission):**
  1. The attacker makes 64 throwaway EVM keys.
  2. For each key, the attacker calls `join_evm` (0.35 NEAR) and stakes 1 base unit of USDT with `ft_transfer_call` and `member_id = 0x...`. The member becomes `active`.
  3. Then `request_exit_evm`.
  4. After the epoch is finalized, `withdraw_stake_evm`. The 1 unit comes back, the member is `exited`, and `refund_sponsor` returns the 0.35 NEAR.
  5. Net cost: gas and one epoch of waiting.
  6. From then on, every `join` and `join_evm` fails with `ERR_MAX_MEMBERS`. There is no owner path to free a slot.
- **Side effect:** the refund returns the full `JOIN_STORAGE_DEPOSIT`, but the member record, the sidecar, the `member_index` entry and every D5 position stay. Positions are never pruned (`docs/security-review.md`). That storage is then paid from the float that the other members' deposits provide.
- **Fix:** choose one.
  1. Simplest, same as `join`: drop the refund. Delete `refund_sponsor` and its two calls, and keep the `sponsor` field for information only.
  2. Keep a refund but make it smaller than the cost of the slot. Refund `JOIN_STORAGE_DEPOSIT - EVM_EXIT_RETAINED`, where `EVM_EXIT_RETAINED` covers the retained bytes (measure them in near-sandbox) plus a fee that makes squatting cost something, for example 0.1 NEAR. Refund only when the member was `admitted` or had at least one settled epoch.
  3. Longer term: count only members that have not exited against `MAX_MEMBERS`. Then exits free slots, while every loop over `member_index` (`settlement.rs:256`, `pool.rs:147`) stays bounded, for example by removing exited members from the index once they have no open position.

### M2. The placeholder `pubkey_voucher` is a small-order point: forgeable for any message

- **Where:**
  - `x207-circle/src/evm.rs:31`: `EVM_PLACEHOLDER_KEY = "ed25519:11111111111111111111111111111111"`, 32 zero bytes.
  - `evm.rs:280-283` and `:305`: stored in `Member.pubkey_voucher` and emitted in `member_joined`.
- **Why:** 32 zero bytes decode to the point (sqrt(-1), 0), which has order 4. The key is published through `member()`, `list_members()`, the `member_joined` event and the backend read API (`pubkeyVoucher`). It looks like a real key.
- **Measured:**
  - **ed25519-dalek 2.2.0, as nearcore `ed25519_verify` calls it.** `S = 0` and `R` in {identity, A, -A, 2A} forged 129 of 200 random messages. The other messages fall after grinding one field (`resource`, `seq`). nearcore's only extra check is on the top 3 bits of `S`, and `S = 0` passes it.
  - **@noble/curves 1.9.7 `ed25519.verify`, default options (`zip215: true`).** The signature `R = identity, S = 0` verified for 50 of 50 random messages. This is a universal forgery. With `{ zip215: false }`: 0 of 50.
  - **Node `crypto.verify` (OpenSSL 3.5.9).** 0 of 200 with all 8 small-order `R`. OpenSSL rejects the key.
- **Reachable today:**
  - On chain: no. `voucher.rs:191` and `bundle.rs:369` branch on `is_evm_member`, and the sidecar is never removed.
  - Off chain: yes.
    1. `backend/broker/src/broker.ts:83-86` gets `voucherKeyOf(grantee)` from the read API.
    2. `backend/broker/src/provider/manager.ts:234` passes it to `proveGranteeRef`.
    3. That path ends in `sdk/packages/core/src/grantee-proof.ts:116`, which runs noble `ed25519.verify` with default options.
    4. If an EVM member is the `grantee` of a grant, anyone can forge its grantee proof and take its provider access. `accept_grant` makes the predecessor the grantee (`grant.rs:407-419`), and an EVM member can call it through its eth-implicit account (L4).
    5. `license.ts:288` `verifyRelease` uses the same noble call, so a release "signed" by an EVM seller can be forged into a fake cheat proof.
    6. Any third-party verifier that trusts the published key is exposed in the same way.
- **Fix:**
  1. Use a placeholder that does not decode as a point. For example, the bytes `02 00 .. 00` give `ed25519:8opHzTAnfzRpPEx21XtnrVTX28YQuCpAjcn1PczScKh`. Checked: dalek `VerifyingKey::from_bytes` fails, so nearcore `ed25519_verify` returns `false`, and noble `verify` returns `false`. Update `EVM_PLACEHOLDER_KEY` and the ABI note.
  2. As defense in depth, refuse the placeholder in the ed25519 branches: `require!(pubkey.to_string() != EVM_PLACEHOLDER_KEY, ...)`.
  3. Off chain:
     - The broker and `grantee-proof.ts` refuse a grantee whose `evm_signer` is not null, or whose key is the placeholder.
     - Call noble with `{ zip215: false }` everywhere a signature proves identity.

### M3. A signed `request_exit` is bound to no epoch, cannot be cancelled and may never expire

- **Where:**
  - `x207-circle/src/evm.rs:340-343`: `authorize(&member, "request_exit", 0, "", ...)`. The epoch is always 0.
  - `evm.rs:219-234`: no upper bound on `deadline`.
  - `stake.rs:245-254`: no way to undo `exit_requested_epoch`.
- **Why:**
  - A signed action that fails its business step reverts the nonce bump, so it stays valid until its deadline.
  - Signed arguments of a failed transaction are public.
  - The only way to burn nonce `n` is to run another action that succeeds with nonce `n`.
  - A member that is only a debtor never claims, so its nonce may never move.
- **Scenario:**
  1. An agent signs `request_exit` with nonce 0 and a far deadline while still `pending`. The call fails with `ERR_WRONG_STATUS`. It could also fail later with `ERR_BOND_LOCKED`.
  2. The agent then stakes (staking uses no nonce) and trades for weeks.
  3. Anyone can submit the old signature at any moment and exit the agent. From then on, more stake and vouchers for later epochs are refused (`ERR_EXITING`), and the exit cannot be undone.
  4. With M1, the slot is lost too.
- **Fix:**
  1. Bind the exit to its epoch. Sign `epoch = current_epoch` and require `epoch == self.current_epoch` in `request_exit_evm` (new argument `epoch`, error `ERR_EPOCH_MISMATCH`).
  2. Add `cancel_evm(member, nonce, deadline, signature)` with `action: "cancel"`, which only bumps the nonce.
  3. Cap the horizon in `authorize` and `join_evm`: `require!(deadline <= now_ms + MAX_EVM_DEADLINE_MS, "ERR_EVM_DEADLINE_TOO_FAR")`, for example 24 h.
  4. Regenerate the vectors and update the SDK.

---

## Low

### L1. The refund leaves zero storage margin and runs before buffered writes flush

- **Where:** `x207-circle/src/evm.rs:208-213`, which runs inside `on_withdraw_resolved` (`stake.rs:229`) and `withdraw_stake_for` (`stake.rs:283`).
- **Problem:**
  - The refund is `min(account_balance - storage_usage x byte_cost, 0.35 NEAR)`. The `near_sdk::store` writes to `members` and the contract root flush only at the end of the method, so `storage_usage()` does not include them yet.
  - Today the late writes do not change any size (status, limit and flags). Any later change that adds bytes after the refund would end the receipt with `LackBalanceForState`.
  - In `on_withdraw_resolved` that reverts the callback after the USDT already left. The member then stays `withdraw_pending = true` forever, and D6 keeps the amount in flight.
- **Storage cost:** with M1 unfixed, every exited EVM member's retained bytes come out of the shared float. The total is bounded by 64 members, plus their D5 positions.
- **Fix:**
  1. Keep a margin: `spare.saturating_sub(REFUND_MARGIN)`, for example 0.05 NEAR.
  2. Call `refund_sponsor` as the last statement, after an explicit `self.members.flush()`.
  3. Or apply M1 option 1.

### L2. The signed data marks no network and no deployment

- **Where:** `x207-circle/src/evm.rs:23` and `:116-118` (domain `{name: "x207", version: "1"}`), and the `circle` field = `current_account_id`.
- **Problem:**
  - A circle on another network at the same account id (a top-level or implicit account) would accept the same signatures.
  - Worse: if a circle is deleted and redeployed at the same account with a fresh state, every sidecar nonce restarts at 0. Old `join`, `claim` and `withdraw_stake` signatures that are not expired would then replay.
  - Today the release plan uses a new account per version (`x207-v014.testnet`), so this is a rule people must follow, not one the code enforces.
- **Fix:**
  1. Add a deployment salt to the domain, for example `salt = sha256(circle_id || created_at)`, which `with_config` fixes once.
  2. Or add a `network` string (`"near-testnet"` or `"near-mainnet"`) to every type.

### L3. The circle or the token contract is accepted as `receiver`

- **Where:**
  - `x207-circle/src/evm.rs:330-337` (`claim_evm`)
  - `evm.rs:348-352` (`withdraw_stake_evm`)
  - `settlement.rs:416` and `stake.rs:292`
- **Problem:** the only refusal is `withdraw_stake_evm` to Intents.
  - `receiver = env::current_account_id()` relies on the token refusing a transfer to oneself. A token that allows it, such as a hand-rolled testnet mock, would book the funds as paid out while the USDT stays in the circle, attributed to nobody. D6 would then drift.
  - `receiver = usdt_token_id` strands the funds at the token contract.
  - The member signed both, so this is a footgun, not theft.
- **Fix:** `require!(receiver != env::current_account_id() && receiver != self.usdt_token_id, "ERR_BAD_EVM_RECEIVER: ...")` in both methods.

### L4. EVM members still reach the predecessor-gated methods

- **Where:**
  - `claim` (`settlement.rs:367`), `request_exit` and `withdraw_stake` (`stake.rs`)
  - `accept_grant` (`grant.rs:407`), `complete_escrow` (`escrow.rs:323`), `post_intent` (`bundle.rs:245`) and the deal entry points
- **Problem:**
  - Since NEP-518, the account `0x<address>` exists on NEAR as an eth-implicit wallet contract, driven by the same secp256k1 key. Through it the agent can be the predecessor.
  - This is not an authority bypass: it is the same key. But it skips nonce and deadline.
  - It goes against design decision 7 (grant, escrow and deal are not for EVM members).
  - It is also how an EVM member can become a grantee, buyer or deal party that holds the forgeable placeholder key (M2).
- **Fix:**
  1. Where decision 7 must hold, add `require!(!crate::evm::is_evm_member(&caller), "ERR_EVM_UNSUPPORTED: ...")` in `accept_grant`, `complete_escrow`, the deal entry points and `post_intent`.
  2. Or document the route and fix M2.

### L5. `join` accepts small-order ed25519 keys (already in 0.0.13)

- **Where:** `x207-circle/src/member.rs:132-134`.
- **Problem:** only the curve type is checked. A member may register the zero key, or any of the 8 small-order encodings. Its vouchers are then forgeable by anyone, as measured in M2.
  - The direct victim is that member: anyone can sign debts in its name.
  - But it also gives a member a way to deny its own vouchers ("somebody forged them") in an arbiter dispute.
- **Fix:**
  1. Refuse the small-order encodings with `ERR_BAD_PUBKEY`. A constant table of the 8 canonical encodings and their sign-bit variants is enough.
  2. Or refuse keys that fail a decode-and-`is_small_order` check in the SDK before `join`.

### L6. `on_claim_resolved` over-credits after a partial use in a shortfall epoch (already in 0.0.13)

- **Where:** `x207-circle/src/settlement.rs:475` with `:396`.
- **Problem:**
  - Under a shortfall the payout is `position.amount x pool_final / claimable_final`. After a partial use `u`, the code sets `position.amount -= u`, the paid amount, not the share of the entitlement that it covers.
  - The retry then pays `(A - u) x r`, so the total is `u + (A - u) x r`, which is more than the entitlement `A x r`. The pool check stops it from going negative, so the excess comes from other creditors.
  - It needs `intents_id` to return a partial use. The real Intents contract uses all or nothing, so the risk is low.
  - The EVM feature reaches this path only through `claim_evm` with receiver = Intents.
- **Fix:** under a shortfall, reduce by `ceil(u x claimable_final / pool_final)`. Or keep a `paid` field and pay `A x r - paid`.

---

## Info

- **I1. Signature strings are not canonical.**
  - `decode_hex` accepts upper-case hex, and `v` may be 27/28 or 0/1 (`evm.rs:74-85`, `:182-186`). So one signature has many valid strings.
  - Nothing on chain keys on the string, so this is not exploitable.
  - Off-chain stores must deduplicate on `(epoch, payer, payee, seq)` or the digest, never on the signature text.
- **I2. The test vectors are live on the real deployment.**
  - `tests/fixtures/evm-eip712.json` signs for circle `x207-v014.testnet`, which is the planned deployment account. It uses Hardhat test key #1, which is public (`scripts/gen-evm-vectors.mjs:18`), and deadline 1.9e12 ms (2030).
  - Anyone can `join_evm` the address `0x70997970...` on the live circle and take one of the 64 slots. Anyone can sign any action for it, because the key is public.
  - Never use that key for the demo agent.
  - Generate the vectors for a circle id that will not be deployed, or with a past deadline and a test clock.
- **I3. The facilitator branches on the signature prefix, not on the member.**
  - `sdk/packages/scheme/facilitator/verify.ts:130-132`: an `ed25519:` signature for an EVM payer goes to `verifyVoucher` with the placeholder key.
  - It is safe today only because Node's OpenSSL rejects the small-order key (measured, 0 of 200).
  - Branch on `evm_signer(payer) !== null` instead.
- **I4. No withdraw route for an EVM-only agent.**
  - `withdraw_stake_evm` rightly refuses Intents, because a plain `ft_transfer` there credits nobody.
  - So an agent with only an EVM key can get its stake back only through a NEAR account, or through its own eth-implicit account registered at USDT.
  - Consider `ft_transfer_call` to Intents with `msg = member`, as `claim_for` already does.
- **I5. A pending EVM member can never exit.** `request_exit_for` needs `active` or `suspended`, so the sponsor's 0.35 NEAR of a member that never staked is never refunded. This is the same as `join`.
- **I6. One nonce lane.** A signed action whose business step cannot run blocks every later pre-signed action, which must be re-signed. The SDK should sign one action at a time and read `evm_signer` first.
- **I7. Events.** `member_joined` carries no `sponsor`, and no event records the nonce that an action used. Indexers cannot rebuild the sidecar from logs.
- **I8. `unstable` feature.** `near-sdk` is pinned to `=5.29.1` with `unstable` only for `env::ecrecover`. Recheck the signature of `ecrecover` on any SDK bump.

---

## Tried and NOT exploitable

1. **High-s twin.** `ecrecover(.., malleability_flag = true)`. nearcore `check_signature_values(true)` requires `r < n` and `s < n/2 + 1` (read in near-crypto 0.37.4). The vector `highSSignature` gives `None` (`evm.rs:475-476`).
2. **Bad `v` and bad `r`/`s`.**
   - Only 27/28/0/1 map to 0/1. 2/3 (r overflow) and EIP-155 values are refused before the host call, so no host error is reachable: `v < 4`, flag is 1, the signature is 64 bytes and the hash 32 bytes.
   - `r = 0` or `s = 0` fail in libsecp256k1 recover, which gives `None`.
3. **Malformed strings.** A wrong prefix, a length other than 130, non-hex or multibyte UTF-8 give `None`. Nothing panics. `bytes[64]` is safe because the length was checked first.
4. **Zero or unrecoverable result.**
   - `None` is never equal to `Some(address)`.
   - `verify_voucher` and `verify_bundle` compare the recovered address, as lowercase hex, with the member id byte for byte.
   - `authorize` compares the 20 bytes with the sidecar.
   - The zero address cannot be joined without its key.
5. **Replay across circles.**
   - `circle = current_account_id` is in all three types.
   - Voucher check 1 requires `voucher.circle == current_account_id`.
   - The bundle message is built on chain, with the circle and the epoch in it.
6. **Replay across Voucher, Bundle and Action.** The typehashes differ, so the struct hashes differ.
7. **Replay across actions.**
   - `action`, `epoch`, `receiver`, `termsHash`, `nonce` and `deadline` are all hashed.
   - A claim signature refused as withdraw is tested (`evm.rs:594`).
   - A `join` signature carries a 64-hex `termsHash`, while every other action signs `""`.
8. **Replay of a done action.** The nonce bump stops it. A second `join_evm` gives `ERR_ALREADY_MEMBER`. Members are never removed, so the nonce never resets in place.
9. **Upper and lower case, and `0x` ids.**
   - NEAR `AccountId` refuses upper case.
   - `parse_address` refuses upper case and a wrong length.
   - `looks_evm` without a case check is harmless, because the sidecar key decides.
   - A NEAR-key member whose id looks like `0x...` has no sidecar. Its vouchers go the ed25519 path, and the `*_evm` methods give `ERR_NOT_EVM_MEMBER` (tested, `evm.rs:574-578`).
10. **Squatting a victim's address with `join`.** The member id is the predecessor. The NEP-518 eth-implicit account is driven only by the matching secp256k1 key, so another predecessor cannot take `0x<victim>`.
11. **Squatting with `join_evm`.**
    - It needs the victim's `join` signature. The `terms_hash` and the deadline are signed.
    - Someone who front-runs the victim's own signature becomes the `sponsor`. They pay the deposit and get it back. The victim loses nothing.
12. **The ed25519 path for EVM members on chain.**
    - Voucher check 12 and `commit_bundle` take the EIP-712 branch whenever the sidecar exists.
    - An `ed25519:` signature for an EVM payer is refused (tested, `evm.rs:675-676`).
    - No other on-chain code verifies `pubkey_voucher`.
13. **Double refund.**
    - The refund runs once per member: in the stake-0 path or in the successful callback, never both.
    - `withdraw_pending` and `ERR_MEMBER_EXITED` stop a second withdraw.
    - `Exited` is terminal, and there is no re-join.
14. **Refund that breaks solvency at the moment it runs.** The refund is capped at the balance above `storage_usage`. See L1 for the margin.
15. **A third party burning a victim's nonce.**
    - It needs a valid signature.
    - A failed business step, or `ERR_GAS_TOO_LOW` (`CLAIM_MIN_GAS` 100 Tgas), reverts the bump.
    - Gas that is too low for the static-gas promises in `withdraw_stake_for` makes the whole receipt fail.
    - A front-runner who submits the same signature produces the same effect.
16. **Callback spoofing and exposed helpers.**
    - `on_claim_resolved` and `on_withdraw_resolved` are `#[private]`.
    - `claim_for`, `request_exit_for`, `withdraw_stake_for`, `authorize` and `refund_sponsor` are not in `abi.json` (checked).
    - The `*_evm` methods are not payable, as intended.
17. **Swapping the claim receiver or destination.**
    - The receiver is signed, and `Intents` versus `Near` follows from it.
    - A failed `ft_transfer` (for example, a receiver not registered at USDT) puts the position back to `claimable`. Nothing is stranded.
    - Partial use is booked back. L6 is the only flaw there.
18. **Stranding the stake at Intents.** `withdraw_stake_evm` refuses `intents_id` before `authorize`.
19. **Storage key collision.** `StorageKey::EvmSigners = b'v'` is unique among the enum prefixes, and every raw key starts with `x207_`.
20. **State written before a panic.** NEAR reverts all writes of a receipt that panics. The explicit `flush()` calls in `authorize` and `join_evm` do not survive a later panic. The detached refund and excess transfers are dropped with the receipt.
21. **Facilitator forgery with the placeholder key.** Node `crypto.verify` (OpenSSL 3.5.9) refused all 8 small-order `R` with `S = 0` on 200 messages. It is not exploitable today, but see I3.

## Disposition (Worker F, 2026-11-05)

- High: none.
- M2 (placeholder key): fixed. `EVM_PLACEHOLDER_KEY` is now a 32-byte value that is not a curve point; test `tc_ct_104_08_placeholder_key_is_not_a_curve_point`.
- M1 (member slots): accepted for the testnet demo and reported to the coordinator as a design question. Every member row, EVM or not, counts against `MAX_MEMBERS` (64) for ever, and the sponsor refund makes a slot cheap to burn in an `open` circle. Options if it matters: keep the deposit (no refund), or use `admission = dao`.
- M3 (stale `request_exit` signature): accepted. SDK/relayer must sign short deadlines (minutes) for `request_exit` and `withdraw_stake`; a signature that fails for a business reason stays valid only until its deadline.
- Low and Info findings: see above; none changes the interface.

## Domain binding (audits C55 and C60)

The contract is not an EVM contract, so the usual EIP-712 anchors do not fit. Wallets such as MetaMask refuse `signTypedData` when the `chainId` of the domain differs from the chain they are connected to (an agent on Base is on 8453), so a made-up NEAR `chainId` would make real wallets fail; and `verifyingContract` must be a 20-byte address, which a NEAR account is not. The domain `salt` (`keccak256("near:" + network + ":" + contract account id)`) gives the same binding: a signature is valid for one contract on one network only (test `tc_ct_104_09`). The `circle` field of every message binds it again. `x207-v014.testnet` runs this salted code (on-chain `code_hash` sha256 `7d731dfb...39f9` (reproducible build; until 2026-11-05 it was `743109cb...c2a87`, the host build that SD-090 ran on) equals the salted build; the wasm holds the type string `EIP712Domain(string name,string version,bytes32 salt)`).
