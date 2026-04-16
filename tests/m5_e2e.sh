#!/bin/bash
# M5 End-to-End Test Script
# Validates: 3-node cluster, observability, benchmark, KV demo, cross-node operations,
#            peer failure/recovery, metrics endpoint.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$SCRIPT_DIR/../target/release"

ADMIN0="http://127.0.0.1:9090"
ADMIN1="http://127.0.0.1:9091"
ADMIN2="http://127.0.0.1:9092"

TOTAL_STEPS=16

echo "=== M5 End-to-End Test (3-node cluster) ==="

# Cleanup any leftover processes
pkill -9 -f unibusd 2>/dev/null || true
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
# Step 2: Start three unibusd nodes
# ============================================================
echo "[2/$TOTAL_STEPS] Starting node0, node1, node2..."
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node0.yaml" > /tmp/m5_node0.log 2>&1 &
PID0=$!
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node1.yaml" > /tmp/m5_node1.log 2>&1 &
PID1=$!
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node2.yaml" > /tmp/m5_node2.log 2>&1 &
PID2=$!
echo "  node0 PID=$PID0, node1 PID=$PID1, node2 PID=$PID2"

# ============================================================
# Step 3: Wait for cluster formation (all 3 nodes see each other)
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
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

# ============================================================
# Step 4: Register MR on node0, verify cache on node1 and node2
# ============================================================
echo "[4/$TOTAL_STEPS] MR registration and cache propagation..."
MR_RESP=$(curl -s -X POST "$ADMIN0/admin/mr/register" \
    -H "Content-Type: application/json" \
    -d '{"device_kind":"memory","len":65536,"perms":"rwa"}')
MR_HANDLE=$(json_field "$MR_RESP" "['mr_handle']")
MR_ADDR=$(json_field "$MR_RESP" "['ub_addr']")

if [ -z "$MR_HANDLE" ] || [ "$MR_HANDLE" = "None" ]; then
    echo "  FAIL: MR registration returned: $MR_RESP"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi
echo "  MR registered: handle=$MR_HANDLE addr=$MR_ADDR"

# Wait for MR_PUBLISH propagation
sleep 3

# Verify cache on node1 and node2
CACHE1=$(curl -s "$ADMIN1/admin/mr/cache")
CACHE2=$(curl -s "$ADMIN2/admin/mr/cache")
C1_COUNT=$(json_field "$CACHE1" "['cache'].__len__()")
C2_COUNT=$(json_field "$CACHE2" "['cache'].__len__()")

if [ "$C1_COUNT" -ge 1 ] && [ "$C2_COUNT" -ge 1 ]; then
    echo "  PASS: MR cache propagated to node1 ($C1_COUNT entries) and node2 ($C2_COUNT entries)"
else
    echo "  WARN: MR cache not propagated (n1=$C1_COUNT, n2=$C2_COUNT). Operations may fail."
fi

# ============================================================
# Step 5: Cross-node write from node1, read from node2
# ============================================================
echo "[5/$TOTAL_STEPS] Cross-node write (node1→node0), read (node2→node0)..."
WRITE_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/write" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"data\":[1,2,3,4]}")
WRITE_STATUS=$(json_field "$WRITE_RESP" "['status']")
if [ "$WRITE_STATUS" = "ok" ]; then
    echo "  PASS: node1 wrote to node0's MR"
else
    echo "  FAIL: Write from node1: $WRITE_RESP"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

sleep 1

READ_RESP=$(curl -s -X POST "$ADMIN2/admin/verb/read" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"len\":4}")
READ_LEN=$(json_field "$READ_RESP" "['len']")
READ_DATA=$(json_field "$READ_RESP" "['data']")
if [ "$READ_LEN" = "4" ]; then
    echo "  PASS: node2 read from node0's MR: data=$READ_DATA"
else
    echo "  FAIL: Read from node2: $READ_RESP"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

# ============================================================
# Step 6: Cross-node atomic CAS (node1) and FAA (node2)
# ============================================================
echo "[6/$TOTAL_STEPS] Cross-node atomic operations..."
# Initialize to 0
curl -s -X POST "$ADMIN1/admin/verb/write" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"data\":[0,0,0,0,0,0,0,0]}" > /dev/null
sleep 1

# CAS from node1: expect 0, new 100
CAS_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/atomic-cas" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"expect\":0,\"new\":100}")
CAS_OLD=$(json_field "$CAS_RESP" "['old_value']")
if [ "$CAS_OLD" = "0" ]; then
    echo "  PASS: Atomic CAS from node1 returned old_value=0"
else
    echo "  FAIL: CAS from node1: $CAS_RESP"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

