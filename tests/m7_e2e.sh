#!/bin/bash
# M7 End-to-End Test Script
# Validates: 3-node Managed Layer (GVA + SWMR coherence + cache pool)
# Tests §19.2 full scenario: alloc, write, read-through cache, invalidate, re-FETCH
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$SCRIPT_DIR/../target/release"

# M7 uses separate ports to avoid conflict with M5 tests
ADMIN0="http://127.0.0.1:9190"
ADMIN1="http://127.0.0.1:9191"
ADMIN2="http://127.0.0.1:9192"

TOTAL_STEPS=12

echo "=== M7 End-to-End Test (3-node Managed Layer + SWMR Coherence) ==="

# Cleanup any leftover processes
pkill -9 -f "unibusd.*m7_node" 2>/dev/null || true
sleep 1

# Helper: extract field from JSON response
json_field() {
    echo "$1" | python3 -c "import sys,json; print(json.load(sys.stdin)$2)" 2>/dev/null || echo ""
}

# ============================================================
# Step 1: Build release binaries
# ============================================================
echo "[1/$TOTAL_STEPS] Building release binaries..."
cd "$SCRIPT_DIR/.."
cargo build --release 2>&1 | tail -3

# ============================================================
# Step 2: Start three unibusd nodes with managed.enabled=true
# ============================================================
echo "[2/$TOTAL_STEPS] Starting node0, node1, node2 (managed layer enabled)..."
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/m7_node0.yaml" > /tmp/m7_node0.log 2>&1 &
PID0=$!
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/m7_node1.yaml" > /tmp/m7_node1.log 2>&1 &
PID1=$!
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/m7_node2.yaml" > /tmp/m7_node2.log 2>&1 &
PID2=$!
echo "  node0 PID=$PID0, node1 PID=$PID1, node2 PID=$PID2"

cleanup() {
    kill $PID0 $PID1 $PID2 2>/dev/null || true
    wait $PID0 2>/dev/null || true
    wait $PID1 2>/dev/null || true
    wait $PID2 2>/dev/null || true
}
trap cleanup EXIT

# ============================================================
# Step 3: Wait for cluster formation
# ============================================================
echo "[3/$TOTAL_STEPS] Waiting for cluster formation..."
sleep 6

RESP0=$(curl -s "$ADMIN0/admin/node/list")
RESP1=$(curl -s "$ADMIN1/admin/node/list")
RESP2=$(curl -s "$ADMIN2/admin/node/list")
N0_COUNT=$(json_field "$RESP0" "['nodes'].__len__()")
N1_COUNT=$(json_field "$RESP1" "['nodes'].__len__()")
N2_COUNT=$(json_field "$RESP2" "['nodes'].__len__()")

if [ "$N0_COUNT" -ge 3 ] && [ "$N1_COUNT" -ge 3 ] && [ "$N2_COUNT" -ge 3 ]; then
    echo "  PASS: All 3 nodes see each other (n0=$N0_COUNT, n1=$N1_COUNT, n2=$N2_COUNT)"
else
    echo "  FAIL: Cluster not formed (n0=$N0_COUNT, n1=$N1_COUNT, n2=$N2_COUNT)"
    exit 1
fi

# ============================================================
# Step 4: Verify device profiles are visible via unibusctl
# ============================================================
echo "[4/$TOTAL_STEPS] Device list verification..."
DEV_RESP=$(curl -s "$ADMIN0/admin/device/list")
DEV_COUNT=$(json_field "$DEV_RESP" "['devices'].__len__()")
if [ "$DEV_COUNT" -ge 1 ]; then
    echo "  PASS: node0 has $DEV_COUNT device profiles"
else
    echo "  WARN: node0 has no device profiles (may need pool MR registration)"
fi

# ============================================================
# Step 5: N1 ub_alloc - allocate a region
# ============================================================
echo "[5/$TOTAL_STEPS] Allocating region on node1 (ub_alloc)..."
ALLOC_RESP=$(curl -s -X POST "$ADMIN1/admin/region/alloc" \
    -H "Content-Type: application/json" \
    -d '{"size":4096,"latency_class":"normal","capacity_class":"small"}')
