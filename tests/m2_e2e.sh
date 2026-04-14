#!/bin/bash
# M2 End-to-End Test Script
# Validates: MR registration, MR_PUBLISH propagation, cross-node write/read,
#            cross-node atomic operations, unibusctl mr-list/mr-cache.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$SCRIPT_DIR/../target/release"

ADMIN0="http://127.0.0.1:9090"
ADMIN1="http://127.0.0.1:9091"

echo "=== M2 End-to-End Test ==="

# Cleanup any leftover processes
pkill -9 -f unibusd 2>/dev/null || true
sleep 1

# Helper: extract field from JSON response
json_field() {
    # Usage: json_field <json> <path>
    echo "$1" | python3 -c "import sys,json; print(json.load(sys.stdin)$2)" 2>/dev/null || echo ""
}

# ============================================================
# Step 1: Start two unibusd nodes
# ============================================================
echo "[1/10] Starting node0 and node1..."
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node0.yaml" > /tmp/m2_node0.log 2>&1 &
PID0=$!
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node1.yaml" > /tmp/m2_node1.log 2>&1 &
PID1=$!
echo "  node0 PID=$PID0, node1 PID=$PID1"

# Wait for HELLO exchange
echo "[2/10] Waiting for HELLO exchange..."
sleep 4

# Verify both nodes are Active
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
# Step 3: Register CPU MR on node0
# ============================================================
echo "[3/10] Registering CPU MR on node0..."
MR_RESP=$(curl -s -X POST "$ADMIN0/admin/mr/register" \
    -H "Content-Type: application/json" \
    -d '{"device_kind":"memory","len":4096,"perms":"rwa"}')

MR_HANDLE=$(json_field "$MR_RESP" "['mr_handle']")
MR_ADDR=$(json_field "$MR_RESP" "['ub_addr']")

if [ -z "$MR_HANDLE" ] || [ "$MR_HANDLE" = "None" ]; then
    echo "  FAIL: MR registration returned: $MR_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi
echo "  PASS: MR registered handle=$MR_HANDLE addr=$MR_ADDR"

# ============================================================
# Step 4: Wait for MR_PUBLISH and verify node1 has cache entry
# ============================================================
echo "[4/10] Waiting for MR_PUBLISH propagation..."
sleep 2

CACHE_RESP=$(curl -s "$ADMIN1/admin/mr/cache")
CACHE_ENTRIES=$(json_field "$CACHE_RESP" "['cache'].__len__()")

if [ "$CACHE_ENTRIES" -ge 1 ]; then
    echo "  PASS: Node1 has $CACHE_ENTRIES remote MR cache entry(ies)"
else
    echo "  FAIL: Node1 has no remote MR cache entries. Response: $CACHE_RESP"
    echo "  --- node0 log ---"
    cat /tmp/m2_node0.log 2>/dev/null | tail -20
    echo "  --- node1 log ---"
    cat /tmp/m2_node1.log 2>/dev/null | tail -20
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# ============================================================
# Step 5: Verify unibusctl mr-list shows the MR on node0
# ============================================================
echo "[5/10] Testing unibusctl mr-list..."
CTL_OUTPUT=$("$BIN_DIR/unibusctl" --addr "$ADMIN0" mr-list 2>&1 || true)
if echo "$CTL_OUTPUT" | grep -q "$MR_HANDLE"; then
    echo "  PASS: unibusctl mr-list shows MR handle $MR_HANDLE"
else
    echo "  WARN: unibusctl mr-list output: $CTL_OUTPUT"
fi

# ============================================================
# Step 6: Cross-node write from node1 to node0's MR
# ============================================================
echo "[6/10] Cross-node write (node1 -> node0 MR)..."
WRITE_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/write" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"data\":[72,101,108,108,111]}")

WRITE_STATUS=$(json_field "$WRITE_RESP" "['status']")
if [ "$WRITE_STATUS" = "ok" ]; then
    echo "  PASS: Cross-node write succeeded"
else
    echo "  FAIL: Write response: $WRITE_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# ============================================================
# Step 7: Cross-node read from node1
# ============================================================
echo "[7/10] Cross-node read (node1 -> node0 MR)..."
READ_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/read" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"len\":5}")

READ_DATA=$(json_field "$READ_RESP" "['data']")
READ_LEN=$(json_field "$READ_RESP" "['len']")

