#!/usr/bin/env bash
# P1 SDK gRPC 契约测试：三语言（TS/Go/Python）对同一 dev-single 的 :8383 数据面：
# GetConfig / GetItem / Watch / ListMembers。
set -u
BIN=${BIN:-/home/alex/Projects/Defing/server/target/debug/dsh}
REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=http://127.0.0.1:8384
PORT=8384
PROJECT=sdk-project

cleanup() { [ -n "${PID:-}" ] && kill $PID 2>/dev/null || true; pkill -x dsh 2>/dev/null || true; }
trap cleanup EXIT

echo "== 启动 dev-single（HTTP :8384 / gRPC :8383）=="
$BIN --dev-single --admin-password admin123 --http-addr 127.0.0.1:$PORT >/tmp/dsh-sdk-grpc.log 2>&1 &
PID=$!
sleep 1
curl -sf $BASE/healthz >/dev/null || { echo "FAIL healthz"; cat /tmp/dsh-sdk-grpc.log; exit 1; }

echo "== 准备项目 =="
TOKEN=$(curl -sf -X POST $BASE/api/v1/login -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
AUTH="Authorization: Bearer $TOKEN"
curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects -H 'Content-Type: application/json' -d "{\"name\":\"$PROJECT\"}" >/dev/null || { echo "FAIL project"; exit 1; }
curl -sf -H "$AUTH" -X PUT $BASE/api/v1/projects/$PROJECT/structure-draft -H 'Content-Type: application/json' -d '{"base_version":1,"groups":[{"name":"redis","items":[{"key":"host","type":"string","required":true}]}]}' >/dev/null
curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects/$PROJECT/structure-draft/publish -H 'Content-Type: application/json' -d '{"comment":"s","request_id":"s1"}' >/dev/null
curl -sf -H "$AUTH" -X PUT $BASE/api/v1/projects/$PROJECT/branches/dev/draft -H 'Content-Type: application/json' -d '{"updates":[{"group":"redis","key":"host","value":{"type":"string","str_value":"10.0.0.9"}}]}' >/dev/null
curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects/$PROJECT/branches/dev/publish -H 'Content-Type: application/json' -d '{"comment":"v2","request_id":"r1"}' >/dev/null

publish_change() { # $1=新值
  curl -sf -H "$AUTH" -X PUT $BASE/api/v1/projects/$PROJECT/branches/dev/draft -H 'Content-Type: application/json' -d "{\"updates\":[{\"group\":\"redis\",\"key\":\"host\",\"value\":{\"type\":\"string\",\"str_value\":\"$1\"}}]}" >/dev/null
  curl -sf -H "$AUTH" -X POST $BASE/api/v1/projects/$PROJECT/branches/dev/publish -H 'Content-Type: application/json' -d "{\"comment\":\"next\",\"request_id\":\"n-$(date +%s%N)\"}" >/dev/null
}

run_lang() { # $1=名称 $2=命令
  echo "== $1 SDK gRPC =="
  publish_change "10.0.0.9"
  DSH_GRPC=127.0.0.1:8383 DSH_HTTP=$BASE DSH_PROJECT=$PROJECT sh -c "$2" > /tmp/sdk-grpc-$1.out 2>&1 &
  local TP=$!
  # 持续发布直到测试进程退出（消除编译/启动窗口竞态）
  for i in $(seq 1 60); do
    if ! kill -0 "$TP" 2>/dev/null; then break; fi
    sleep 1
    publish_change "10.0.0.$((100 + i))"
  done
  if wait $TP; then
    cat /tmp/sdk-grpc-$1.out
    echo "  $1 ✅"
  else
    cat /tmp/sdk-grpc-$1.out
    echo "  $1 ❌"
    exit 1
  fi
}

echo "== 安装依赖 =="
(cd $REPO/sdk/ts && npm install --cache /tmp/dsh-npm-cache --silent >/dev/null 2>&1) && echo "  ts deps ok" || { echo "  ts deps FAIL"; exit 1; }
pip install --quiet --disable-pip-version-check grpcio 2>/dev/null && echo "  py deps ok"
(cd $REPO/sdk/go && go mod tidy >/dev/null 2>&1) && echo "  go deps ok"

run_lang "ts"   "cd $REPO/sdk/ts && node --experimental-strip-types grpc-test.ts"
run_lang "go"   "export GOCACHE=/tmp/dsh-gocache && cd $REPO/sdk/go && go run ./grpc-test"
run_lang "py"   "cd $REPO/sdk/python && python3 grpc-test.py"

echo
echo "======== P1 SDK gRPC 契约测试全部通过 ========"
