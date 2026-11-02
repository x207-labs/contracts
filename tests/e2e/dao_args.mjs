// Prints the JSON arguments the scripts in ../../dao build, for tests/ct048_sputnik_dao.rs. Nothing is signed or sent.
//   node dao_args.mjs policy <member,member,...>
//   node dao_args.mjs propose <contract> <params json>
//   node dao_args.mjs act <id> <action>
//   node dao_args.mjs admit <contract> <account>
//   node dao_args.mjs asset-class <contract> <asset type> <class> <terms hash>
import { buildActProposalArgs, buildAddProposalArgs, buildAdmitProposalArgs, buildAssetClassProposalArgs, buildPolicy, parseParamsJson } from '../../dao/lib.mjs';

const [kind, a, b, c, d] = process.argv.slice(2);
if (kind === 'policy') console.log(JSON.stringify(buildPolicy(a.split(','))));
else if (kind === 'propose') console.log(JSON.stringify(buildAddProposalArgs({ contract: a, params: parseParamsJson(b) })));
else if (kind === 'act') console.log(JSON.stringify(buildActProposalArgs({ id: a, action: b })));
else if (kind === 'admit') console.log(JSON.stringify(buildAdmitProposalArgs({ contract: a, account: b })));
else if (kind === 'asset-class') console.log(JSON.stringify(buildAssetClassProposalArgs({ contract: a, assetType: b, assetClass: c, deliveryMethods: ['near_access_key'], acceptedProofs: ['rpc_access_key'], termsHash: d })));
else throw new Error(`unknown kind ${kind}`);
