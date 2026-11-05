# EVM agents (v0.0.14: CT-104, CT-105)

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
