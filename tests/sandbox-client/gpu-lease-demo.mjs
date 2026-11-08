// Published-client proof, run by gpu-lease-blueprint-lib/tests/demo.rs against its funded Anvil stack.
// Extends the Sandbox SDK source test tests/unit/gpu-lease-demo.test.ts with chain-checked money and pinned refusals.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import {
  createViemGpuLeaseChain,
  GPU_LEASE_VAULT_ABI,
  GpuLeaseOperatorClient,
  gpuLease,
  projectGpuLeaseRefund,
  releaseGpuLease,
} from "@tangle-network/sandbox/gpu-lease";
import { parseAbi } from "viem";
import { generatePrivateKey, privateKeyToAccount } from "viem/accounts";

const EARNINGS_ABI = parseAbi(["function operatorEarningsOf(address) view returns (uint256)"]);

const required = (name) => {
  const value = process.env[name];
  if (!value) throw new Error(`published-client demo requires ${name}`);
  return value;
};
const step = (name, facts = "") => console.log(`DEMO_STEP ${name} ${facts}`.trimEnd());
// The package exports no ./package.json subpath; read the installed manifest directly.
const sandboxPackage = JSON.parse(
  readFileSync(new URL("node_modules/@tangle-network/sandbox/package.json", import.meta.url)),
);
const pinned = JSON.parse(readFileSync(new URL("package.json", import.meta.url))).dependencies["@tangle-network/sandbox"];
assert.equal(sandboxPackage.version, pinned, "installed Sandbox client differs from the package.json pin");
step("client", `package=${sandboxPackage.name}@${sandboxPackage.version}`);

