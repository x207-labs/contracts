# dao/: Sputnik DAO scripts for x207-circle (CT-045)

The scripts only print `near` (near-cli-rs) commands. They never sign or send. A human reads the
command, runs it, and confirms any mainnet run.

1. Create the DAO through the factory (council members vote, proposal period 1 hour, ADR-25):

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

Tests of the builders: `node --test dao/`.

Not verified yet: the policy shape follows `sputnikdao2` and has not run against the factory wasm.
The sandbox run with the Sputnik wasm belongs to the integration test item; the testnet or mainnet
run needs owner approval.
