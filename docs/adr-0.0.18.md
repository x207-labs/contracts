# ADR: contract 0.0.18 (P1, UM-076): sale of NEAR accounts through a locker, plus the fixes of the audit

Status: proposed on branch `p1-0.0.18`, 2026-11-08. The owner opened P1 on condition that the main circle stays on 0.0.17 until 0.0.18 passes every test and rehearsal ("kalau 0.0.18 lulus dan bekerja dengan baik, migrate ke sana"). Nothing here touches `main`, `x207-v017.testnet`, the demo or the judge documents. Design source: the section "P1 dibedah" of the vault note `x207 - Backlog Pipeline Aset Barter.md`; this ADR turns it into contract changes and fixes the points that note leaves open.

0.0.18 carries four groups of changes:

1. a locker contract and an escrow of kind `account` in the circle (P1);
2. CT-123: the fixes of A-CT-01 and A-CT-02 (audit of 2026-11-08);
3. CT-115: `cargo fmt` of the whole tree;
4. CT-122: `near-sdk` 5.29.1 to 5.30.1 and the allocator.

## 1. P1: selling an account through a locker

### 1.1 Decisions taken (defaults of the coordinator, the owner may correct them)

One mechanism for items #11 to #14: the seller installs the official locker on the account that is sold, the circle holds the sale as an escrow, the buyer pays with a voucher and receives a full-access key. Version 1 sells only accounts without contract code. For #12 (Chain Signatures addresses) only accounts with zero `sign` calls to the MPC contract. #13 (lockup) is sold through the account of the owner of the lockup, with no vesting adapter. #14 (receivable) is sold as a circle member account: the buyer inherits stake and debt and must wait one full epoch. Reputation is reset when the owner changes (backend). The demo needs #11 and #14, and #12 with a fake signer.

### 1.2 Locker contract (new crate `contracts/locker`, own wasm)

State: `circle: AccountId`, `grant_id: u64`, `return_pubkey: PublicKey`, `state: Pending | Held | Released | Returned`.

| Method | Caller | Effect |
| --- | --- | --- |
| `new(circle, grant_id, return_pubkey)` | the account itself (the transaction that deploys it) | stores the state as `Pending` and calls `circle.account_on_lock(grant_id)`; its callback `on_registered` decides the next state |
| `on_registered()` | private | circle accepted: state `Held`. Circle refused (the call failed): state `Returned` and `AddKey(return_pubkey, full access)` on the account itself, so a refused registration never leaves a keyless account |
| `release(grant_id, pubkey)` | `circle` only, state `Held` | state `Released` and `AddKey(pubkey, full access)` on the account |
| `give_back(grant_id)` | `circle` only, state `Held` | state `Returned` and `AddKey(return_pubkey, full access)` |
| `get_state()` | view | `{circle, grant_id, return_pubkey, state}` |

The seller's deploy transaction is one batch on the sold account: `DeployContract(locker)`, `FunctionCall(new)`, then `DeleteKey` for every key of the account. A batch is atomic, so there is no moment with the locker and the seller's keys. `release` and `give_back` check `predecessor == circle` and `grant_id`, so a call from the account itself (which has no keys anyway), from another account, or for another grant fails. The locker never reads the keys or the code of other accounts, and it holds no funds of its own: the account balance stays on the account and moves with the keys.

After `release` the locker stays as code on the account of the buyer, who has full access and may deploy over it or delete it. It cannot change keys again, because its only entry points require `predecessor == circle` and the state is `Released`.

### 1.3 Circle: escrow of kind `account`

