#!/usr/bin/env node
// CT-127, what was left after the first testnet run: on x207-v018.testnet, on the build of CT-129 (sha256 4701ecce..d198):
//   A. claim: alice takes her residual of epoch 6 (the epoch of the #14 voucher of the first run);
//   B. cancel_expired_account_escrow: a plain account is locked with a grant that runs out after a few minutes; after that a stranger (bob) calls
//      cancel_expired_account_escrow and the account is back with the return key of the seller;
//   C. #11 again (plain account, locked and bought with a voucher, the buyer holds the only key) and #14 again (member account, bought in the epoch after
//      the lock epoch, once the epochs before are finalized; the member voucher key rotates), then the voucher of #14 is settled and its epoch finalized.
// Testnet only. Keys live in <keys-dir>/p1-v018-keys.json (never printed); the result file holds hashes, account names and contract answers only.
//   node scripts/ct127-close-run.mjs --keys-dir <dir> --locker-wasm <x207_locker.wasm> --out docs/testnet-epochs/ct127-close-run.json
// Safe to repeat: finished steps are kept in the --out file and skipped.
import { existsSync, mkdirSync, readFileSync, writeFileSync, chmodSync } from "node:fs";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const sdkRoot = resolve(here, "../../sdk");
const req = createRequire(resolve(sdkRoot, "package.json"));
const { Account, JsonRpcProvider, KeyPair, KeyPairSigner, actions, parseNearAmount } = req("near-api-js");
const { signVoucher, checkAccountLockBeforePay } = await import(resolve(sdkRoot, "packages/core/dist/index.js"));

const arg = (name) => {
  const i = process.argv.indexOf(`--${name}`);
  if (i < 0 || !process.argv[i + 1]) throw new Error(`--${name} is required`);
  return process.argv[i + 1];
};
const keysDir = resolve(arg("keys-dir"));
const lockerWasm = readFileSync(resolve(arg("locker-wasm")));
const outFile = resolve(arg("out"));
const CIRCLE = "x207-v018.testnet";
const OWNER = "x207-v18-own.testnet";
const USDT = "usdt.x207-deployer.testnet";
const CREDIT = 1_000_000n;
const PRICE = 10n * CREDIT;
const RPC = "https://rpc.testnet.fastnear.com";
const NAMES = { alice: `alice.${OWNER}`, bob: `bob.${OWNER}`, sold2: `sold2.${OWNER}`, exp2: `exp2.${OWNER}`, dan2: `dan2.${OWNER}` };
const FUNDING = { sold2: 1.7, exp2: 1.7, dan2: 2.3 };
const provider = new JsonRpcProvider({ url: RPC });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const keysFile = resolve(keysDir, "p1-v018-keys.json");
const secrets = JSON.parse(readFileSync(keysFile, "utf8"));
const keyOf = (name, kind) => {
  if (!(secrets[name]?.[kind])) {
    secrets[name] ??= {};
    secrets[name][kind] = KeyPair.fromRandom("ed25519").toString();
    writeFileSync(keysFile, JSON.stringify(secrets));
    chmodSync(keysFile, 0o600);
  }
  return KeyPair.fromString(secrets[name][kind]);
};
const fileKey = (account) => KeyPair.fromString(JSON.parse(readFileSync(resolve(keysDir, "testnet", `${account}.json`), "utf8")).secret_key);
const accountWith = (id, keyPair) => new Account(id, provider, new KeyPairSigner(keyPair));
const ownerAcc = accountWith(OWNER, fileKey(OWNER));
const accessKey = (name) => keyOf(name, "access");
const voucherKey = (name) => keyOf(name, "voucher");
const acc = (name) => accountWith(NAMES[name], accessKey(name));

const out = existsSync(outFile) ? JSON.parse(readFileSync(outFile, "utf8")) : { circle: CIRCLE, owner: OWNER, accounts: NAMES, steps: [], checks: {} };
mkdirSync(dirname(outFile), { recursive: true });
const save = () => writeFileSync(outFile, JSON.stringify(out, null, 2));

