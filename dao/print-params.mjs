#!/usr/bin/env node
// CT-046: prints the read-only near-cli-rs command that shows the active parameters (`get_params`).
//
//   node dao/print-params.mjs --contract x207-circle.testnet [--network testnet|mainnet]
import { getParamsCommand, networkOf, parseArgs } from './lib.mjs';

try {
  const args = parseArgs(process.argv.slice(2));
  if (typeof args.contract !== 'string') throw new Error('missing --contract');
  console.log(getParamsCommand({ contract: args.contract, network: networkOf(args) }));
} catch (error) {
  console.error(`error: ${error.message}`);
  process.exit(1);
}
