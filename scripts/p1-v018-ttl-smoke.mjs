#!/usr/bin/env node
// CT-129 on testnet: redeploy the reproducible 0.0.18 wasm (with max_escrow_ttl) over x207-v018.testnet, keep its state, then smoke:
//   1. get_max_escrow_ttl answers (default 72 hours) and the old state still reads (get_circle, get_epoch);
//   2. one valid lock: a grant of 24 hours is accepted, the escrow is held;
//   3. one lock refused for the time: a grant of 80 hours, the circle answers ERR_ESCROW_TTL_TOO_LONG, the locker gives the account back.
// Testnet only. Keys come from <keys-dir>/p1-v018-keys.json and <keys-dir>/testnet/<account>.json (never printed).
//   node scripts/p1-v018-ttl-smoke.mjs --keys-dir <dir> --circle-wasm <x207_circle.wasm> --locker-wasm <x207_locker.wasm> --out docs/testnet-epochs/ct129-ttl-smoke.json
// Safe to repeat: finished steps are kept in the --out file and skipped (the redeploy is skipped when the code hash is already the wasm given).
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const req = createRequire(resolve(here, "../../sdk-p1/package.json"));
const { Account, JsonRpcProvider, KeyPair, KeyPairSigner, actions, parseNearAmount } = req("near-api-js");
const bs58 = req("bs58");

const arg = (name) => {
  const i = process.argv.indexOf(`--${name}`);
  if (i < 0 || !process.argv[i + 1]) throw new Error(`--${name} is required`);
  return process.argv[i + 1];
};
const keysDir = resolve(arg("keys-dir"));
const circleWasm = readFileSync(resolve(arg("circle-wasm")));
const lockerWasm = readFileSync(resolve(arg("locker-wasm")));
const outFile = resolve(arg("out"));
const CIRCLE = "x207-v018.testnet";
const OWNER = "x207-v18-own.testnet";
const P2 = "x207-v18-p2.testnet";
const ALICE = `alice.${OWNER}`;
const PRICE = 10_000_000n;
const HOUR_MS = 3_600_000;
const RPC = "https://rpc.testnet.fastnear.com";
const provider = new JsonRpcProvider({ url: RPC });
const sha = (buf) => createHash("sha256").update(buf).digest();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const keysFile = resolve(keysDir, "p1-v018-keys.json");
const secrets = JSON.parse(readFileSync(keysFile, "utf8"));
const fileKey = (account) => KeyPair.fromString(JSON.parse(readFileSync(resolve(keysDir, "testnet", `${account}.json`), "utf8")).secret_key);
const accountWith = (id, keyPair) => new Account(id, provider, new KeyPairSigner(keyPair));
const circleAcc = accountWith(CIRCLE, fileKey(CIRCLE));
const p2Acc = accountWith(P2, fileKey(P2));
const alice = accountWith(ALICE, KeyPair.fromString(secrets.alice.access));
// the two accounts of the smoke are new; their keys are kept in the same secrets file
const keyOf = (name, kind) => {
  secrets[name] ??= {};
  secrets[name][kind] ??= KeyPair.fromRandom("ed25519").toString();
  writeFileSync(keysFile, JSON.stringify(secrets));
  return KeyPair.fromString(secrets[name][kind]);
};
const NAMES = { ttlok: `ttlok.${P2}`, ttllong: `ttllong.${P2}` };

const out = existsSync(outFile) ? JSON.parse(readFileSync(outFile, "utf8")) : { circle: CIRCLE, steps: [], checks: {} };
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
      if (i >= 6) throw e;
      await sleep(3000 * (i + 1));
    }
  }
}
const view = async (method, args = {}, account = CIRCLE) =>
  JSON.parse(Buffer.from((await rpc("query", { request_type: "call_function", finality: "optimistic", account_id: account, method_name: method, args_base64: Buffer.from(JSON.stringify(args)).toString("base64") })).result).toString("utf8"));
const chainMs = async () => Number(BigInt((await rpc("block", { finality: "optimistic" })).header.timestamp_nanosec) / 1_000_000n);

const failureOf = (outcome) => {
  const all = [outcome.status, ...(outcome.receipts_outcome ?? []).map((o) => o.outcome.status)].filter((s) => s && typeof s === "object" && "Failure" in s);
  return all.length ? JSON.stringify(all) : null;
};
async function send(label, account, receiver, call) {
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
  const v = outcome.status?.SuccessValue;
  const refusals = JSON.stringify(outcome).match(/ERR_[A-Z_]+/g) ?? [];
  const step = { label, ok: call.expectFail ? failureOf(outcome) !== null : failureOf(outcome) === null || call.tolerateReceiptFailure === true, tx: outcome.transaction_outcome.id, value: v ? JSON.parse(Buffer.from(v, "base64").toString("utf8")) : null, errors: [...new Set(refusals)] };
  out.steps = out.steps.filter((s) => s.label !== label);
  out.steps.push(step);
  save();
  console.log(`${step.ok ? "ok  " : "FAIL"} ${label} ${step.tx}${step.errors.length ? ` ${step.errors.join(",")}` : ""}`);
  if (!step.ok) throw new Error(`${label} failed`);
  return step;
}

