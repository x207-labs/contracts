#!/usr/bin/env node
// CT-131: the voucher-key rotation of a sold member account, proven again on the final build of the circle 0.0.18 (sha256 4701ecce..d198, code hash 5nBeeNae..Aib).
// The first proof was on build e64d4ec7 (scripts/p1-v018-epoch-run.mjs). On x207-v018.testnet this script:
//   1. makes a new member dan3 (join, stake), offers a near:member_account grant for it and locks it through the locker;
//   2. finishes the lock epoch, finalizes every earlier epoch, and delivers the account in the next open epoch (complete_account_escrow with a voucher of bob);
//   3. checks the rotation: a voucher of dan3 signed with the OLD voucher key is refused by register_voucher, one signed with the buyer key is accepted;
//   4. closes the delivery epoch, commits, settles every debtor (bob 10 credits, dan3 1 credit, dan3 signs with the buyer key), finalizes: the position of alice is claimable;
//   5. alice claims her claimable positions of epochs 8, 13 and the delivery epoch.
// Testnet only. Keys live in <keys-dir>/p1-v018-keys.json (never printed); the result file holds hashes, account names and contract answers only.
//   node scripts/ct131-rotation-run.mjs --keys-dir <dir> --locker-wasm <x207_locker.wasm> --out docs/testnet-epochs/ct131-rotation-run.json
// Safe to repeat: finished steps are kept in the --out file and skipped. The keeper of the backend may close, commit or finalize an epoch first: state is read before every such call.
import { existsSync, mkdirSync, readFileSync, writeFileSync, chmodSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const sdkRoot = resolve(here, "../../sdk");
const req = createRequire(resolve(sdkRoot, "package.json"));
const { Account, JsonRpcProvider, KeyPair, KeyPairSigner, actions, parseNearAmount } = req("near-api-js");
const { signVoucher } = await import(resolve(sdkRoot, "packages/core/dist/index.js"));

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
const NAMES = { alice: `alice.${OWNER}`, bob: `bob.${OWNER}`, dan3: `dan3.${OWNER}`, exp2: `exp2.${OWNER}` };
const DAN3_FUNDING = 2.3;
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

// ---- 0. the build, funds, the new member account
out.checks.codeHash = (await provider.viewAccount({ accountId: CIRCLE })).code_hash;
if (out.checks.codeHash !== "5nBeeNaeWbVH7Q1c3mfG1NPThENb9PA8BvVgjauf6Aib") throw new Error(`the circle is not the final build of CT-129: ${out.checks.codeHash}`);
{
  const exists = await provider.viewAccount({ accountId: NAMES.dan3 }).then(() => true, () => false);
  if (!exists) {
    // the owner account has too little NEAR for a locker account: the test account exp2 of CT-127 (its lock was cancelled, the return key is ours) is deleted and its NEAR goes to the owner
    const exp2Exists = await provider.viewAccount({ accountId: NAMES.exp2 }).then(() => true, () => false);
    if (exp2Exists) {
      const exp2Return = accountWith(NAMES.exp2, keyOf("exp2", "return"));
      const r = await exp2Return.deleteAccount(OWNER);
      out.steps.push({ label: "delete exp2 (test account of CT-127, NEAR back to the owner)", ok: true, tx: r.transaction_outcome.id, failure: null, value: null });
      save();
      console.log("deleted exp2", r.transaction_outcome.id);
    }
    await ownerAcc.createAccount({ newAccountId: NAMES.dan3, publicKey: accessKey("dan3").getPublicKey(), nearToTransfer: BigInt(parseNearAmount(String(DAN3_FUNDING))) });
    console.log(`created ${NAMES.dan3}`);
  }
}
save();

// ---- epoch helpers (state first: the keeper may have acted)
const epochNow = () => view("get_epoch", {});
const closeEpoch = async (n) => {
  const e = await view("get_epoch", { epoch: n });
  if (e && e.phase !== "open" && e.phase !== "cutoff") {
    console.log(`close_epoch ${n}: already ${e.phase} (by the keeper or an earlier run)`);
    return;
  }
  await circleCall(`close_epoch ${n}`, ownerAcc, "close_epoch", { epoch: n }).catch(async (err) => {
    if (!String(err.message).includes("ERR_WRONG_PHASE") || ["open", "cutoff"].includes((await view("get_epoch", { epoch: n })).phase)) throw err;
    console.log(`close_epoch ${n}: the keeper was first`);
  });
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
const settleCall = (label, account, epoch, amount) => send(label, account, USDT, { method: "ft_transfer_call", args: { receiver_id: CIRCLE, amount: String(amount), msg: JSON.stringify({ action: "settle", epoch }) }, deposit: 1n, gas: 150_000_000_000_000n });

// ---- 1. the member dan3: usdt, join, stake
{
  const a = acc("dan3");
  await send("usdt storage_deposit dan3", a, USDT, { method: "storage_deposit", args: { account_id: NAMES.dan3 }, deposit: BigInt(parseNearAmount("0.01")), gas: 30_000_000_000_000n });
  await send("usdt mint dan3", ownerAcc, USDT, { method: "mint", args: { account_id: NAMES.dan3, amount: String(300n * CREDIT) }, gas: 30_000_000_000_000n });
  await circleCall("join dan3", a, "join", { circle_id: "agents-main", pubkey_voucher: voucherKey("dan3").getPublicKey().toString(), terms_hash: "a".repeat(64) }, { deposit: BigInt(parseNearAmount("0.35")) });
  await send("stake dan3", a, USDT, { method: "ft_transfer_call", args: { receiver_id: CIRCLE, amount: String(150n * CREDIT), msg: JSON.stringify({ action: "stake" }) }, deposit: 1n, gas: 100_000_000_000_000n });
  out.checks.memberBefore = await view("member", { account_id: NAMES.dan3 });
  save();
}

// ---- 2. the lock epoch: offer and lock the member account
if (out.lockEpoch === undefined) {
  const e0 = await freshOpenEpoch(300_000);
  out.lockEpoch = e0.epoch;
  save();
}
{
  const expiresAt = String(BigInt((await chainMs()) + 24 * 3_600_000) * 1_000_000n);
  if (out.grant14 === undefined) {
    const step = await circleCall("offer_grant near:member_account (dan3)", acc("alice"), "offer_grant", { input: { asset_type: "near:member_account", delivery_method: "escrow_account", limits: { scope: JSON.stringify({ account_id: NAMES.dan3 }), quota: null, expires_at: expiresAt }, unit: "once", unit_price: PRICE.toString(), operator: null, proof_hash: null, proof_kind: null } });
    out.grant14 = Number(step.value);
    save();
  }
  await send("lock dan3 (member account)", acc("dan3"), NAMES.dan3, { batch: [
    actions.deployContract(lockerWasm),
    actions.functionCall("new", { circle: CIRCLE, grant_id: out.grant14, return_pubkey: keyOf("dan3", "return").getPublicKey().toString() }, 100_000_000_000_000n, 0n),
    actions.deleteKey(accessKey("dan3").getPublicKey()),
  ] });
  out.checks.escrowAfterLock = await view("get_account_escrow", { grant_id: out.grant14 });
  save();
}

const voucherFor = (epoch, seq) => {
  const { signature, ...voucher } = signVoucher({ circle: CIRCLE, payer: NAMES.bob, payee: NAMES.alice, epoch, seq, cumulativeAmount: (PRICE * BigInt(seq)).toString(), unit: "credit", resource: "https://x207.example/ct131-rotation" }, voucherKey("bob").toString());
  return { voucher, signature };
};
const buyerKey = keyOf("buyer14c", "delivery");
const delivered = () => out.steps.some((s) => s.label === "complete_account_escrow dan3 (member account delivered)" && s.ok === true);

// ---- 3. finish the lock epoch, finalize every earlier epoch, deliver in the next open epoch
if (!delivered()) {
  const e = await epochNow();
  if (e.epoch === out.lockEpoch && (e.phase === "open" || e.phase === "cutoff")) {
    await waitUntilMs(nsToMs(e.cutoff_at), `cutoff of epoch ${e.epoch}`);
    await closeEpoch(e.epoch);
  }
  let cur;
  for (;;) {
    cur = await epochNow();
    if (cur.phase === "cutoff" || (cur.phase === "open" && (await chainMs()) + 120_000 >= nsToMs(cur.cutoff_at))) {
      await waitUntilMs(nsToMs(cur.cutoff_at), `cutoff of epoch ${cur.epoch}`);
      await closeEpoch(cur.epoch);
      cur = await epochNow();
    }
    const checked = cur.epoch;
    for (let n = 1; n < checked; n += 1) await finalizeEpoch(n);
    cur = await epochNow();
    if (cur.epoch === checked && cur.phase === "open" && (await chainMs()) + 120_000 < nsToMs(cur.cutoff_at)) break;
  }
  out.deliveryEpoch ??= cur.epoch;
  save();
  const { voucher, signature } = voucherFor(out.deliveryEpoch, 1);
  await circleCall("complete_account_escrow dan3 (member account delivered)", acc("bob"), "complete_account_escrow", { grant_id: out.grant14, voucher, signature, delivery_pubkey: buyerKey.getPublicKey().toString() });
}
out.checks.afterDelivery = { keys: await keysOf(NAMES.dan3), buyerKey: buyerKey.getPublicKey().toString(), oldVoucherKey: voucherKey("dan3").getPublicKey().toString(), member: await view("member", { account_id: NAMES.dan3 }), escrow: await view("get_account_escrow", { grant_id: out.grant14 }) };
save();

// ---- 4. the rotation: a voucher of dan3 with the OLD voucher key is refused, one with the buyer key is accepted (in the delivery epoch, still open)
{
  const rotation = (secret) => {
    const { signature, ...voucher } = signVoucher({ circle: CIRCLE, payer: NAMES.dan3, payee: NAMES.alice, epoch: out.deliveryEpoch, seq: 1, cumulativeAmount: CREDIT.toString(), unit: "credit", resource: "https://x207.example/ct131-rotation-dan3" }, secret);
    return { voucher, signature };
  };
  const e = await view("get_epoch", { epoch: out.deliveryEpoch });
  if (e.phase !== "open") console.log(`warning: the delivery epoch is already ${e.phase}; register_voucher may be refused for the phase`);
  await circleCall("register_voucher of dan3 with the OLD voucher key (must be refused)", acc("alice"), "register_voucher", rotation(voucherKey("dan3").toString()), { expectFail: true });
  await circleCall("register_voucher of dan3 with the BUYER key (accepted)", acc("alice"), "register_voucher", rotation(buyerKey.toString()));
  out.checks.rotationEpoch = out.deliveryEpoch;
  save();
}

// ---- 5. the delivery epoch: close, commit, every debtor settles, finalize
{
  const n = out.deliveryEpoch;
  const e = await view("get_epoch", { epoch: n });
  if (e.phase === "open" || e.phase === "cutoff") {
    await waitUntilMs(nsToMs(e.cutoff_at), `cutoff of epoch ${n}`);
    await closeEpoch(n);
  }
  const payers = { [NAMES.bob]: acc("bob"), [NAMES.dan3]: accountWith(NAMES.dan3, buyerKey) };
  await finalizeEpoch(n, async () => {
    const positions = await view("list_positions", { epoch: n, from_index: 0, limit: 50 });
    out.checks.positionsBeforeSettle = positions;
    for (const p of positions) {
      if (BigInt(p.net) >= 0n) continue;
      const payer = payers[p.account_id];
      if (!payer) throw new Error(`debtor ${p.account_id} has no signer in this script`);
      await settleCall(`settle epoch ${n} (${p.account_id} pays ${p.amount})`, payer, n, p.amount);
    }
  });
  out.checks.epochFinal = await view("get_epoch", { epoch: n });
  out.checks.positionsAfterFinalize = await view("list_positions", { epoch: n, from_index: 0, limit: 50 });
  save();
}

// ---- 6. alice claims her claimable positions (8, 13, delivery epoch)
for (const n of [8, 13, out.deliveryEpoch]) {
  const pos = await view("net_position", { epoch: n, account_id: NAMES.alice });
  out.checks[`aliceEpoch${n}Before`] = pos;
  if (!pos || pos.status !== "claimable") {
    console.log(`epoch ${n}: alice position is ${pos?.status ?? "none"}, no claim`);
    continue;
  }
  await circleCall(`claim epoch ${n} (alice, near)`, acc("alice"), "claim", { epoch: n, destination: "near" }, { deposit: 1n, gas: 120_000_000_000_000n });
  out.checks[`aliceEpoch${n}After`] = await view("net_position", { epoch: n, account_id: NAMES.alice });
  save();
}
out.notCovered = ["the GC behaviour of node 2.14.0", "the rotation is shown on testnet with one member (dan3), not under a second concurrent sale"];
save();
console.log("done; written to", outFile);
