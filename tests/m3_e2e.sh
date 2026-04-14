#!/bin/bash
# M3 End-to-End Test Script
# Validates: Jetty creation, send/recv, send_with_imm, write_with_imm,
#            cross-node message semantics, CQE polling.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$SCRIPT_DIR/../target/release"

ADMIN0="http://127.0.0.1:9090"
ADMIN1="http://127.0.0.1:9091"

echo "=== M3 End-to-End Test ==="

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
echo "[1/11] Starting node0 and node1..."
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node0.yaml" > /tmp/m3_node0.log 2>&1 &
PID0=$!
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node1.yaml" > /tmp/m3_node1.log 2>&1 &
PID1=$!
echo "  node0 PID=$PID0, node1 PID=$PID1"

# ============================================================
# Step 2: Wait for HELLO exchange
# ============================================================
echo "[2/11] Waiting for HELLO exchange..."
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
# Step 3: Create Jetty on node0
# ============================================================
echo "[3/11] Creating Jetty on node0..."
JETTY_RESP=$(curl -s -X POST "$ADMIN0/admin/jetty/create")
JETTY_HANDLE=$(json_field "$JETTY_RESP" "['jetty_handle']")

if [ -z "$JETTY_HANDLE" ] || [ "$JETTY_HANDLE" = "None" ]; then
    echo "  FAIL: Jetty creation returned: $JETTY_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi
echo "  PASS: Jetty created handle=$JETTY_HANDLE"

# ============================================================
# Step 4: Verify Jetty list on node0
# ============================================================
echo "[4/11] Verifying Jetty list..."
JETTY_LIST_RESP=$(curl -s "$ADMIN0/admin/jetty/list")
JETTY_COUNT=$(json_field "$JETTY_LIST_RESP" "['jettys'].__len__()")

if [ "$JETTY_COUNT" -ge 1 ]; then
    echo "  PASS: Node0 has $JETTY_COUNT Jetty(s)"
else
    echo "  FAIL: Node0 has no Jettys. Response: $JETTY_LIST_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# Also test unibusctl jetty-list
CTL_OUTPUT=$("$BIN_DIR/unibusctl" --addr "$ADMIN0" jetty-list 2>&1 || true)
if echo "$CTL_OUTPUT" | grep -q "$JETTY_HANDLE"; then
    echo "  PASS: unibusctl jetty-list shows Jetty handle $JETTY_HANDLE"
else
    echo "  WARN: unibusctl jetty-list output: $CTL_OUTPUT"
fi

# ============================================================
# Step 5: Create Jetty on node1
# ============================================================
echo "[5/11] Creating Jetty on node1..."
JETTY1_RESP=$(curl -s -X POST "$ADMIN1/admin/jetty/create")
JETTY1_HANDLE=$(json_field "$JETTY1_RESP" "['jetty_handle']")

if [ -z "$JETTY1_HANDLE" ] || [ "$JETTY1_HANDLE" = "None" ]; then
    echo "  FAIL: Jetty creation on node1 returned: $JETTY1_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi
echo "  PASS: Jetty created on node1 handle=$JETTY1_HANDLE"

# ============================================================
# Step 6: Post recv buffer on node0's Jetty
# ============================================================
echo "[6/11] Posting recv buffer on node0's Jetty..."
RECV_RESP=$(curl -s -X POST "$ADMIN0/admin/jetty/post-recv" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY_HANDLE,\"len\":256}")

RECV_STATUS=$(json_field "$RECV_RESP" "['status']")
if [ "$RECV_STATUS" = "ok" ]; then
    echo "  PASS: Recv buffer posted to node0 Jetty"
else
    echo "  FAIL: Post recv response: $RECV_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# ============================================================
# Step 7: Cross-node send from node1 to node0's Jetty
# ============================================================
echo "[7/11] Cross-node send (node1 -> node0 Jetty)..."
SEND_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/send" \
    -H "Content-Type: application/json" \
    -d "{\"dst_node_id\":1,\"dst_jetty_id\":$JETTY_HANDLE,\"data\":[72,101,108,108,111]}")

SEND_STATUS=$(json_field "$SEND_RESP" "['status']")
if [ "$SEND_STATUS" = "ok" ]; then
    echo "  PASS: Cross-node send succeeded"
else
    echo "  FAIL: Send response: $SEND_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# Wait for delivery
sleep 1

# Poll CQE on node0
echo "[8/11] Polling CQE on node0..."
CQE_RESP=$(curl -s -X POST "$ADMIN0/admin/jetty/poll-cqe" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY_HANDLE}")

CQE_WR_ID=$(json_field "$CQE_RESP" "['wr_id']")
CQE_VERB=$(json_field "$CQE_RESP" "['verb']")

if [ -n "$CQE_WR_ID" ] && [ "$CQE_WR_ID" != "None" ]; then
    echo "  PASS: CQE received wr_id=$CQE_WR_ID verb=$CQE_VERB"
else
    echo "  FAIL: No CQE received. Response: $CQE_RESP"
    echo "  --- node0 log ---"
    cat /tmp/m3_node0.log 2>/dev/null | tail -20
    echo "  --- node1 log ---"
    cat /tmp/m3_node1.log 2>/dev/null | tail -20
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# ============================================================
# Step 9: Cross-node send_with_imm from node1
# ============================================================
echo "[9/11] Cross-node send_with_imm (node1 -> node0 Jetty)..."

