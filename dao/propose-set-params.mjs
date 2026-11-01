#!/usr/bin/env node
// CT-046: prints the near-cli-rs command that adds a DAO proposal calling `set_params` on x207-circle.
// It never signs or sends. Range checks stay with the contract: out-of-range params still make a
// proposal, and the contract rejects them when the DAO executes it.
//
//   node dao/propose-set-params.mjs --dao x207.sputnik-dao.testnet --contract x207-circle.testnet \
//     --signer a.testnet --params-file params.json [--description "..."] [--network testnet|mainnet]
import { readFileSync } from 'node:fs';
import { addProposalCommand, buildAddProposalArgs, networkOf, parseArgs, parseParamsJson } from './lib.mjs';

try {
  const args = parseArgs(process.argv.slice(2));
  for (const required of ['dao', 'contract', 'signer']) {
    if (typeof args[required] !== 'string') throw new Error(`missing --${required}`);
  }
  if (typeof args['params-file'] !== 'string' && typeof args.params !== 'string') throw new Error('missing --params-file or --params');
  const params = parseParamsJson(typeof args.params === 'string' ? args.params : readFileSync(args['params-file'], 'utf8'));
  const network = networkOf(args);
  const addProposalArgs = buildAddProposalArgs({ contract: args.contract, params, description: args.description });
  if (network === 'mainnet') console.error('MAINNET: review the command below and run it yourself; this script never sends it.');
  console.log(addProposalCommand({ dao: args.dao, signer: args.signer, network, addProposalArgs }));
} catch (error) {
  console.error(`error: ${error.message}`);
  process.exit(1);
}