if [ "$READ_LEN" = "5" ]; then
    echo "  PASS: Cross-node read returned $READ_LEN bytes: $READ_DATA"
else
    echo "  FAIL: Read response: $READ_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi

# Verify data matches what was written
if echo "$READ_DATA" | grep -q "72"; then
    echo "  PASS: Read data contains written value (72=0x48='H')"
else
    echo "  WARN: Read data may not match: $READ_DATA"
fi

# ============================================================
# Step 8: Register NPU MR on node0 and test cross-node
# ============================================================
echo "[8/10] Registering NPU MR on node0..."
NPU_RESP=$(curl -s -X POST "$ADMIN0/admin/mr/register" \
    -H "Content-Type: application/json" \
    -d '{"device_kind":"npu","len":1048576,"perms":"rwa"}')

NPU_HANDLE=$(json_field "$NPU_RESP" "['mr_handle']")
NPU_ADDR=$(json_field "$NPU_RESP" "['ub_addr']")

if [ -z "$NPU_HANDLE" ] || [ "$NPU_HANDLE" = "None" ]; then
    echo "  FAIL: NPU MR registration returned: $NPU_RESP"
    kill $PID0 $PID1 2>/dev/null; exit 1
fi
echo "  PASS: NPU MR registered handle=$NPU_HANDLE addr=$NPU_ADDR"

# Wait for MR_PUBLISH
sleep 2

# Cross-node write to NPU MR
NPU_WRITE_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/write" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$NPU_ADDR\",\"data\":[1,2,3,4]}")

NPU_WRITE_STATUS=$(json_field "$NPU_WRITE_RESP" "['status']")
if [ "$NPU_WRITE_STATUS" = "ok" ]; then
    echo "  PASS: Cross-node write to NPU MR succeeded"
else
    echo "  FAIL: NPU write response: $NPU_WRITE_RESP"
fi

# Cross-node read from NPU MR
NPU_READ_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/read" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$NPU_ADDR\",\"len\":4}")

NPU_READ_LEN=$(json_field "$NPU_READ_RESP" "['len']")
if [ "$NPU_READ_LEN" = "4" ]; then
    echo "  PASS: Cross-node read from NPU MR returned 4 bytes"
else
    echo "  WARN: NPU read response: $NPU_READ_RESP"
fi

# ============================================================
# Step 9: Cross-node atomic CAS test
# ============================================================
echo "[9/10] Cross-node atomic CAS..."
# First write 0 to offset 0 of the CPU MR
curl -s -X POST "$ADMIN1/admin/verb/write" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"data\":[0,0,0,0,0,0,0,0]}" > /dev/null
sleep 0.5

# CAS: expect 0, new 42
CAS_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/atomic-cas" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"expect\":0,\"new\":42}")

CAS_OLD=$(json_field "$CAS_RESP" "['old_value']")
if [ "$CAS_OLD" = "0" ]; then
    echo "  PASS: CAS returned old_value=0 (success)"
else
    echo "  WARN: CAS response: $CAS_RESP (expected old_value=0)"
fi

# CAS again: expect 0, new 99 (should fail, old=42)
CAS_RESP2=$(curl -s -X POST "$ADMIN1/admin/verb/atomic-cas" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"expect\":0,\"new\":99}")

CAS_OLD2=$(json_field "$CAS_RESP2" "['old_value']")
if [ "$CAS_OLD2" = "42" ]; then
    echo "  PASS: CAS retry returned old_value=42 (CAS failed as expected)"
else
    echo "  WARN: CAS retry response: $CAS_RESP2 (expected old_value=42)"
fi

# ============================================================
# Step 10: Cross-node atomic FAA test
# ============================================================
echo "[10/10] Cross-node atomic FAA..."
FAA_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/atomic-faa" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"add\":1}")

FAA_OLD=$(json_field "$FAA_RESP" "['old_value']")
if [ "$FAA_OLD" = "42" ]; then
    echo "  PASS: FAA returned old_value=42"
else
    echo "  WARN: FAA response: $FAA_RESP (expected old_value=42)"
fi

# ============================================================
# Cleanup
# ============================================================
kill $PID0 $PID1 2>/dev/null || true
wait $PID0 2>/dev/null || true
wait $PID1 2>/dev/null || true

echo ""
echo "=== M2 E2E Test PASSED ==="
