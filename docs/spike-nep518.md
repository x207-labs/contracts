# Spike SD-092: can an EVM agent act on NEAR through NEP-518 without an x207 relayer?

Date: 2026-11-05. Network: testnet (nearcore 2.14.0-rc.5, protocol 88). Time used: about 1 hour.

## Answer

**Yes.** An `0x` key controls an eth-implicit NEAR account. A signed Ethereum transaction (chain id 398) sent to `rlp_execute` on that account makes the account call any contract, and the contract sees the `0x` account as `predecessor_account_id`. No x207 relayer is needed. Anybody can submit the `rlp_execute` call (the caller pays the gas), and a relayer is only a convenience.

Three on-chain results on testnet, all with a fresh spike key (not a Hardhat key, not a demo key; stored in `.secrets/testnet-deployer/spike-nep518.json`, never printed):

| Step | Tx | Result |
|---|---|---|
| Fund `0x4c72ec820ddd843ab669ff95205dd53dc4254582` with 1 NEAR (a `Transfer` to a 0x id) | `Aiz5cP86jRm7fHBWsDuKv6qgCmDw44ko2UA3C88KetSt` | Account created. `code_hash` = `1111…1111` (empty) and `global_contract_hash` = `3PpYvRxBfC5BkZxTw8ZFG3D52w1ZRhvDDWirKoxphMDn`: the wallet contract is a global contract installed automatically. View `get_nonce` returns `"0"`. |
| `functionCall("usdt.x207-deployer.testnet", "storage_deposit", {account_id: 0x…})` with 0.00125 NEAR as `value` | `2VEL2cw4r8Gywa47b6UEoYGVRQiSfAFPmW4eAEHybW8r` | Success: the token registered the account (`total` 0.00125 NEAR). The deposit came out of the 0x account's own balance. |
| `ft_transfer` of 3000 units to `x207-deployer.testnet` (the account held 5000 after an owner `mint`) | `GqWKKZjmskwT4vgAkLitj9xFoAKQTe2txDgPRMaXDNgW` | Success: the balance went from 5000 to 2000. So a `0x` key can move the balance that a token keeps for its `0x` id (the SD-090 payout case). |
| `functionCall("x207-v015.testnet", "set_auto_settle", {enabled: true})`, 1 yocto | `BQ1AgLZV71kub29evxTgMytCWwxDjoKcEriziiV6v68q` | The contract panicked `ERR_NOT_MEMBER: 0x4c72ec82…` because the spike account is not a member. That is the expected answer and proves the circle sees the 0x account as `predecessor`. |

## SD-095: the SD-090 agent spends its own payout

The agent `0x58cd3d4987ad29a7bafdb8cdb90bfbe995e680b7` (the payout of 80000 from the SD-090 clearing run on v014, kept by the token under its 0x id) had no NEAR account. 1 NEAR sent to it (tx `GL8d5tvsneybDdui3ktz7HezeEXidHGM9GYcagfhwEN3`) created the eth-implicit account with the wallet contract (`get_nonce` = 0). Then its own key (read from a file, never printed) signed a type-2 transaction, chain id 398, `functionCall("usdt.x207-deployer.testnet", "ft_transfer", {receiver_id: "x207-deployer.testnet", amount: "30000"}, 30 Tgas, 1 yocto)`, sent to `rlp_execute` by `x207-deployer.testnet` (tx `J59PWbv4MHUz2o7DPJcNk1fUZGrnZc8xy7N39VTXwx6m`, success).

| | before | after |
|---|---|---|
| balance of the agent at the token | 80000 | 50000 |
| balance of `x207-deployer.testnet` at the token | 3000 | 33000 |

No `storage_deposit` was needed for the agent (the relayer paid it when the payout arrived). So the payout of a clearing run is spent by the 0x key alone; x207 runs no relayer for it. Record: `docs/testnet-epochs/sd090-evm-agent-run.json`, field `spendFromOwnKey`.

## What I found out

- NEP-518 is live on testnet, and the wallet contract is installed on a 0x account as soon as it is created by a transfer. No setup, no deploy step.
- `rlp_execute(target, tx_bytes_b64)` is a normal function call on the 0x account. The Ethereum transaction must be signed by the key of that address, with chain id **398** (testnet; mainnet 397), `nonce` equal to `get_nonce`, and `to = keccak256(target)[12..32]`. `gas`, `maxFeePerGas`, `maxPriorityFeePerGas` can be 21000 and 0 (they are not charged).
- Encoding that worked: type 2 (EIP-1559) transaction, `data` = ABI of `functionCall(string receiver_id, string method_name, bytes args, uint64 gas, uint32 yocto_near)` (selector `0x6179b707`). `args` are the JSON bytes. NEAR attached to the call is `value` in wei (1 wei = 1e6 yoctoNEAR) plus the `uint32` yocto remainder, which is how a **1 yocto** deposit (`ft_transfer`, `claim`, `request_exit`, `set_auto_settle`) is passed: `yocto_near = 1`, `value = 0`.
- Gas on testnet: the wallet receipts burn about 2.7 + 1.4 Tgas on top of the target call. The `uint64 gas` field was set to 30 Tgas for every call above; the caller of `rlp_execute` attached 300 Tgas.
- The wallet runs one transaction at a time (an in-flight flag), and the nonce goes up per successful call, as in Ethereum.
- `value` of the transaction is paid from the 0x account's NEAR balance, so the account needs a little NEAR (storage of the global contract is shared; the account itself used 186 bytes).

## What this means for x207

1. **An EVM member can call the NEAR-native methods of the circle itself** if it is a member whose id is its own `0x` address: `set_auto_settle`, `request_exit`, `claim`, `withdraw_stake`, `ft_transfer_call` for stake (`member_id` = the sender). The `*_evm` methods with `eip712:` signatures remain useful when the agent has no NEAR at all (a relayer pays everything), but they are not the only path.
2. **Joining stays a decision of the circle.** `join` needs the `pubkey_voucher` of the member; a 0x member would still use `join_evm` (EIP-712 `Action join`), because only that records the signer for vouchers. After that, its NEAR calls and its EIP-712 signatures belong to the same member id. A member id that is both an EVM signer and a live NEAR account is allowed by the contract today, and `evm_signer`/`member` do not conflict.
3. **Payouts to a 0x id are spendable.** Balances that a token keeps for `0x…` (SD-090 `claim` with destination `near`) can be moved by the key, after `storage_deposit` on that token (the account must also hold a little NEAR). This answers the open question of SD-090.
4. Two things stay open and are not tested here: (a) a full circle flow from a real member (claim, stake) through `rlp_execute`; (b) gas for a `claim` through the wallet, which burns the cross-contract `ft_transfer` after the wallet receipts; the 300 Tgas used here leaves a large margin. Both need an admitted 0x member on v015, a DAO `admit_member`.

## Not found / not used

- The `https://eth-rpc.testnet.near.org` JSON-RPC relayer was not used. Nothing here depends on it; I submitted `rlp_execute` with a normal NEAR transaction from `x207-deployer.testnet`.
- I did not find the source of the wallet contract in a public repository; the behaviour above comes from NEP-518 and the on-chain results.

## How to repeat

`scratchpad` script (not in the repo): sign a type-2 transaction with viem (`chainId: 398`, `to = keccak256(target)[12..32]`, `data = encodeFunctionData(functionCall…)`), base64 the signed bytes, then call `rlp_execute` on the 0x account with `{ target, tx_bytes_b64 }`. The spike address and key are test values only; the key is not committed.
