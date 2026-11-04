// Run: node --test dao/
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { test } from 'node:test';
import {
  buildCreateArgs, buildInitArgs, buildPolicy, createDaoCommand, decodeCreateArgs, deployOwnedCommand,
} from './lib.mjs';

const council = ['alice.near', 'bob.near', 'carol.near'];

test('policy: one hour proposal period in nanoseconds and the demo council', () => {
  const policy = buildPolicy(council);
  assert.equal(policy.proposal_period, '3600000000000');
  assert.deepEqual(policy.roles.find((r) => r.name === 'council').kind, { Group: council });
  assert.ok(policy.roles.find((r) => r.name === 'council').permissions.includes('*:VoteApprove'));
});

test('create args decode back to the same config and policy', () => {
  const createArgs = buildCreateArgs({ name: 'x207', council });
  assert.equal(createArgs.name, 'x207');
  const decoded = decodeCreateArgs(createArgs);
  assert.equal(decoded.config.name, 'x207');
  assert.deepEqual(decoded.policy, buildPolicy(council));
});

test('duplicate council members collapse, empty council and bad ids fail', () => {
  assert.deepEqual(buildPolicy(['a.near', 'a.near']).roles[1].kind, { Group: ['a.near'] });
  assert.throws(() => buildPolicy([]), /at least one member/);
  assert.throws(() => buildPolicy(['Bad Id']), /invalid NEAR account id/);
  assert.throws(() => buildCreateArgs({ name: 'x', council }), /invalid NEAR account id/);
});

test('init args: owner is the DAO, params null, circle id validated', () => {
  const init = buildInitArgs({ daoAccount: 'x207.sputnik-dao.near', circleId: 'agents-main' });
  assert.deepEqual(init, {
    owner_id: 'x207.sputnik-dao.near', circle_id: 'agents-main',
    usdt_token_id: 'usdt.tether-token.near', intents_id: 'intents.near', params: null,
  });
  assert.throws(() => buildInitArgs({ daoAccount: 'x207.sputnik-dao.near', circleId: 'agents.main' }), /invalid circle_id/);
  assert.throws(() => buildInitArgs({ daoAccount: 'x207.sputnik-dao.near', circleId: 'a'.repeat(33) }), /invalid circle_id/);
});

test('commands name the factory, method, network and never carry a key', () => {
  const createArgs = buildCreateArgs({ name: 'x207', council });
  const create = createDaoCommand({ signer: 'alice.near', network: 'testnet', createArgs });
  assert.match(create, /^near contract call-function as-transaction sputnikv2\.testnet create json-args /);
  assert.match(create, /prepaid-gas '300 Tgas' attached-deposit '7 NEAR' sign-as alice\.near/);
  const main = createDaoCommand({ signer: 'alice.near', network: 'mainnet', createArgs });
  assert.match(main, /as-transaction sputnik-dao\.near create .* attached-deposit '1 NEAR'/);
  assert.match(create, /network-config testnet sign-with-keychain send$/);
  const deploy = deployOwnedCommand({
    contract: 'x207-circle.near', wasm: 'target/near/x207_circle.wasm', network: 'testnet',
    initArgs: buildInitArgs({ daoAccount: 'x207.sputnik-dao.near', circleId: 'agents-main' }),
  });
  assert.match(deploy, /with-init-call new json-args /);
  assert.doesNotMatch(create + deploy, /private|seed|ed25519:/i);
});

test('CLI prints a command and rejects a missing flag', () => {
  const ok = execFileSync('node', ['dao/create-dao.mjs', '--name', 'x207', '--council', 'alice.near', '--signer', 'alice.near']).toString();
  assert.match(ok, /network-config testnet/);
  const bad = spawnSync('node', ['dao/create-dao.mjs', '--name', 'x207']);
  assert.equal(bad.status, 1);
  assert.match(bad.stderr.toString(), /missing --council/);
  const net = spawnSync('node', ['dao/deploy-owned.mjs', '--contract', 'c.near', '--dao', 'd.near', '--circle', 'a', '--network', 'foo']);
  assert.equal(net.status, 1);
});

// --- CT-046 ---------------------------------------------------------------------------------
import { addProposalCommand, buildAddProposalArgs, decodeProposalParams, getParamsCommand, parseParamsJson } from './lib.mjs';

const goodParams = {
  epoch_length: 3600, payment_window: 1800, proposal_window: 600, base_limit_ratio: 8000, limit_growth: 1000,
  limit_cap: '1000000000', limit_decay: 0, fees: { clearing_fee_bps: 0, solver_fee_bps: 0 }, admission: 'open',
};

test('proposal: FunctionCall to set_params whose args decode back to the input', () => {
  const args = buildAddProposalArgs({ contract: 'x207-circle.near', params: goodParams });
  const call = args.proposal.kind.FunctionCall;
  assert.equal(call.receiver_id, 'x207-circle.near');
  assert.equal(call.actions.length, 1);
  assert.equal(call.actions[0].method_name, 'set_params');
  assert.equal(call.actions[0].deposit, '0');
  assert.deepEqual(decodeProposalParams(args), goodParams);
  assert.deepEqual(JSON.parse(Buffer.from(call.actions[0].args, 'base64')), { params: goodParams });
});

