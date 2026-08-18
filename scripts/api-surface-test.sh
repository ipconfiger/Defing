#!/usr/bin/env bash
# P0 管理面契约补全 e2e：覆盖 openapi 中此前缺失的端点
#   项目详情/删除、分支详情/删除、分支对比、值提升、共享库 CRUD+发布、共享引用绑定、
#   （cluster/remove 由 cluster-demo 扩展覆盖，本脚本 dev-single 无 raft）
set -euo pipefail
BIN=${BIN:-/home/alex/Projects/Defing/server/target/debug/defing}
PORT=${PORT:-8384}
BASE=${BASE:-http://127.0.0.1:$PORT}

cleanup() { [ -n "${PID:-}" ] && kill $PID 2>/dev/null || true; }
trap cleanup EXIT

echo "== 启动 defing --dev-single =="
head -c 32 /dev/urandom > /tmp/dsh-api-surface.key
$BIN --dev-single --admin-password admin123 --http-addr 127.0.0.1:$PORT \
  --master-key-file /tmp/dsh-api-surface.key >/tmp/dsh-api-surface.log 2>&1 &
PID=$!
for i in $(seq 1 20); do
  curl -sf $BASE/healthz >/dev/null && break
  sleep 0.5
done
curl -sf $BASE/healthz >/dev/null || { echo "  healthz FAIL"; cat /tmp/dsh-api-surface.log; exit 1; }

AUTH="Authorization: Bearer $(curl -sf -X POST $BASE/api/v1/login -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")"
J() { curl -sf -H "$AUTH" -H 'Content-Type: application/json' "$@"; }

echo "== 1. 建项目 + 结构(host/port/password secret) + 发布 =="
J -X POST $BASE/api/v1/projects -d '{"name":"order-service"}' >/dev/null
J -X PUT $BASE/api/v1/projects/order-service/structure-draft -d '{"base_version":1,"groups":[{"name":"redis","items":[{"key":"host","type":"string","required":true},{"key":"port","type":"int"},{"key":"password","type":"secret","secret":true}]}]}' >/dev/null
J -X POST $BASE/api/v1/projects/order-service/structure-draft/publish -d '{"comment":"s","request_id":"s1"}' >/dev/null
J -X PUT $BASE/api/v1/projects/order-service/branches/dev/draft -d '{"updates":[{"group":"redis","key":"host","value":{"type":"string","str_value":"10.0.0.1"}},{"group":"redis","key":"port","value":{"type":"int","int_value":6379}},{"group":"redis","key":"password","value":{"type":"string","str_value":"s3cret"}}]}' >/dev/null
J -X POST $BASE/api/v1/projects/order-service/branches/dev/publish -d '{"comment":"v2","request_id":"r1"}' >/dev/null
J -X PUT $BASE/api/v1/projects/order-service/branches/test/draft -d '{"updates":[{"group":"redis","key":"host","value":{"type":"string","str_value":"10.0.0.2"}}]}' >/dev/null
J -X POST $BASE/api/v1/projects/order-service/branches/test/publish -d '{"comment":"t2","request_id":"r2"}' >/dev/null

echo "== 2. 项目详情 =="
D=$(J $BASE/api/v1/projects/order-service)
echo "$D" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['id']=='order-service' and d['name']=='order-service'" && echo "  project_detail OK"

echo "== 3. 分支详情(dev 含草稿信息) =="
J $BASE/api/v1/projects/order-service/branches/dev | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['active_version']==2 and d['structure_version']>=1" && echo "  branch_detail OK"

echo "== 4. 分支对比 dev vs test (host 不同 → diff; password 仅 dev → missing) =="
J "$BASE/api/v1/projects/order-service/diff?branch_a=dev&branch_b=test" > /tmp/diff.json
cat /tmp/diff.json | python3 -c "
import json,sys
d=json.load(sys.stdin)
kinds={x['key']:x for x in d['diffs']}
assert 'host' in kinds and kinds['host']['branch_a']['str_value']=='10.0.0.1' and kinds['host']['branch_b']['str_value']=='10.0.0.2', d
assert any('password' in m or 'port' in m for m in d['missing']), d
" && echo "  branch_diff OK"

