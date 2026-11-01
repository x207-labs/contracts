// Runs the backend indexer against a near-sandbox chain and prints what the read API answers, as one JSON
// object on stdout. Used by tests/be006_indexer_e2e.rs. Needs `npm run build` in ../backend.
//   node index_chain.mjs <rpc url> <contract id> <start block> <circle id> <account...>
import { FailoverRpcClient, Indexer, RpcBlockSource, createApiServer, openDatabase, rpcChainReader } from "../../../backend/dist/index.js";

const [rpcUrl, contractId, startArg, circleId, ...accounts] = process.argv.slice(2);
const startBlock = Number(startArg);

async function view(method, args) {
  const response = await fetch(rpcUrl, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0", id: "e2e", method: "query",
      params: { request_type: "call_function", finality: "optimistic", account_id: contractId, method_name: method, args_base64: Buffer.from(JSON.stringify(args)).toString("base64") },
    }),
  });
  const body = await response.json();
  return JSON.parse(Buffer.from(body.result.result).toString("utf8"));
}

const db = openDatabase(":memory:");
const client = new FailoverRpcClient({ endpoints: [rpcUrl], timeoutMs: 30_000 });
const source = new RpcBlockSource(rpcChainReader(client), contractId, startBlock);
const indexer = new Indexer({ db, source, circleId, contractId, startBlock, fetchParams: () => view("get_params", {}) });
await indexer.bootstrap();
// Follow the chain while the test runs: a sandbox drops old outcomes after a few epochs of height, so the
// indexer must read each block soon after it is final. A line on stdin ends the run.
let stop = false;
process.stdin.on("data", () => {
  stop = true;
});
process.stdin.resume();
let processed = 0;
let haltError = null;
let target = null; // final head when the test asked to stop; the sandbox keeps producing blocks after that
const lastHeight = () => db.prepare("SELECT last_block_height AS h FROM indexer_cursor").get().h;
try {
  for (;;) {
    const n = await indexer.drain(200);
    processed += n;
    if (stop && target === null) target = await source.finalHeight();
    if (target !== null && lastHeight() >= target) break;
    if (n === 0) await new Promise((r) => setTimeout(r, 150));
  }
} catch (error) {
  haltError = String(error);
}
const debug = { head: await source.finalHeight(), startBlock };

const server = createApiServer({ db });
await new Promise((ready) => server.listen(0, "127.0.0.1", ready));
const base = `http://127.0.0.1:${server.address().port}/x207/circles/${circleId}`;
const get = async (path) => {
  const response = await fetch(base + path);
  return { status: response.status, body: await response.json() };
};

const out = { debug, processed, haltError, report: {}, members: {}, memberList: null, events: db.prepare("SELECT event, handled FROM events_raw ORDER BY block_height, log_index").all() };
for (const epoch of [1, 2, 3]) out.report[epoch] = await get(`/epochs/${epoch}`);
for (const account of accounts) out.members[account] = await get(`/members/${account}`);
out.memberList = await get("/members");
server.close();
db.close();
console.log(JSON.stringify(out));
