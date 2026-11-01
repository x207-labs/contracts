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
