#!/usr/bin/env node
// CT-047: reads a proposal. Without --from-json it prints the read-only command for `get_proposal`;
// with --from-json (the JSON that command printed, as a file or `-` for stdin) it prints the status.
//
//   node dao/proposal-status.mjs --dao x207.sputnik-dao.testnet --id 0 [--network testnet|mainnet]
//   node dao/proposal-status.mjs --from-json proposal.json
import { readFileSync } from 'node:fs';
import { getProposalCommand, networkOf, parseArgs, proposalStatus } from './lib.mjs';

try {
  const args = parseArgs(process.argv.slice(2));
  if (typeof args['from-json'] === 'string') {
    const text = readFileSync(args['from-json'] === '-' ? 0 : args['from-json'], 'utf8');
    console.log(proposalStatus(JSON.parse(text)));
  } else {
    for (const required of ['dao', 'id']) {
      if (typeof args[required] !== 'string') throw new Error(`missing --${required}`);
    }
    console.log(getProposalCommand({ dao: args.dao, id: args.id, network: networkOf(args) }));
  }
} catch (error) {
  console.error(`error: ${error.message}`);
  process.exit(1);
}
