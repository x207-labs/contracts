# EVM agents (v0.0.14: CT-104, CT-105; v0.0.15: CT-108)

An agent with only an `0x` key joins a circle, pays and gets paid. Code: `x207-circle/src/evm.rs`. Byte-exact vectors from viem: `tests/fixtures/evm-eip712.json` (made by `scripts/gen-evm-vectors.mjs`, key = the public hardhat test key #1).

- **Member id** = lowercase `0x` address (42 characters) as `AccountId`. The record sits in a sidecar map (`evm_signers`, storage prefix `v`): `{ address, sponsor, nonce }`. `Member.pubkey_voucher` holds a placeholder that is not a curve point.
- **Signature** `eip712:0x<130 hex>` (r || s || v; v = 27/28, 0/1 also accepted). High-s signatures are refused.
- **Domain** `EIP712Domain(string name,string version,bytes32 salt)` = `{ name: "x207", version: "1", salt }` with `salt = keccak256("near:" + network + ":" + contract account id)`; network is `testnet` for `*.testnet`, else `mainnet`. No `chainId`, no `verifyingContract`. Every message also has `circle` = the contract account id.
- **Types** `Voucher`, `Bundle`, `Action` as in the design note.

| `Action.action` | epoch | receiver | termsHash | nonce |
|---|---|---|---|---|
| `join` | 0 | "" | the `terms_hash` argument | 0 |
| `claim` | claimed epoch | payout account | "" | signer nonce |
| `request_exit` | 0 | "" | "" | signer nonce |
| `withdraw_stake` | 0 | payout account | "" | signer nonce |
| `auto_settle` (on) / `auto_settle_off` (off) | 0 | "" | "" | signer nonce |

`deadline` is in milliseconds; an action is valid while `block_timestamp_ms <= deadline`. The nonce starts at 0 after `join_evm` and goes up by one per successful action (view `evm_signer`). Use short deadlines.

## Methods (anyone may relay; the signature is the gate)

- `join_evm(address, terms_hash, deadline, signature)` payable, at least 0.35 NEAR. The caller is the `sponsor`; the excess goes back to it. Once the member has exited, up to 0.35 NEAR (what the contract holds above its storage stake) goes to the sponsor.
- `claim_evm(member, epoch, receiver, nonce, deadline, signature)`: `receiver` = the Intents contract gives `ft_transfer_call` that credits the `0x` id at Intents; any other receiver gets a plain `ft_transfer`.
- `request_exit_evm(member, nonce, deadline, signature)`
- `withdraw_stake_evm(member, receiver, nonce, deadline, signature)`: plain `ft_transfer`; the Intents contract is refused (`ERR_BAD_EVM_RECEIVER`).
- Vouchers and bundles: when the payer (participant) is in `evm_signers` the signature must be `eip712:`, else `ed25519:`.
- Stake and settle: unchanged `ft_transfer_call` with `member_id = 0x...`.
- Errors: `ERR_BAD_EVM_SIG`, `ERR_EVM_NONCE`, `ERR_EVM_EXPIRED`, `ERR_NOT_EVM_MEMBER`, `ERR_BAD_EVM_ADDRESS`, `ERR_BAD_EVM_RECEIVER`.

Not for EVM members in this version: intents, grants, escrow, deals (they still need the predecessor). Review: `security-review-evm.md`.

**Why the domain has a `salt` and no `chainId` or `verifyingContract`.** The contract is not an EVM contract, so the usual EIP-712 anchors do not fit. Wallets such as MetaMask refuse `signTypedData` when the `chainId` of the domain differs from the chain they are connected to (an agent on Base is on 8453), so a made-up NEAR `chainId` would make real wallets fail; and `verifyingContract` must be a 20-byte address, which a NEAR account is not. The domain `salt` (`keccak256("near:" + network + ":" + contract account id)`) gives the same binding: a signature is valid for one contract on one network only (test `tc_ct_104_09`). The `circle` field of every message binds it again. `x207-v014.testnet` runs this salted code (on-chain `code_hash` sha256 `7d731dfb...39f9` (reproducible build; until 2026-11-05 it was `743109cb...c2a87`, the host build that SD-090 ran on) equals the salted build; the wasm holds the type string `EIP712Domain(string name,string version,bytes32 salt)`).

## Auto-settle from stake (v0.0.15, CT-108)

Opt-in. A member that has set `auto_settle` and does not pay its `due` in the payment window has the `due` taken from its own stake by `finalize_epoch`. The member stays `active`: no `member_defaulted`, no suspension. The position goes `due` to `paid`, `outstanding_debt` drops by the `due`, and creditors claim in full. If the free stake (stake minus a locked bond) is below the `due`, nothing partial happens: the old default path runs unchanged. Default is off. The flag lives in a sidecar map (storage prefix `u`), so `Member` keeps its layout.

- `set_auto_settle(enabled)`: the caller turns it on or off. Errors: `ERR_NOT_MEMBER`, `ERR_MEMBER_EXITED`.
- `set_auto_settle_evm(member, enabled, nonce, deadline, signature)`: for an EVM member; anyone relays. `Action.action` is `auto_settle` when `enabled` is true and `auto_settle_off` when false, so the signature fixes the choice (a signed "on" cannot be replayed as "off"). Epoch 0, receiver "", termsHash "".
- View `is_auto_settle(account_id) -> bool`.
- Timing (deviation from the backlog row CT-108, which said "opt-in at `join_evm`"): the flag is a toggle, set at any time. It stores the block timestamp and only counts for an epoch committed at or after it, so opting in once the `due` is known (after `commit_epoch`) does not avoid a default (review M1). The EVM `join_evm` signature stays unchanged.
- Events: `auto_settle_set { account_id, enabled }` and `auto_settled { epoch, account_id, due, stake, status: "active", position_status: "paid", outstanding_debt }`. `auto_settled` appears in the `finalize_epoch` logs; with the usual log-room rule it can be left out and a note line `x207: N auto_settled events left out` is written.
- Cost: the stake shrinks by the `due`, so the next `limit` follows (`bound_limit`). Measured gas of `finalize_epoch` with 64 members, 63 auto-settled: 76.9 Tgas.
