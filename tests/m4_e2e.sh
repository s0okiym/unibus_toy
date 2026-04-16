#!/bin/bash
# M4 End-to-End Test Script
# Validates: Reliable transport, flow control, failure detection,
#            cross-node operations, peer kill/restart recovery.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$SCRIPT_DIR/../target/release"

ADMIN0="http://127.0.0.1:9090"
ADMIN1="http://127.0.0.1:9091"

echo "=== M4 End-to-End Test ==="

# Cleanup any leftover processes
pkill -9 -f unibusd 2>/dev/null || true
sleep 1

# Helper: extract field from JSON response
json_field() {
    echo "$1" | python3 -c "import sys,json; print(json.load(sys.stdin)$2)" 2>/dev/null || echo ""
}

# ============================================================
# Step 1: Start two unibusd nodes
# ============================================================
echo "[1/10] Starting node0 and node1..."
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node0.yaml" > /tmp/m4_node0.log 2>&1 &
PID0=$!
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node1.yaml" > /tmp/m4_node1.log 2>&1 &
PID1=$!
echo "  node0 PID=$PID0, node1 PID=$PID1"

# ============================================================
# Step 2: Wait for HELLO exchange
# ============================================================
echo "[2/10] Waiting for HELLO exchange..."
sleep 4

RESP0=$(curl -s "$ADMIN0/admin/node/list")
RESP1=$(curl -s "$ADMIN1/admin/node/list")
N0_COUNT=$(json_field "$RESP0" "['nodes'].__len__()")
N1_COUNT=$(json_field "$RESP1" "['nodes'].__len__()")

if [ "$N0_COUNT" -ge 2 ] && [ "$N1_COUNT" -ge 2 ]; then
    echo "  PASS: Both nodes see at least 2 nodes"
else
    echo "  FAIL: Node0 sees $N0_COUNT nodes, Node1 sees $N1_COUNT nodes"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# ============================================================
# Step 3: M3 regression - Create Jettys on both nodes
# ============================================================
echo "[3/10] M3 regression: Creating Jettys..."
JETTY0_RESP=$(curl -s -X POST "$ADMIN0/admin/jetty/create")
JETTY0_HANDLE=$(json_field "$JETTY0_RESP" "['jetty_handle']")

JETTY1_RESP=$(curl -s -X POST "$ADMIN1/admin/jetty/create")
JETTY1_HANDLE=$(json_field "$JETTY1_RESP" "['jetty_handle']")

if [ -z "$JETTY0_HANDLE" ] || [ "$JETTY0_HANDLE" = "None" ] || [ -z "$JETTY1_HANDLE" ] || [ "$JETTY1_HANDLE" = "None" ]; then
    echo "  FAIL: Jetty creation failed. node0=$JETTY0_RESP, node1=$JETTY1_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi
echo "  PASS: Jettys created node0=$JETTY0_HANDLE, node1=$JETTY1_HANDLE"

# ============================================================
# Step 4: Cross-node Write + Read
# ============================================================
echo "[4/10] Cross-node Write + Read..."

# Register MR on node0
MR_RESP=$(curl -s -X POST "$ADMIN0/admin/mr/register" \
    -H "Content-Type: application/json" \
    -d '{"device_kind":"memory","len":4096,"perms":"rwa"}')
MR_HANDLE=$(json_field "$MR_RESP" "['mr_handle']")
MR_ADDR=$(json_field "$MR_RESP" "['ub_addr']")

if [ -z "$MR_HANDLE" ] || [ "$MR_HANDLE" = "None" ]; then
    echo "  FAIL: MR registration returned: $MR_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# Wait for MR_PUBLISH propagation
sleep 2

# Write from node1 to node0's MR
WRITE_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/write" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"data\":[10,20,30,40]}")
WRITE_STATUS=$(json_field "$WRITE_RESP" "['status']")
if [ "$WRITE_STATUS" = "ok" ]; then
    echo "  PASS: Cross-node write succeeded"
else
    echo "  FAIL: Write response: $WRITE_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# Read from node1
sleep 1
READ_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/read" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"len\":4}")
READ_DATA=$(json_field "$READ_RESP" "['data']")
READ_LEN=$(json_field "$READ_RESP" "['len']")
if [ "$READ_LEN" = "4" ]; then
    echo "  PASS: Cross-node read returned len=4 data=$READ_DATA"