echo "== 5. 值提升 dev → prod (草稿无冲突 → 全部 applied) =="
R=$(J -X POST $BASE/api/v1/projects/order-service/promote -d '{"from":"dev","to":"prod"}')
echo "$R" | python3 -c "import json,sys; r=json.load(sys.stdin); assert len(r['applied'])==3 and r['skipped']==[] and r['missing_from']==[], r" && echo "  promote OK"
echo "== 5b. 再 promote：prod 草稿已修改 → 全部 skipped（force=false）=="
R=$(J -X POST $BASE/api/v1/projects/order-service/promote -d '{"from":"dev","to":"prod"}')
echo "$R" | python3 -c "import json,sys; r=json.load(sys.stdin); assert len(r['skipped'])==3 and r['applied']==[], r" && echo "  promote idempotent-skip OK"
echo "== 5c. force=true 覆盖 =="
R=$(J -X POST $BASE/api/v1/projects/order-service/promote -d '{"from":"dev","to":"prod","force":true}')
echo "$R" | python3 -c "import json,sys; r=json.load(sys.stdin); assert len(r['applied'])==3, r" && echo "  promote force OK"
echo "== 5d. items 过滤 + missing_from（prod 草稿已有 host → skipped；不存在项 → missing_from）=="
R=$(J -X POST $BASE/api/v1/projects/order-service/promote -d '{"from":"dev","to":"prod","items":["redis/host","redis/nope"]}')
echo "$R" | python3 -c "import json,sys; r=json.load(sys.stdin); assert 'redis/host' in r['skipped'] and 'redis/nope' in r['missing_from'], r" && echo "  promote filter OK"

echo "== 6. 共享草稿 CRUD + 发布 =="
J -X POST $BASE/api/v1/shared -d '{"group":"lib","key":"timeout","type":"int","value":{"type":"int","int_value":30}}' >/dev/null
J -X PUT $BASE/api/v1/shared-draft -d '{"group":"lib","key":"timeout","type":"int","value":{"type":"int","int_value":60}}' >/dev/null
N=$(J $BASE/api/v1/shared-draft | python3 -c "import json,sys; print(len(json.load(sys.stdin)))")
[ "$N" = "1" ] && echo "  shared-draft list OK (1 draft)" || { echo "  shared-draft FAIL n=$N"; exit 1; }
SP=$(J -X POST $BASE/api/v1/shared/publish -d '{"comment":"lib v1","request_id":"sp1"}')
echo "$SP" | python3 -c "import json,sys; r=json.load(sys.stdin); assert r['version']==1, r" && echo "  shared publish OK"
J $BASE/api/v1/shared | python3 -c "import json,sys; l=json.load(sys.stdin); assert len(l)==1 and l[0]['key']=='timeout' and l[0]['version']==1, l" && echo "  shared list OK"

echo "== 7. secret 共享项：写明文→加密存储→列表脱敏 =="
J -X POST $BASE/api/v1/shared -d '{"group":"lib","key":"api-key","type":"secret","secret":true,"value":{"type":"string","str_value":"topsecret"}}' >/dev/null
J -X POST $BASE/api/v1/shared/publish -d '{"comment":"key","request_id":"sp2"}' >/dev/null
J $BASE/api/v1/shared | python3 -c "import json,sys; l=json.load(sys.stdin); sk=[x for x in l if x['key']=='api-key'][0]; assert sk['value'].get('masked')==True and 'topsecret' not in json.dumps(l), l" && echo "  secret shared masked OK"

echo "== 8. 共享引用绑定/解绑 =="
J -X POST $BASE/api/v1/shared/refs -d '{"project":"order-service","group":"redis","item_key":"port","shared_group":"lib","shared_key":"timeout"}' >/dev/null
J "$BASE/api/v1/shared/refs?project=order-service" | python3 -c "import json,sys; l=json.load(sys.stdin); assert len(l)==1 and l[0]['item_key']=='port', l" && echo "  ref bind+list OK"
J -X DELETE $BASE/api/v1/shared/refs -d '{"project":"order-service","group":"redis","item_key":"port"}' >/dev/null
J "$BASE/api/v1/shared/refs?project=order-service" | python3 -c "import json,sys; l=json.load(sys.stdin); assert l==[], l" && echo "  ref unbind OK"

echo "== 9. 自定义分支创建/详情/删除 =="
J -X POST $BASE/api/v1/projects/order-service/branches -d '{"name":"staging"}' >/dev/null
J $BASE/api/v1/projects/order-service/branches/staging >/dev/null
CODE=$(curl -s -H "$AUTH" -o /dev/null -w '%{http_code}' -X DELETE $BASE/api/v1/projects/order-service/branches/staging)
[ "$CODE" = "204" ] && echo "  branch delete OK (204)" || { echo "  branch delete FAIL $CODE"; exit 1; }

