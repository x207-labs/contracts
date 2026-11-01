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
  assert.match(create, /^near contract call-function as-transaction sputnik-dao\.near create json-args /);
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
