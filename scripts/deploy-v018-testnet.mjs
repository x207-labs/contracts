#!/usr/bin/env node
// P1 (UM-076): deploy the reproducible 0.0.18 wasm to a NEW testnet account and register the official locker code hash.
//   node scripts/deploy-v018-testnet.mjs --keys-dir <dir> --wasm-dir <dir with x207_circle.wasm and x207_locker.wasm> --circle x207-v018.testnet --owner x207-v18-own.testnet
// Testnet only, never an account that already holds code. Keys come from <keys-dir>/testnet/<account>.json and are never printed.
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { resolve } from "node:path";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const sdk = createRequire(resolve(here, "../../sdk-p1/package.json"));
const { Account, JsonRpcProvider, KeyPairSigner } = sdk("near-api-js");
const bs58 = sdk("bs58");
const arg = (name) => {
  const i = process.argv.indexOf(`--${name}`);
  if (i < 0 || !process.argv[i + 1]) throw new Error(`--${name} is required`);
  return process.argv[i + 1];
};
const keysDir = resolve(arg("keys-dir"));
const wasmDir = resolve(arg("wasm-dir"));
const [circleId, ownerId] = [arg("circle"), arg("owner")];
const USDT = "usdt.x207-deployer.testnet";
for (const id of [circleId, ownerId]) if (!id.endsWith(".testnet")) throw new Error("only .testnet accounts");
const provider = new JsonRpcProvider({ url: "https://rpc.testnet.fastnear.com" });
const accountOf = (id) => new Account(id, provider, KeyPairSigner.fromSecretKey(JSON.parse(readFileSync(resolve(keysDir, "testnet", `${id}.json`), "utf8")).secret_key));
const sha = (buf) => createHash("sha256").update(buf).digest();

const circleWasm = readFileSync(resolve(wasmDir, "x207_circle.wasm"));
const lockerWasm = readFileSync(resolve(wasmDir, "x207_locker.wasm"));
const lockerHash = bs58.encode(sha(lockerWasm));
const PARAMS = {
  epoch_length: 300, payment_window: 900, proposal_window: 60, base_limit_ratio: 8000, limit_growth: 1000,
  limit_cap: "1000000000", limit_decay: 0, fees: { clearing_fee_bps: 0, solver_fee_bps: 0 }, admission: "open",
};
const existing = await provider.viewAccount({ accountId: circleId });
if (existing.code_hash !== "11111111111111111111111111111111") throw new Error(`${circleId} already holds code: refusing`);
const circle = accountOf(circleId);
const deployed = await circle.deployContract(circleWasm);
console.log(`deploy ${circleId} sha256 ${sha(circleWasm).toString("hex")} tx ${deployed.transaction.hash}`);
const init = await circle.callFunctionRaw({
  contractId: circleId, methodName: "new",
  args: { owner_id: ownerId, circle_id: "agents-main", usdt_token_id: USDT, intents_id: "intents.testnet", params: PARAMS },
  deposit: 0n, gas: 100_000_000_000_000n, waitUntil: "FINAL",
});
console.log(`new ${circleId} tx ${init.transaction.hash}`);
const owner = accountOf(ownerId);
const set = await owner.callFunctionRaw({
  contractId: circleId, methodName: "set_locker_code_hashes", args: { hashes: [lockerHash] },
  deposit: 0n, gas: 30_000_000_000_000n, waitUntil: "FINAL",
});
console.log(`set_locker_code_hashes ${lockerHash} (sha256 ${sha(lockerWasm).toString("hex")}) tx ${set.transaction.hash}`);
const onChain = await provider.viewAccount({ accountId: circleId });
console.log(`code_hash on chain ${onChain.code_hash} (base58 of sha256 of the wasm: ${bs58.encode(sha(circleWasm))})`);
