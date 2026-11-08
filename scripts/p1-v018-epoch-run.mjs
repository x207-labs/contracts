#!/usr/bin/env node
// P1 (UM-076): one test epoch on x207-v018.testnet (interface 0.0.18) with the two account sales of the plan:
//   #11  a plain NEAR account is sold through the locker: lock (locker deployed, key deleted), SDK pre-pay check, buyer pays with a voucher, buyer holds the only key;
//   #14  a member account is sold: the lock is accepted, the purchase is refused until the epochs before the current one are finalized, then it is delivered and the member voucher key rotates.
// Testnet only. Keys live in <keys-dir>/p1-v018-keys.json (git-ignored, never printed); the result file holds hashes, account names and contract answers only.
//   node scripts/p1-v018-epoch-run.mjs --keys-dir <dir> --wasm-dir <dir with x207_locker.wasm> --out docs/testnet-epochs/p1-v018-run.json
// Safe to repeat: finished steps are kept in the --out file and skipped.
import { existsSync, mkdirSync, readFileSync, writeFileSync, chmodSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const sdkRoot = resolve(here, "../../sdk-p1");
const req = createRequire(resolve(sdkRoot, "package.json"));
const { Account, JsonRpcProvider, KeyPair, KeyPairSigner, actions, parseNearAmount } = req("near-api-js");
const { signVoucher, checkAccountLockBeforePay } = await import(resolve(sdkRoot, "packages/core/dist/index.js"));

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  if (i >= 0 && process.argv[i + 1]) return process.argv[i + 1];
  if (fallback !== undefined) return fallback;
  throw new Error(`--${name} is required`);
};
const keysDir = resolve(arg("keys-dir"));
const wasmDir = resolve(arg("wasm-dir"));
const outFile = resolve(arg("out"));
const CIRCLE = "x207-v018.testnet";
const OWNER = "x207-v18-own.testnet";
const P2 = "x207-v18-p2.testnet";
const USDT = "usdt.x207-deployer.testnet";
const CREDIT = 1_000_000n;
const PRICE = 10n * CREDIT;
const NAMES = { alice: `alice.${OWNER}`, bob: `bob.${OWNER}`, sold: `sold.${P2}`, dan: `dan.${P2}` };
const RPC = "https://rpc.testnet.fastnear.com";
const provider = new JsonRpcProvider({ url: RPC });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const lockerWasm = readFileSync(resolve(wasmDir, "x207_locker.wasm"));

// ---- secrets: one file, created on first run
const keysFile = resolve(keysDir, "p1-v018-keys.json");
const secrets = existsSync(keysFile) ? JSON.parse(readFileSync(keysFile, "utf8")) : {};
const keyOf = (name, kind) => {
  secrets[name] ??= {};
  secrets[name][kind] ??= KeyPair.fromRandom("ed25519").toString();
  writeFileSync(keysFile, JSON.stringify(secrets));
  chmodSync(keysFile, 0o600);
  return KeyPair.fromString(secrets[name][kind]);
};
const fileKey = (account) => KeyPair.fromString(JSON.parse(readFileSync(resolve(keysDir, "testnet", `${account}.json`), "utf8")).secret_key);
const accountWith = (id, keyPair) => new Account(id, provider, new KeyPairSigner(keyPair));
const ownerAcc = accountWith(OWNER, fileKey(OWNER));
const p2Acc = accountWith(P2, fileKey(P2));
const accessKey = (name) => keyOf(name, "access");
const voucherKey = (name) => keyOf(name, "voucher");
const acc = (name) => accountWith(NAMES[name], accessKey(name));

// ---- result file
const out = existsSync(outFile) ? JSON.parse(readFileSync(outFile, "utf8")) : { circle: CIRCLE, owner: OWNER, usdt: USDT, accounts: NAMES, steps: [], checks: {} };
mkdirSync(dirname(outFile), { recursive: true });
const save = () => writeFileSync(outFile, JSON.stringify(out, null, 2));
const done = (label) => out.steps.find((s) => s.label === label && s.ok === s.expectOk);

// ---- chain access
async function rpc(method, params) {
  for (let i = 0; ; i++) {
    try {
      const r = await fetch(RPC, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }) });
      const b = await r.json();
      if (b.error) throw new Error(JSON.stringify(b.error));
      return b.result;
    } catch (e) {
      if (i >= 6) throw e;
      await sleep(3000 * (i + 1));
    }
  }
}
const view = async (method, args = {}, account = CIRCLE) =>
  JSON.parse(Buffer.from((await rpc("query", { request_type: "call_function", finality: "optimistic", account_id: account, method_name: method, args_base64: Buffer.from(JSON.stringify(args)).toString("base64") })).result).toString("utf8"));
