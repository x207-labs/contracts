#!/usr/bin/env node
// CT-132: the overspend demo behind the sdk option settleMode "register-first" (SD-121), on the circle 0.0.18 at x207-v018.testnet.
// One buyer (bob) signs two vouchers, one to alice and one to dan3; together they are above the headroom of bob. Each seller sits behind its OWN facilitator
// (a mock here, which says /verify valid, as two real facilitators would: neither sees the voucher the other holds). On the chain:
//   1. the voucher to alice is registered (accepted);
//   2. the voucher to dan3 is refused with ERR_INSUFFICIENT_CREDIT (this is the contract being first come, first served: the transaction hash is recorded);
//   3. the sdk middleware is run for both sellers with the real answers of the chain: a seller in "register-first" mode does not serve the refused voucher (402),
//      a seller in the default mode serves it (200) although the chain refuses it. That is the risk the option closes.
// Testnet only. Keys live in <keys-dir>/p1-v018-keys.json (never printed); the result file holds hashes, account names and contract answers only.
//   node scripts/ct132-overspend-demo.mjs --keys-dir <dir> --out docs/testnet-epochs/ct132-overspend-run.json
// Safe to repeat: finished steps are kept in the --out file and skipped. A run needs an open epoch with at least 2 minutes left before its cutoff; the epoch is
// chosen on the first run and kept in the --out file.
// NOT covered: a real facilitator process (the facilitators are mocks that always say valid), the 2 s latency of the chain call in a live seller, a buyer that holds more than one open voucher per seller.
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const sdkRoot = resolve(here, "../../sdk");
const req = createRequire(resolve(sdkRoot, "package.json"));
const { Account, JsonRpcProvider, KeyPair, KeyPairSigner } = req("near-api-js");
const { signVoucher, buildRegisterVoucherCall, registerRefusal } = await import(resolve(sdkRoot, "packages/core/dist/index.js"));
const { x207Middleware } = await import(resolve(sdkRoot, "packages/scheme/dist/index.js"));

const arg = (name) => {
  const i = process.argv.indexOf(`--${name}`);
  if (i < 0 || !process.argv[i + 1]) throw new Error(`--${name} is required`);
  return process.argv[i + 1];
};
const keysDir = resolve(arg("keys-dir"));
const outFile = resolve(arg("out"));
const CIRCLE = "x207-v018.testnet";
const CIRCLE_ID = "agents-main";
const OWNER = "x207-v18-own.testnet";
const RPC = "https://rpc.testnet.fastnear.com";
const NAMES = { alice: `alice.${OWNER}`, bob: `bob.${OWNER}`, dan3: `dan3.${OWNER}` };
const provider = new JsonRpcProvider({ url: RPC });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const secrets = JSON.parse(readFileSync(resolve(keysDir, "p1-v018-keys.json"), "utf8"));
const keyOf = (name, kind) => {
  if (!secrets[name]?.[kind]) throw new Error(`no ${kind} key for ${name} in p1-v018-keys.json`);
  return KeyPair.fromString(secrets[name][kind]);
};
const accountWith = (id, keyPair) => new Account(id, provider, new KeyPairSigner(keyPair));
// alice and bob still hold their access keys; dan3 was delivered (CT-131) and its access key is gone, so the seller dan3 sends nothing: alice sends for both sellers
// (register_voucher is permissionless and the signer of the call is not part of the voucher)
const alice = accountWith(NAMES.alice, keyOf("alice", "access"));

const out = existsSync(outFile) ? JSON.parse(readFileSync(outFile, "utf8")) : { circle: CIRCLE, accounts: NAMES, steps: [], checks: {} };
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
const view = async (method, args = {}) =>
  JSON.parse(Buffer.from((await rpc("query", { request_type: "call_function", finality: "optimistic", account_id: CIRCLE, method_name: method, args_base64: Buffer.from(JSON.stringify(args)).toString("base64") })).result).toString("utf8"));
const chainMs = async () => Number(BigInt((await rpc("block", { finality: "optimistic" })).header.timestamp_nanosec) / 1_000_000n);
const nsToMs = (ns) => Number(BigInt(ns) / 1_000_000n);

