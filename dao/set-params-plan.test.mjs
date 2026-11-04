// Run: node --test dao/
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { approvalsNeeded, diffParams, planSetParams, rolesAllowing } from './set-params-plan.mjs';

const current = { epoch_length: 600, limit_decay: 0, fees: { clearing_fee_bps: 10, solver_fee_bps: 5 }, admission: 'open' };
const policy = {
  roles: [
    { name: 'all', kind: 'Everyone', permissions: ['*:AddProposal'] },
    { name: 'council', kind: { Group: ['a.near', 'b.near', 'c.near'] }, permissions: ['*:Finalize', '*:AddProposal', '*:VoteApprove'] },
  ],
  default_vote_policy: { weight_kind: 'RoleWeight', quorum: '0', threshold: [1, 2] },
  proposal_bond: '100000000000000000000000', proposal_period: '3600000000000',
};
const state = { owner: 'dao.near', interfaceVersion: '0.0.13', current, policy, lastProposalId: 7 };
const plan = (next, extra = {}) => planSetParams({ dao: 'dao.near', contract: 'circle.near', signer: 'a.near', next, state: { ...state, ...extra } });

test('diff: a changed value, a nested value, an unknown and a missing key', () => {
  assert.deepEqual(diffParams(current, { ...current, limit_decay: 50 }).map((c) => [c.key, c.kind]), [['limit_decay', 'changed']]);
  assert.deepEqual(diffParams(current, { ...current, fees: { clearing_fee_bps: 12, solver_fee_bps: 5 } }).map((c) => c.key), ['fees.clearing_fee_bps']);
  assert.deepEqual(diffParams(current, { ...current, extra: 1 }).map((c) => c.kind), ['unknown_key']);
  const { admission, ...without } = current;
  assert.deepEqual(diffParams(current, without).map((c) => [c.key, c.kind]), [['admission', 'missing_key']]);
});

test('plan: a good change names the next proposal id, the bond and 2 of 3 votes; nothing else is a problem', () => {
  const p = plan({ ...current, limit_decay: 50 });
  assert.deepEqual(p.problems, []);
  assert.equal(p.expectedProposalId, 7);
  assert.equal(p.approvals.needed, 2);
  assert.equal(p.bondNear, '0.1000 NEAR');
  assert.equal(p.changes.length, 1);
});

test('plan: the problems that stop it (wrong owner, no change, a dropped key, a signer that may not add)', () => {
  assert.match(plan({ ...current, limit_decay: 50 }, { owner: 'someone.near' }).problems[0], /owner of circle.near is someone.near/);
  assert.deepEqual(plan(current).problems, ['nothing to propose: the params are the ones that are active now']);
  const { admission, ...without } = current;
  assert.match(plan({ ...without, limit_decay: 1 }).problems.join(), /admission is missing/);
  const closed = { ...policy, roles: [policy.roles[1]] };
  const stranger = planSetParams({ dao: 'dao.near', contract: 'circle.near', signer: 'z.near', next: { ...current, limit_decay: 50 }, state: { ...state, policy: closed } });
  assert.match(stranger.problems[0], /z.near may not add proposals/);
});

test('roles and votes: Everyone may add, only the council votes, a simple majority of the group', () => {
  assert.deepEqual(rolesAllowing(policy, 'z.near', 'AddProposal'), ['all']);
  assert.deepEqual(rolesAllowing(policy, 'a.near', 'VoteApprove'), ['council']);
  assert.deepEqual(rolesAllowing(policy, 'z.near', 'VoteApprove'), []);
  assert.equal(approvalsNeeded({ ...policy, roles: [policy.roles[1]] }).needed, 2);
  assert.equal(approvalsNeeded({ ...policy, roles: [{ ...policy.roles[1], kind: { Group: ['a.near', 'b.near'] } }] }).needed, 2);
});

test('the script has no sending mode and needs no network for its refusals', () => {
  const script = fileURLToPath(new URL('./mainnet-set-params.mjs', import.meta.url));
  const run = spawnSync(process.execPath, [script, '--no-dry-run', '--dao', 'a.near', '--contract', 'b.near', '--signer', 'c.near', '--params', '{}'], { encoding: 'utf8' });
  assert.equal(run.status, 1);
  assert.match(run.stderr, /there is no sending mode/);
});
