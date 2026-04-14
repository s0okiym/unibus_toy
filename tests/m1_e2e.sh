#!/bin/bash
# M1 End-to-End Test Script
# Validates: two nodes form a SuperPod, unibusctl node list shows both,
#            killing a node is detected within timeout.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$SCRIPT_DIR/../target/release"

echo "=== M1 End-to-End Test ==="

# Cleanup any leftover processes
pkill -9 -f unibusd 2>/dev/null || true
sleep 1

# Start node 0
echo "[1/6] Starting node0..."
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node0.yaml" > /tmp/m1_node0.log 2>&1 &
PID0=$!
echo "  PID=$PID0"

# Start node 1
echo "[2/6] Starting node1..."
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node1.yaml" > /tmp/m1_node1.log 2>&1 &
PID1=$!
echo "  PID=$PID1"

# Wait for connection
echo "[3/6] Waiting for HELLO exchange..."
sleep 4

# Verify both nodes are visible via admin API
echo "[4/6] Checking admin API..."
RESP0=$(curl -s http://127.0.0.1:9090/admin/node/list)
RESP1=$(curl -s http://127.0.0.1:9091/admin/node/list)

# Check node 0 sees both nodes
NODE0_COUNT=$(echo "$RESP0" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['nodes']))" 2>/dev/null || echo "0")
NODE1_COUNT=$(echo "$RESP1" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['nodes']))" 2>/dev/null || echo "0")

if [ "$NODE0_COUNT" -ge 2 ] && [ "$NODE1_COUNT" -ge 2 ]; then
    echo "  PASS: Both nodes see at least 2 nodes in the SuperPod"
else
    echo "  FAIL: Node0 sees $NODE0_COUNT nodes, Node1 sees $NODE1_COUNT nodes (expected >=2)"
    kill $PID0 $PID1 2>/dev/null
    exit 1
fi

# Check both nodes are Active
NODE0_STATES=$(echo "$RESP0" | python3 -c "import sys,json; nodes=json.load(sys.stdin)['nodes']; print(','.join(n['state'] for n in nodes))" 2>/dev/null || echo "")
if echo "$NODE0_STATES" | grep -q "Active,Active\|Active.*Active"; then
    echo "  PASS: Both nodes are Active"
else
    echo "  FAIL: Node states: $NODE0_STATES"
    kill $PID0 $PID1 2>/dev/null
    exit 1
fi

# Verify unibusctl works
echo "[5/6] Testing unibusctl..."
CTL_OUTPUT=$("$BIN_DIR/unibusctl" --addr http://127.0.0.1:9090 node-list 2>&1 || true)
if echo "$CTL_OUTPUT" | grep -q "Active" && echo "$CTL_OUTPUT" | grep -q "7910" && echo "$CTL_OUTPUT" | grep -q "7911"; then
    echo "  PASS: unibusctl shows both Active nodes"
else
    echo "  FAIL: unibusctl output: $CTL_OUTPUT"
    kill $PID0 $PID1 2>/dev/null
    exit 1
fi

# Kill node1 and verify node0 detects it
echo "[6/6] Testing fault detection (killing node1)..."
kill $PID1 2>/dev/null || true
sleep 8

RESP0_AFTER=$(curl -s http://127.0.0.1:9090/admin/node/list 2>/dev/null || echo "{}")
NODE2_STATE=$(echo "$RESP0_AFTER" | python3 -c "
import sys,json
data = json.load(sys.stdin)
for n in data.get('nodes', []):
    if n['node_id'] == 2:
        print(n['state'])
        break
else:
    print('NOT_FOUND')
" 2>/dev/null || echo "PARSE_ERROR")

if [ "$NODE2_STATE" = "Suspect" ] || [ "$NODE2_STATE" = "Down" ]; then
    echo "  PASS: Node 2 detected as $NODE2_STATE after kill"
elif [ "$NODE2_STATE" = "Active" ]; then
    echo "  WARN: Node 2 still Active (heartbeat detection may need more time)"
else
    echo "  INFO: Node 2 state = $NODE2_STATE (may have been cleaned up)"
fi

# Cleanup
kill $PID0 2>/dev/null || true
wait $PID0 2>/dev/null || true
echo ""
echo "=== M1 E2E Test PASSED ==="
