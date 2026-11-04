// CT-050 preparation: the pure part of `mainnet-set-params.mjs`. Nothing here talks to a network, signs or sends.
import { PROPOSAL_BOND, assertAccountId } from './lib.mjs';

const ONE_NEAR = 10n ** 24n;

/** The keys of `Params` as `get_params` shows them: a change of any of them, and what it is. Nested objects (`fees`) are compared by value, key by key. */
export function diffParams(current, next) {
  const changes = [];
  const flat = (o, prefix = '') => Object.entries(o).flatMap(([k, v]) => (v !== null && typeof v === 'object' && !Array.isArray(v) ? flat(v, `${prefix}${k}.`) : [[`${prefix}${k}`, v]]));
  const a = new Map(flat(current));
  const b = new Map(flat(next));
  for (const [key, value] of b) {
    if (!a.has(key)) changes.push({ key, from: undefined, to: value, kind: 'unknown_key' });
    else if (JSON.stringify(a.get(key)) !== JSON.stringify(value)) changes.push({ key, from: a.get(key), to: value, kind: 'changed' });
  }
  for (const key of a.keys()) if (!b.has(key)) changes.push({ key, from: a.get(key), to: undefined, kind: 'missing_key' });
  return changes;
}

/** The roles of a Sputnik policy that let `account` do `action` ("AddProposal", "VoteApprove"). `Everyone` counts for any account. */
export function rolesAllowing(policy, account, action) {
  return (policy.roles ?? []).filter((role) => {
    const member = role.kind === 'Everyone' || (role.kind?.Group ?? []).includes(account);
    return member && (role.permissions ?? []).some((p) => p === `*:${action}` || p === `*:*` || p === `call:${action}`);
  }).map((r) => r.name);
}

/** How many council votes approve a proposal at the default vote policy (`RoleWeight`, `threshold` [n, d]): more than n/d of the group. */
export function approvalsNeeded(policy) {
  const council = (policy.roles ?? []).find((r) => Array.isArray(r.kind?.Group) && (r.permissions ?? []).includes('*:VoteApprove'));
  if (!council) throw new Error('the DAO policy has no group that can vote');
  const [n, d] = policy.default_vote_policy?.threshold ?? [1, 2];
  const size = council.kind.Group.length;
  return { council: council.kind.Group, size, needed: Math.floor((size * n) / d) + 1 };
}

const yocto = (value) => `${(BigInt(value) * 10000n / ONE_NEAR) / 10000n}.${String((BigInt(value) * 10000n / ONE_NEAR) % 10000n).padStart(4, '0')} NEAR`;

/**
 * What a dry run says before anything is sent. `state` is what was read from the chain: `owner`, `interfaceVersion`, `current` (get_params), `policy`, `lastProposalId`.
 * `problems` stop the plan (nothing to send); `warnings` are for the human to read.
 */
export function planSetParams({ dao, contract, signer, next, state }) {
  assertAccountId(dao);
  assertAccountId(contract);
  assertAccountId(signer);
  const problems = [];
  const warnings = [];
  if (state.owner !== dao) problems.push(`the owner of ${contract} is ${state.owner}, not ${dao}: a proposal of this DAO could not call set_params`);
  const changes = diffParams(state.current, next);
  for (const c of changes) {
    if (c.kind === 'unknown_key') problems.push(`${c.key} is not a key of the Params of this contract (${state.interfaceVersion})`);
    if (c.kind === 'missing_key') problems.push(`${c.key} is missing: set_params takes the whole Params, and the proposal would drop it`);
  }
  if (changes.filter((c) => c.kind === 'changed').length === 0 && problems.length === 0) problems.push('nothing to propose: the params are the ones that are active now');
  const canAdd = rolesAllowing(state.policy, signer, 'AddProposal');
  if (canAdd.length === 0) problems.push(`${signer} may not add proposals to ${dao}`);
  let approvals = null;
  try {
    approvals = approvalsNeeded(state.policy);
  } catch (error) {
    problems.push(error.message);
  }
  if (approvals && !approvals.council.includes(signer)) warnings.push(`${signer} is not a council member: it can add the proposal, but the council has to vote it (${approvals.needed} of ${approvals.size})`);
  const bond = BigInt(state.policy.proposal_bond ?? '0');
  const periodH = Number(BigInt(state.policy.proposal_period ?? '0') / 3_600_000_000_000n);
  if (periodH < 1) warnings.push('the proposal period is under one hour');
  return {
    problems, warnings, changes: changes.filter((c) => c.kind === 'changed'),
    expectedProposalId: Number(state.lastProposalId),
    approvals, bondNear: yocto(bond), periodHours: periodH,
    note: `${PROPOSAL_BOND} is the bond the command attaches (it comes back when the proposal is finalized); the DAO policy says ${yocto(bond)}`,
  };
}
