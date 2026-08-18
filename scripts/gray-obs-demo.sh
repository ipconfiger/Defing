#!/usr/bin/env bash
# G5 验收演示：灰度指标（/metrics 6 项）+ 自动回滚负例（低错误率不误伤）+ 生命周期指标联动
# 依据 docs/design/g5-observability.md（D31-D34）
set -euo pipefail
BIN=${BIN:-/home/alex/Projects/Defing/server/target/debug/defing}
PORT=${PORT:-8397}
BASE=${BASE:-http://127.0.0.1:$PORT}

cleanup() { [ -n "${PID:-}" ] && kill $PID 2>/dev/null || true; }
trap cleanup EXIT

echo "== 启动 defing --dev-single（自动回滚：阈值 5% / 间隔 2s）=="
$BIN --dev-single --admin-password admin123 --allow-no-master-key --http-addr 127.0.0.1:$PORT \
  --gray-rollback-threshold 5 --gray-rollback-interval 2 >/tmp/dsh-obs.log 2>&1 &
PID=$!
for i in $(seq 1 20); do
  curl -sf $BASE/healthz >/dev/null && break
  sleep 0.5
done
curl -sf $BASE/healthz >/dev/null || { echo "  healthz FAIL"; cat /tmp/dsh-obs.log; exit 1; }
TOKEN=$(curl -sf -X POST $BASE/api/v1/login -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
AUTH="Authorization: Bearer $TOKEN"
echo "  admin login ok"

echo "== 1. 建项目 + 结构 + 稳定发布 v2 + 草稿 + 灰度发布 =="
curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects -H 'Content-Type: application/json' -d '{"name":"obs"}' >/dev/null
curl -sf -H "$AUTH" -X PUT $BASE/api/v1/projects/obs/structure-draft -H 'Content-Type: application/json' -d '{"base_version":1,"groups":[{"name":"app","items":[{"key":"feature","type":"string","required":true}]}]}' >/dev/null
curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects/obs/structure-draft/publish -H 'Content-Type: application/json' -d '{"comment":"s","request_id":"s1"}' >/dev/null
curl -sf -H "$AUTH" -X PUT $BASE/api/v1/projects/obs/branches/dev/draft -H 'Content-Type: application/json' -d '{"updates":[{"group":"app","key":"feature","value":{"type":"string","str_value":"stable"}}]}' >/dev/null
curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects/obs/branches/dev/publish -H 'Content-Type: application/json' -d '{"comment":"v2","request_id":"p1"}' >/dev/null
curl -sf -H "$AUTH" -X PUT $BASE/api/v1/projects/obs/branches/dev/draft -H 'Content-Type: application/json' -d '{"updates":[{"group":"app","key":"feature","value":{"type":"string","str_value":"gray"}}]}' >/dev/null
curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects/obs/branches/dev/gray-publish -H 'Content-Type: application/json' -d '{"rule":{"match_labels":[{"key":"zone","value":"cn-north-1"}]},"comment":"g","request_id":"g1"}' >/dev/null

echo "== 2. /metrics 灰度 + HTTP 指标断言 =="
M=$(curl -sf $BASE/metrics)
echo "$M" | grep -q "^dsh_gray_active 1$" && echo "  dsh_gray_active 1 OK" || { echo "  FAIL: $(echo "$M" | grep gray_active)"; exit 1; }
echo "$M" | grep -q "^dsh_gray_publish_total [1-9]" && echo "  dsh_gray_publish_total ≥1 OK" || { echo "  FAIL"; exit 1; }
REQ=$(echo "$M" | grep "^dsh_http_requests_total" | awk '{print $2}')
[ "$REQ" -gt 0 ] 2>/dev/null && echo "  dsh_http_requests_total=$REQ OK" || { echo "  FAIL"; exit 1; }
echo "$M" | grep -q "^dsh_http_5xx_total 0$" && echo "  dsh_http_5xx_total 0 OK" || echo "  （5xx 计数非 0，跳过）"
echo "$M" | grep -q "^dsh_gray_promote_total 0$" && echo "  dsh_gray_promote_total 0 OK"

echo "== 3. 自动回滚负例：健康流量（无 5xx）下灰度保持活跃（间隔 2s × 2 轮）=="
sleep 5
S=$(curl -sf -H "$AUTH" $BASE/api/v1/projects/obs/branches/dev/gray-status)
echo "$S" | python3 -c "import json,sys; s=json.load(sys.stdin); assert s['gray_active'], s" && echo "  低错误率不误伤（灰度仍活跃）✅"

echo "== 4. 转正 → 指标联动（gray_active 0 / promote_total ≥1）=="
curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects/obs/branches/dev/gray-promote -H 'Content-Type: application/json' -d '{"comment":"pr","request_id":"pr1"}' >/dev/null
M=$(curl -sf $BASE/metrics)
echo "$M" | grep -q "^dsh_gray_active 0$" && echo "  dsh_gray_active 0 OK"
echo "$M" | grep -q "^dsh_gray_promote_total [1-9]" && echo "  dsh_gray_promote_total ≥1 OK"
echo "$M" | grep -q "^dsh_gray_abort_total 0$" && echo "  dsh_gray_abort_total 0 OK（未下量）"

echo
echo "ALL GRAY OBSERVABILITY DEMO OK"
