#!/usr/bin/env node
// CT-056: prints the near-cli-rs command that adds a DAO proposal calling `admit_member` on x207-circle for a new
// account (circle with `admission` `dao`). It never signs or sends; the council votes with vote-proposal.mjs.
//
//   node dao/propose-admit-member.mjs --dao x207.sputnik-dao.testnet --contract x207-circle.testnet \\
//     --signer a.testnet --account newagent.testnet [--description "..."] [--network testnet|mainnet]
import { addProposalCommand, buildAdmitProposalArgs, networkOf, parseArgs } from './lib.mjs';

try {
  const args = parseArgs(process.argv.slice(2));
  for (const required of ['dao', 'contract', 'signer', 'account']) {
    if (typeof args[required] !== 'string') throw new Error(`missing --${required}`);
  }
  const network = networkOf(args);
  const addProposalArgs = buildAdmitProposalArgs({ contract: args.contract, account: args.account, description: args.description });
  if (network === 'mainnet') console.error('MAINNET: review the command below and run it yourself; this script never sends it.');
  console.log(addProposalCommand({ dao: args.dao, signer: args.signer, network, addProposalArgs }));
} catch (error) {
  console.error(`error: ${error.message}`);
  process.exit(1);
}
