# dao/: Sputnik DAO scripts for x207-circle (CT-045)

The scripts only print `near` (near-cli-rs) commands. They never sign or send. A human reads the
command, runs it, and confirms any mainnet run.

1. Create the DAO through the factory (council members vote, proposal period 1 hour, ADR-25). The factory, the deposit (testnet 7 NEAR, mainnet 1 NEAR) and 300 Tgas of gas follow `--network`; `--factory` and `--deposit` override:

   ```bash
   node dao/create-dao.mjs --name x207 --council a.testnet,b.testnet --signer a.testnet --network testnet
   ```

2. Deploy `x207-circle` with the DAO as owner. `owner_id` is fixed by `new` and no method changes it,
   so "move the owner to the DAO" means deploying with `new(owner_id = <DAO>)`:

   ```bash
   cargo near build non-reproducible-wasm
   node dao/deploy-owned.mjs --contract x207-circle.testnet --dao x207.sputnik-dao.testnet --circle agents-main --network testnet
   ```

3. Check: `get_owner` returns the DAO account; the DAO policy lists the council and a
   `proposal_period` of `3600000000000` ns.

4. Propose a parameter change (a DAO proposal that calls `set_params`) and read the active parameters
   (CT-046). Out-of-range values still make a proposal; the contract rejects them when the DAO runs it:

   ```bash
   node dao/propose-set-params.mjs --dao x207.sputnik-dao.testnet --contract x207-circle.testnet --signer a.testnet --params-file params.json
   node dao/print-params.mjs --contract x207-circle.testnet
   ```

5. Vote and read the proposal (CT-047). Only council members can vote; the DAO rejects others:

   ```bash
   node dao/vote-proposal.mjs --dao x207.sputnik-dao.testnet --id 0 --signer a.testnet
   node dao/proposal-status.mjs --dao x207.sputnik-dao.testnet --id 0
   ```

   `proposal-status.mjs --from-json <file|->` prints the `status` of the JSON that `get_proposal` returned
   (`InProgress`, then `Approved` once enough council members voted).

6. Admit a new account when `admission` is `dao` (CT-056). The council votes with `vote-proposal.mjs`:

   ```bash
   node dao/propose-admit-member.mjs --dao x207.sputnik-dao.testnet --contract x207-circle.testnet --signer a.testnet --account newagent.testnet
   ```

7. Register an asset type in the F8 registry (CT-074):

   ```bash
   node dao/propose-asset-class.mjs --dao x207.sputnik-dao.testnet --contract x207-circle.testnet --signer a.testnet --asset-type near-rpc:access --class B --delivery near_access_key --proofs rpc_access_key,zktls --terms-hash <64 lowercase hex>
   ```

8. Mainnet `set_params` (CT-050): a dry run that reads the chain and prints the proposal (it has no sending mode; it never signs):

   ```bash
   node dao/mainnet-set-params.mjs --dao <dao>.sputnik-dao.near --contract <circle>.near --signer <member>.near --params-file params.json
   ```

   It reads `get_owner`, `interface_version`, `get_params`, `get_policy` and `get_last_proposal_id`, shows what changes, checks that the DAO owns the circle, that the new params are the whole `Params` (no key added or dropped), that something changes and that the signer may add a proposal, says how many council votes approve, and prints the three commands (add, vote, read). `--network testnet` runs it against the testnet DAO (`x207v8.sputnikv2.testnet`, circle `x207-v013.testnet`).

Tests of the builders: `node --test dao/`.

Not verified yet: the `act_proposal` arguments (`id`, `action`) follow `sputnikdao2` and may need the `proposal` field of newer DAO versions; the policy shape follows `sputnikdao2` and has not run against the factory wasm.
The sandbox run with the Sputnik wasm belongs to the integration test item; the testnet or mainnet
run needs owner approval.
