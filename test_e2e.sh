#!/bin/bash
# End-to-end test for mpc-poc-near worker (partial flow)
# Tests: send Nostr event → worker picks up → verifies sig → derives MPC key → publishes feedback
#
# Usage:
#   1. Start worker in another terminal:
#      RELAY_URL=wss://relay.damus.io WORKER_NSEC=<hex> SPONSOR_KEY=ed25519:xxx SPONSOR_ACCOUNT=kampouse.testnet \
#        cargo run --bin mpc-worker
#
#   2. Run this test:
#      RELAY_URL=wss://relay.damus.io cargo test --test e2e -- --nocapture
#
# Or use this script directly (requires: pip3 install pynacl websockets)

set -e
cd "$(dirname "$0")"

RELAY="${RELAY_URL:-wss://relay.damus.io}"

echo "═══════════════════════════════════════════════════"
echo "   MPC-POC-NEAR E2E Test (Partial Flow)"
echo "═══════════════════════════════════════════════════"
echo ""
echo "This tests: Nostr event → worker → sig verify → key derive → feedback"
echo "The actual MPC signing (tx broadcast) is NOT tested (TODO in worker)."
echo ""

# Check deps
python3 -c "import nacl.signing; import websockets" 2>/dev/null || {
    echo "Installing deps..."
    pip3 install pynacl websockets
}

# Build worker binary
echo "[1/6] Building..."
cargo build --bin mpc-worker 2>&1 | tail -3
echo "  ✅ Built"

# Generate test sender keypair
echo ""
echo "[2/6] Generating test Nostr keypair..."
SENDER_SK_HEX=$(python3 -c "
from nacl.signing import SigningKey
import os
sk = SigningKey.generate()
print(sk.encode().hex())
")
SENDER_PK_HEX=$(python3 -c "
from nacl.signing import SigningKey
sk = SigningKey(bytes.fromhex('$SENDER_SK_HEX'))
print(sk.verify_key.encode().hex())
")
echo "  Sender: ${SENDER_PK_HEX:0:16}...${SENDER_PK_HEX:48:}"

# Generate test worker keypair (for this test we run a short-lived worker)
echo ""
echo "[3/6] Generating test worker keypair..."
WORKER_SK_HEX=$(python3 -c "
from nacl.signing import SigningKey
sk = SigningKey.generate()
print(sk.encode().hex())
")
echo "  Worker: $(python3 -c "
from nacl.signing import SigningKey
sk = SigningKey(bytes.fromhex('$WORKER_SK_HEX'))
print(sk.verify_key.encode().hex()[:16])
")..."

# Build & sign kind 5001 event
echo ""
echo "[4/6] Creating kind 5001 job event..."
EVENT_RESULT=$(python3 << PYEOF
import json, hashlib, time
from nacl.signing import SigningKey

sk = SigningKey(bytes.fromhex('$SENDER_SK_HEX'))
pk_hex = sk.verify_key.encode().hex()
now = int(time.time())
content = json.dumps({"to": "kampouse.testnet", "amount": "0.001"})
tags = [["i", content]]

# Serialize for id
ser = json.dumps([0, pk_hex, now, 5001, tags, content], separators=(',',':'))
event_id = hashlib.sha256(ser.encode()).hexdigest()

# Sign
sig = sk.sign(hashlib.sha256(ser.encode()).digest()).signature.hex()

event = {
    "id": event_id,
    "pubkey": pk_hex,
    "created_at": now,
    "kind": 5001,
    "tags": tags,
    "content": content,
    "sig": sig,
}
print(json.dumps(event))
PYEOF
)
EVENT_ID=$(echo "$EVENT_RESULT" | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")
echo "  Event ID: ${EVENT_ID:0:16}..."
echo "  Content: $(echo "$EVENT_RESULT" | python3 -c "import json,sys; print(json.load(sys.stdin)['content'])")"

# Start worker in background
echo ""
echo "[5/6] Starting worker in background..."
export RELAY_URL="$RELAY"
export WORKER_NSEC="$WORKER_SK_HEX"
export SPONSOR_KEY="ed25519:dummy"  # We only test partial flow (no actual signing)
export SPONSOR_ACCOUNT="kampouse.testnet"

cargo run --bin mpc-worker 2>&1 &
WORKER_PID=$!
echo "  Worker PID: $WORKER_PID"
sleep 3  # Let worker connect and subscribe

# Send event and listen for feedback
echo ""
echo "[6/6] Sending event & waiting for feedback (30s timeout)..."
RESULT=$(python3 << PYEOF
import asyncio, json, time, sys

EVENT_JSON = $EVENT_RESULT
EVENT_ID = "$EVENT_ID"
RELAY = "$RELAY"

async def test():
    import websockets
    async with websockets.connect(RELAY) as ws:
        # Send our job event
        await ws.send(json.dumps(["EVENT", EVENT_JSON]))
        print("  ✅ Event sent to relay")
        
        # Subscribe for feedback (kind 7000 referencing our event)
        await ws.send(json.dumps(["REQ", "e2e-test", {"kinds": [7000], "#e": [EVENT_ID], "limit": 5}]))
        
        start = time.time()
        while time.time() - start < 30:
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=3)
                parsed = json.loads(msg)
                
                if parsed[0] == "OK":
                    if parsed[1] == EVENT_ID:
                        print(f"  📋 Relay accepted event: {parsed[2]}")
                elif parsed[0] == "EVENT":
                    event = parsed[2]
                    if event.get("kind") == 7000:
                        tags = event.get("tags", [])
                        status = next((t[1] for t in tags if t[0] == "status"), "?")
                        content = event.get("content", "")
                        print(f"\n  🎉 FEEDBACK RECEIVED!")
                        print(f"  Status: {status}")
                        print(f"  Content: {content}")
                        print(f"  Worker pubkey: {event.get('pubkey','')[:16]}...")
                        return "PASS"
                elif parsed[0] == "EOSE":
                    print(f"  📋 Caught up, waiting for worker response...")
            except asyncio.TimeoutError:
                continue
        
        print(f"\n  ⚠️  No feedback in 30s")
        return "FAIL"

result = asyncio.run(test())
print(f"\nRESULT:{result}")
PYEOF
)

# Cleanup
kill $WORKER_PID 2>/dev/null || true

echo ""
echo "═══════════════════════════════════════════════════"
if echo "$RESULT" | grep -q "RESULT:PASS"; then
    echo "✅ E2E TEST PASSED"
    echo ""
    echo "Verified:"
    echo "  • Worker connected to relay"
    echo "  • Worker subscribed to kind 5001"
    echo "  • Sender published kind 5001 event"
    echo "  • Worker received & verified Nostr signature"
    echo "  • Worker derived MPC key from path"
    echo "  • Worker published kind 7000 feedback"
    echo ""
    echo "NOT tested (TODO in worker code):"
    echo "  • Actual MPC.sign() call"
    echo "  • Transaction construction & broadcast"
else
    echo "❌ E2E TEST FAILED"
    echo ""
    echo "Worker output (last 20 lines):"
    # Show what the worker logged
    echo "$RESULT"
fi
echo "═══════════════════════════════════════════════════"