test('proposal: out-of-range params still build (the contract rejects them at execution)', () => {
  const args = buildAddProposalArgs({ contract: 'x207-circle.near', params: { ...goodParams, payment_window: 899 } });
  assert.equal(decodeProposalParams(args).payment_window, 899);
});

test('params JSON: broken text and non-objects are rejected', () => {
  assert.throws(() => parseParamsJson('{nope'), /not valid JSON/);
  assert.throws(() => parseParamsJson('[1]'), /must be a JSON object/);
  assert.throws(() => parseParamsJson('null'), /must be a JSON object/);
  assert.deepEqual(parseParamsJson(JSON.stringify(goodParams)), goodParams);
});

test('commands: add_proposal carries the bond, get_params is read-only', () => {
  const args = buildAddProposalArgs({ contract: 'x207-circle.near', params: goodParams });
  const add = addProposalCommand({ dao: 'x207.sputnik-dao.near', signer: 'alice.near', network: 'testnet', addProposalArgs: args });
  assert.match(add, /call-function as-transaction x207\.sputnik-dao\.near add_proposal json-args /);
  assert.match(add, /attached-deposit '0\.1 NEAR'/);
  const view = getParamsCommand({ contract: 'x207-circle.near', network: 'testnet' });
  assert.equal(view, "near contract call-function as-read-only x207-circle.near get_params json-args '{}' network-config testnet now");
});

test('CLI: broken params JSON exits non-zero and prints no command', () => {
  const bad = spawnSync('node', ['dao/propose-set-params.mjs', '--dao', 'd.near', '--contract', 'c.near', '--signer', 's.near', '--params', '{nope']);
  assert.equal(bad.status, 1);
  assert.equal(bad.stdout.toString(), '');
  assert.match(bad.stderr.toString(), /not valid JSON/);
  const ok = execFileSync('node', ['dao/propose-set-params.mjs', '--dao', 'd.near', '--contract', 'c.near', '--signer', 's.near', '--params', JSON.stringify(goodParams)]).toString();
  assert.match(ok, /add_proposal/);
  const view = execFileSync('node', ['dao/print-params.mjs', '--contract', 'c.near']).toString();
  assert.match(view, /get_params/);
});

// --- CT-047 ---------------------------------------------------------------------------------
import {
  actProposalCommand, buildActProposalArgs, getProposalCommand, proposalStatus,
} from './lib.mjs';

test('vote args: default VoteApprove, other actions checked, id validated', () => {
  assert.deepEqual(buildActProposalArgs({ id: '7' }), { id: 7, action: 'VoteApprove' });
  assert.deepEqual(buildActProposalArgs({ id: 0, action: 'VoteReject' }), { id: 0, action: 'VoteReject' });
  assert.throws(() => buildActProposalArgs({ id: 1, action: 'Delete' }), /action must be one of/);
  assert.throws(() => buildActProposalArgs({ id: -1 }), /invalid proposal id/);
  assert.throws(() => buildActProposalArgs({ id: 'x' }), /invalid proposal id/);
});

test('vote and status commands', () => {
  const vote = actProposalCommand({ dao: 'x207.sputnik-dao.near', signer: 'alice.near', network: 'testnet', actArgs: buildActProposalArgs({ id: 3 }) });
  assert.match(vote, /as-transaction x207\.sputnik-dao\.near act_proposal json-args '\{"id":3,"action":"VoteApprove"\}'/);
  const view = getProposalCommand({ dao: 'x207.sputnik-dao.near', id: 3, network: 'testnet' });
  assert.equal(view, `near contract call-function as-read-only x207.sputnik-dao.near get_proposal json-args '{"id":3}' network-config testnet now`);
});

test('status reader: InProgress then Approved, garbage rejected', () => {
  assert.equal(proposalStatus({ id: 0, status: 'InProgress' }), 'InProgress');
  assert.equal(proposalStatus({ id: 0, status: 'Approved' }), 'Approved');
  assert.throws(() => proposalStatus({}), /missing status/);
  assert.throws(() => proposalStatus(null), /missing status/);
});

test('CLI: vote prints a command, status reads a file and stdin', () => {
  const vote = execFileSync('node', ['dao/vote-proposal.mjs', '--dao', 'd.near', '--id', '2', '--signer', 's.near']).toString();
  assert.match(vote, /act_proposal/);
  const bad = spawnSync('node', ['dao/vote-proposal.mjs', '--dao', 'd.near', '--id', '2', '--signer', 's.near', '--action', 'Nope']);
  assert.equal(bad.status, 1);
  const status = execFileSync('node', ['dao/proposal-status.mjs', '--from-json', '-'], { input: '{"status":"Approved"}' }).toString();
  assert.equal(status.trim(), 'Approved');
  assert.match(execFileSync('node', ['dao/proposal-status.mjs', '--dao', 'd.near', '--id', '2']).toString(), /get_proposal/);
});