const rpcUrl = required("DEMO_RPC_URL");
const tangleAddress = required("DEMO_TANGLE_ADDRESS");
// The proxy makes the job RPC unavailable on demand, proving a failed release leaves the lease active.
let unavailable = false;
const proxy = createServer(async (request, response) => {
  const chunks = [];
  for await (const chunk of request) chunks.push(Buffer.from(chunk));
  const body = Buffer.concat(chunks).toString();
  const rpc = JSON.parse(body);
  if (
    unavailable &&
    ["eth_fillTransaction", "eth_estimateGas"].includes(rpc.method) &&
    rpc.params[0]?.to?.toLowerCase() === tangleAddress.toLowerCase()
  ) {
    response.end(JSON.stringify({ jsonrpc: "2.0", id: rpc.id, error: { code: -32603, message: "job RPC unavailable" } }));
    return;
  }
  const upstream = await fetch(rpcUrl, { method: "POST", headers: { "content-type": "application/json" }, body });
  response.writeHead(upstream.status, { "content-type": "application/json" });
  response.end(await upstream.text());
});
await new Promise((done) => proxy.listen(0, "127.0.0.1", done));
try {
  const endpoint = { baseUrl: required("DEMO_OPERATOR_URL"), address: required("DEMO_OPERATOR_ADDRESS") };
  const operator = new GpuLeaseOperatorClient(endpoint);
  const chain = createViemGpuLeaseChain({
    rpcUrl: `http://127.0.0.1:${proxy.address().port}`,
    privateKey: required("DEMO_BUYER_KEY"),
    vaultAddress: required("DEMO_VAULT_ADDRESS"),
    blueprintAddress: required("DEMO_BLUEPRINT_ADDRESS"),
    tangleAddress,
    serviceId: BigInt(required("DEMO_SERVICE_ID")),
    resultTimeoutMs: 60_000,
  });
  const deviceCount = Number(process.env.DEMO_DEVICE_COUNT ?? "1");
  const gpuClass = process.env.DEMO_GPU_CLASS ?? "nvidia-h100";
  const idle = async () =>
    (await operator.capabilities()).compute.classes.find((gpu) => gpu.id === gpuClass)?.idleCount;
  const idleBefore = await idle();
  assert.equal(typeof idleBefore, "number");
  assert.ok(idleBefore >= deviceCount, `idle ${idleBefore} < requested ${deviceCount}`);
  const { publicClient } = chain;
  const vault = required("DEMO_VAULT_ADDRESS");
  const operatorAddress = endpoint.address;
  const earnings = () =>
    publicClient.readContract({ address: vault, abi: EARNINGS_ABI, functionName: "operatorEarningsOf", args: [operatorAddress] });
  const buyerBalance = () => publicClient.getBalance({ address: chain.account, blockTag: "latest" });
  // Gas the buyer paid in blocks (from, to]: every buyer-sent receipt's gasUsed * effectiveGasPrice.
  const buyerGas = async (from, to) => {
    let gas = 0n;
    for (let n = from + 1n; n <= to; n++) {
      for (const tx of (await publicClient.getBlock({ blockNumber: n, includeTransactions: true })).transactions) {
        if (tx.from.toLowerCase() !== chain.account.toLowerCase()) continue;
        const r = await publicClient.getTransactionReceipt({ hash: tx.hash });
        gas += r.gasUsed * r.effectiveGasPrice;
      }
    }
    return gas;
  };
  const head = () => publicClient.getBlockNumber({ cacheTime: 0 });

  // Quote, escrow through the job, and open the owner session in one client call.
  const { lease: resolved } = await gpuLease({
    gpuClass,
    durationSeconds: Number(required("DEMO_DURATION_SECONDS")),
    deviceCount,
    operators: [endpoint],
    chain,
  });
  const { leaseId, credential, quote } = resolved;
  const lease = await chain.readLease(leaseId);
  assert.equal(lease.deviceCount, deviceCount);
  assert.equal(lease.lessee.toLowerCase(), chain.account.toLowerCase());
  assert.equal(lease.escrowWei, quote.escrowTotalWei);
  const idleLive = await idle();
  assert.equal(idleLive, idleBefore - deviceCount);
  step("create", `lease=${leaseId} escrowWei=${lease.escrowWei} devices=${lease.deviceCount} idleBefore=${idleBefore} idleLive=${idleLive}`);

  // Owner auth: only the funded lessee's EIP-191 signature yields a credential, and only it attaches.
  const stranger = privateKeyToAccount(generatePrivateKey());
  await assert.rejects(operator.sessionChallenge(leaseId, stranger.address), /\/api\/session\/challenge refused: 403$/);
  const ownerChallenge = await operator.sessionChallenge(leaseId, chain.account);
  await assert.rejects(
    operator.sessionVerify(ownerChallenge.nonce, await stranger.signMessage({ message: ownerChallenge.message })),
    /\/api\/session\/verify refused: 401$/,
  );
  await assert.rejects(operator.access(leaseId, "invalid"), /\/access refused: 403$/);
  const access = await operator.access(leaseId, credential.token);
  assert.equal(access.deviceIds.length, deviceCount);
  step("authorize", `owner=${chain.account} scopes=${credential.scopes?.join(",")} devices=${access.deviceIds.join(",")} strangerChallenge=403 strangerSignature=401 invalidToken=403`);

  const meter = await projectGpuLeaseRefund(leaseId, chain);
  assert.equal(meter.refundWei, lease.pricePerSecondWei * BigInt(deviceCount) * (lease.expiry - meter.nowSeconds));
  step("meter", `refundWei=${meter.refundWei}`);

  // Forced failure: vault.release settles on-chain (state 1 = Released, escrow 0) and pays both
  // parties; only the RELEASE job submission then fails, so the operator is never told.
  const [balance0, earnings0, block0] = [await buyerBalance(), await earnings(), await head()];
  unavailable = true;
  await assert.rejects(releaseGpuLease(leaseId, chain), /job RPC unavailable/);
  unavailable = false;
  const settled = await chain.readLease(leaseId);
  assert.equal(settled.state, 1);
  assert.equal(settled.escrowWei, 0n);
  const [released] = await publicClient.getContractEvents({
    address: vault, abi: GPU_LEASE_VAULT_ABI, eventName: "LeaseReleased", args: { leaseId }, fromBlock: block0 + 1n, strict: true,
  });
  const { refund, operatorTake } = released.args;
  const settleBlock = await publicClient.getBlock({ blockNumber: released.blockNumber });
  assert.equal(refund, lease.pricePerSecondWei * BigInt(deviceCount) * (lease.expiry - settleBlock.timestamp));
  assert.equal(refund + operatorTake, lease.escrowWei);
  const [balance1, earnings1, block1] = [await buyerBalance(), await earnings(), await head()];
  const gas1 = await buyerGas(block0, block1);
  assert.equal(balance1 - balance0, refund - gas1);
  assert.equal(earnings1 - earnings0, operatorTake);
  // Measured, not asserted: what the operator still holds for a lease already settled on-chain.
  const idleSettled = await idle();
  const accessSettled = await operator.access(leaseId, credential.token).then((a) => `ok:${a.deviceIds.length}`, (e) => e.message.replace(/.* refused: /, "refused:"));
  const statusSettled = await operator.leaseStatus(leaseId).then(() => "present", (e) => e.message.replace(/.* refused: /, "refused:"));
  step("release-settled-job-failed", `leaseState=1 escrowWei=0 refundWei=${refund} operatorTakeWei=${operatorTake} buyerDeltaWei=${balance1 - balance0} buyerGasWei=${gas1} earningsDeltaWei=${earnings1 - earnings0} settleTimestamp=${settleBlock.timestamp} idleAfterSettle=${idleSettled} allocatedAfterSettle=${idleBefore - idleSettled} accessAfterSettle=${accessSettled} statusAfterSettle=${statusSettled}`);

  // Retry: the client finds the existing LeaseReleased, sends no second vault.release, and submits the job.
  const receipt = await releaseGpuLease(leaseId, chain);
  assert.equal(receipt.refundWei, refund);
  assert.equal(receipt.operatorTakeWei, operatorTake);
  const [balance2, earnings2, block2] = [await buyerBalance(), await earnings(), await head()];
  const gas2 = await buyerGas(block1, block2);
  assert.equal(balance2 - balance1, -gas2, "retry must not pay the buyer again");
  assert.equal(earnings2, earnings1, "retry must not pay the operator again");
  const releases = await publicClient.getContractEvents({
    address: vault, abi: GPU_LEASE_VAULT_ABI, eventName: "LeaseReleased", args: { leaseId }, fromBlock: 0n, strict: true,
  });
  assert.equal(releases.length, 1);
  assert.equal((await chain.readLease(leaseId)).state, 1);
  await assert.rejects(operator.access(leaseId, credential.token), /\/access refused: 403$/);
  await assert.rejects(operator.leaseStatus(leaseId), /refused: 404$/);
  const idleAfter = await idle();
  assert.equal(idleAfter, idleBefore);
  step("release", `refundWei=${receipt.refundWei} operatorTakeWei=${receipt.operatorTakeWei} leaseReleasedEvents=${releases.length} retryBuyerDeltaWei=${balance2 - balance1} retryGasWei=${gas2} retryEarningsDeltaWei=${earnings2 - earnings1} credentialAfter=403 statusAfter=404 idleAfter=${idleAfter}`);
  console.log(`DEMO_PASS client=${sandboxPackage.name}@${sandboxPackage.version} lease=${leaseId}`);
} finally {
  await new Promise((done) => proxy.close(() => done()));
}
