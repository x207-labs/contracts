// Builders for the Sputnik DAO that owns `x207-circle` (ADR-20, ADR-25). Pure functions: nothing
// here signs or sends a transaction.

export const DEFAULT_FACTORY = 'sputnik-dao.near';
/** ADR-25: the demo DAO votes for one hour. */
export const PROPOSAL_PERIOD_SEC = 3600;
export const NS_PER_SEC = 1_000_000_000n;
const ONE_NEAR = 10n ** 24n;
/** The factory `create` call needs about 6 NEAR for the new account; attach a margin. */
export const FACTORY_DEPOSIT = '7 NEAR';

const ACCOUNT_ID = /^(([a-z\d]+[-_])*[a-z\d]+\.)*([a-z\d]+[-_])*[a-z\d]+$/;

export function assertAccountId(id) {
  if (typeof id !== 'string' || id.length < 2 || id.length > 64 || !ACCOUNT_ID.test(id)) {
    throw new Error(`invalid NEAR account id: ${JSON.stringify(id)}`);
  }
  return id;
}

/** Sputnik DAO v2 policy: council group may vote, anyone may add a proposal, simple majority. */
export function buildPolicy(council, proposalPeriodSec = PROPOSAL_PERIOD_SEC) {
  const members = [...new Set(council.map(assertAccountId))];
  if (members.length === 0) throw new Error('council needs at least one member');
  if (!Number.isInteger(proposalPeriodSec) || proposalPeriodSec <= 0) throw new Error('proposalPeriodSec must be a positive integer');
  return {
    roles: [
      { name: 'all', kind: 'Everyone', permissions: ['*:AddProposal'], vote_policy: {} },
      {
        name: 'council',
        kind: { Group: members },
        permissions: ['*:Finalize', '*:AddProposal', '*:VoteApprove', '*:VoteReject', '*:VoteRemove'],
        vote_policy: {},
      },
    ],
    default_vote_policy: { weight_kind: 'RoleWeight', quorum: '0', threshold: [1, 2] },
    proposal_bond: (ONE_NEAR / 10n).toString(),
    proposal_period: (BigInt(proposalPeriodSec) * NS_PER_SEC).toString(),
    bounty_bond: (ONE_NEAR / 10n).toString(),
    bounty_forgiveness_period: (7n * 24n * 3600n * NS_PER_SEC).toString(),
  };
}

/** JSON arguments of the factory method `create` (`args` is the base64 of the DAO init arguments). */
export function buildCreateArgs({ name, council, purpose = 'x207 circle governance', proposalPeriodSec = PROPOSAL_PERIOD_SEC }) {
  assertAccountId(name);
  const daoArgs = {
    config: { name, purpose, metadata: '' },
    policy: buildPolicy(council, proposalPeriodSec),
  };
  return { name, args: Buffer.from(JSON.stringify(daoArgs)).toString('base64') };
}

export function decodeCreateArgs(createArgs) {
  return JSON.parse(Buffer.from(createArgs.args, 'base64').toString('utf8'));
}

/** `new` arguments of `x207-circle` (spec section 4): `params` null means ADR-25 defaults. */
export function buildInitArgs({ daoAccount, circleId, usdtTokenId = 'usdt.tether-token.near', intentsId = 'intents.near' }) {
  assertAccountId(daoAccount);
  assertAccountId(usdtTokenId);
  assertAccountId(intentsId);
  if (!/^[-_a-zA-Z0-9]{1,32}$/.test(circleId)) throw new Error(`invalid circle_id: ${circleId}`);
  return { owner_id: daoAccount, circle_id: circleId, usdt_token_id: usdtTokenId, intents_id: intentsId, params: null };
}

const sh = (s) => `'${s.replaceAll("'", "'\\''")}'`;

/** near-cli-rs command that creates the DAO through the factory. Printed, never run here. */
export function createDaoCommand({ factory = DEFAULT_FACTORY, signer, network, createArgs }) {
  assertAccountId(signer);
  return [
    'near contract call-function as-transaction', factory, 'create',
    'json-args', sh(JSON.stringify(createArgs)),
    'prepaid-gas', sh('100 Tgas'), 'attached-deposit', sh(FACTORY_DEPOSIT),
    'sign-as', signer, 'network-config', network, 'sign-with-keychain', 'send',
  ].join(' ');
}

/** near-cli-rs command that deploys the wasm with `new(owner_id = DAO)`. The owner is fixed at init. */
export function deployOwnedCommand({ contract, wasm, network, initArgs }) {
  assertAccountId(contract);
  return [
    'near contract deploy', contract, 'use-file', sh(wasm),
    'with-init-call', 'new', 'json-args', sh(JSON.stringify(initArgs)),
    'prepaid-gas', sh('30 Tgas'), 'attached-deposit', sh('0 NEAR'),
    'network-config', network, 'sign-with-keychain', 'send',
  ].join(' ');
}

/** `mainnet` is only printed with an explicit flag; everything else defaults to testnet. */
export function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 1) {
    if (!argv[i].startsWith('--')) throw new Error(`unexpected argument: ${argv[i]}`);
    const key = argv[i].slice(2);
    const next = argv[i + 1];
    if (next === undefined || next.startsWith('--')) { out[key] = true; } else { out[key] = next; i += 1; }
  }
  return out;
}

