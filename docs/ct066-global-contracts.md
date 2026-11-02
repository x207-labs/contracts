# CT-066: Global Contracts (NEP-591) for many circles from one code

Study only; no change to the contract. Sources read on 2026-11-02: the text of NEP-591 (`near/NEPs` `neps/nep-0591.md`, status Final, version 1.0.0),
the source of `near-sdk` 5.29.1 in the local cargo registry, and the sandbox binary `neard 2.13.4`.

## What NEP-591 says (verified in the NEP text)
- A contract is deployed once as a *global contract*, either by **code hash** (immutable) or by **owner account id** (the owner can redeploy and
  update it for every user). An account then runs it with `UseGlobalContractAction`, a reference of 32 bytes (hash) or the length of the account id.
- Deploying burns tokens for storage: `global_contract_storage_amount_per_byte` is **10 times** `storage_amount_per_byte`, burned, not locked.
  Using it locks storage only for the identifier bytes, so a circle account pays almost nothing for code.
- The contract is copied to every shard; using a non-existing global contract fails with `GlobalContractDoesNotExist`, so a user waits for the
  distribution to finish. Invalid wasm is not detected at deploy time (tokens are burned anyway).
- The NEP names the cost of growing shard count as a known risk.

## What this build can do (verified locally)
- `near-sdk` 5.29.1 has `Promise::deploy_global_contract`, `deploy_global_contract_by_account_id`, `use_global_contract` and
  `use_global_contract_by_account_id` in `promise.rs` (over `env::promise_batch_action_*_global_contract*`). A factory contract could create
  circle accounts and point them at the code.
- The sandbox binary `neard 2.13.4` contains global contract code (41 mentions of `GlobalContract` in the binary), so a sandbox test of
  deploy-by-hash and use is plausible. It was not run: `near-workspaces` 0.23 has no helper for the action, so it needs raw transactions.

## Cost for x207 (arithmetic from the NEP and our wasm)
Our `x207_circle.wasm` is 453 120 bytes. With 10^19 yoctoNEAR per byte (the value behind the 3 NEAR for 300 KB in the NEP), a local deployment locks
about 4.5 NEAR per circle, and the deploy of the testnet account needed about 4.7 NEAR in practice. A global deployment by hash burns about 45 NEAR
once, then each circle locks a few mNEAR. The burn is paid back in locked NEAR after about ten circles, and the locked NEAR of a local copy is
returnable while the burn is not. For one circle (the hackathon case) a local deployment is cheaper.

## Fit and risks for the circle contract
1. By hash: the code of a circle cannot change except by moving the account to another hash. That fits `propose_upgrade` / `apply_upgrade` (CT-055),
   which would become "switch the global identifier" instead of "deploy code"; the current `apply_upgrade` deploys code, so it needs a variant.
2. By account id: the owner of that account can update every circle at once. For circles owned by their own DAOs this takes away the timelock of
   CT-055 and puts all circles under one key; not recommended.
3. State is per account, so the borsh layout of `X207Circle` and the storage keys do not change.
4. `migrate()` after a switch to a new hash works as today (it runs on the new code).
5. Distribution delay: a new circle must wait for the global code to reach its shard before `new` can be called.

## Verdict
Feasible with `near-sdk` 5.29.1 and the current protocol. Worth it only from about ten circles; keep local deployment for the hackathon.
Use deploy by hash, and design `apply_upgrade` to switch the identifier. Not tested on a node.
