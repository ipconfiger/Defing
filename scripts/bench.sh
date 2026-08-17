#!/usr/bin/env bash
# dsh 基准冒烟（A3）：读 QPS / 写 QPS（内存 vs redb 落盘对比）/ watch 延迟 / 二进制大小 / 内存 RSS。
# 用法: scripts/bench.sh [--release]
# 设计目标（design-v2 §12）：写 QPS ≥10k、watch ≥10k、发布→SDK ≤1s、内存 ≤128MB、二进制 ≤50MB(release)
# perf 方案①：写 QPS 对比行（WRITE_QPS_MEM / WRITE_QPS_REDB）——落盘模式为生产形态。
# 说明：写/读基准用 Python3 实现（零依赖，避免 Go 工具链跨机兼容问题）。
set -u
BIN=${BIN:-/home/alex/Projects/Defing/server/target/debug/dsh}
BASE=${BASE:-http://127.0.0.1:8384}
PORT=${PORT:-8384}
WORK=$(mktemp -d /tmp/dsh-bench.XXXXXX)
RELEASE=0
[ "${1:-}" = "--release" ] && RELEASE=1 && BIN=/home/alex/Projects/Defing/server/target/release/dsh

cleanup() { pkill -x dsh 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

# Python 写基准（草稿+发布循环，与 Go 版 writeBench 同语义）
cat > "$WORK/wbench.py" <<'PYEOF'
import json, time, urllib.request, uuid, sys
BASE = sys.argv[1]; TOKEN = open(sys.argv[2]).read().strip(); N = int(sys.argv[3])
def req(method, path, body=None):
    r = urllib.request.Request(BASE+path, method=method)
    r.add_header("Authorization", "Bearer "+TOKEN)
    r.add_header("Content-Type", "application/json")
    data = json.dumps(body).encode() if body is not None else None
    with urllib.request.urlopen(r, data=data) as resp:
        return resp.status
ok = 0; t0 = time.time()
for i in range(N):
    rid = f"b-{i}-{uuid.uuid4().hex[:6]}"
    try:
        req("PUT", "/api/v1/projects/bench-proj/branches/dev/draft",
            {"updates":[{"group":"g","key":"k","value":{"type":"string","str_value":"v"}}]})
        st = req("POST", "/api/v1/projects/bench-proj/branches/dev/publish",
            {"comment":"bench","request_id":rid})
        if st == 200: ok += 1
    except Exception:
        pass
el = time.time()-t0
print(f"WRITE_QPS={ok/el:.0f}")
PYEOF

# Python 读基准（数据面 snapshot 并发读）
cat > "$WORK/rbench.py" <<'PYEOF'
import json, time, threading, urllib.request, sys
BASE = sys.argv[1]; N = int(sys.argv[2]); C = int(sys.argv[3])
def get():
    r = urllib.request.Request(BASE+"/v1/projects/bench-proj/branches/dev/snapshot")
    with urllib.request.urlopen(r) as resp:
        return len(resp.read())
ok = [0]; lock = threading.Lock()
def worker():
    for _ in range(N // C):
        try:
            get()
            with lock: ok[0] += 1
        except Exception:
            pass
t0 = time.time()
threads = [threading.Thread(target=worker) for _ in range(C)]
for t in threads: t.start()
for t in threads: t.join()
el = time.time()-t0
print(f"READ_QPS={ok[0]/el:.0f}")
PYEOF

prep_project() {
    local base="$1" port="$2" data_dir="$3"
    if [ -n "$data_dir" ]; then
        $BIN --dev-single --admin-password admin123 --allow-no-master-key --http-addr 127.0.0.1:$port --data-dir "$data_dir" >"$WORK/server-$port.log" 2>&1 &
    else
        $BIN --dev-single --admin-password admin123 --allow-no-master-key --http-addr 127.0.0.1:$port >"$WORK/server-$port.log" 2>&1 &
    fi
    echo $! > "$WORK/pid-$port"
    for i in $(seq 1 20); do curl -sf "$base/healthz" >/dev/null 2>&1 && break; sleep 0.5; done
    curl -sf "$base/healthz" >/dev/null || { echo "FAIL: server not ready ($base)"; cat "$WORK/server-$port.log"; exit 1; }
    local token
    token=$(curl -sf -X POST "$base/api/v1/login" -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
    echo "$token" > "$WORK/token-$port"
    local auth="Authorization: Bearer $token"
    curl -sf -o /dev/null -H "$auth" -X POST "$base/api/v1/projects" -H 'Content-Type: application/json' -d '{"name":"bench-proj"}'
    curl -sf -o /dev/null -H "$auth" -X PUT "$base/api/v1/projects/bench-proj/structure-draft" -H 'Content-Type: application/json' -d '{"base_version":1,"groups":[{"name":"g","items":[{"key":"k","type":"string","required":true}]}]}'
    curl -sf -o /dev/null -H "$auth" -X POST "$base/api/v1/projects/bench-proj/structure-draft/publish" -H 'Content-Type: application/json' -d '{"comment":"s","request_id":"s1"}'
    curl -sf -o /dev/null -H "$auth" -X PUT "$base/api/v1/projects/bench-proj/branches/dev/draft" -H 'Content-Type: application/json' -d '{"updates":[{"group":"g","key":"k","value":{"type":"string","str_value":"v"}}]}'
    curl -sf -o /dev/null -H "$auth" -X POST "$base/api/v1/projects/bench-proj/branches/dev/publish" -H 'Content-Type: application/json' -d '{"comment":"v1","request_id":"r1"}'
}

echo "== 启动 dev-single（内存模式）=="
prep_project "http://127.0.0.1:$PORT" "$PORT" ""
PID=$(cat "$WORK/pid-$PORT")

echo "== 读基准（GET /snapshot 数据面）=="
python3 "$WORK/rbench.py" "http://127.0.0.1:$PORT" 20000 100

echo "== 写基准（内存模式）=="
python3 "$WORK/wbench.py" "http://127.0.0.1:$PORT" "$WORK/token-$PORT" 400
kill $PID 2>/dev/null; sleep 0.5

echo "== 启动 dev-single（redb 落盘模式，perf 方案①对比）=="
RB_PORT=$((PORT + 10))
RB_DIR="$WORK/redb-data"
prep_project "http://127.0.0.1:$RB_PORT" "$RB_PORT" "$RB_DIR"
RB_PID=$(cat "$WORK/pid-$RB_PORT")
echo "== 写基准（redb 落盘模式）=="
python3 "$WORK/wbench.py" "http://127.0.0.1:$RB_PORT" "$WORK/token-$RB_PORT" 200
kill $RB_PID 2>/dev/null; sleep 0.5

echo "== watch 延迟（发布 → SSE 事件，redb 模式）=="
RB_TOKEN=$(cat "$WORK/token-$RB_PORT")
(curl -sN --max-time 15 "http://127.0.0.1:$RB_PORT/v1/projects/bench-proj/branches/dev/watch" > "$WORK/events.txt" 2>/dev/null &
WATCH_PID=$!
sleep 0.5
T0=$(date +%s%N)
curl -sf -o /dev/null -H "Authorization: Bearer $RB_TOKEN" -X PUT "http://127.0.0.1:$RB_PORT/api/v1/projects/bench-proj/branches/dev/draft" -H 'Content-Type: application/json' -d '{"updates":[{"group":"g","key":"k","value":{"type":"string","str_value":"v2"}}]}'
curl -sf -o /dev/null -H "Authorization: Bearer $RB_TOKEN" -X POST "http://127.0.0.1:$RB_PORT/api/v1/projects/bench-proj/branches/dev/publish" -H 'Content-Type: application/json' -d '{"comment":"v2","request_id":"r2"}'
for i in $(seq 1 100); do grep -q "v2" "$WORK/events.txt" 2>/dev/null && break; sleep 0.01; done
T1=$(date +%s%N)
kill $WATCH_PID 2>/dev/null
echo "WATCH_LATENCY_MS=$(( (T1 - T0) / 1000000 ))")

echo "== 二进制大小 =="
SZ=$(stat -c%s "$BIN" 2>/dev/null || stat -f%z "$BIN")
echo "BINARY_BYTES=$SZ ($(( SZ / 1024 / 1024 ))MB)"
[ $RELEASE = 1 ] && { [ $(( SZ / 1024 / 1024 )) -le 50 ] && echo "  ✓ ≤50MB" || echo "  ✗ >50MB"; }

echo "== 内存 RSS（redb 模式）=="
RSS=$(ps -o rss= -p $RB_PID 2>/dev/null | tr -d ' ')
echo "RSS_KB=$RSS ($(( RSS / 1024 ))MB)"

echo "== bench done =="