Asset type: an owner-registered D10 entry of class `A` (for example `near:account` with delivery method `escrow`, and `near:member_account` for #14). The scope of the grant is `{"account_id": "<sold account>"}` (an account id, at most 1024 bytes of JSON as before). `class_a_asset` gets a fourth variant `Account { account_id }`; a scope with more than one kind of asset is refused as before.

New state `AccountEscrow { account_id, state, buyer, snapshot, locked_epoch, delivery_pubkey }` under its own storage key (`StorageKey::AccountEscrows`), so the stored `Escrow`, `FtEscrow` and `MtEscrow` of 0.0.17 read unchanged. `Held` in `escrow.rs` gets a fourth variant `Account`. States and take-back are the same as for the other escrows (`Held`, `Completing`, `Cancelling`, `Delivered`, `Cancelled`, `Stuck`).

New methods:

- `account_on_lock(grant_id)`: called by the locker. `predecessor` is the sold account. Checks: class A and `Offered` grant, the scope names `predecessor`, the grantor is a member and not frozen, no escrow exists yet, the grant has not expired. Records the escrow as `Held` with `locked_epoch = current_epoch`. Emits `account_escrow_held {grant_id, account_id}` (new event). A panic makes the locker return the account (see `on_registered`).
- `complete_account_escrow(grant_id, voucher, signature, delivery_pubkey)`: same checks and the same registration as `complete_escrow` (the voucher pair is locked, `ERR_ESCROW_TOO_LATE` near the cutoff), plus the checks of 1.4, then `locker.release(grant_id, delivery_pubkey)` followed by `on_escrow_resolved`. The existing `complete_escrow` on an account escrow fails with `ERR_ESCROW_NEEDS_PUBKEY` (the old method has no key argument, so the ABI of 0.0.17 stays valid).
- `cancel_escrow` and expiry: for an account escrow `cancel_escrow` calls `locker.give_back(grant_id)`. Cancel keeps its rule (grantor only, after `expires_at`). Amended during the build: the grantor must differ from the sold account (`ERR_ACCOUNT_SELF`, checked in `offer_grant` and again in `account_on_lock`). The buyer pays the grantor, so a grantor that sold its own account would be paid into the account the buyer receives, and it could not call `cancel_escrow` while the locker holds its keys. For #14 the seller is therefore a second account of the same person (the grantor is a member) and `cancel_escrow` by the grantor always works. Because of that rule the grantor always keeps its own keys. Still, on the coordinator's decision a second method `cancel_expired_account_escrow(grant_id)` lets anyone take an expired account back, for a grantor that lost its keys: (a) only at or after `expires_at`, (b) the locker adds only the `return_pubkey` it recorded at the lock (the call carries no key), (c) only while the escrow is `Held`, so a second call is refused (`ERR_ESCROW_NOT_HELD`), (d) the escrow turns `Cancelling` in the same transaction, so it cannot race `complete_account_escrow` (the first transaction wins, the other finds a state that is not `Held`; a sale also needs an unexpired grant). Refused for token escrows (`ERR_ESCROW_NOT_ACCOUNT`).
- `get_account_escrow(grant_id)`: view, like `get_escrow`.

`on_escrow_resolved` gets the `Account` arm: delivered grants become `active` with the buyer and `delivery_pubkey`; a failed `release` is taken back exactly as for tokens.

Gas: `release` is one function call plus one `AddKey` action; the existing `NFT_TRANSFER_GAS` (30 Tgas) and `RESOLVE_GAS` are reused and measured in the unit and sandbox tests.

### 1.4 Rules for accounts that are members (#14)

An account that is a member of this circle may only be sold with the asset type registered as `near:member_account`; owners decide by registering or not registering that D10 entry (this is the flag D10 of the design, without changing the borsh layout of `AssetClassEntry`). For such a sale:

- `complete_account_escrow` is refused (`ERR_ESCROW_EPOCH_WAIT`) until the epoch of the lock has ended (`current_epoch > locked_epoch`) AND every epoch before the open one that the circle still keeps is `Finalized` (looked at back to 32 epochs; a pruned row ends the walk). `current_epoch` moves at the cutoff, not at the finalization, so the first condition alone would allow a sale while the debt of the locked epoch can still change by clearing. This is the simplest rule that covers "all earlier epochs of the member" without an index of the epochs a member took part in: it can delay a sale while any epoch of the circle is unfinalized, which is acceptable for a rare operation.
- Vouchers of the sold account: from `account_on_lock` until the escrow ends (delivered or taken back) the account is marked in an open account escrow and `voucher` registration refuses it as payer (`ERR_ACCOUNT_LOCKED`), so the seller's voucher key adds no debt between the lock, the check of the buyer and the delivery. Vouchers of the old key that were signed before and not yet submitted: while the escrow is open they are refused as above; after delivery the member key is the key of the buyer, so their signature no longer verifies. They are void by design, never registered, and the buyer inherits only what was registered before the lock. Vouchers already registered stand as debt of the member.
- The member must not be `suspended`, `defaulted`, `exit_requested`, `exited` or `frozen`, and must have no open bond (`bond_locked_of`), no open deal and no in-flight relayer fee. Violations fail with `ERR_ACCOUNT_NOT_SELLABLE`. The buyer inherits stake and `outstanding_debt`.
- On delivery the voucher key of the member is rotated to `delivery_pubkey` (it must be ED25519, like `join`). Without this rotation the seller could still sign vouchers with the old `pubkey_voucher` after the sale and put debt on the buyer. This is the reason `delivery_pubkey` is mandatory for account escrows.
- An account that is a member of a different circle is not checked (the contract cannot see other circles); the SDK check of 1.6 covers what it can.
- While an escrow of the account is `Held` or later, the sold account cannot act as a member of this circle through keys (it has none); through EVM relayers an EVM member id is not an account and is not sellable (`ERR_ACCOUNT_NOT_SELLABLE`).

### 1.5 Threat table (every row gets a test)

| Threat | Defence in 0.0.18 | Test |
| --- | --- | --- |
| Seller keeps one key (full-access or function-call) | not enforceable on chain: the contract cannot read the keys of another account; the SDK refuses to pay unless `view_access_key_list` is empty (1.6) | SDK test with a recorded RPC answer; contract test: none possible, stated in the review |
| Fake locker (other code on the account) | `account_on_lock` only trusts `predecessor`; the code is checked by `code_hash` against the official list that the owner publishes with `set_locker_code_hashes` and the view `get_locker_code_hashes` (list for the SDK, not enforced on chain) | contract: list round trip; SDK: wrong hash refused |
| `release` or `give_back` from a fake circle or the account itself | locker checks `predecessor == circle` and `grant_id` and the state | locker unit tests (all three callers, wrong grant, wrong state, twice) |
| Circle refuses the registration after the keys were deleted | `on_registered` adds `return_pubkey` | locker test and sandbox: grant already `Active`, grant expired, wrong scope |
| `cancel` leaves the account without keys | `give_back` adds `return_pubkey`; expiry cancel callable by anyone for member grantors | contract + sandbox |
| Voucher payment fails after release (or release fails after payment) | `release` is the last step before the callback; failure restores the registration exactly (snapshot, pair lock) as for the tokens | unit test with a failing `release` |
| Buyer pays twice, two buyers race | the pair lock and `ERR_ESCROW_NOT_HELD` | unit test |
| #14 debt grows after the lock | one epoch wait, voucher key rotation, state checks (1.4) | unit + sandbox: signed voucher of the seller after the sale is refused |
| #14 seller exits or is frozen while escrowed | `ERR_ACCOUNT_NOT_SELLABLE` at `account_on_lock` and again at complete | unit |
| #12 MPC signature prepared before the sale | not on chain: the backend refuses listings with any `sign` history and the SDK shows the warning | backend test (separate item) |
| Approvals and permissions the account already gave stay valid | stated limit; the SDK tells the buyer to revoke them | none |
| Existing sub-accounts | not transferred; the listing must say so | none |
| Account with contract code | v1 refuses: SDK checks `code_hash` is the locker; seller deploys the locker over the empty account only | SDK test |

### 1.6 SDK pre-pay check (SD item, after the contract)

Before the buyer calls `complete_account_escrow` the SDK checks over RPC: `view_access_key_list` of the sold account is empty (function-call keys included), `view_code` hash equals one of `get_locker_code_hashes`, the locker `get_state` is `Held` and names this circle and this grant, the account balance and `view_account` storage match the listing. On any difference it throws a named error and sends nothing. The ABI of `@x207/core` is pinned to 0.0.18 on the branch only.

## 2. CT-123: fixes of the audit (A-CT-01, A-CT-02)

- A-CT-01 (amended during the build): admission is checked at the stake, after the join, so a check in `join_evm` would not close the hole. The fix is removal. `remove_pending(account_id)`: the owner removes a `pending` member without stake, debt or withdrawal at any time; anyone may remove one that was never admitted after 7 days (`PENDING_GC_AFTER_NS`). A join into a full circle (`join`, `join_evm`) first removes the oldest such row (`make_room`), after the signature check, so a flood of pending rows cannot hold all 64 slots for good. The join deposit of an EVM member goes back to its sponsor and `member_count` goes down. Events `member_removed {account_id, by_owner}`. `join_evm` is not stricter in `dao` circles. The 64-join case is covered by unit tests TC-CT-123-01..04.
- A-CT-02 (amended): the planned credit to the stake of an `Exited` member would lock funds, because the stake of such a member can no longer be withdrawn. The reserve is kept; the path is documented as unreachable in normal flows and left as is.

## 3. CT-115: formatting

`cargo fmt` of the workspace (the check is already red in 0.0.17: 36 files). It changes the wasm sha256 (measured 2026-11-08: `621c792b…` instead of `cbb8a910…`), which is acceptable here because 0.0.18 gets a new sha anyway. Done as its own commit first on the branch, so the diff of the real changes is readable (`git diff -w` or `--ignore-all-space` between the fmt commit and the head).

## 4. CT-122: dependencies

`near-sdk` 5.30.1 (`=5.30.1` in `Cargo.toml`, `Cargo.lock` regenerated, `cargo deny`/audit notes in `dependency-audit.md` updated). `wee_alloc` (unmaintained notice RUSTSEC-2022-0054): near-sdk makes it a default feature; the plan is to build with `default-features = false` and the `lol_alloc`-style replacement only if 5.30.1 offers a supported feature for it, otherwise keep it and record why. This is decided at coding time by what the crate allows, and the measured wasm size and gas are compared with 0.0.17.

## 5. ABI and compatibility

- `INTERFACE_VERSION` 0.0.18; `abi.json` gains `account_on_lock`, `complete_account_escrow`, `cancel_expired_account_escrow`, `get_account_escrow`, `set_locker_code_hashes`, `get_locker_code_hashes`, `remove_pending`; events `account_escrow_held`, `member_removed`. No existing method changes its signature; `ct068_abi_freeze` counts are updated.
- No state migration: new storage keys only. A circle upgraded from 0.0.17 keeps all rows. `join_evm` becomes stricter in `dao` circles (documented).
- The locker has its own reproducible wasm and its sha256 goes into the official list; its crate is built by the same `repro-build.sh` pattern.
- The wasm of the circle gets a new sha256, a new `KNOWN_SHA256` entry and a new prebuilt, as for every release. 0.0.17 and the tag `v0.0.17` stay as they are.

## 6. Order of work and acceptance

1. This ADR (stage 1 report).
2. Commits on the branch: fmt, then near-sdk bump, then CT-123, then locker crate, then circle escrow, then SDK pin. Unit and sandbox tests for every row of 1.5 that can be tested on chain (stage 2 report).
3. Reproducible build, deploy to a NEW testnet account (not `x207-v017.testnet`), one epoch with a #11 sale and a #14 sale (stage 3 report).
4. SDK pre-pay check and ABI pin on the branch `p1-0.0.18` of `sdk`.

Acceptance for the migration decision (owner, after the coordinator's review): all unit tests, the sandbox suite, the repro sha reproduced twice, the testnet epoch with #11 and #14 finalised, and an updated security review. Until then `main` is untouched.

## 7. Open points for the coordinator

1. 1.4 adds a rotation of the member voucher key on delivery and an owner-registered `near:member_account` type. If the owner prefers no member sales in v1, #14 drops out and 1.4 becomes a single refusal.
2. Closed: the grantor of an account sale must differ from the sold account (see 1.3); `cancel_expired_account_escrow` is kept as the coordinator asked, for a grantor that lost its keys.
3. The allocator replacement in 4 depends on what near-sdk 5.30.1 offers; it is the one item that may end as "kept, reason recorded".
