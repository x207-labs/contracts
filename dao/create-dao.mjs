#!/usr/bin/env node
// CT-045: prints the near-cli-rs command that creates the x207 Sputnik DAO through the factory.
// It never signs or sends anything. A human reads the command, runs it, and confirms mainnet.
//
//   node dao/create-dao.mjs --name x207 --council a.testnet,b.testnet --signer a.testnet [--network testnet|mainnet] [--factory <account>] [--deposit '<n> NEAR']
import { buildCreateArgs, createDaoCommand, networkOf, parseArgs } from './lib.mjs';

try {
  const args = parseArgs(process.argv.slice(2));
  for (const required of ['name', 'council', 'signer']) {
    if (typeof args[required] !== 'string') throw new Error(`missing --${required}`);
  }
  const network = networkOf(args);
  const createArgs = buildCreateArgs({ name: args.name, council: args.council.split(',') });
  if (network === 'mainnet') console.error('MAINNET: review the command below and run it yourself; this script never sends it.');
  console.log(createDaoCommand({ ...(args.factory ? { factory: args.factory } : {}), ...(args.deposit ? { deposit: args.deposit } : {}), signer: args.signer, network, createArgs }));
} catch (error) {
  console.error(`error: ${error.message}`);
  process.exit(1);
}