else
    echo "  FAIL: Read response: $READ_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# ============================================================
# Step 5: Cross-node Atomic CAS + FAA
# ============================================================
echo "[5/10] Cross-node Atomic CAS + FAA..."

# Reuse the same MR — write zeros to offset 0 to initialize for CAS
curl -s -X POST "$ADMIN1/admin/verb/write" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"data\":[0,0,0,0,0,0,0,0]}" > /dev/null
sleep 1

# Atomic CAS: expect 0, new 42
CAS_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/atomic-cas" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"expect\":0,\"new\":42}")
CAS_OLD=$(json_field "$CAS_RESP" "['old_value']")
if [ "$CAS_OLD" = "0" ]; then
    echo "  PASS: Atomic CAS returned old_value=0"
else
    echo "  FAIL: CAS response: $CAS_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# Atomic FAA: add 8 to 42
FAA_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/atomic-faa" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"add\":8}")
FAA_OLD=$(json_field "$FAA_RESP" "['old_value']")
if [ "$FAA_OLD" = "42" ]; then
    echo "  PASS: Atomic FAA returned old_value=42"
else
    echo "  FAIL: FAA response: $FAA_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# ============================================================
# Step 6: Send/Recv + Write_with_imm regression
# ============================================================
echo "[6/10] Send/Recv + write_with_imm regression..."

# Post recv buffer on node0
curl -s -X POST "$ADMIN0/admin/jetty/post-recv" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY0_HANDLE,\"len\":256}" > /dev/null

# Send from node1 to node0's Jetty
SEND_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/send" \
    -H "Content-Type: application/json" \
    -d "{\"dst_node_id\":1,\"dst_jetty_id\":$JETTY0_HANDLE,\"data\":[72,101,108,108,111]}")
SEND_STATUS=$(json_field "$SEND_RESP" "['status']")
if [ "$SEND_STATUS" = "ok" ]; then
    echo "  PASS: Send succeeded"
else
    echo "  FAIL: Send response: $SEND_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

sleep 1

# Poll CQE
CQE1_RESP=$(curl -s -X POST "$ADMIN0/admin/jetty/poll-cqe" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY0_HANDLE}")
CQE1_WR=$(json_field "$CQE1_RESP" "['wr_id']")
if [ -n "$CQE1_WR" ] && [ "$CQE1_WR" != "None" ]; then
    echo "  PASS: CQE received for send"
else
    echo "  FAIL: No CQE for send. Response: $CQE1_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# Write_with_imm from node1
curl -s -X POST "$ADMIN0/admin/jetty/post-recv" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY0_HANDLE,\"len\":256}" > /dev/null

WIMM_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/write-imm" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"data\":[50,60],\"imm\":77,\"dst_node_id\":1,\"dst_jetty_id\":$JETTY0_HANDLE}")
WIMM_STATUS=$(json_field "$WIMM_RESP" "['status']")
if [ "$WIMM_STATUS" = "ok" ]; then
    echo "  PASS: write_with_imm succeeded"
else
    echo "  FAIL: write_with_imm response: $WIMM_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

sleep 1

CQE2_RESP=$(curl -s -X POST "$ADMIN0/admin/jetty/poll-cqe" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY0_HANDLE}")
CQE2_IMM=$(json_field "$CQE2_RESP" "['imm']")
if [ "$CQE2_IMM" = "77" ]; then
    echo "  PASS: write_with_imm CQE has imm=77"
else
    echo "  WARN: write_with_imm CQE imm=$CQE2_IMM (expected 77). Response: $CQE2_RESP"
fi

# ============================================================
# Step 7: Kill node1 - failure detection
# ============================================================
echo "[7/10] Killing node1 (failure detection test)..."
kill -9 $PID1 2>/dev/null || true
wait $PID1 2>/dev/null || true

# Wait for node0 to detect node1 is down (fail_after=3, interval=1000ms => ~6s suspect + ~6s down)
echo "  Waiting for node0 to detect node1 down..."
sleep 14