export function networkOf(args) {
  const network = args.network ?? 'testnet';
  if (!['testnet', 'mainnet'].includes(network)) throw new Error(`--network must be testnet or mainnet, got ${network}`);
  return network;
}

// --- CT-046: proposal that calls `set_params` on the circle -------------------------------------

/** Gas the DAO attaches to the `set_params` call (a small state write). */
export const SET_PARAMS_GAS = '30000000000000';
/** Sputnik default proposal bond: 0.1 NEAR, attached to `add_proposal`. */
export const PROPOSAL_BOND = '0.1 NEAR';

/** `Params` is whatever JSON the owner passes; range checks stay with the contract (`validate_params`). */
export function parseParamsJson(text) {
  let params;
  try {
    params = JSON.parse(text);
  } catch (error) {
    throw new Error(`params is not valid JSON: ${error.message}`);
  }
  if (params === null || typeof params !== 'object' || Array.isArray(params)) {
    throw new Error('params must be a JSON object');
  }
  return params;
}

/** Arguments of Sputnik `add_proposal`: one FunctionCall action to `set_params({ params })`. */
export function buildAddProposalArgs({ contract, params, description = 'Set x207 circle params' }) {
  assertAccountId(contract);
  const args = Buffer.from(JSON.stringify({ params })).toString('base64');
  return {
    proposal: {
      description,
      kind: {
        FunctionCall: {
          receiver_id: contract,
          actions: [{ method_name: 'set_params', args, deposit: '0', gas: SET_PARAMS_GAS }],
        },
      },
    },
  };
}

/** The `params` object back from the base64 args of a proposal built by `buildAddProposalArgs`. */
export function decodeProposalParams(addProposalArgs) {
  const action = addProposalArgs.proposal.kind.FunctionCall.actions[0];
  return JSON.parse(Buffer.from(action.args, 'base64').toString('utf8')).params;
}

/** Gas the DAO attaches to the `admit_member` call. */
export const ADMIT_MEMBER_GAS = '30000000000000';

/** Arguments of Sputnik `add_proposal`: one FunctionCall action to `admit_member({ account_id })` (CT-056). */
export function buildAdmitProposalArgs({ contract, account, description }) {
  assertAccountId(contract);
  assertAccountId(account);
  const args = Buffer.from(JSON.stringify({ account_id: account })).toString('base64');
  return {
    proposal: {
      description: description ?? `Admit ${account} to the x207 circle`,
      kind: {
        FunctionCall: {
          receiver_id: contract,
          actions: [{ method_name: 'admit_member', args, deposit: '0', gas: ADMIT_MEMBER_GAS }],
        },
      },
    },
  };
}

export function addProposalCommand({ dao, signer, network, addProposalArgs }) {
  assertAccountId(dao);
  assertAccountId(signer);
  return [
    'near contract call-function as-transaction', dao, 'add_proposal',
    'json-args', sh(JSON.stringify(addProposalArgs)),
    'prepaid-gas', sh('100 Tgas'), 'attached-deposit', sh(PROPOSAL_BOND),
    'sign-as', signer, 'network-config', network, 'sign-with-keychain', 'send',
  ].join(' ');
}

/** Read-only: prints the active parameters, the same JSON as the view `get_params`. */
export function getParamsCommand({ contract, network }) {
  assertAccountId(contract);
  return [
    'near contract call-function as-read-only', contract, 'get_params',
    'json-args', "'{}'", 'network-config', network, 'now',
  ].join(' ');
}

// --- CT-047: voting and reading a proposal -------------------------------------------------------

export const VOTE_ACTIONS = ['VoteApprove', 'VoteReject', 'VoteRemove', 'Finalize'];

function assertProposalId(id) {
  const n = Number(id);
  if (!Number.isInteger(n) || n < 0) throw new Error(`invalid proposal id: ${JSON.stringify(id)}`);
  return n;
}

/** Arguments of Sputnik `act_proposal`. Only council members may vote; the DAO enforces it. */
export function buildActProposalArgs({ id, action = 'VoteApprove' }) {
  if (!VOTE_ACTIONS.includes(action)) throw new Error(`action must be one of ${VOTE_ACTIONS.join(', ')}, got ${action}`);
  return { id: assertProposalId(id), action };
}

export function actProposalCommand({ dao, signer, network, actArgs }) {
  assertAccountId(dao);
  assertAccountId(signer);
  return [
    'near contract call-function as-transaction', dao, 'act_proposal',
    'json-args', sh(JSON.stringify(actArgs)),
    'prepaid-gas', sh('200 Tgas'), 'attached-deposit', sh('0 NEAR'),
    'sign-as', signer, 'network-config', network, 'sign-with-keychain', 'send',
  ].join(' ');
}

export function getProposalCommand({ dao, id, network }) {
  assertAccountId(dao);
  return [
    'near contract call-function as-read-only', dao, 'get_proposal',
    'json-args', sh(JSON.stringify({ id: assertProposalId(id) })), 'network-config', network, 'now',
  ].join(' ');
}

/** Status of a `get_proposal` result (`InProgress`, `Approved`, `Rejected`, `Removed`, `Expired`, `Moved`, `Failed`). */
export function proposalStatus(proposal) {
  if (proposal === null || typeof proposal !== 'object' || typeof proposal.status !== 'string') {
    throw new Error('not a proposal: missing status');
  }
  return proposal.status;
}