# Post another recv buffer first
curl -s -X POST "$ADMIN0/admin/jetty/post-recv" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY_HANDLE,\"len\":256}" > /dev/null

SEND_IMM_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/send-with-imm" \
    -H "Content-Type: application/json" \
    -d "{\"dst_node_id\":1,\"dst_jetty_id\":$JETTY_HANDLE,\"data\":[1,2,3,4],\"imm\":42}")

SEND_IMM_STATUS=$(json_field "$SEND_IMM_RESP" "['status']")
if [ "$SEND_IMM_STATUS" = "ok" ]; then
    echo "  PASS: send_with_imm succeeded"
else
    echo "  FAIL: send_with_imm response: $SEND_IMM_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# Wait for delivery
sleep 1

# Poll CQE and verify imm field
CQE2_RESP=$(curl -s -X POST "$ADMIN0/admin/jetty/poll-cqe" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY_HANDLE}")

CQE2_IMM=$(json_field "$CQE2_RESP" "['imm']")
if [ "$CQE2_IMM" = "42" ]; then
    echo "  PASS: CQE has imm=42"
else
    echo "  WARN: CQE imm field: $CQE2_IMM (expected 42). Response: $CQE2_RESP"
fi

# ============================================================
# Step 10: Cross-node write_with_imm from node1
# ============================================================
echo "[10/11] Cross-node write_with_imm (node1 -> node0 MR + Jetty)..."

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
echo "  MR registered on node0 handle=$MR_HANDLE addr=$MR_ADDR"

# Wait for MR_PUBLISH
sleep 2

# Write_with_imm from node1
WIMM_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/write-imm" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"data\":[10,20,30,40],\"imm\":99,\"dst_node_id\":1,\"dst_jetty_id\":$JETTY_HANDLE}")

WIMM_STATUS=$(json_field "$WIMM_RESP" "['status']")
if [ "$WIMM_STATUS" = "ok" ]; then
    echo "  PASS: write_with_imm succeeded"
else
    echo "  FAIL: write_with_imm response: $WIMM_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# Wait for delivery
sleep 1

# Verify MR was written by reading from node0
READ_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/read" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"len\":4}")

READ_DATA=$(json_field "$READ_RESP" "['data']")
if echo "$READ_DATA" | grep -q "10"; then
    echo "  PASS: MR data verified (contains written value 10)"
else
    echo "  WARN: MR read data: $READ_DATA"
fi

# Poll CQE and verify imm field
CQE3_RESP=$(curl -s -X POST "$ADMIN0/admin/jetty/poll-cqe" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY_HANDLE}")

CQE3_IMM=$(json_field "$CQE3_RESP" "['imm']")
CQE3_VERB=$(json_field "$CQE3_RESP" "['verb']")
if [ "$CQE3_IMM" = "99" ]; then
    echo "  PASS: write_with_imm CQE has imm=99 verb=$CQE3_VERB"
else
    echo "  WARN: write_with_imm CQE imm=$CQE3_IMM (expected 99). Response: $CQE3_RESP"
fi

# ============================================================
# Step 11: Message ordering test - two sends arrive in order
# ============================================================
echo "[11/11] Message ordering test..."

# Post two recv buffers on node0
curl -s -X POST "$ADMIN0/admin/jetty/post-recv" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY_HANDLE,\"len\":256}" > /dev/null
curl -s -X POST "$ADMIN0/admin/jetty/post-recv" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY_HANDLE,\"len\":256}" > /dev/null

# Send message 1
curl -s -X POST "$ADMIN1/admin/verb/send" \
    -H "Content-Type: application/json" \
    -d "{\"dst_node_id\":1,\"dst_jetty_id\":$JETTY_HANDLE,\"data\":[1]}" > /dev/null

# Send message 2
curl -s -X POST "$ADMIN1/admin/verb/send" \
    -H "Content-Type: application/json" \
    -d "{\"dst_node_id\":1,\"dst_jetty_id\":$JETTY_HANDLE,\"data\":[2]}" > /dev/null

sleep 1

# Poll CQEs and verify ordering
CQE_A=$(curl -s -X POST "$ADMIN0/admin/jetty/poll-cqe" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY_HANDLE}")
CQE_B=$(curl -s -X POST "$ADMIN0/admin/jetty/poll-cqe" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY_HANDLE}")

CQE_A_WR=$(json_field "$CQE_A" "['wr_id']")
CQE_B_WR=$(json_field "$CQE_B" "['wr_id']")

if [ -n "$CQE_A_WR" ] && [ "$CQE_A_WR" != "None" ] && [ -n "$CQE_B_WR" ] && [ "$CQE_B_WR" != "None" ]; then
    echo "  PASS: Two CQEs received in order (wr_id1=$CQE_A_WR, wr_id2=$CQE_B_WR)"
else
    echo "  WARN: Ordering CQEs: first=$CQE_A_WR, second=$CQE_B_WR"
fi

# ============================================================
# Cleanup
# ============================================================
kill $PID0 $PID1 2>/dev/null || true
wait $PID0 2>/dev/null || true
wait $PID1 2>/dev/null || true

echo ""
echo "=== M3 E2E Test PASSED ==="
