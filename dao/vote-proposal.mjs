#!/usr/bin/env node
// CT-047: prints the near-cli-rs command that votes on a DAO proposal (default VoteApprove).
// It never signs or sends. Only council members can vote; the DAO rejects everyone else.
//
//   node dao/vote-proposal.mjs --dao x207.sputnik-dao.testnet --id 0 --signer a.testnet [--action VoteApprove] [--network testnet|mainnet]
import { actProposalCommand, buildActProposalArgs, networkOf, parseArgs } from './lib.mjs';

try {
  const args = parseArgs(process.argv.slice(2));
  for (const required of ['dao', 'id', 'signer']) {
    if (typeof args[required] !== 'string') throw new Error(`missing --${required}`);
  }
  const network = networkOf(args);
  const actArgs = buildActProposalArgs({ id: args.id, action: args.action });
  if (network === 'mainnet') console.error('MAINNET: review the command below and run it yourself; this script never sends it.');
  console.log(actProposalCommand({ dao: args.dao, signer: args.signer, network, actArgs }));
} catch (error) {
  console.error(`error: ${error.message}`);
  process.exit(1);
}