async function rpc(method, params) {
  for (let i = 0; ; i++) {
    try {
      const r = await fetch(RPC, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }) });
      const b = await r.json();
      if (b.error) throw new Error(JSON.stringify(b.error));
      return b.result;
    } catch (e) {
      if (i >= 12) throw e;
      await sleep(5000 * (i + 1));
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
    console.log(`waiting ${Math.ceil((targetMs - now) / 1000)}s for ${why}`);
    await sleep(Math.min(targetMs - now + 2000, 60_000));
  }
};
const nsToMs = (ns) => Number(BigInt(ns) / 1_000_000n);
const keysOf = async (id) => (await rpc("query", { request_type: "view_access_key_list", finality: "final", account_id: id })).keys.map((k) => k.public_key);

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
async function send(label, account, receiver, call, { expectFail = false } = {}) {
  const previous = out.steps.find((s) => s.label === label && s.ok === true);
  if (previous) {
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
  const step = { label, ok: expectFail ? failure !== null : failure === null, tx: outcome.transaction_outcome.id, failure, value: valueOf(outcome) };
  out.steps = out.steps.filter((s) => s.label !== label);
  out.steps.push(step);
  save();
  console.log(`${step.ok ? "ok  " : "FAIL"} ${label} ${step.tx}${failure ? ` ${failure}` : ""}`);
  if (!step.ok) throw new Error(`${label}: ${expectFail ? "the contract ACCEPTED what must be refused" : failure}`);
  return step;
}
const circleCall = (label, account, method, args, { expectFail, ...call } = {}) => send(label, account, CIRCLE, { method, args, ...call }, { expectFail });

// ---- 0. accounts (new ones are made by the owner account of the circle)
for (const name of ["sold2", "exp2", "dan2"]) {
  const exists = await provider.viewAccount({ accountId: NAMES[name] }).then(() => true, () => false);
  if (exists) continue;
  await ownerAcc.createAccount({ newAccountId: NAMES[name], publicKey: accessKey(name).getPublicKey(), nearToTransfer: BigInt(parseNearAmount(String(FUNDING[name]))) });
  console.log(`created ${NAMES[name]}`);
}
out.checks.codeHash = (await provider.viewAccount({ accountId: CIRCLE })).code_hash;
if (out.checks.codeHash !== "5nBeeNaeWbVH7Q1c3mfG1NPThENb9PA8BvVgjauf6Aib") throw new Error(`the circle is not the build of CT-129: ${out.checks.codeHash}`);
save();

// ---- A. alice claims her residual of epoch 6
{
  const epoch6 = await view("get_epoch", { epoch: 6 });
  if (epoch6?.phase !== "finalized") throw new Error("epoch 6 is not finalized");
  out.checks.alicePositionBeforeClaim = await view("net_position", { epoch: 6, account_id: NAMES.alice });
  const usdtBefore = await view("ft_balance_of", { account_id: NAMES.alice }, USDT);
  await circleCall("claim epoch 6 (alice, near)", acc("alice"), "claim", { epoch: 6, destination: "near" }, { deposit: 1n, gas: 120_000_000_000_000n });
  out.checks.alicePositionAfterClaim = await view("net_position", { epoch: 6, account_id: NAMES.alice });
  out.checks.aliceUsdtGained = (BigInt(await view("ft_balance_of", { account_id: NAMES.alice }, USDT)) - BigInt(out.checks.aliceUsdtBefore ?? usdtBefore)).toString();
  out.checks.aliceUsdtBefore ??= usdtBefore;
  save();
}

// ---- epoch helpers
const epochNow = () => view("get_epoch", {});
// the keeper of the backend may close, commit or finalize an epoch before this script does: read the state first, skip what is done
const closeEpoch = async (n) => {
  const e = await view("get_epoch", { epoch: n });
  if (e && e.phase !== "open" && e.phase !== "cutoff") {
    console.log(`close_epoch ${n}: already ${e.phase} (by the keeper or an earlier run)`);
    return;
  }
  await circleCall(`close_epoch ${n}`, ownerAcc, "close_epoch", { epoch: n });
};
async function freshOpenEpoch(minMs) {
  for (;;) {
    const e = await epochNow();
    const cutoff = nsToMs(e.cutoff_at);
    if (e.phase === "open" && (await chainMs()) + minMs < cutoff) return e;
    if (e.phase === "open" || e.phase === "cutoff") {
      await waitUntilMs(cutoff, `cutoff of epoch ${e.epoch}`);
      await closeEpoch(e.epoch);
      continue;
    }
    throw new Error(`unexpected phase ${e.phase} of the current epoch ${e.epoch}`);
  }
}
async function finalizeEpoch(n, insidePaymentWindow) {
  const e = await view("get_epoch", { epoch: n });
  if (!e || e.phase === "finalized") return;
  if (e.phase === "clearing" || e.phase === "commit") {
    await waitUntilMs(nsToMs(e.proposal_deadline), `proposal deadline of epoch ${n}`);
    const again = await view("get_epoch", { epoch: n });
    if (again.phase === "clearing" || again.phase === "commit") {
      // the keeper may commit in the same seconds: a refusal for the phase then means it was done by the keeper
      await circleCall(`commit_epoch ${n}`, ownerAcc, "commit_epoch", { epoch: n }).catch((err) => {
        if (!String(err.message).includes("ERR_WRONG_PHASE")) throw err;
        console.log(`commit_epoch ${n}: the keeper was first (${err.message.slice(0, 120)})`);
      });
    } else console.log(`commit_epoch ${n}: already ${again.phase} (by the keeper)`);
  }
  if (insidePaymentWindow) await insidePaymentWindow();
  const p = await view("get_epoch", { epoch: n });
  await waitUntilMs(nsToMs(p.payment_deadline), `payment deadline of epoch ${n}`);
  if ((await view("get_epoch", { epoch: n })).phase === "finalized") return console.log(`finalize_epoch ${n}: already finalized (by the keeper)`);
  await circleCall(`finalize_epoch ${n}`, ownerAcc, "finalize_epoch", { epoch: n }).catch(async (err) => {
    if (!String(err.message).includes("ERR_WRONG_PHASE") || (await view("get_epoch", { epoch: n })).phase !== "finalized") throw err;
    console.log(`finalize_epoch ${n}: the keeper was first`);
  });
}
const setEpochLength = async (label, seconds) => {
  const params = await view("get_params", {});
  await circleCall(label, ownerAcc, "set_params", { params: { ...params, epoch_length: seconds } });
};
const settleCall = (label, account, epoch, amount) => send(label, account, USDT, { method: "ft_transfer_call", args: { receiver_id: CIRCLE, amount: String(amount), msg: JSON.stringify({ action: "settle", epoch }) }, deposit: 1n, gas: 150_000_000_000_000n });

// ---- 1. the member dan2: usdt, join, stake
{
  const a = acc("dan2");
  await send("usdt storage_deposit dan2", a, USDT, { method: "storage_deposit", args: { account_id: NAMES.dan2 }, deposit: BigInt(parseNearAmount("0.01")), gas: 30_000_000_000_000n });
  await send("usdt mint dan2", ownerAcc, USDT, { method: "mint", args: { account_id: NAMES.dan2, amount: String(300n * CREDIT) }, gas: 30_000_000_000_000n });
  await circleCall("join dan2", a, "join", { circle_id: "agents-main", pubkey_voucher: voucherKey("dan2").getPublicKey().toString(), terms_hash: "a".repeat(64) }, { deposit: BigInt(parseNearAmount("0.35")) });
  await send("stake dan2", a, USDT, { method: "ft_transfer_call", args: { receiver_id: CIRCLE, amount: String(150n * CREDIT), msg: JSON.stringify({ action: "stake" }) }, deposit: 1n, gas: 100_000_000_000_000n });
}

// ---- 2. the lock epoch: three grants, three locks
if (out.lockEpoch === undefined) {
  await setEpochLength("set_params epoch_length 600 (applies to the next epoch)", 600);
  const e0 = await freshOpenEpoch(300_000);
  // 1800 and not 900: an epoch is finalized about 965 s after its cutoff, so the epoch after it must still be open then (with 900 the delivery never finds an open epoch)
  await setEpochLength("set_params epoch_length 1800 (applies to the epoch after the lock epoch)", 1800);
  out.lockEpoch = e0.epoch;
  save();
}
const offer = async (label, assetType, soldName, expiresMs) => {
  const expiresAt = String(BigInt(expiresMs) * 1_000_000n);
  const step = await circleCall(label, acc("alice"), "offer_grant", { input: { asset_type: assetType, delivery_method: "escrow_account", limits: { scope: JSON.stringify({ account_id: NAMES[soldName] }), quota: null, expires_at: expiresAt }, unit: "once", unit_price: PRICE.toString(), operator: null, proof_hash: null, proof_kind: null } });
  return Number(step.value);
};
const dayFromNow = async () => (await chainMs()) + 24 * 3_600_000;
const lockBatch = (name, grantId) => [
  actions.deployContract(lockerWasm),
  actions.functionCall("new", { circle: CIRCLE, grant_id: grantId, return_pubkey: keyOf(name, "return").getPublicKey().toString() }, 100_000_000_000_000n, 0n),
  actions.deleteKey(accessKey(name).getPublicKey()),
];
// the account of B: the grant runs out 9 minutes after the offer
out.grantExp ??= await (async () => { out.expiresExpMs = (await chainMs()) + 9 * 60_000; save(); return offer("offer_grant 9 min near:account (exp2)", "near:account", "exp2", out.expiresExpMs); })();
out.grant11 ??= await offer("offer_grant #11 near:account (sold2)", "near:account", "sold2", await dayFromNow());
out.grant14 ??= await offer("offer_grant #14 near:member_account (dan2)", "near:member_account", "dan2", await dayFromNow());
save();
const readOptions = (grantId, sold) => ({ rpcUrls: [RPC], circle: CIRCLE, grantId, soldAccount: sold, circleView: (m, a) => view(m, a) });
await send("lock exp2 (9 min grant)", acc("exp2"), NAMES.exp2, { batch: lockBatch("exp2", out.grantExp) });
out.checks.escrowExpAfterLock = await view("get_account_escrow", { grant_id: out.grantExp });
await send("lock sold2 (#11)", acc("sold2"), NAMES.sold2, { batch: lockBatch("sold2", out.grant11) });
out.checks.afterLock11 = await checkAccountLockBeforePay(readOptions(out.grant11, NAMES.sold2)).then((r) => ({ ok: r.ok, reasons: r.reasons }));
await send("lock dan2 (#14)", acc("dan2"), NAMES.dan2, { batch: lockBatch("dan2", out.grant14) });
out.checks.escrow14AfterLock = await view("get_account_escrow", { grant_id: out.grant14 });
save();

const voucherFor = (epoch, seq) => {
  const { signature, ...voucher } = signVoucher({ circle: CIRCLE, payer: NAMES.bob, payee: NAMES.alice, epoch, seq, cumulativeAmount: (PRICE * BigInt(seq)).toString(), unit: "credit", resource: "https://x207.example/ct127-close" }, voucherKey("bob").toString());
  return { voucher, signature };
};

// ---- C1. #11: pay and receive the key
const buyerKey11 = keyOf("buyer11b", "delivery").getPublicKey().toString();
{
  const { voucher, signature } = voucherFor(out.lockEpoch, 1);
  await circleCall("complete_account_escrow #11 (buyer pays, gets the key)", acc("bob"), "complete_account_escrow", { grant_id: out.grant11, voucher, signature, delivery_pubkey: buyerKey11 });
  out.checks.after11 = { keys: await keysOf(NAMES.sold2), buyerKey: buyerKey11, locker: await view("get_state", {}, NAMES.sold2), escrow: await view("get_account_escrow", { grant_id: out.grant11 }) };
  save();
}

// ---- B. cancel_expired after the 9 minutes
{
  await waitUntilMs(out.expiresExpMs + 5_000, "the grant of exp2 to run out");
  const early = out.steps.find((s) => s.label === "cancel_expired_account_escrow (bob, a stranger)");
  out.checks.keysExpBeforeCancel = out.checks.keysExpBeforeCancel ?? (early ? undefined : await keysOf(NAMES.exp2));
  await circleCall("cancel_expired_account_escrow (bob, a stranger)", acc("bob"), "cancel_expired_account_escrow", { grant_id: out.grantExp });
  out.checks.afterCancelExp = { keys: await keysOf(NAMES.exp2), returnKey: keyOf("exp2", "return").getPublicKey().toString(), locker: await view("get_state", {}, NAMES.exp2), escrow: await view("get_account_escrow", { grant_id: out.grantExp }) };
  save();
}

// ---- finish the lock epoch: close, commit, bob settles his 10 credits, finalize; every earlier epoch too
{
  const e = await epochNow();
  if (e.epoch === out.lockEpoch && (e.phase === "open" || e.phase === "cutoff")) {
    await waitUntilMs(nsToMs(e.cutoff_at), `cutoff of epoch ${e.epoch}`);
    await closeEpoch(e.epoch);
  }
  for (let n = 1; n <= out.lockEpoch; n += 1) {
    await finalizeEpoch(n, n !== out.lockEpoch ? null : () => settleCall(`settle epoch ${n} (bob pays 10 credits for #11)`, acc("bob"), n, PRICE));
  }
}

// ---- C2. #14: delivered in the epoch after the lock epoch, once every earlier epoch is final
const buyerKey14 = keyOf("buyer14b", "delivery").getPublicKey().toString();
{
  let e;
  // a resumed run must not walk the epochs again once #14 is delivered: that loop would finalize the delivery epoch before bob settles its voucher
  const delivered = out.steps.some((s) => s.label === "complete_account_escrow #14 (member account delivered)" && s.ok === true);
  for (; !delivered;) {
    e = await epochNow();
    if (e.phase === "cutoff" || (e.phase === "open" && (await chainMs()) + 120_000 >= nsToMs(e.cutoff_at))) {
      await waitUntilMs(nsToMs(e.cutoff_at), `cutoff of epoch ${e.epoch}`);
      await closeEpoch(e.epoch);
      e = await epochNow();
    }
    const checked = e.epoch;
    for (let n = 1; n < checked; n += 1) await finalizeEpoch(n);
    e = await epochNow();
    // the epoch may have rolled while an earlier one was finalized (the keeper closes the empty ones): then the new earlier epochs are finalized in the next pass
    if (e.epoch === checked && e.phase === "open" && (await chainMs()) + 120_000 < nsToMs(e.cutoff_at)) break;
  }
  if (!delivered) out.deliveryEpoch14 ??= e.epoch;
  const { voucher, signature } = voucherFor(out.deliveryEpoch14, 1);
  await circleCall("complete_account_escrow #14 (member account delivered)", acc("bob"), "complete_account_escrow", { grant_id: out.grant14, voucher, signature, delivery_pubkey: buyerKey14 });
  out.checks.after14 = { keys: await keysOf(NAMES.dan2), buyerKey: buyerKey14, member: await view("member", { account_id: NAMES.dan2 }), locker: await view("get_state", {}, NAMES.dan2), escrow: await view("get_account_escrow", { grant_id: out.grant14 }) };
  save();
}

// ---- the voucher of #14: close its epoch, commit, bob settles, finalize
{
  const n = out.deliveryEpoch14;
  const e = await view("get_epoch", { epoch: n });
  if (e.phase === "open" || e.phase === "cutoff") {
    await waitUntilMs(nsToMs(e.cutoff_at), `cutoff of epoch ${n}`);
    await closeEpoch(n);
  }
  await finalizeEpoch(n, async () => {
    const positions = await view("list_positions", { epoch: n, from_index: 0, limit: 50 });
    out.checks.positionsDelivery14 = positions;
    for (const p of positions) {
      if (BigInt(p.net) >= 0n) continue;
      if (p.account_id !== NAMES.bob) throw new Error(`debtor ${p.account_id} has no signer in this script`);
      await settleCall(`settle epoch ${n} (bob pays ${p.amount} for #14)`, acc("bob"), n, p.amount);
    }
  });
  out.checks.delivery14Final = await view("get_epoch", { epoch: n });
  out.checks.positionsDelivery14After = await view("list_positions", { epoch: n, from_index: 0, limit: 50 });
  save();
}
out.notCovered = ["the GC behaviour of node 2.14.0", "alice has not claimed the residual of the two new epochs of this run"];
save();
console.log("done; written to", outFile);
