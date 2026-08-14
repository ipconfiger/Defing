#!/usr/bin/env bash
# dsh 基准冒烟（A3）：读 QPS / 写 QPS / watch 延迟 / 二进制大小 / 内存 RSS。
# 用法: scripts/bench.sh [--release]
# 设计目标（design-v2 §12）：写 QPS ≥10k、watch ≥10k、发布→SDK ≤1s、内存 ≤128MB、二进制 ≤50MB(release)
set -u
BIN=${BIN:-/home/alex/Projects/Defing/server/target/debug/dsh}
BASE=http://127.0.0.1:8384
PORT=8384
WORK=$(mktemp -d /tmp/dsh-bench.XXXXXX)
RELEASE=0
[ "${1:-}" = "--release" ] && RELEASE=1 && BIN=/home/alex/Projects/Defing/server/target/release/dsh

cleanup() { [ -n "${PID:-}" ] && kill $PID 2>/dev/null || true; pkill -x dsh 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

echo "== 启动 dev-single =="
$BIN --dev-single --admin-password admin123 --http-addr 127.0.0.1:$PORT >/tmp/dsh-bench.log 2>&1 &
PID=$!
sleep 1
curl -sf $BASE/healthz >/dev/null || { echo "FAIL: server not ready"; cat /tmp/dsh-bench.log; exit 1; }

TOKEN=$(curl -sf -X POST $BASE/api/v1/login -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
AUTH="Authorization: Bearer $TOKEN"
# 准备 bench 项目
curl -sf -o /dev/null -H "$AUTH" -X POST $BASE/api/v1/projects -H 'Content-Type: application/json' -d '{"name":"bench-proj"}'
curl -sf -o /dev/null -H "$AUTH" -X PUT $BASE/api/v1/projects/bench-proj/structure-draft -H 'Content-Type: application/json' -d '{"base_version":1,"groups":[{"name":"g","items":[{"key":"k","type":"string","required":true}]}]}'
curl -sf -o /dev/null -H "$AUTH" -X POST $BASE/api/v1/projects/bench-proj/structure-draft/publish -H 'Content-Type: application/json' -d '{"comment":"s","request_id":"s1"}'
curl -sf -o /dev/null -H "$AUTH" -X PUT $BASE/api/v1/projects/bench-proj/branches/dev/draft -H 'Content-Type: application/json' -d '{"updates":[{"group":"g","key":"k","value":{"type":"string","str_value":"v"}}]}'
curl -sf -o /dev/null -H "$AUTH" -X POST $BASE/api/v1/projects/bench-proj/branches/dev/publish -H 'Content-Type: application/json' -d '{"comment":"v1","request_id":"r1"}'

echo "== 读基准（GET /snapshot 数据面）=="
(cd scripts/bench && GOCACHE=/tmp/dsh-gocache go run main.go -base $BASE -read-n 40000 -read-c 200 -write-n 0) 2>&1 | grep -E "QPS"

echo "== 写基准（草稿+发布；单写者串行 apply，design-v2 注记）=="
(cd scripts/bench && GOCACHE=/tmp/dsh-gocache go run main.go -base $BASE -read-n 0 -read-c 1 -write-n 1000 -write-c 1 -token "$TOKEN") 2>&1 | grep -E "QPS"

echo "== watch 延迟（发布 → SSE 事件）=="
(curl -sN --max-time 15 "$BASE/v1/projects/bench-proj/branches/dev/watch" > "$WORK/events.txt" 2>/dev/null &
WATCH_PID=$!
sleep 0.5
T0=$(date +%s%N)
curl -sf -o /dev/null -H "$AUTH" -X PUT $BASE/api/v1/projects/bench-proj/branches/dev/draft -H 'Content-Type: application/json' -d '{"updates":[{"group":"g","key":"k","value":{"type":"string","str_value":"v2"}}]}'
curl -sf -o /dev/null -H "$AUTH" -X POST $BASE/api/v1/projects/bench-proj/branches/dev/publish -H 'Content-Type: application/json' -d '{"comment":"v2","request_id":"r2"}'
for i in $(seq 1 100); do grep -q "v2" "$WORK/events.txt" 2>/dev/null && break; sleep 0.01; done
T1=$(date +%s%N)
kill $WATCH_PID 2>/dev/null
echo "WATCH_LATENCY_MS=$(( (T1 - T0) / 1000000 ))")

echo "== 二进制大小 =="
SZ=$(stat -c%s "$BIN" 2>/dev/null || stat -f%z "$BIN")
echo "BINARY_BYTES=$SZ ($(( SZ / 1024 / 1024 ))MB)"
[ $RELEASE = 1 ] && { [ $(( SZ / 1024 / 1024 )) -le 50 ] && echo "  ✓ ≤50MB" || echo "  ✗ >50MB"; }

echo "== 内存 RSS =="
RSS=$(ps -o rss= -p $PID 2>/dev/null | tr -d ' ')
echo "RSS_KB=$RSS ($(( RSS / 1024 ))MB)"
[ -n "$RSS" ] && [ $(( RSS / 1024 )) -le 128 ] && echo "  ✓ ≤128MB" || echo "  （dev-single 内存态基准参考）"

echo "== bench done =="
