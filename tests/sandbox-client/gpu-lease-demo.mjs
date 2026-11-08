// Published-client proof, run by gpu-lease-blueprint-lib/tests/demo.rs against its funded Anvil stack.
// Mirrors the Sandbox SDK source test tests/unit/gpu-lease-demo.test.ts, importing the installed npm package.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { createServer } from "node:http";
import {
  createViemGpuLeaseChain,
  GpuLeaseOperatorClient,
  gpuLease,
  projectGpuLeaseRefund,
  releaseGpuLease,
} from "@tangle-network/sandbox/gpu-lease";
import { generatePrivateKey, privateKeyToAccount } from "viem/accounts";

const required = (name) => {
  const value = process.env[name];
  if (!value) throw new Error(`published-client demo requires ${name}`);
  return value;
};
const step = (name, facts = "") => console.log(`DEMO_STEP ${name} ${facts}`.trimEnd());
const sandboxPackage = createRequire(import.meta.url)("@tangle-network/sandbox/package.json");
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
  step("create", `lease=${leaseId} escrowWei=${lease.escrowWei} devices=${lease.deviceCount} idleBefore=${idleBefore}`);

  // Owner auth: only the funded lessee's EIP-191 signature yields a credential, and only it attaches.
  const stranger = privateKeyToAccount(generatePrivateKey());
  await assert.rejects(async () => {
    const challenge = await operator.sessionChallenge(leaseId, stranger.address);
    await operator.sessionVerify(challenge.nonce, await stranger.signMessage({ message: challenge.message }));
  });
  await assert.rejects(async () => {
    const challenge = await operator.sessionChallenge(leaseId, chain.account);
    await operator.sessionVerify(challenge.nonce, await stranger.signMessage({ message: challenge.message }));
  });
  await assert.rejects(operator.access(leaseId, "invalid"));
  const access = await operator.access(leaseId, credential.token);
  assert.equal(access.deviceIds.length, deviceCount);
  step("authorize", `owner=${chain.account} scopes=${credential.scopes?.join(",")} devices=${access.deviceIds.join(",")} strangerRefused=2 invalidTokenRefused=1`);

  const meter = await projectGpuLeaseRefund(leaseId, chain);
  assert.equal(meter.refundWei, lease.pricePerSecondWei * BigInt(deviceCount) * (lease.expiry - meter.nowSeconds));
  step("meter", `refundWei=${meter.refundWei}`);

  unavailable = true;
  await assert.rejects(releaseGpuLease(leaseId, chain));
  assert.equal((await chain.readLease(leaseId)).state, 1);
  unavailable = false;
  step("release-unavailable", "rejected=1 leaseState=1");

  const receipt = await releaseGpuLease(leaseId, chain);
  assert.equal(receipt.refundWei + receipt.operatorTakeWei, lease.escrowWei);
  assert.equal((await chain.readLease(leaseId)).state, 1);
  await assert.rejects(operator.access(leaseId, credential.token));
  await assert.rejects(operator.leaseStatus(leaseId));
  assert.equal(await idle(), idleBefore);
  step("release", `refundWei=${receipt.refundWei} operatorTakeWei=${receipt.operatorTakeWei} credentialRevoked=1 idleAfter=${idleBefore}`);
  console.log(`DEMO_PASS client=${sandboxPackage.name}@${sandboxPackage.version} lease=${leaseId}`);
} finally {
  await new Promise((done) => proxy.close(() => done()));
}
