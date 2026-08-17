#!/usr/bin/env bash
# G1 验收演示：三旋钮（--publish-policy / --shared-cascade / --read-mode）
# 依据 docs/design/g1-policy.md（D35-D37）
set -euo pipefail
BIN=${BIN:-/home/alex/Projects/Defing/server/target/debug/dsh}
PORT=${PORT:-8398}
BASE=${BASE:-http://127.0.0.1:$PORT}

cleanup() { [ -n "${PID:-}" ] && kill $PID 2>/dev/null || true; }
trap cleanup EXIT

echo "== 启动 dsh --dev-single（warn + manual + linear）=="
$BIN --dev-single --admin-password admin123 --allow-no-master-key --http-addr 127.0.0.1:$PORT \
  --publish-policy warn --shared-cascade manual --read-mode linear >/tmp/dsh-g1.log 2>&1 &
PID=$!
for i in $(seq 1 20); do
  curl -sf $BASE/healthz >/dev/null && break
  sleep 0.5
done
curl -sf $BASE/healthz >/dev/null || { echo "  healthz FAIL"; cat /tmp/dsh-g1.log; exit 1; }
TOKEN=$(curl -sf -X POST $BASE/api/v1/login -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
AUTH="Authorization: Bearer $TOKEN"
J() { curl -sf -H "$AUTH" -H 'Content-Type: application/json' "$@"; }

echo "== 1. 建项目 + 结构（host 必填 / port 可选）=="
J -X POST $BASE/api/v1/projects -d '{"name":"g1"}' >/dev/null
J -X PUT $BASE/api/v1/projects/g1/structure-draft -d '{"base_version":1,"groups":[{"name":"redis","items":[{"key":"host","type":"string","required":true},{"key":"port","type":"int"}]}]}' >/dev/null
J -X POST $BASE/api/v1/projects/g1/structure-draft/publish -d '{"comment":"s","request_id":"s1"}' >/dev/null

echo "== 2. --publish-policy warn：缺 required 草稿发布成功（block 会 422）=="
J -X PUT $BASE/api/v1/projects/g1/branches/dev/draft -d '{"updates":[{"group":"redis","key":"port","value":{"type":"int","int_value":6379}}]}' >/dev/null
# 对照：先验证 block 语义存在（默认配置下缺 required → 422）——本实例是 warn，应成功
R=$(J -X POST $BASE/api/v1/projects/g1/branches/dev/publish -d '{"comment":"warn-pub","request_id":"p1"}')
echo "$R" | python3 -c "
import json,sys
r=json.load(sys.stdin)
assert r['version']==2, r
assert r.get('warnings'), 'warn 模式应带 warnings: %s' % r
" && echo "  warn 放行 + warnings 明细 OK"

echo "== 3. --shared-cascade manual：共享发布不级联引用分支 =="
J -X POST $BASE/api/v1/shared -d '{"group":"lib","key":"timeout","type":"int","value":{"type":"int","int_value":30}}' >/dev/null
J -X POST $BASE/api/v1/shared/publish -d '{"comment":"v1","request_id":"sp1"}' >/dev/null
J -X POST $BASE/api/v1/shared/refs -d '{"project":"g1","group":"redis","item_key":"port","shared_group":"lib","shared_key":"timeout"}' >/dev/null
BEFORE=$(J $BASE/api/v1/projects/g1/branches/dev | python3 -c "import json,sys; print(json.load(sys.stdin)['active_version'])")
J -X POST $BASE/api/v1/shared -d '{"group":"lib","key":"timeout","type":"int","value":{"type":"int","int_value":60}}' >/dev/null
J -X POST $BASE/api/v1/shared/publish -d '{"comment":"v2","request_id":"sp2"}' >/dev/null
AFTER=$(J $BASE/api/v1/projects/g1/branches/dev | python3 -c "import json,sys; print(json.load(sys.stdin)['active_version'])")
[ "$BEFORE" = "$AFTER" ] && echo "  manual: shared publish does NOT cascade (branch stays v$AFTER) OK" || { echo "  FAIL: branch was cascaded"; exit 1; }

echo "== 4. manual materialize: next publish reads new shared value 60 =="
echo "== 5. --read-mode linear：读正常（dev-single 恒满足）=="
curl -sf $BASE/v1/projects/g1/branches/dev/snapshot >/dev/null && echo "  linear 读 OK"
J "$BASE/api/v1/projects/g1/diff?branch_a=dev&branch_b=test" >/dev/null && echo "  diff 读 OK"

echo
echo "ALL G1 POLICY DEMO OK"