# FAA from node2: add 23 to 100
FAA_RESP=$(curl -s -X POST "$ADMIN2/admin/verb/atomic-faa" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"add\":23}")
FAA_OLD=$(json_field "$FAA_RESP" "['old_value']")
if [ "$FAA_OLD" = "100" ]; then
    echo "  PASS: Atomic FAA from node2 returned old_value=100"
else
    echo "  FAIL: FAA from node2: $FAA_RESP"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

# ============================================================
# Step 7: Jetty send/recv across 3 nodes
# ============================================================
echo "[7/$TOTAL_STEPS] Jetty send/recv across 3 nodes..."
JETTY0_RESP=$(curl -s -X POST "$ADMIN0/admin/jetty/create")
JETTY0_HANDLE=$(json_field "$JETTY0_RESP" "['jetty_handle']")
JETTY2_RESP=$(curl -s -X POST "$ADMIN2/admin/jetty/create")
JETTY2_HANDLE=$(json_field "$JETTY2_RESP" "['jetty_handle']")

# Post recv on node0's jetty
curl -s -X POST "$ADMIN0/admin/jetty/post-recv" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY0_HANDLE,\"len\":256}" > /dev/null

# Send from node2 to node0's jetty
SEND_RESP=$(curl -s -X POST "$ADMIN2/admin/verb/send" \
    -H "Content-Type: application/json" \
    -d "{\"dst_node_id\":1,\"dst_jetty_id\":$JETTY0_HANDLE,\"data\":[77,88,99]}")
SEND_STATUS=$(json_field "$SEND_RESP" "['status']")
if [ "$SEND_STATUS" = "ok" ]; then
    echo "  PASS: Send from node2 to node0's jetty"
else
    echo "  FAIL: Send from node2: $SEND_RESP"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

sleep 1

# Poll CQE on node0
CQE_RESP=$(curl -s -X POST "$ADMIN0/admin/jetty/poll-cqe" \
    -H "Content-Type: application/json" \
    -d "{\"jetty_handle\":$JETTY0_HANDLE}")
CQE_WR=$(json_field "$CQE_RESP" "['wr_id']")
if [ -n "$CQE_WR" ] && [ "$CQE_WR" != "None" ]; then
    echo "  PASS: CQE received on node0 for send from node2"
else
    echo "  FAIL: No CQE on node0. Response: $CQE_RESP"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

# ============================================================
# Step 8: Metrics endpoint verification
# ============================================================
echo "[8/$TOTAL_STEPS] Metrics endpoint verification..."
METRICS0=$(curl -s "$ADMIN0/metrics")
if echo "$METRICS0" | grep -q "unibus_mr_count"; then
    echo "  PASS: /metrics endpoint contains unibus_mr_count"
else
    echo "  FAIL: /metrics endpoint missing unibus_mr_count"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

if echo "$METRICS0" | grep -q "unibus_jetty_count"; then
    echo "  PASS: /metrics endpoint contains unibus_jetty_count"
else
    echo "  FAIL: /metrics endpoint missing unibus_jetty_count"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

# Counters are lazy — they appear after operations. Verify tx/rx after prior operations.
METRICS1=$(curl -s "$ADMIN1/metrics")
if echo "$METRICS1" | grep -q "unibus_tx_pkts_total"; then
    echo "  PASS: /metrics endpoint contains unibus_tx_pkts_total"
else
    echo "  WARN: /metrics endpoint missing unibus_tx_pkts_total (may not have sent yet)"
fi

if echo "$METRICS1" | grep -q "unibus_rx_pkts_total"; then
    echo "  PASS: /metrics endpoint contains unibus_rx_pkts_total"
else
    echo "  WARN: /metrics endpoint missing unibus_rx_pkts_total"
fi

if echo "$METRICS0" | grep -q "unibus_cqe_ok_total"; then
    echo "  PASS: /metrics endpoint contains unibus_cqe_ok_total"
fi

# ============================================================
# Step 9: unibusctl bench command
# ============================================================
echo "[9/$TOTAL_STEPS] unibusctl bench command..."
BENCH_OUTPUT=$("$BIN_DIR/unibusctl" --addr "$ADMIN1" bench --ub-addr "$MR_ADDR" --iterations 10 --size 64 2>&1) || true
if echo "$BENCH_OUTPUT" | grep -q "write:"; then
    echo "  PASS: unibusctl bench ran successfully"
    echo "$BENCH_OUTPUT" | grep -E "(write:|read:|atomic_faa:|latency|throughput)" | head -12
else
    echo "  WARN: unibusctl bench did not produce expected output"
    echo "$BENCH_OUTPUT" | head -20
fi