// ---- 1. redeploy over the same account: the state stays
const want = bs58.encode(sha(circleWasm));
const before = await provider.viewAccount({ accountId: CIRCLE });
out.checks.codeHashBefore = out.checks.codeHashBefore ?? before.code_hash;
if (before.code_hash !== want) {
  out.checks.circleBefore = await view("get_circle");
  out.checks.epochBefore = await view("get_epoch");
  const deployed = await circleAcc.deployContract(circleWasm);
  console.log(`redeploy ${CIRCLE} sha256 ${sha(circleWasm).toString("hex")} tx ${deployed.transaction.hash}`);
  out.redeploy = { tx: deployed.transaction.hash, sha256: sha(circleWasm).toString("hex"), base58: want };
  save();
  await sleep(4000);
}
out.checks.codeHashAfter = (await provider.viewAccount({ accountId: CIRCLE })).code_hash;
if (out.checks.codeHashAfter !== want) throw new Error("the code hash on chain is not the wasm given");
out.checks.maxEscrowTtl = await view("get_max_escrow_ttl");
out.checks.circleAfter = await view("get_circle");
out.checks.epochAfter = await view("get_epoch");
out.checks.interfaceVersion = await view("interface_version");
console.log("get_max_escrow_ttl:", out.checks.maxEscrowTtl, "interface:", JSON.stringify(out.checks.interfaceVersion));
save();

// ---- 2. the two accounts of the smoke
for (const name of Object.keys(NAMES)) {
  const exists = await provider.viewAccount({ accountId: NAMES[name] }).then(() => true, () => false);
  if (exists) continue;
  await p2Acc.createAccount({ newAccountId: NAMES[name], publicKey: keyOf(name, "access").getPublicKey(), nearToTransfer: BigInt(parseNearAmount("1.6")) });
  console.log(`created ${NAMES[name]}`);
}

const now = await chainMs();
const offer = async (label, sold, hours) => {
  const expires = String((BigInt(now) + BigInt(hours) * BigInt(HOUR_MS)) * 1_000_000n);
  const step = await send(label, alice, CIRCLE, { method: "offer_grant", args: { input: { asset_type: "near:account", delivery_method: "escrow_account", limits: { scope: JSON.stringify({ account_id: NAMES[sold] }), quota: null, expires_at: expires }, unit: "once", unit_price: PRICE.toString(), operator: null, proof_hash: null, proof_kind: null } } });
  return Number(step.value);
};
const lockBatch = (name, grantId) => [
  actions.deployContract(lockerWasm),
  actions.functionCall("new", { circle: CIRCLE, grant_id: grantId, return_pubkey: keyOf(name, "return").getPublicKey().toString() }, 100_000_000_000_000n, 0n),
  actions.deleteKey(keyOf(name, "access").getPublicKey()),
];
const keysOf = async (id) => (await rpc("query", { request_type: "view_access_key_list", finality: "final", account_id: id })).keys.map((k) => k.public_key);

// ---- 3. a valid lock: 24 hours
out.grantOk ??= await offer("offer_grant 24 h near:account (ttlok)", "ttlok", 24);
save();
await send("lock ttlok (24 h grant, must be accepted)", accountWith(NAMES.ttlok, keyOf("ttlok", "access")), NAMES.ttlok, { batch: lockBatch("ttlok", out.grantOk) });
out.checks.escrowOk = await view("get_account_escrow", { grant_id: out.grantOk });
if (out.checks.escrowOk?.state !== "held" && out.checks.escrowOk?.state !== "Held") throw new Error(`escrow of the valid lock is ${JSON.stringify(out.checks.escrowOk)}`);
save();

// ---- 4. a lock refused for the time: 80 hours
out.grantLong ??= await offer("offer_grant 80 h near:account (ttllong)", "ttllong", 80);
save();
const refused = await send("lock ttllong (80 h grant, circle must refuse, locker gives the key back)", accountWith(NAMES.ttllong, keyOf("ttllong", "access")), NAMES.ttllong, { batch: lockBatch("ttllong", out.grantLong), tolerateReceiptFailure: true });
out.checks.refusalNamed = refused.errors.includes("ERR_ESCROW_TTL_TOO_LONG");
out.checks.escrowLong = await view("get_account_escrow", { grant_id: out.grantLong });
out.checks.keysLongAfter = await keysOf(NAMES.ttllong);
out.checks.returnKeyLong = keyOf("ttllong", "return").getPublicKey().toString();
out.checks.lockerStateLong = await view("get_state", {}, NAMES.ttllong);
save();
if (!out.checks.refusalNamed) throw new Error("ERR_ESCROW_TTL_TOO_LONG not found in the receipts of the refused lock");
if (out.checks.escrowLong !== null) throw new Error("an escrow was recorded for the refused lock");
console.log("done; written to", outFile);