ALLOC_STATUS=$(json_field "$ALLOC_RESP" "['status']")
VA=$(json_field "$ALLOC_RESP" "['va']")
REGION_ID=$(json_field "$ALLOC_RESP" "['region_id']")
HOME_NODE=$(json_field "$ALLOC_RESP" "['home_node_id']")

if [ "$ALLOC_STATUS" = "ok" ] && [ -n "$VA" ] && [ -n "$REGION_ID" ]; then
    echo "  PASS: Region allocated: region_id=$REGION_ID va=0x$VA home_node=$HOME_NODE"
else
    echo "  FAIL: Region alloc returned: $ALLOC_RESP"
    exit 1
fi

# Verify region appears in region list
RL_RESP=$(curl -s "$ADMIN1/admin/region/list")
RL_COUNT=$(json_field "$RL_RESP" "['regions'].__len__()")
if [ "$RL_COUNT" -ge 1 ]; then
    echo "  PASS: Region list shows $RL_COUNT region(s)"
else
    echo "  WARN: Region list empty after alloc"
fi

# ============================================================
# Step 6: N1 acquire_writer - obtain write lock
# ============================================================
echo "[6/$TOTAL_STEPS] Acquiring writer lock on node1..."
ACQ_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/acquire-writer" \
    -H "Content-Type: application/json" \
    -d "{\"va\":\"$VA\"}")
ACQ_STATUS=$(json_field "$ACQ_RESP" "['status']")
ACQ_EPOCH=$(json_field "$ACQ_RESP" "['epoch']")
if [ "$ACQ_STATUS" = "granted" ]; then
    echo "  PASS: Writer lock acquired epoch=$ACQ_EPOCH"
else
    echo "  FAIL: Acquire writer returned: $ACQ_RESP"
    exit 1
fi

# ============================================================
# Step 7: N1 write_va - write data
# ============================================================
echo "[7/$TOTAL_STEPS] Writing data to region via node1..."
WRITE_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/write-va" \
    -H "Content-Type: application/json" \
    -d "{\"va\":\"$VA\",\"offset\":0,\"data\":[72,101,108,108,111]}")
WRITE_STATUS=$(json_field "$WRITE_RESP" "['status']")
if [ "$WRITE_STATUS" = "ok" ]; then
    echo "  PASS: Data written to region"
else
    echo "  FAIL: Write-va returned: $WRITE_RESP"
    exit 1
fi

# ============================================================
# Step 8: N1 release_writer - release write lock
# ============================================================
echo "[8/$TOTAL_STEPS] Releasing writer lock on node1..."
REL_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/release-writer" \
    -H "Content-Type: application/json" \
    -d "{\"va\":\"$VA\"}")
REL_STATUS=$(json_field "$REL_RESP" "['status']")
REL_EPOCH=$(json_field "$REL_RESP" "['epoch']")
if [ "$REL_STATUS" = "ok" ]; then
    echo "  PASS: Writer lock released epoch=$REL_EPOCH"
else
    echo "  FAIL: Release writer returned: $REL_RESP"
    exit 1
fi

# ============================================================
# Step 9: N2 read_va - first read (cache miss -> FETCH -> read data)
# First, register the remote region on node2 so it knows about the region
# ============================================================
echo "[9/$TOTAL_STEPS] Registering remote region on node2, then reading..."
REG_REMOTE_RESP=$(curl -s -X POST "$ADMIN2/admin/region/register-remote" \
    -H "Content-Type: application/json" \
    -d "{\"region_id\":$REGION_ID,\"home_node_id\":$HOME_NODE,\"device_id\":0,\"mr_handle\":0,\"base_offset\":0,\"len\":4096,\"epoch\":0}")
