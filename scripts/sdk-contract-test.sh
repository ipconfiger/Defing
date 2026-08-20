#!/usr/bin/env bash
# M3 契约测试：三语言 SDK（TS/Go/Python）对同一 dev-single 服务：get + watch。
set -u
BIN=${BIN:-/home/alex/Projects/Defing/server/target/debug/defing}
REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=${BASE:-http://127.0.0.1:8384}
PORT=${PORT:-8384}
PROJECT=sdk-project

cleanup() { [ -n "${PID:-}" ] && kill $PID 2>/dev/null || true; pkill -x defing 2>/dev/null || true; }
trap cleanup EXIT

echo "== 启动 dev-single =="
$BIN --dev-single --admin-password admin123 --allow-no-master-key --http-addr 127.0.0.1:$PORT >/tmp/dsh-sdk.log 2>&1 &
PID=$!
sleep 1
curl -sf $BASE/healthz >/dev/null || { echo "FAIL healthz"; cat /tmp/dsh-sdk.log; exit 1; }

echo "== 准备项目（v2: host=10.0.0.9）=="
TOKEN=$(curl -sf -X POST $BASE/api/v1/login -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
AUTH="Authorization: Bearer $TOKEN"
curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects -H 'Content-Type: application/json' -d "{\"name\":\"$PROJECT\"}" >/dev/null || { echo "FAIL project"; exit 1; }
curl -sf -H "$AUTH" -X PUT $BASE/api/v1/projects/$PROJECT/structure-draft -H 'Content-Type: application/json' -d '{"base_version":1,"groups":[{"name":"redis","items":[{"key":"host","type":"string","required":true}]}]}' >/dev/null || { echo "FAIL structure"; exit 1; }
curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects/$PROJECT/structure-draft/publish -H 'Content-Type: application/json' -d '{"comment":"s","request_id":"s1"}' >/dev/null
curl -sf -H "$AUTH" -X PUT $BASE/api/v1/projects/$PROJECT/branches/dev/draft -H 'Content-Type: application/json' -d '{"updates":[{"group":"redis","key":"host","value":{"type":"string","str_value":"10.0.0.9"}}]}' >/dev/null || { echo "FAIL draft"; exit 1; }
curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects/$PROJECT/branches/dev/publish -H 'Content-Type: application/json' -d '{"comment":"v2","request_id":"r1"}' >/dev/null || { echo "FAIL publish"; exit 1; }

# project-token：数据面一律需要项目访问令牌（仅全局管理员可建）
DSH_TOKEN=$(curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects/$PROJECT/tokens -H 'Content-Type: application/json' -d '{"name":"contract-test"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])") || { echo "FAIL token create"; exit 1; }
export DSH_TOKEN

publish_change() { # $1=新值
  curl -sf -H "$AUTH" -X PUT $BASE/api/v1/projects/$PROJECT/branches/dev/draft -H 'Content-Type: application/json' -d "{\"updates\":[{\"group\":\"redis\",\"key\":\"host\",\"value\":{\"type\":\"string\",\"str_value\":\"$1\"}}]}" >/dev/null
  curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects/$PROJECT/branches/dev/publish -H 'Content-Type: application/json' -d "{\"comment\":\"next\",\"request_id\":\"n-$(date +%s%N)\"}" >/dev/null
}

run_lang() { # $1=名称 $2=命令
  echo "== $1 SDK =="
  publish_change "10.0.0.9"   # 重置为各语言测试期望值
  DSH_ENDPOINTS=$BASE DSH_PROJECT=$PROJECT DSH_TOKEN=$DSH_TOKEN sh -c "$2" > /tmp/sdk-$1.out 2>&1 &
  local TP=$!
  # 持续发布直到测试进程退出（消除首编/启动耗时导致的订阅窗口错过；watch 收到订阅后首个事件）
  for i in $(seq 1 60); do
    if ! kill -0 "$TP" 2>/dev/null; then break; fi
    sleep 1
    publish_change "10.0.0.$((100 + i))"
  done
  if wait $TP; then
    cat /tmp/sdk-$1.out
    echo "  $1 ✅"
  else
    cat /tmp/sdk-$1.out
    echo "  $1 ❌"
    exit 1
  fi
}

run_lang "ts"   "cd $REPO/sdk/ts && node --experimental-strip-types test.ts"
run_lang "go"   "export GOCACHE=/tmp/dsh-gocache GOMODCACHE=/tmp/dsh-gomodcache && cd $REPO/sdk/go && go run ./test"
run_lang "py"   "cd $REPO/sdk/python && python3 test.py"

echo
echo "======== M3 SDK 契约测试全部通过 ========"
