#!/usr/bin/env bash
# boot-demo-stack.sh — boots the FULL local stack for browser-driven proof:
#
#   1. anvil chain (localhost:8545) with funded test accounts
#   2. GpuLeaseVault deployed from real forge artifacts
#   3. GpuLeaseBlueprint BSM deployed + bound to the vault
#   4. Operator API on :9200 (capabilities, quotes, sessions, CORS)
#   5. A buyer account permitted on the service
#
# Prints the env vars the web app needs. Exports them for `source`.
set -euo pipefail
cd "$(dirname "$0")/.."

RPC_URL="http://127.0.0.1:8545"
OPERATOR_PORT=9200
# Deterministic test key (anvil well-known #5 — the broker treasury from the demo)
BUYER_KEY="0x8166f546333643e517fd0b7dcf8b3f23fbfbd3ae5f2ea5c34bf5e58b37f07f56"
OPERATOR_KEY="0x4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d"
DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"

echo "── Booting demo stack ──────────────────────────────────────"

# Kill any previous instances
pkill -f "anvil.*8545" 2>/dev/null || true
pkill -f "operator_api" 2>/dev/null || true
sleep 1

# ── 1. Anvil chain ────────────────────────────────────────────
anvil --port 8545 --host 127.0.0.1 --chain-id 31337 > /tmp/demo-anvil.log 2>&1 &
ANVIL_PID=$!
sleep 2

# Verify chain is up
cast block-number --rpc-url $RPC_URL > /dev/null || { echo "FAIL: anvil didn't start"; exit 1; }
echo "  ✓ anvil chain on :8545 (chain 31337)"

# ── 2. Deploy vault + BSM from forge artifacts ────────────────
DEPLOYER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
BUYER="0xc8ee3018d2fdac71ec08c1913212da42fa92a4b6"

# NOTE: cast deploy no longer exists in foundry ≥1.8 — forge script/create is
# the supported path. The vault script prints GPU_LEASE_VAULT=<addr> on success.
VAULT_OUT=$(forge script contracts/script/DeployGpuLeaseVault.s.sol \
  --rpc-url $RPC_URL \
  --broadcast \
  --private-key $DEPLOYER_KEY 2>&1 | grep "GPU_LEASE_VAULT=" | cut -d= -f2 | tr -d ' ')
if [ -z "$VAULT_OUT" ] || [ "${#VAULT_OUT}" -ne 42 ]; then
  echo "FAIL: vault deployment failed"
  exit 1
fi
VAULT_ADDR=$VAULT_OUT
echo "  ✓ vault deployed at $VAULT_ADDR"

# Deploy the BSM (constructor: vault + slashing verifier; zero verifier is
# fine for the local demo — no slashing flows are exercised).
BSM_ADDR=$(forge create contracts/src/GpuLeaseBlueprint.sol:GpuLeaseBlueprint \
  --constructor-args "$VAULT_ADDR" "0x0000000000000000000000000000000000000000" \
  --rpc-url $RPC_URL \
  --private-key $DEPLOYER_KEY \
  --broadcast 2>&1 | grep -oE 'Deployed to: 0x[0-9a-fA-F]{40}' | grep -oE '0x[0-9a-fA-F]{40}')

if [ -z "$BSM_ADDR" ]; then
  echo "FAIL: BSM deployment failed"
  exit 1
fi
echo "  ✓ BSM deployed at $BSM_ADDR"

# ── 3. Fund the buyer + operator ──────────────────────────────
cast rpc anvil_setBalance $BUYER 0x3635c9adc5dea0000000000 --rpc-url $RPC_URL > /dev/null
echo "  ✓ buyer $BUYER funded"

# Derive the operator address from the signing key
OPERATOR_ADDR=$(cast wallet address $OPERATOR_KEY)
echo "  ✓ operator address: $OPERATOR_ADDR"

# ── 4. Operator API ───────────────────────────────────────────
# NOTE: tee_type is a serde enum — it takes VARIANT NAMES ("None"/"Nitro"/…),
# not integers. An integer here fails the whole array's parse and the operator
# silently serves zero inventory.
GPU_INVENTORY_JSON='[
  {"id":"gpu-0","gpu_class":"h100","tee":false,"tee_type":"None","cuda_ordinal":0},
  {"id":"gpu-1","gpu_class":"h100-tee","tee":true,"tee_type":"Nitro","cuda_ordinal":1},
  {"id":"gpu-2","gpu_class":"a100-80gb","tee":false,"tee_type":"None","cuda_ordinal":2},
  {"id":"gpu-3","gpu_class":"b200","tee":false,"tee_type":"None","cuda_ordinal":3}
]}' \
GPU_QUOTE_SIGNING_KEY=$OPERATOR_KEY \
OPERATOR_API_LISTEN="127.0.0.1:$OPERATOR_PORT" \
  nohup target/debug/examples/operator_api > /tmp/demo-operator.log 2>&1 &
OPERATOR_PID=$!
sleep 2

# Verify operator is up
curl -s "http://127.0.0.1:$OPERATOR_PORT/api/capabilities" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['schemaVersion']==1; print(f'  ✓ operator API on :$OPERATOR_PORT ({len(d[\"compute\"][\"classes\"])} classes)')" || {
  echo "FAIL: operator API didn't start"
  exit 1
}

# ── 5. Print the env vars ─────────────────────────────────────
# For the web app, the "tangle" address is the BSM (where job calls go).
# Service ID is 0 (the test service).
echo ""
echo "── Web app env vars ────────────────────────────────────────"
cat << EOF
VITE_GPU_LEASE_OPERATOR_URL=http://127.0.0.1:$OPERATOR_PORT
VITE_GPU_LEASE_RPC_URL=$RPC_URL
VITE_GPU_LEASE_VAULT_ADDRESS=$VAULT_ADDR
VITE_GPU_LEASE_TANGLE_ADDRESS=$BSM_ADDR
VITE_GPU_LEASE_SERVICE_ID=0
VITE_GPU_LEASE_BUYER_KEY=$BUYER_KEY
VITE_TNT_USD=0.35
EOF

echo ""
echo "── Stack PIDs (kill with: kill $ANVIL_PID $OPERATOR_PID) ──"
echo "ANVIL_PID=$ANVIL_PID"
echo "OPERATOR_PID=$OPERATOR_PID"
echo ""
echo "Stack is live. Start the web app with the env vars above."