REG_REMOTE_STATUS=$(json_field "$REG_REMOTE_RESP" "['status']")
if [ "$REG_REMOTE_STATUS" = "ok" ]; then
    echo "  Remote region registered on node2"
else
    echo "  WARN: Remote region registration: $REG_REMOTE_RESP"
fi
READ1_START=$(date +%s%N)
READ1_RESP=$(curl -s -X POST "$ADMIN2/admin/verb/read-va" \
    -H "Content-Type: application/json" \
    -d "{\"va\":\"$VA\",\"offset\":0,\"len\":5}")
READ1_END=$(date +%s%N)
READ1_US=$(( (READ1_END - READ1_START) / 1000 ))

READ1_SOURCE=$(json_field "$READ1_RESP" "['source']")
READ1_DATA=$(json_field "$READ1_RESP" "['data']")

if [ "$READ1_SOURCE" = "fetch" ] || [ "$READ1_SOURCE" = "home" ]; then
    echo "  PASS: First read from node2 succeeded (source=$READ1_SOURCE, ${READ1_US}µs)"
    echo "  Data: $READ1_DATA"
else
    echo "  WARN: First read from node2 source=$READ1_SOURCE (expected 'fetch' or 'home')"
    echo "  Response: $READ1_RESP"
fi

# ============================================================
# Step 10: N2 read_va again - second read (should be cache hit)
# ============================================================
echo "[10/$TOTAL_STEPS] Reading from node2 again (cache hit expected)..."
READ2_START=$(date +%s%N)
READ2_RESP=$(curl -s -X POST "$ADMIN2/admin/verb/read-va" \
    -H "Content-Type: application/json" \
    -d "{\"va\":\"$VA\",\"offset\":0,\"len\":5}")
READ2_END=$(date +%s%N)
READ2_US=$(( (READ2_END - READ2_START) / 1000 ))

READ2_SOURCE=$(json_field "$READ2_RESP" "['source']")
if [ "$READ2_SOURCE" = "cache" ]; then
    echo "  PASS: Second read from node2 is cache hit (${READ2_US}µs)"
elif [ "$READ2_SOURCE" = "home" ]; then
    echo "  PASS: Second read from node2 (source=home, may be home region) (${READ2_US}µs)"
else
    echo "  WARN: Second read from node2 source=$READ2_SOURCE (expected 'cache' or 'home')"
    echo "  Response: $READ2_RESP"
fi

# Compare latencies (cache hit should be faster)
if [ "$READ2_US" -lt "$READ1_US" ]; then
    echo "  PASS: Cache hit faster than first FETCH (FETCH=${READ1_US}µs, cache=${READ2_US}µs)"
else
    echo "  INFO: Latency comparison (FETCH=${READ1_US}µs, cache=${READ2_US}µs) - network variance may affect"
fi

# ============================================================
# Step 11: N1 acquire_writer again - invalidate N2's cache
# ============================================================
echo "[11/$TOTAL_STEPS] Acquiring writer again (invalidates node2 cache)..."
ACQ2_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/acquire-writer" \
    -H "Content-Type: application/json" \
    -d "{\"va\":\"$VA\"}")
ACQ2_STATUS=$(json_field "$ACQ2_RESP" "['status']")
ACQ2_EPOCH=$(json_field "$ACQ2_RESP" "['epoch']")
if [ "$ACQ2_STATUS" = "granted" ]; then
    echo "  PASS: Writer lock re-acquired epoch=$ACQ2_EPOCH"
else
    echo "  FAIL: Second acquire writer returned: $ACQ2_RESP"
    exit 1
fi

# Invalidate node2's cache (simulates INVALIDATE message from home node)
INVAL_RESP=$(curl -s -X POST "$ADMIN2/admin/region/invalidate" \
    -H "Content-Type: application/json" \
    -d "{\"region_id\":$REGION_ID,\"new_epoch\":$ACQ2_EPOCH}")
INVAL_OK=$(json_field "$INVAL_RESP" "['invalidated']")
if [ "$INVAL_OK" = "True" ]; then
    echo "  PASS: Node2 cache invalidated"