function failureText(outcome) {
  const found = [outcome.status, ...(outcome.receipts_outcome ?? []).map((o) => o.outcome.status)].find((s) => s && typeof s === "object" && "Failure" in s);
  if (!found) return null;
  const text = JSON.stringify(found);
  const m = text.match(/"ExecutionError":"((?:[^"\\]|\\.)*)"/) ?? text.match(/"FunctionCallError":\{"[A-Za-z]+":"((?:[^"\\]|\\.)*)"/);
  return m ? m[1].replace(/\\"/g, '"') : text.slice(0, 300);
}
// Sends register_voucher from the account and returns { tx, failure }; the step is kept in the --out file and not repeated.
async function register(label, account, { voucher, signature }, { expectFail }) {
  const previous = out.steps.find((s) => s.label === label && s.ok === true);
  if (previous) {
    console.log(`${label}: already done (${previous.tx})`);
    return previous;
  }
  const call = buildRegisterVoucherCall({ circleContract: CIRCLE, voucher, signature });
  let outcome;
  try {
    outcome = await account.callFunctionRaw({ contractId: call.receiverId, methodName: call.methodName, args: call.args, deposit: call.deposit, gas: call.gas, waitUntil: "FINAL" });
  } catch (e) {
    if (!e?.transactionResult?.transaction_outcome) throw e;
    outcome = e.transactionResult;
  }
  const failure = failureText(outcome);
  const step = { label, ok: expectFail ? failure !== null : failure === null, tx: outcome.transaction_outcome.id, failure };
  out.steps = out.steps.filter((s) => s.label !== label);
  out.steps.push(step);
  save();
  console.log(`${step.ok ? "ok  " : "FAIL"} ${label} ${step.tx}${failure ? ` ${failure}` : ""}`);
  if (!step.ok) throw new Error(`${label}: ${expectFail ? "the contract ACCEPTED what must be refused" : failure}`);
  return step;
}

// ---- 0. the build and an open epoch with time left
const codeHash = (await provider.viewAccount({ accountId: CIRCLE })).code_hash;
if (codeHash !== "5nBeeNaeWbVH7Q1c3mfG1NPThENb9PA8BvVgjauf6Aib") throw new Error(`the circle is not the final build of CT-129: ${codeHash}`);
out.checks.codeHash = codeHash;
if (out.epoch === undefined) {
  for (;;) {
    const e = await view("get_epoch", {});
    if (e.phase === "open" && (await chainMs()) + 120_000 < nsToMs(e.cutoff_at)) {
      out.epoch = e.epoch;
      break;
    }
    console.log(`epoch ${e.epoch} is ${e.phase} or too close to its cutoff; waiting 30s for the next open epoch`);
    await sleep(30_000);
  }
  save();
}
const epoch = out.epoch;
console.log(`epoch ${epoch}`);

// ---- 1. the amounts: each voucher is 60 percent of the headroom of bob, so two are above it and one is not
if (out.headroomBefore === undefined) {
  out.headroomBefore = String(await view("headroom", { account_id: NAMES.bob }));
  save();
}
const headroom = BigInt(out.headroomBefore);
if (headroom <= 0n) throw new Error(`bob has no headroom: ${headroom}`);
const each = (headroom * 6n) / 10n;
if (each === 0n || each * 2n <= headroom) throw new Error(`headroom too small for the demo: ${headroom}`);
out.amountEach = String(each);
save();

const bobVoucherKey = keyOf("bob", "voucher").toString();
const voucherTo = (payee) => {
  const { signature, ...voucher } = signVoucher({ circle: CIRCLE, payer: NAMES.bob, payee, epoch, seq: 1, cumulativeAmount: String(each), unit: "credit", resource: "https://seller.example/api/summarize" }, bobVoucherKey);
  return { voucher, signature };
};
const toAlice = voucherTo(NAMES.alice);
const toDan3 = voucherTo(NAMES.dan3);

// ---- 2. the chain: the first voucher is accepted, the second is refused
await register("register_voucher bob -> alice (first, accepted)", alice, toAlice, { expectFail: false });
const refused = await register("register_voucher bob -> dan3 (second, must be refused: ERR_INSUFFICIENT_CREDIT)", alice, toDan3, { expectFail: true });
if (!String(refused.failure).includes("ERR_INSUFFICIENT_CREDIT")) throw new Error(`the second voucher was refused for another reason: ${refused.failure}`);
out.checks.headroomAfter = String(await view("headroom", { account_id: NAMES.bob }));
save();

// ---- 3. the sdk middleware with the chain behind it. Each seller has its own facilitator (a mock that says valid for everything).
const b64 = (value) => Buffer.from(JSON.stringify(value)).toString("base64");
const network = `x207:${CIRCLE_ID}`;
const extra = { circleContract: CIRCLE, unit: "credit", decimals: 6, epoch };
const facilitatorFor = (payee) => async (input, init) => {
  const path = new URL(String(input)).pathname;
  const json = (value) => new Response(JSON.stringify(value), { status: 200 });
  if (path === "/supported") return json({ kinds: [{ x402Version: 2, scheme: "batch-settlement", network, extra }], extensions: [], signers: {} });
  if (path === "/verify") return json({ isValid: true, payer: NAMES.bob });
  if (path === "/settle") {
    const body = JSON.parse(String(init.body));
    const v = body.paymentPayload.payload.voucher;
    return json({ success: true, transaction: `${v.payer}:${v.payee}:${v.epoch}:${v.seq}`, network, payer: v.payer, extensions: { x207: { epoch: v.epoch, seq: v.seq, cumulativeAmount: v.cumulativeAmount, registration: "pending" } } });
  }
  return new Response("{}", { status: 404 });
};
const header = (payee, { voucher, signature }) => ({
  "PAYMENT-SIGNATURE": b64({ x402Version: 2, resource: { url: "https://seller.example/api/summarize" }, accepted: { scheme: "batch-settlement", network, amount: String(each), asset: "USD", payTo: payee, maxTimeoutSeconds: 60, extra }, payload: { voucher, signature } }),
});
// the seller sends register_voucher through the chain itself; this run replays the answer that the chain already gave in step 2 instead of sending a third transaction
const chainAnswer = (step) => async () => (step.failure === null ? { ok: true } : (registerRefusal(new Error(step.failure)) ?? (() => { throw new Error(step.failure); })()));
const accepted = out.steps.find((s) => s.label.startsWith("register_voucher bob -> alice"));
const seller = (payee, mode) => {
  let served = 0;
  const middleware = x207Middleware({
    routes: { "/api/summarize": { price: "$0.02" } }, facilitatorUrl: "http://facilitator.test", circleId: CIRCLE_ID, payTo: payee, fetch: facilitatorFor(payee),
    ...(mode === "register-first" ? { settleMode: "register-first", registerVoucher: chainAnswer(payee === NAMES.alice ? accepted : refused) } : {}),
  });
  return async (voucherAndSignature) => {
    const response = await middleware(new Request("https://seller.example/api/summarize", { headers: header(payee, voucherAndSignature) }), async () => {
      served += 1;
      return new Response("served", { status: 200 });
    });
    return { status: response.status, served: served > 0 };
  };
};
out.middleware = {
  "register-first, seller alice (voucher accepted by the chain)": await seller(NAMES.alice, "register-first")(toAlice),
  "register-first, seller dan3 (voucher refused by the chain)": await seller(NAMES.dan3, "register-first")(toDan3),
  "default verify-first, seller dan3 (same voucher, the chain refuses it)": await seller(NAMES.dan3, "verify-first")(toDan3),
};
const m = out.middleware;
if (m["register-first, seller alice (voucher accepted by the chain)"].status !== 200) throw new Error("register-first seller of the accepted voucher did not serve");
if (m["register-first, seller dan3 (voucher refused by the chain)"].served) throw new Error("register-first seller served a voucher the chain refused");
if (!m["default verify-first, seller dan3 (same voucher, the chain refuses it)"].served) throw new Error("the default seller did not serve (the gap is not shown)");
out.notCovered = [
  "the facilitators are mocks that always say valid; no real facilitator process was run",
  "the middleware replays the chain answers recorded in this run instead of sending a third register_voucher",
  "the 2 s latency of the chain call in a live seller",
  "a buyer with more than one open voucher per seller, bundle vouchers, alsoAccept entries",
];
save();
console.log("done; written to", outFile);
