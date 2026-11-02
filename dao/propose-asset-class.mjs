#!/usr/bin/env node
// CT-074: prints the near-cli-rs command that adds a DAO proposal calling `set_asset_class` on x207-circle, so the D10 registry
// of F8 is set by governance. It never signs or sends; the council votes with vote-proposal.mjs.
//
//   node dao/propose-asset-class.mjs --dao x207.sputnik-dao.testnet --contract x207-circle.testnet --signer a.testnet \\
//     --asset-type near-rpc:access --class B --delivery near_access_key --proofs rpc_access_key,zktls \\
//     --terms-hash <64 hex> [--description "..."] [--network testnet|mainnet]
import { addProposalCommand, buildAssetClassProposalArgs, networkOf, parseArgs } from './lib.mjs';

const list = (value) => (typeof value === 'string' && value !== '' ? value.split(',') : []);

try {
  const args = parseArgs(process.argv.slice(2));
  for (const required of ['dao', 'contract', 'signer', 'asset-type', 'class', 'terms-hash']) {
    if (typeof args[required] !== 'string') throw new Error(`missing --${required}`);
  }
  const network = networkOf(args);
  const addProposalArgs = buildAssetClassProposalArgs({
    contract: args.contract, assetType: args['asset-type'], assetClass: args.class, deliveryMethods: list(args.delivery),
    acceptedProofs: list(args.proofs), termsHash: args['terms-hash'], description: args.description,
  });
  if (network === 'mainnet') console.error('MAINNET: review the command below and run it yourself; this script never sends it.');
  console.log(addProposalCommand({ dao: args.dao, signer: args.signer, network, addProposalArgs }));
} catch (error) {
  console.error(`error: ${error.message}`);
  process.exit(1);
}