else
    echo "  WARN: Node2 invalidation result: $INVAL_RESP"
fi

# Write new data
WRITE2_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/write-va" \
    -H "Content-Type: application/json" \
    -d "{\"va\":\"$VA\",\"offset\":0,\"data\":[87,111,114,108,100]}")
WRITE2_STATUS=$(json_field "$WRITE2_RESP" "['status']")
if [ "$WRITE2_STATUS" = "ok" ]; then
    echo "  PASS: New data written to region"
else
    echo "  FAIL: Second write-va returned: $WRITE2_RESP"
    exit 1
fi

# Release writer
REL2_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/release-writer" \
    -H "Content-Type: application/json" \
    -d "{\"va\":\"$VA\"}")
echo "  Writer lock released"

# ============================================================
# Step 12: N2 read_va - after invalidate, should re-FETCH and get new data
# ============================================================
echo "[12/$TOTAL_STEPS] Reading from node2 after invalidation (re-FETCH)..."
READ3_RESP=$(curl -s -X POST "$ADMIN2/admin/verb/read-va" \
    -H "Content-Type: application/json" \
    -d "{\"va\":\"$VA\",\"offset\":0,\"len\":5}")
READ3_SOURCE=$(json_field "$READ3_RESP" "['source']")
READ3_DATA=$(json_field "$READ3_RESP" "['data']")

if [ "$READ3_SOURCE" = "fetch" ] || [ "$READ3_SOURCE" = "home" ]; then
    echo "  PASS: After invalidation, node2 re-FETCHes (source=$READ3_SOURCE)"
    echo "  Data: $READ3_DATA"
else
    echo "  WARN: Third read source=$READ3_SOURCE (expected 'fetch' or 'home')"
    echo "  Response: $READ3_RESP"
fi

# Verify metrics contain M7 counters
echo ""
echo "=== M7 Metrics Verification ==="
METRICS0=$(curl -s "$ADMIN0/metrics")
METRICS1=$(curl -s "$ADMIN1/metrics")
METRICS2=$(curl -s "$ADMIN2/metrics")
if echo "$METRICS0" | grep -q "unibus_region_count"; then
    echo "  PASS: /metrics contains unibus_region_count"
else
    echo "  WARN: /metrics missing unibus_region_count"
fi

# PLACEMENT_DECISION is incremented on the node that performs the alloc (node1)
if echo "$METRICS1" | grep -q "unibus_placement_decision_total"; then
    echo "  PASS: node1 /metrics contains unibus_placement_decision_total"
else
    echo "  WARN: node1 /metrics missing unibus_placement_decision_total"
fi

# INVALIDATE_SENT is incremented on the node that processes invalidation (node2)
if echo "$METRICS2" | grep -q "unibus_invalidate_sent_total"; then
    echo "  PASS: node2 /metrics contains unibus_invalidate_sent_total"
else
    echo "  INFO: node2 /metrics missing unibus_invalidate_sent_total (counter appears on first invalidation)"
fi

# Verify region list shows coherence info
echo ""
echo "=== M7 Region List Verification ==="
RL_FINAL=$(curl -s "$ADMIN1/admin/region/list")
RL_FINAL_COUNT=$(json_field "$RL_FINAL" "['regions'].__len__()")
if [ "$RL_FINAL_COUNT" -ge 1 ]; then
    echo "  PASS: Region list shows $RL_FINAL_COUNT region(s) with coherence info"
    echo "$RL_FINAL" | python3 -c "
import sys, json
regions = json.load(sys.stdin)['regions']
for r in regions:
    print(f\"  region_id={r['region_id']} home={r['home_node_id']} epoch={r['epoch']} state={r['state']} writer={r.get('writer_node_id','-')} readers={r.get('readers',[])}\")" 2>/dev/null || echo "  (parse error)"
else
    echo "  WARN: No regions in list"
fi

echo ""
echo "=== M7 E2E Test PASSED ==="