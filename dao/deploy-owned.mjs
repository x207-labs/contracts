#!/usr/bin/env node
// CT-045: prints the near-cli-rs command that deploys `x207-circle` with the DAO as owner.
// `owner_id` is fixed by `new`; the contract has no method that changes it (spec section 4).
//
//   node dao/deploy-owned.mjs --contract x207-circle.testnet --dao x207.sputnik-dao.testnet --circle agents-main [--network testnet|mainnet]
import { buildInitArgs, deployOwnedCommand, networkOf, parseArgs } from './lib.mjs';

try {
  const args = parseArgs(process.argv.slice(2));
  for (const required of ['contract', 'dao', 'circle']) {
    if (typeof args[required] !== 'string') throw new Error(`missing --${required}`);
  }
  const network = networkOf(args);
  const initArgs = buildInitArgs({ daoAccount: args.dao, circleId: args.circle });
  if (network === 'mainnet') console.error('MAINNET: review the command below and run it yourself; this script never sends it.');
  console.log(deployOwnedCommand({
    contract: args.contract,
    wasm: args.wasm ?? 'target/near/x207_circle.wasm',
    network,
    initArgs,
  }));
} catch (error) {
  console.error(`error: ${error.message}`);
  process.exit(1);
}