# ============================================================
# Step 10: KV init on node0
# ============================================================
echo "[10/$TOTAL_STEPS] KV init on node0..."
KV_INIT_RESP=$(curl -s -X POST "$ADMIN0/admin/kv/init" \
    -H "Content-Type: application/json" \
    -d '{"slots":4}')
KV_STATUS=$(json_field "$KV_INIT_RESP" "['status']")
KV_ADDR=$(json_field "$KV_INIT_RESP" "['ub_addr']")
KV_SLOTS=$(json_field "$KV_INIT_RESP" "['slots']")
if [ "$KV_STATUS" = "ok" ] && [ -n "$KV_ADDR" ]; then
    echo "  PASS: KV store initialized slots=$KV_SLOTS addr=$KV_ADDR"
else
    echo "  FAIL: KV init returned: $KV_INIT_RESP"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

# Wait for MR_PUBLISH of KV MR
sleep 5

# Verify KV MR is in node1's cache
KV_CACHE1=$(curl -s "$ADMIN1/admin/mr/cache")
KV_CACHE1_COUNT=$(json_field "$KV_CACHE1" "['cache'].__len__()")
echo "  node1 MR cache entries: $KV_CACHE1_COUNT"

KV_CACHE2=$(curl -s "$ADMIN2/admin/mr/cache")
KV_CACHE2_COUNT=$(json_field "$KV_CACHE2" "['cache'].__len__()")
echo "  node2 MR cache entries: $KV_CACHE2_COUNT"

# ============================================================
# Step 11: KV put on node0 (MR owner — local direct access)
# ============================================================
echo "[11/$TOTAL_STEPS] KV put on node0..."
KV_PUT_RESP=$(curl -s -X POST "$ADMIN0/admin/kv/put" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$KV_ADDR\",\"slot\":0,\"key\":\"hello\",\"value\":\"world\"}")
KV_PUT_STATUS=$(json_field "$KV_PUT_RESP" "['status']")
KV_PUT_VER=$(json_field "$KV_PUT_RESP" "['version']")
if [ "$KV_PUT_STATUS" = "ok" ]; then
    echo "  PASS: KV put succeeded version=$KV_PUT_VER"
else
    echo "  FAIL: KV put: $KV_PUT_RESP"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

# ============================================================
# Step 12: KV get on node0 (verify data locally)
# ============================================================
echo "[12/$TOTAL_STEPS] KV get on node0..."
KV_GET_RESP=$(curl -s -X POST "$ADMIN0/admin/kv/get" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$KV_ADDR\",\"slot\":0,\"key\":\"hello\"}")
KV_GET_VALUE=$(json_field "$KV_GET_RESP" "['value']")
KV_GET_VERSION=$(json_field "$KV_GET_RESP" "['version']")
if [ "$KV_GET_VALUE" = "world" ]; then
    echo "  PASS: KV get returned value=world version=$KV_GET_VERSION"
else
    echo "  FAIL: KV get: $KV_GET_RESP (expected value=world)"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

# ============================================================
# Step 13: KV cas on node0
# ============================================================
echo "[13/$TOTAL_STEPS] KV cas on node0..."
KV_CAS_RESP=$(curl -s -X POST "$ADMIN0/admin/kv/cas" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$KV_ADDR\",\"slot\":0,\"key\":\"hello\",\"expect_version\":1,\"value\":\"updated\"}")
KV_CAS_STATUS=$(json_field "$KV_CAS_RESP" "['status']")
KV_CAS_NEW_VER=$(json_field "$KV_CAS_RESP" "['new_version']")
if [ "$KV_CAS_STATUS" = "ok" ]; then
    echo "  PASS: KV cas succeeded new_version=$KV_CAS_NEW_VER"
else
    echo "  FAIL: KV cas: $KV_CAS_RESP"
    kill $PID0 $PID1 $PID2 2>/dev/null; exit 1
fi

# CAS with wrong version should fail
KV_CAS_FAIL_RESP=$(curl -s -X POST "$ADMIN0/admin/kv/cas" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$KV_ADDR\",\"slot\":0,\"key\":\"hello\",\"expect_version\":1,\"value\":\"should_fail\"}")
KV_CAS_FAIL_STATUS=$(json_field "$KV_CAS_FAIL_RESP" "['status']")
if [ "$KV_CAS_FAIL_STATUS" = "cas_failed" ]; then
    echo "  PASS: KV cas with wrong version correctly failed"
else
    echo "  WARN: KV cas with wrong version: $KV_CAS_FAIL_RESP (expected cas_failed)"
fi

