#!/usr/bin/env node
// CT-050 preparation: the proposal that changes the params of x207-circle on MAINNET, checked against the chain and printed. DRY RUN by default, and the only mode: this script NEVER signs and never sends
// (it only calls read-only views). The owner reads the report, runs the printed command with the owner's own key, and the council votes with `vote-proposal.mjs`.
//
//   node dao/mainnet-set-params.mjs --dao <dao>.sputnik-dao.near --contract <circle>.near --signer <member>.near --params-file params.json \
//        [--network mainnet|testnet] [--rpc-url <url>] [--description "..."] [--dry-run]
//
// What it reads: `get_owner` and `interface_version` of the circle, `get_params`, and of the DAO `get_policy` and `get_last_proposal_id`. What it checks: the owner is that DAO, the new params are the whole Params of
// this contract (no key added or dropped), at least one value changes, the signer may add a proposal, and how many council votes approve. `--no-dry-run` is refused: there is no sending mode.
import { readFileSync } from 'node:fs';
import { addProposalCommand, buildAddProposalArgs, networkOf, parseArgs, parseParamsJson, buildActProposalArgs, actProposalCommand, getProposalCommand, getParamsCommand } from './lib.mjs';
import { planSetParams } from './set-params-plan.mjs';

const RPC = { mainnet: 'https://rpc.mainnet.fastnear.com', testnet: 'https://rpc.testnet.fastnear.com' };

async function view(rpcUrl, account, method, args = {}) {
  const response = await fetch(rpcUrl, {
    method: 'POST', headers: { 'content-type': 'application/json' }, signal: AbortSignal.timeout(15_000),
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'query', params: { request_type: 'call_function', finality: 'final', account_id: account, method_name: method, args_base64: Buffer.from(JSON.stringify(args)).toString('base64') } }),
  });
  const body = await response.json();
  if (body.error || body.result?.error) throw new Error(`${account}.${method}: ${JSON.stringify(body.error ?? body.result.error).slice(0, 200)}`);
  return JSON.parse(Buffer.from(body.result.result).toString('utf8'));
}

export async function main(argv) {
  const args = parseArgs(argv);
  if (args['no-dry-run']) throw new Error('there is no sending mode: this script only prints. Run the printed command yourself.');
  for (const required of ['dao', 'contract', 'signer']) if (typeof args[required] !== 'string') throw new Error(`missing --${required}`);
  if (typeof args['params-file'] !== 'string' && typeof args.params !== 'string') throw new Error('missing --params-file or --params');
  const network = args.network === undefined ? 'mainnet' : networkOf(args);
  const rpcUrl = typeof args['rpc-url'] === 'string' ? args['rpc-url'] : RPC[network];
  const next = parseParamsJson(typeof args.params === 'string' ? args.params : readFileSync(args['params-file'], 'utf8'));

  const [owner, interfaceVersion, current, policy, lastProposalId] = await Promise.all([
    view(rpcUrl, args.contract, 'get_owner'), view(rpcUrl, args.contract, 'interface_version'), view(rpcUrl, args.contract, 'get_params'),
    view(rpcUrl, args.dao, 'get_policy'), view(rpcUrl, args.dao, 'get_last_proposal_id'),
  ]);
  const plan = planSetParams({ dao: args.dao, contract: args.contract, signer: args.signer, next, state: { owner, interfaceVersion, current, policy, lastProposalId } });

  const out = (line = '') => console.log(line);
  out(`DRY RUN on ${network}: nothing is signed or sent. Read from ${rpcUrl}.`);
  out(`circle ${args.contract} (interface ${interfaceVersion}), owner ${owner}`);
  out(`DAO ${args.dao}: next proposal id ${plan.expectedProposalId}, bond ${plan.bondNear}, voting period ${plan.periodHours} h, ${plan.approvals ? `${plan.approvals.needed} of ${plan.approvals.size} council votes approve` : 'no council found'}`);
  out(`changes (${plan.changes.length}):`);
  for (const c of plan.changes) out(`  ${c.key}: ${JSON.stringify(c.from)} -> ${JSON.stringify(c.to)}`);
  for (const w of plan.warnings) out(`warning: ${w}`);
  if (plan.problems.length > 0) {
    for (const p of plan.problems) out(`PROBLEM: ${p}`);
    out('no command printed: fix the problems above first.');
    process.exitCode = 2;
    return plan;
  }
  const addProposalArgs = buildAddProposalArgs({ contract: args.contract, params: next, description: typeof args.description === 'string' ? args.description : undefined });
  out('\n1. add the proposal (any account that may add one; it attaches the bond, 0.1 NEAR by default):');
  out(addProposalCommand({ dao: args.dao, signer: args.signer, network, addProposalArgs }));
  out(`\n2. each council member votes (proposal id ${plan.expectedProposalId}, if nobody else adds one first):`);
  out(actProposalCommand({ dao: args.dao, signer: 'council-member.placeholder', network, actArgs: buildActProposalArgs({ id: plan.expectedProposalId, action: 'VoteApprove' }) }).replace('council-member.placeholder', '<council member>'));
  out('\n3. read the proposal, then the params (they take effect from the next epoch):');
  out(getProposalCommand({ dao: args.dao, id: plan.expectedProposalId, network }));
  out(getParamsCommand({ contract: args.contract, network }));
  return plan;
}

if (process.argv[1] && import.meta.url === new URL(`file://${process.argv[1]}`).href) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(`error: ${error.message}`);
    process.exit(1);
  });
}