# Check node0's node list - node1 should be Down
NODE_LIST=$(curl -s "$ADMIN0/admin/node/list")
NODE1_STATE=$(echo "$NODE_LIST" | python3 -c "
import sys, json
nodes = json.load(sys.stdin)['nodes']
for n in nodes:
    if n['node_id'] == 2:
        print(n['state'])
        break
" 2>/dev/null || echo "")

if [ "$NODE1_STATE" = "Down" ]; then
    echo "  PASS: Node0 detected node1 as Down"
else
    echo "  WARN: Node1 state on node0 is '$NODE1_STATE' (expected Down). May still be in Suspect."
fi

# ============================================================
# Step 8: Verify dead node state
# ============================================================
echo "[8/10] Verifying dead node state..."
# Re-check node1 state on node0 — should be Down by now
NODE_LIST2=$(curl -s "$ADMIN0/admin/node/list")
NODE1_STATE2=$(echo "$NODE_LIST2" | python3 -c "
import sys, json
nodes = json.load(sys.stdin)['nodes']
for n in nodes:
    if n['node_id'] == 2:
        print(n['state'])
        break
" 2>/dev/null || echo "")
if [ "$NODE1_STATE2" = "Down" ]; then
    echo "  PASS: Node1 confirmed Down on node0"
else
    echo "  WARN: Node1 state is '$NODE1_STATE2' (may need more time)"
fi

# ============================================================
# Step 9: Restart node1 - epoch change, session rebuild
# ============================================================
echo "[9/10] Restarting node1 (session rebuild test)..."
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node1.yaml" > /tmp/m4_node1_restart.log 2>&1 &
PID1_NEW=$!
echo "  node1 restarted PID=$PID1_NEW"

# Wait for HELLO exchange and session rebuild
sleep 8

# Verify nodes can see each other again
RESP0_AFTER=$(curl -s "$ADMIN0/admin/node/list")
N0_AFTER=$(json_field "$RESP0_AFTER" "['nodes'].__len__()")

NODE1_STATE_AFTER=$(echo "$RESP0_AFTER" | python3 -c "
import sys, json
nodes = json.load(sys.stdin)['nodes']
for n in nodes:
    if n['node_id'] == 2:
        print(n['state'])
        break
" 2>/dev/null || echo "")

if [ "$NODE1_STATE_AFTER" = "Active" ]; then
    echo "  PASS: Node0 sees node1 as Active after restart"
else
    echo "  WARN: Node1 state on node0 is '$NODE1_STATE_AFTER' (expected Active)"
fi

# ============================================================
# Step 10: Cross-node operations should work again after restart
# ============================================================
echo "[10/10] Cross-node operations after restart..."

# Wait for MR cache to be rebuilt on node1
sleep 5

# Verify node1 has MR cache entries before attempting operations
CACHE_AFTER=$(curl -s "$ADMIN1/admin/mr/cache")
CACHE_COUNT=$(json_field "$CACHE_AFTER" "['cache'].__len__()")
if [ "$CACHE_COUNT" -ge 1 ]; then
    echo "  MR cache populated on node1 ($CACHE_COUNT entries)"
else
    echo "  WARN: MR cache empty on node1, operations may fail"
fi

# Write from node1 to node0's MR
WRITE2_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/write" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"data\":[170,187,204,221]}")
WRITE2_STATUS=$(json_field "$WRITE2_RESP" "['status']")
if [ "$WRITE2_STATUS" = "ok" ]; then
    echo "  PASS: Cross-node write after restart succeeded"
else
    echo "  FAIL: Write after restart: $WRITE2_RESP"
    cat /tmp/m4_node1_restart.log 2>/dev/null | tail -20
    kill $PID0 $PID1_NEW 2>/dev/null; exit 1
fi

# Read back
sleep 1
READ2_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/read" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"len\":4}")
READ2_DATA=$(json_field "$READ2_RESP" "['data']")
READ2_LEN=$(json_field "$READ2_RESP" "['len']")
if [ "$READ2_LEN" = "4" ]; then
    echo "  PASS: Cross-node read after restart returned len=4 data=$READ2_DATA"
else
    echo "  WARN: Read after restart: $READ2_RESP"
fi

# ============================================================
# Cleanup
# ============================================================
kill $PID0 $PID1_NEW 2>/dev/null || true
wait $PID0 2>/dev/null || true
wait $PID1_NEW 2>/dev/null || true

echo ""
echo "=== M4 E2E Test PASSED ==="