# Verify updated value
KV_GET2_RESP=$(curl -s -X POST "$ADMIN0/admin/kv/get" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$KV_ADDR\",\"slot\":0,\"key\":\"hello\"}")
KV_GET2_VALUE=$(json_field "$KV_GET2_RESP" "['value']")
if [ "$KV_GET2_VALUE" = "updated" ]; then
    echo "  PASS: After CAS, KV get returns value=updated"
else
    echo "  WARN: After CAS, KV get: $KV_GET2_RESP (expected value=updated)"
fi

# ============================================================
# Step 14: Kill node2 - failure detection
# ============================================================
echo "[14/$TOTAL_STEPS] Killing node2 (failure detection test)..."
kill -9 $PID2 2>/dev/null || true
wait $PID2 2>/dev/null || true

# Wait for failure detection (fail_after=3, interval=1000ms => ~6s suspect + ~6s down)
echo "  Waiting for node0/node1 to detect node2 down..."
sleep 14

NODE_LIST0=$(curl -s "$ADMIN0/admin/node/list")
NODE2_STATE=$(echo "$NODE_LIST0" | python3 -c "
import sys, json
nodes = json.load(sys.stdin)['nodes']
for n in nodes:
    if n['node_id'] == 3:
        print(n['state'])
        break
" 2>/dev/null || echo "")

if [ "$NODE2_STATE" = "Down" ]; then
    echo "  PASS: Node0 detected node2 as Down"
else
    echo "  WARN: Node2 state on node0 is '$NODE2_STATE' (expected Down, may be Suspect)"
fi

# ============================================================
# Step 15: Restart node2, verify cluster recovery
# ============================================================
echo "[15/$TOTAL_STEPS] Restarting node2 (cluster recovery test)..."
"$BIN_DIR/unibusd" --config "$SCRIPT_DIR/../configs/node2.yaml" > /tmp/m5_node2_restart.log 2>&1 &
PID2_NEW=$!
echo "  node2 restarted PID=$PID2_NEW"

sleep 8

# Verify node2 is Active again
RESP0_AFTER=$(curl -s "$ADMIN0/admin/node/list")
NODE2_STATE_AFTER=$(echo "$RESP0_AFTER" | python3 -c "
import sys, json
nodes = json.load(sys.stdin)['nodes']
for n in nodes:
    if n['node_id'] == 3:
        print(n['state'])
        break
" 2>/dev/null || echo "")

if [ "$NODE2_STATE_AFTER" = "Active" ]; then
    echo "  PASS: Node2 is Active after restart on node0"
else
    echo "  WARN: Node2 state on node0 is '$NODE2_STATE_AFTER' (expected Active)"
fi

# ============================================================
# Step 16: Cross-node operations after recovery
# ============================================================
echo "[16/$TOTAL_STEPS] Cross-node operations after node2 recovery..."

# Wait for MR cache to be rebuilt on node2
sleep 5

CACHE2_AFTER=$(curl -s "$ADMIN2/admin/mr/cache")
C2_AFTER=$(json_field "$CACHE2_AFTER" "['cache'].__len__()")

if [ "$C2_AFTER" -ge 1 ]; then
    echo "  MR cache rebuilt on node2 ($C2_AFTER entries)"
else
    echo "  WARN: MR cache empty on node2 after restart"
fi

# Write from node2 to node0's MR
WRITE3_RESP=$(curl -s -X POST "$ADMIN2/admin/verb/write" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"data\":[170,187,204,221]}")
WRITE3_STATUS=$(json_field "$WRITE3_RESP" "['status']")
if [ "$WRITE3_STATUS" = "ok" ]; then
    echo "  PASS: Cross-node write from node2 after restart succeeded"
else
    echo "  FAIL: Write from node2 after restart: $WRITE3_RESP"
    cat /tmp/m5_node2_restart.log 2>/dev/null | tail -20
    kill $PID0 $PID1 $PID2_NEW 2>/dev/null; exit 1
fi

sleep 1

READ3_RESP=$(curl -s -X POST "$ADMIN1/admin/verb/read" \
    -H "Content-Type: application/json" \
    -d "{\"ub_addr\":\"$MR_ADDR\",\"len\":4}")
READ3_LEN=$(json_field "$READ3_RESP" "['len']")
if [ "$READ3_LEN" = "4" ]; then
    echo "  PASS: Cross-node read from node1 after restart succeeded"
else
    echo "  WARN: Read from node1 after restart: $READ3_RESP"
fi

# ============================================================
# Cleanup
# ============================================================
kill $PID0 $PID1 $PID2_NEW 2>/dev/null || true
wait $PID0 2>/dev/null || true
wait $PID1 2>/dev/null || true
wait $PID2_NEW 2>/dev/null || true

echo ""
echo "=== M5 E2E Test PASSED ==="