// CT-056
import { buildAdmitProposalArgs } from './lib.mjs';

test('admit proposal: FunctionCall admit_member with account_id in base64 args', () => {
  const args = buildAdmitProposalArgs({ contract: 'x207-circle.near', account: 'newagent.near' });
  const call = args.proposal.kind.FunctionCall;
  assert.equal(call.receiver_id, 'x207-circle.near');
  assert.equal(call.actions[0].method_name, 'admit_member');
  assert.deepEqual(JSON.parse(Buffer.from(call.actions[0].args, 'base64').toString('utf8')), { account_id: 'newagent.near' });
  assert.throws(() => buildAdmitProposalArgs({ contract: 'x207-circle.near', account: 'Not An Account' }), /invalid NEAR account id/);
});

test('propose-admit-member prints the add_proposal command and refuses a missing account', () => {
  const out = execFileSync('node', ['dao/propose-admit-member.mjs', '--dao', 'x207.sputnik-dao.testnet', '--contract', 'x207-circle.testnet', '--signer', 'a.testnet', '--account', 'newagent.testnet'], { encoding: 'utf8' });
  assert.match(out, /add_proposal/);
  assert.match(out, /admit_member|YWNjb3VudF9pZ/);
  const bad = spawnSync('node', ['dao/propose-admit-member.mjs', '--dao', 'x207.sputnik-dao.testnet', '--contract', 'c.testnet', '--signer', 'a.testnet']);
  assert.notEqual(bad.status, 0);
});

// CT-074
import { buildAssetClassProposalArgs } from './lib.mjs';

test('asset class proposal: FunctionCall set_asset_class with asset_type and entry in base64 args', () => {
  const termsHash = 'ab'.repeat(32);
  const args = buildAssetClassProposalArgs({ contract: 'x207-circle.near', assetType: 'near-rpc:access', assetClass: 'B', deliveryMethods: ['near_access_key'], acceptedProofs: ['zktls'], termsHash });
  const call = args.proposal.kind.FunctionCall;
  assert.equal(call.receiver_id, 'x207-circle.near');
  assert.equal(call.actions[0].method_name, 'set_asset_class');
  assert.deepEqual(JSON.parse(Buffer.from(call.actions[0].args, 'base64').toString('utf8')), {
    asset_type: 'near-rpc:access',
    entry: { asset_type: 'near-rpc:access', class: 'B', delivery_methods: ['near_access_key'], accepted_proofs: ['zktls'], terms_hash: termsHash },
  });
  const base = { contract: 'x207-circle.near', assetType: 'a:b', assetClass: 'B', termsHash };
  assert.throws(() => buildAssetClassProposalArgs({ ...base, assetType: 'nocolon' }), /<provider>:<kind>/);
  assert.throws(() => buildAssetClassProposalArgs({ ...base, assetClass: 'Z' }), /class must be/);
  assert.throws(() => buildAssetClassProposalArgs({ ...base, acceptedProofs: ['telepathy'] }), /unknown proof kind/);
  assert.throws(() => buildAssetClassProposalArgs({ ...base, termsHash: 'AB' }), /terms hash/);
});

test('propose-asset-class prints the add_proposal command and refuses a missing class', () => {
  const common = ['dao/propose-asset-class.mjs', '--dao', 'x207.sputnik-dao.testnet', '--contract', 'x207-circle.testnet', '--signer', 'a.testnet', '--asset-type', 'near-rpc:access', '--terms-hash', 'ab'.repeat(32)];
  const out = execFileSync('node', [...common, '--class', 'B', '--delivery', 'near_access_key'], { encoding: 'utf8' });
  assert.match(out, /add_proposal/);
  assert.match(out, /set_asset_class|c2V0X2Fzc2V0X2NsYXNz|YXNzZXRfdHlwZ/);
  assert.notEqual(spawnSync('node', common).status, 0);
});

test('create-dao CLI: --factory and --deposit need a string value and take effect', () => {
  const base = ['dao/create-dao.mjs', '--name', 'x207', '--council', 'alice.near', '--signer', 'alice.near', '--network', 'mainnet'];
  const ok = execFileSync('node', [...base, '--factory', 'my-factory.near', '--deposit', '2 NEAR']).toString();
  assert.match(ok, /as-transaction my-factory\.near create /);
  assert.match(ok, /attached-deposit '2 NEAR'/);
  for (const flag of ['--deposit', '--factory']) {
    const bad = spawnSync('node', [...base, flag]);
    assert.equal(bad.status, 1);
    assert.match(bad.stderr.toString(), new RegExp(`${flag} needs a value`));
  }
  const odd = spawnSync('node', [...base, '--deposit', 'lots']);
  assert.equal(odd.status, 1);
  assert.match(odd.stderr.toString(), /--deposit must look like/);
});