const chainMs = async () => Number(BigInt((await rpc("block", { finality: "optimistic" })).header.timestamp_nanosec) / 1_000_000n);
const waitUntilMs = async (targetMs, why) => {
  for (;;) {
    const now = await chainMs();
    if (now >= targetMs) return;
    const wait = Math.min(targetMs - now + 2000, 60_000);
    console.log(`waiting ${Math.ceil((targetMs - now) / 1000)}s for ${why}`);
    await sleep(wait);
  }
};
const nsToMs = (ns) => Number(BigInt(ns) / 1_000_000n);

function failureText(outcome) {
  const found = [outcome.status, ...(outcome.receipts_outcome ?? []).map((o) => o.outcome.status)].find((s) => s && typeof s === "object" && "Failure" in s);
  if (!found) return null;
  const text = JSON.stringify(found);
  const m = text.match(/"ExecutionError":"((?:[^"\\]|\\.)*)"/) ?? text.match(/"FunctionCallError":\{"[A-Za-z]+":"((?:[^"\\]|\\.)*)"/);
  return m ? m[1].replace(/\\"/g, '"') : text.slice(0, 300);
}
const valueOf = (outcome) => {
  const v = outcome.status?.SuccessValue;
  return v ? JSON.parse(Buffer.from(v, "base64").toString("utf8")) : null;
};

/** Sends one transaction (or one batch) and records it; `expectFail` steps must fail. */
async function send(label, account, receiver, call, { expectFail = false, force = false } = {}) {
  const previous = out.steps.find((s) => s.label === label && s.ok === true);
  if (previous && !force) {
    console.log(`${label}: already done (${previous.tx})`);
    return previous;
  }
  let outcome;
  try {
    if (call.batch) outcome = await account.signAndSendTransaction({ receiverId: receiver, actions: call.batch, waitUntil: "FINAL" });
    else outcome = await account.callFunctionRaw({ contractId: receiver, methodName: call.method, args: call.args ?? {}, deposit: call.deposit ?? 0n, gas: call.gas ?? 100_000_000_000_000n, waitUntil: "FINAL" });
  } catch (e) {
    if (!e?.transactionResult?.transaction_outcome) throw e;
    outcome = e.transactionResult;
  }
  const failure = failureText(outcome);
  const step = { label, expectOk: !expectFail, ok: expectFail ? failure !== null : failure === null, tx: outcome.transaction_outcome.id, failure, value: valueOf(outcome) };
  out.steps = out.steps.filter((s) => s.label !== label);
  out.steps.push(step);
  save();
  console.log(`${step.ok ? "ok  " : "FAIL"} ${label} ${step.tx}${failure ? ` ${failure}` : ""}`);
  if (!step.ok) throw new Error(`${label}: ${expectFail ? "the contract ACCEPTED what must be refused" : failure}`);
  return step;
}
const circleCall = (label, account, method, args, { expectFail, force, ...call } = {}) => send(label, account, CIRCLE, { method, args, ...call }, { expectFail, force });

// ---- 0. accounts
for (const [name, parent, near] of [["alice", ownerAcc, 1.5], ["bob", ownerAcc, 1.5], ["sold", p2Acc, 3], ["dan", p2Acc, 3]]) {
  const exists = await provider.viewAccount({ accountId: NAMES[name] }).then(() => true, () => false);
  if (exists) continue;
  await parent.createAccount({ newAccountId: NAMES[name], publicKey: accessKey(name).getPublicKey(), nearToTransfer: BigInt(parseNearAmount(String(near))) });
  console.log(`created ${NAMES[name]}`);
}

// ---- 1. asset classes (owner), members, stake
for (const assetType of ["near:account", "near:member_account"]) {
  const entry = { asset_type: assetType, class: "A", delivery_methods: ["escrow_account"], accepted_proofs: [], terms_hash: "5a".repeat(32) };
  await circleCall(`set_asset_class ${assetType}`, ownerAcc, "set_asset_class", { asset_type: assetType, entry });
}
await send("usdt storage_deposit circle", ownerAcc, USDT, { method: "storage_deposit", args: { account_id: CIRCLE }, deposit: BigInt(parseNearAmount("0.01")), gas: 30_000_000_000_000n });
for (const name of ["alice", "bob", "dan"]) {
  const a = acc(name);
  await send(`usdt storage_deposit ${name}`, a, USDT, { method: "storage_deposit", args: { account_id: NAMES[name] }, deposit: BigInt(parseNearAmount("0.01")), gas: 30_000_000_000_000n });
  await send(`usdt mint ${name}`, ownerAcc, USDT, { method: "mint", args: { account_id: NAMES[name], amount: String(300n * CREDIT) }, gas: 30_000_000_000_000n });
  await circleCall(`join ${name}`, a, "join", { circle_id: "agents-main", pubkey_voucher: voucherKey(name).getPublicKey().toString(), terms_hash: "a".repeat(64) }, { deposit: BigInt(parseNearAmount("0.35")) });
  await send(`stake ${name}`, a, USDT, { method: "ft_transfer_call", args: { receiver_id: CIRCLE, amount: String(150n * CREDIT), msg: JSON.stringify({ action: "stake" }) }, deposit: 1n, gas: 100_000_000_000_000n });
}

// ---- epoch helpers
const epochNow = () => view("get_epoch", {});
/** `set_params` of the owner with a new epoch length; it applies to the epoch that opens at the next `close_epoch`. */
async function setEpochLength(label, seconds) {
  const params = await view("get_params", {});
  await circleCall(label, ownerAcc, "set_params", { params: { ...params, epoch_length: seconds } });
}
const closeEpoch = async (n) => {
  await circleCall(`close_epoch ${n}`, ownerAcc, "close_epoch", { epoch: n });
  out.closedEpochs = [...new Set([...(out.closedEpochs ?? []), n])];
  save();
};
/** An open epoch with at least 4 minutes before its cutoff; closes and rolls epochs that are too late (they are finalized later). Returns the epoch row. */
async function freshOpenEpoch() {
  for (;;) {
    const e = await epochNow();
    const cutoff = nsToMs(e.cutoff_at);
    const now = await chainMs();
    if (e.phase === "open" && now + 240_000 < cutoff) return e;
    if (e.phase === "open" || e.phase === "cutoff") {
      await waitUntilMs(cutoff, `cutoff of epoch ${e.epoch}`);
      await closeEpoch(e.epoch);
      continue;
    }
    throw new Error(`unexpected phase ${e.phase} of the current epoch ${e.epoch}`);
  }
}
/** Drives a closed epoch to finalized: commit after the proposal deadline, finalize after the payment deadline. `insidePaymentWindow` runs after the commit. */
async function finalizeEpoch(n, insidePaymentWindow) {
  const e = await view("get_epoch", { epoch: n });
  if (e.phase === "finalized") return;
  if (e.phase === "clearing" || e.phase === "commit") {
    await waitUntilMs(nsToMs(e.proposal_deadline), `proposal deadline of epoch ${n}`);
    await circleCall(`commit_epoch ${n}`, ownerAcc, "commit_epoch", { epoch: n });
  }
  if (insidePaymentWindow) await insidePaymentWindow();
  const p = await view("get_epoch", { epoch: n });
  await waitUntilMs(nsToMs(p.payment_deadline), `payment deadline of epoch ${n}`);
  await circleCall(`finalize_epoch ${n}`, ownerAcc, "finalize_epoch", { epoch: n });
}

// ---- 2. the two grants and the two locks, in one open epoch
// the lock epoch is 10 minutes long (room for the steps below); the epoch after it will be 30 minutes, so that it is still open when the lock epoch is finalized
await setEpochLength("set_params epoch_length 600 (applies to the next epoch)", 600);
// the lock epoch is chosen once; a resumed run must keep it (a new choice would settle an epoch without the obligation)
if (out.lockEpoch === undefined) {
  const e0 = await freshOpenEpoch();
  await setEpochLength("set_params epoch_length 1800 (applies to the epoch after the lock epoch)", 1800);
  out.lockEpoch = e0.epoch;
}
const e0 = { epoch: out.lockEpoch };
const expiresAt = String((BigInt(await chainMs()) + 24n * 3600n * 1000n) * 1_000_000n);
const offer = async (label, assetType, soldName) => {
  const step = await circleCall(label, acc("alice"), "offer_grant", { input: { asset_type: assetType, delivery_method: "escrow_account", limits: { scope: JSON.stringify({ account_id: NAMES[soldName] }), quota: null, expires_at: expiresAt }, unit: "once", unit_price: PRICE.toString(), operator: null, proof_hash: null, proof_kind: null } });
  return Number(step.value);
};
out.grant11 ??= await offer("offer_grant #11 near:account (sold)", "near:account", "sold");
out.grant14 ??= await offer("offer_grant #14 near:member_account (dan)", "near:member_account", "dan");
save();
const readOptions = (grantId, sold) => ({ rpcUrls: [RPC], circle: CIRCLE, grantId, soldAccount: sold, circleView: (m, a) => view(m, a) });

// before the lock the SDK check must refuse
out.checks.before11 = await checkAccountLockBeforePay(readOptions(out.grant11, NAMES.sold)).then((r) => ({ ok: r.ok, reasons: r.reasons }));
save();
console.log("SDK check before the lock (#11):", JSON.stringify(out.checks.before11));

const lockBatch = (name, grantId) => [
  actions.deployContract(lockerWasm),
  actions.functionCall("new", { circle: CIRCLE, grant_id: grantId, return_pubkey: keyOf(name, "return").getPublicKey().toString() }, 100_000_000_000_000n, 0n),
  actions.deleteKey(accessKey(name).getPublicKey()),
];
await send("lock #11 (deploy locker + new + delete key)", acc("sold"), NAMES.sold, { batch: lockBatch("sold", out.grant11) });
out.checks.keysAfterLock11 = (await rpc("query", { request_type: "view_access_key_list", finality: "final", account_id: NAMES.sold })).keys.map((k) => k.public_key);
out.checks.afterLock11 = await checkAccountLockBeforePay(readOptions(out.grant11, NAMES.sold)).then((r) => ({ ok: r.ok, reasons: r.reasons }));
save();
console.log("SDK check after the lock (#11):", JSON.stringify(out.checks.afterLock11), "keys left:", out.checks.keysAfterLock11.length);
await send("lock #14 member account (deploy locker + new + delete key)", acc("dan"), NAMES.dan, { batch: lockBatch("dan", out.grant14) });
out.checks.escrow14AfterLock = await view("get_account_escrow", { grant_id: out.grant14 });
save();

// ---- 3. #11: pay and receive the key
const voucherFor = (epoch, seq) => {
  const { signature, ...voucher } = signVoucher({ circle: CIRCLE, payer: NAMES.bob, payee: NAMES.alice, epoch, seq, cumulativeAmount: (PRICE * BigInt(seq)).toString(), unit: "credit", resource: "https://x207.example/p1-v018" }, voucherKey("bob").toString());
  return { voucher, signature };
};
const buyerKey11 = keyOf("buyer11", "delivery").getPublicKey().toString();
{
  const { voucher, signature } = voucherFor(e0.epoch, 1);
  await circleCall("complete_account_escrow #11 (buyer pays, gets the key)", acc("bob"), "complete_account_escrow", { grant_id: out.grant11, voucher, signature, delivery_pubkey: buyerKey11 });
}
out.checks.after11 = {
  keys: (await rpc("query", { request_type: "view_access_key_list", finality: "final", account_id: NAMES.sold })).keys.map((k) => k.public_key),
  buyerKey: buyerKey11,
  locker: await view("get_state", {}, NAMES.sold),
  escrow: await view("get_account_escrow", { grant_id: out.grant11 }),
};
save();

// ---- 4. #14: refused before the epochs are finalized
const buyerKey14 = keyOf("buyer14", "delivery").getPublicKey().toString();
{
  const e = await epochNow();
  const { voucher, signature } = voucherFor(e.epoch, 2);
  await circleCall("complete_account_escrow #14 too early (must be refused)", acc("bob"), "complete_account_escrow", { grant_id: out.grant14, voucher, signature, delivery_pubkey: buyerKey14 }, { expectFail: true });
}

// ---- 5. finish the epoch of the locks: close, commit, bob settles his 10 credits, finalize
{
  const e = await epochNow();
  if (e.epoch === out.lockEpoch && (e.phase === "open" || e.phase === "cutoff")) {
    await waitUntilMs(nsToMs(e.cutoff_at), `cutoff of epoch ${e.epoch}`);
    await closeEpoch(e.epoch);
  }
  for (let n = 1; n <= out.lockEpoch; n += 1) {
    await finalizeEpoch(n, n !== out.lockEpoch ? null : async () => {
      await send(`settle epoch ${n} (bob pays 10 credits)`, acc("bob"), USDT, { method: "ft_transfer_call", args: { receiver_id: CIRCLE, amount: PRICE.toString(), msg: JSON.stringify({ action: "settle", epoch: n }) }, deposit: 1n, gas: 150_000_000_000_000n });
    });
  }
}

// ---- 6. #14: delivered in the next epoch, the member voucher key rotates to the buyer key
{
  // roll the current epoch if its cutoff is gone or near, and finalize every epoch before the current one (the rule of a member account sale)
  let e;
  for (;;) {
    e = await epochNow();
    if (e.phase === "cutoff" || (e.phase === "open" && (await chainMs()) + 120_000 >= nsToMs(e.cutoff_at))) {
      await waitUntilMs(nsToMs(e.cutoff_at), `cutoff of epoch ${e.epoch}`);
      await closeEpoch(e.epoch);
      e = await epochNow();
    }
    for (let n = 1; n < e.epoch; n += 1) await finalizeEpoch(n);
    e = await epochNow();
    if (e.phase === "open" && (await chainMs()) + 120_000 < nsToMs(e.cutoff_at)) break;
  }
  out.checks.currentEpochAtDelivery14 = e.epoch;
  const { voucher, signature } = voucherFor(e.epoch, 1);
  await circleCall("complete_account_escrow #14 (member account delivered)", acc("bob"), "complete_account_escrow", { grant_id: out.grant14, voucher, signature, delivery_pubkey: buyerKey14 });
}
out.checks.after14 = {
  keys: (await rpc("query", { request_type: "view_access_key_list", finality: "final", account_id: NAMES.dan })).keys.map((k) => k.public_key),
  buyerKey: buyerKey14,
  member: await view("member", { account_id: NAMES.dan }),
  locker: await view("get_state", {}, NAMES.dan),
  escrow: await view("get_account_escrow", { grant_id: out.grant14 }),
};
// ---- 7. the voucher key of the member rotated: a voucher of dan signed with the OLD key is refused, one signed with the buyer's key is accepted
{
  const e = await epochNow();
  const rotation = (secret) => {
    const { signature, ...voucher } = signVoucher({ circle: CIRCLE, payer: NAMES.dan, payee: NAMES.alice, epoch: e.epoch, seq: 1, cumulativeAmount: CREDIT.toString(), unit: "credit", resource: "https://x207.example/p1-rotation" }, secret);
    return { voucher, signature };
  };
  await circleCall("register_voucher of dan with the OLD voucher key (must be refused)", acc("alice"), "register_voucher", rotation(voucherKey("dan").toString()), { expectFail: true });
  await circleCall("register_voucher of dan with the BUYER key (accepted)", acc("alice"), "register_voucher", rotation(keyOf("buyer14", "delivery").toString()));
  out.checks.rotationEpoch = e.epoch;
}
// ---- 8. the obligation of the #14 voucher: close the delivery epoch, commit, every debtor settles, finalize
{
  const n = out.checks.currentEpochAtDelivery14;
  const payers = { [NAMES.bob]: acc("bob"), [NAMES.dan]: accountWith(NAMES.dan, keyOf("buyer14", "delivery")) };
  let e = await view("get_epoch", { epoch: n });
  if (e.phase === "open" || e.phase === "cutoff") {
    await waitUntilMs(nsToMs(e.cutoff_at), `cutoff of epoch ${n}`);
    await closeEpoch(n);
  }
  await finalizeEpoch(n, async () => {
    const positions = await view("list_positions", { epoch: n, from_index: 0, limit: 50 });
    out.checks.positionsEpoch14 = positions;
    save();
    for (const p of positions) {
      if (BigInt(p.net) >= 0n) continue;
      const payer = payers[p.account_id];
      if (!payer) throw new Error(`debtor ${p.account_id} has no signer in this script`);
      await send(`settle epoch ${n} (${p.account_id} pays ${p.amount})`, payer, USDT, { method: "ft_transfer_call", args: { receiver_id: CIRCLE, amount: String(p.amount), msg: JSON.stringify({ action: "settle", epoch: n }) }, deposit: 1n, gas: 150_000_000_000_000n });
    }
  });
  out.checks.epoch14Final = await view("get_epoch", { epoch: n });
  out.checks.positionsEpoch14After = await view("list_positions", { epoch: n, from_index: 0, limit: 50 });
}
out.notCovered = ["cancel_expired on testnet (24 h expiry; covered by sandbox ct127-08 and unit tests)"];
save();
console.log("done; written to", outFile);