echo "== 10. 删除项目（force 校验 + 强制删除）=="
CODE=$(curl -s -H "$AUTH" -o /dev/null -w '%{http_code}' -X DELETE $BASE/api/v1/projects/order-service)
[ "$CODE" = "422" ] && echo "  delete without force → 422 OK" || { echo "  delete guard FAIL $CODE"; exit 1; }
CODE=$(curl -s -H "$AUTH" -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/v1/projects/order-service?force=true")
[ "$CODE" = "204" ] && echo "  project delete OK (204)" || { echo "  project delete FAIL $CODE"; exit 1; }
curl -s -H "$AUTH" $BASE/api/v1/projects | python3 -c "import json,sys; assert json.load(sys.stdin)==[], sys.stdin.read()" && echo "  project gone OK"

echo "== 11. secret 掩码策略（P0-b）：管理面/渲染/数据面默认掩码；reveal 需会话+审计 =="
J -X POST $BASE/api/v1/projects -d '{"name":"mask-test"}' >/dev/null
J -X PUT $BASE/api/v1/projects/mask-test/structure-draft -d '{"base_version":1,"groups":[{"name":"db","items":[{"key":"host","type":"string","required":true},{"key":"pass","type":"secret","secret":true}]}]}' >/dev/null
J -X POST $BASE/api/v1/projects/mask-test/structure-draft/publish -d '{"comment":"s","request_id":"s1"}' >/dev/null
J -X PUT $BASE/api/v1/projects/mask-test/branches/dev/draft -d '{"updates":[{"group":"db","key":"host","value":{"type":"string","str_value":"db1"}},{"group":"db","key":"pass","value":{"type":"string","str_value":"plainpass"}}]}' >/dev/null
J -X POST $BASE/api/v1/projects/mask-test/branches/dev/publish -d '{"comment":"v1","request_id":"r1"}' >/dev/null

# 管理面 config：默认掩码
C=$(J $BASE/api/v1/projects/mask-test/branches/dev/config)
echo "$C" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['groups']['db']['pass']=='***' and d['groups']['db']['host']=='db1', d" && echo "  admin config 默认掩码 OK"
# 管理面 config reveal=true → 明文 + 审计
C=$(J "$BASE/api/v1/projects/mask-test/branches/dev/config?reveal=true")
echo "$C" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['groups']['db']['pass']=='plainpass', d" && echo "  admin config reveal OK"
J $BASE/api/v1/audit | python3 -c "import json,sys; a=[x for x in json.load(sys.stdin) if x['action']=='config_reveal']; assert len(a)>=1, a" && echo "  config_reveal 审计 OK"
# 数据面 snapshot：secret 掩码
C=$(curl -sf $BASE/v1/projects/mask-test/branches/dev/snapshot)
echo "$C" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['groups']['db']['pass']=='***', d" && echo "  snapshot 数据面掩码 OK"
# 渲染端点：默认掩码（无会话）
R=$(curl -sf "$BASE/v1/projects/mask-test/branches/dev/config?format=json")
echo "$R" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['db']['pass']=='***', d" && echo "  render 默认掩码 OK"
# 渲染端点 reveal=true 无会话 → 401
CODE=$(curl -s -o /tmp/reveal-nosess.json -w '%{http_code}' "$BASE/v1/projects/mask-test/branches/dev/config?format=json&reveal=true")
[ "$CODE" = "401" ] && echo "  render reveal 无会话 → 401 OK" || { echo "  render reveal guard FAIL $CODE: $(cat /tmp/reveal-nosess.json)"; exit 1; }
# 渲染端点 reveal=true 带会话 → 明文 + 审计
R=$(curl -sf -H "$AUTH" "$BASE/v1/projects/mask-test/branches/dev/config?format=json&reveal=true")
echo "$R" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['db']['pass']=='plainpass', d" && echo "  render reveal 带会话 OK"
# 渲染端点 version 参数（历史版本；v1=结构空值版本，v2=含 db1 的值版本）
J -X PUT $BASE/api/v1/projects/mask-test/branches/dev/draft -d '{"updates":[{"group":"db","key":"host","value":{"type":"string","str_value":"db2"}}]}' >/dev/null
J -X POST $BASE/api/v1/projects/mask-test/branches/dev/publish -d '{"comment":"v3","request_id":"r2"}' >/dev/null
R=$(curl -sf "$BASE/v1/projects/mask-test/branches/dev/config?format=json&version=2")
echo "$R" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['db']['host']=='db1', d" && echo "  render version 参数 OK"

echo
echo "======== P0 API surface 全部通过 ========"

echo "== 12. 灰度管理面全链路（G4：4 端点 + 审计 + 数据面联动）=="
J -X POST $BASE/api/v1/projects -d '{"name":"gray-test"}' >/dev/null
J -X PUT $BASE/api/v1/projects/gray-test/structure-draft -d '{"base_version":1,"groups":[{"name":"app","items":[{"key":"feature","type":"string","required":true}]}]}' >/dev/null
J -X POST $BASE/api/v1/projects/gray-test/structure-draft/publish -d '{"comment":"s","request_id":"g-s1"}' >/dev/null
J -X PUT $BASE/api/v1/projects/gray-test/branches/dev/draft -d '{"updates":[{"group":"app","key":"feature","value":{"type":"string","str_value":"stable"}}]}' >/dev/null
J -X POST $BASE/api/v1/projects/gray-test/branches/dev/publish -d '{"comment":"stable v2","request_id":"g-p1"}' >/dev/null

# 无草稿 → 灰度发布 409（NoDraft）
CODE=$(curl -s -H "$AUTH" -o /dev/null -w '%{http_code}' -X POST $BASE/api/v1/projects/gray-test/branches/dev/gray-publish -H 'Content-Type: application/json' -d '{"rule":{"match_labels":[{"key":"zone","value":"cn-north-1"}]},"comment":"x","request_id":"g-e1"}')
[ "$CODE" = "409" ] && echo "  gray-publish 无草稿 → 409 OK" || { echo "  gray-publish guard FAIL $CODE"; exit 1; }

# 编辑草稿（灰度内容）→ 灰度发布
J -X PUT $BASE/api/v1/projects/gray-test/branches/dev/draft -d '{"updates":[{"group":"app","key":"feature","value":{"type":"string","str_value":"gray-feature"}}]}' >/dev/null
R=$(J -X POST $BASE/api/v1/projects/gray-test/branches/dev/gray-publish -d '{"rule":{"match_labels":[{"key":"zone","value":"cn-north-1"}]},"comment":"gray","request_id":"g-g1"}')
echo "$R" | python3 -c "import json,sys; r=json.load(sys.stdin); assert r['gray_seq']==1 and r['event_gray']==True, r" && echo "  gray-publish OK (gray_seq=1)"

# gray-status
S=$(J $BASE/api/v1/projects/gray-test/branches/dev/gray-status)
echo "$S" | python3 -c "import json,sys; s=json.load(sys.stdin); assert s['gray_active'] and s['gray_seq']==1 and s['gray_rule']['match_labels'][0]['value']=='cn-north-1', s" && echo "  gray-status OK"

# 数据面联动：命中 → gray=true + resolved_version=gray_seq；未命中 → gray=false
N=$(curl -sf $BASE/v1/projects/gray-test/branches/dev/snapshot -H 'X-Dsh-Instance: web-1' -H 'X-Dsh-Labels: zone=cn-north-1')
echo "$N" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['gray']==True and d['resolved_version']==1 and d['groups']['app']['feature']=='gray-feature', d" && echo "  数据面命中 → gray=true OK"
S2=$(curl -sf $BASE/v1/projects/gray-test/branches/dev/snapshot -H 'X-Dsh-Instance: web-2' -H 'X-Dsh-Labels: zone=cn-south-1')
echo "$S2" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['gray']==False and d['resolved_version']==2 and d['groups']['app']['feature']=='stable', d" && echo "  数据面未命中 → gray=false OK"

# 转正 → active 推进 + 状态清空
J -X POST $BASE/api/v1/projects/gray-test/branches/dev/gray-promote -d '{"comment":"promote","request_id":"g-pr1"}' >/dev/null
S=$(J $BASE/api/v1/projects/gray-test/branches/dev/gray-status)
echo "$S" | python3 -c "import json,sys; s=json.load(sys.stdin); assert s['active_version']==3 and not s['gray_active'], s" && echo "  gray-promote OK (active=3)"

# 再次灰度 + 下量 → 回落 + 状态清空
J -X PUT $BASE/api/v1/projects/gray-test/branches/dev/draft -d '{"updates":[{"group":"app","key":"feature","value":{"type":"string","str_value":"gray2"}}]}' >/dev/null
J -X POST $BASE/api/v1/projects/gray-test/branches/dev/gray-publish -d '{"rule":{"percentage":100},"comment":"g2","request_id":"g-g2"}' >/dev/null
R=$(J -X POST $BASE/api/v1/projects/gray-test/branches/dev/gray-abort -d '{"comment":"abort","request_id":"g-ab1"}')
echo "$R" | python3 -c "import json,sys; r=json.load(sys.stdin); assert r['fallback_version']==3, r" && echo "  gray-abort OK (fallback=3)"
S=$(J $BASE/api/v1/projects/gray-test/branches/dev/gray-status)
echo "$S" | python3 -c "import json,sys; s=json.load(sys.stdin); assert not s['gray_active'] and s['gray_seq']==0, s" && echo "  gray-status 清空 OK"

# 审计 action 覆盖
J "$BASE/api/v1/audit?action=gray_publish" | python3 -c "import json,sys; a=json.load(sys.stdin); assert len(a)>=2, a" && echo "  audit gray_publish OK"
J "$BASE/api/v1/audit?action=gray_promote" | python3 -c "import json,sys; a=json.load(sys.stdin); assert len(a)>=1, a" && echo "  audit gray_promote OK"
J "$BASE/api/v1/audit?action=gray_abort" | python3 -c "import json,sys; a=json.load(sys.stdin); assert len(a)>=1, a" && echo "  audit gray_abort OK"
