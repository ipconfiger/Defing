#!/usr/bin/env bash
# M1 集群模式 e2e：3 进程 bootstrap/join/promote → 写 → kill 节点2 → 多数派继续写
set -u
BIN=${BIN:-/home/alex/Projects/Defing/server/target/debug/dsh}
WORK=$(mktemp -d /tmp/dsh-cluster-demo.XXXXXX)
PIDS=""

cleanup() {
  for p in $PIDS; do kill "$p" 2>/dev/null || true; done
  pkill -x dsh 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

HA1=127.0.0.1:8601
HA2=127.0.0.1:8602
HA3=127.0.0.1:8603
H1=http://$HA1
H2=http://$HA2
H3=http://$HA3

start_node() { # $1=node_id $2=http $3=raft $4=data $5..=flags
  local id=$1 http=$2 raft=$3 data=$4; shift 4
  $BIN --node-id "$id" --http-addr "$http" --raft-addr "$raft" --grpc-addr "127.0.0.1:88$id" \
       --data-dir "$data" --admin-password admin123 "$@" >"$WORK/node$id.log" 2>&1 &
  PIDS="$PIDS $!"
}

wait_ready() { # $1=http
  for i in $(seq 1 50); do
    curl -sf "$1/healthz" >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  echo "FAIL: $1 not ready"; tail -5 "$WORK"/*.log; exit 1
}

wait_leader() { # $1=http → 输出该节点若是 leader 则输出其 http，否则空
  for i in $(seq 1 40); do
    local host=$(echo "$1" | cut -d'/' -f3)
    local tok=$(cat "$WORK/tok_"$host 2>/dev/null)
    M=$(curl -sf -H "Authorization: Bearer $tok" "$1/api/v1/cluster/members" 2>/dev/null) || { sleep 0.2; continue; }
    NID=$(echo "$M" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['node_id'])")
    CUR=$(echo "$M" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['current_leader'])")
    if [ -n "$CUR" ] && [ "$CUR" != "null" ] && [ "$CUR" = "$NID" ]; then
      echo "$1"; return 0
    fi
    sleep 0.2
  done
  echo ""
}

echo "== 1. bootstrap 节点1 =="
start_node 1 "$HA1" 127.0.0.1:8701 "$WORK/n1" --bootstrap
wait_ready "$H1"
echo "  node1 ready"

echo "== 2. 节点2、3 join =="
start_node 2 "$HA2" 127.0.0.1:8702 "$WORK/n2" --join "$H1"
start_node 3 "$HA3" 127.0.0.1:8703 "$WORK/n3" --join "$H1"
sleep 2
wait_ready "$H2" && wait_ready "$H3"
echo "  node2/3 joined (as learner)"
# 管理员登录（每节点单会话 I7；token 存文件按主机查找）
api() {
  local url=""
  for a in "$@"; do case "$a" in http://*) url="$a";; esac; done
  local host=$(echo "$url" | cut -d'/' -f3)
  local tok=$(cat "$WORK/tok_"$host 2>/dev/null)
  curl -sf -H "Authorization: Bearer $tok" "$@"
}
# 集群级单会话（I7）：登录一次（任意节点，非 leader 自动转发），token 全集群有效
TOK=$(curl -sf -X POST "$H1/api/v1/login" -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
for H in "$H1" "$H2" "$H3"; do
  HOST=$(echo "$H" | cut -d'/' -f3)
  echo "$TOK" > "$WORK/tok_"$HOST
done
echo "  admin login ok (session cluster-wide, token shared by 3 nodes)"

# 跨节点单会话校验：已有会话 → 任意节点二次登录应 409 ERR_SESSION_IN_USE
CODE=$(curl -s -o /tmp/cluster-second-login.json -w "%{http_code}" -X POST "$H3/api/v1/login" -H 'Content-Type: application/json' -d '{"password":"admin123"}')
if [ "$CODE" = "409" ] && grep -q ERR_SESSION_IN_USE /tmp/cluster-second-login.json; then
  echo "  跨节点二次登录 409 ERR_SESSION_IN_USE ✅"
else
  echo "  FAIL: 跨节点二次登录应 409（got $CODE: $(cat /tmp/cluster-second-login.json)）"
  exit 1
fi


echo "== 3. promote 节点2、3 为 voter =="
api -sf -X POST "$H1/api/v1/cluster/promote" -H 'Content-Type: application/json' \
  -d '{"node_id":2,"http_addr":"127.0.0.1:8602","raft_addr":"127.0.0.1:8702"}' >/dev/null && echo "  promote 2 ok"
sleep 1
api -sf -X POST "$H1/api/v1/cluster/promote" -H 'Content-Type: application/json' \
  -d '{"node_id":3,"http_addr":"127.0.0.1:8603","raft_addr":"127.0.0.1:8703"}' >/dev/null && echo "  promote 3 ok"
sleep 2

LEADER=$(wait_leader "$H1")
[ -n "$LEADER" ] || LEADER=$(wait_leader "$H2")
[ -n "$LEADER" ] || LEADER=$(wait_leader "$H3")
echo "  cluster leader: $LEADER"
[ -n "$LEADER" ] || { echo "FAIL: no leader"; exit 1; }

echo "== 4. 经 leader 创建项目 + 结构 + 草稿 + 发布 =="
api -sf -X POST "$LEADER/api/v1/projects" -H 'Content-Type: application/json' -d '{"name":"order-service"}' >/dev/null || { echo "FAIL create project"; exit 1; }
api -sf -X PUT "$LEADER/api/v1/projects/order-service/structure-draft" -H 'Content-Type: application/json' -d '{"base_version":1,"groups":[{"name":"redis","items":[{"key":"host","type":"string","required":true},{"key":"port","type":"int"}]}]}' >/dev/null || { echo "FAIL structure"; exit 1; }
api -sf -X POST "$LEADER/api/v1/projects/order-service/structure-draft/publish" -H 'Content-Type: application/json' -d '{"comment":"init","request_id":"s1"}' >/dev/null || { echo "FAIL publish structure"; exit 1; }
api -sf -X PUT "$LEADER/api/v1/projects/order-service/branches/dev/draft" -H 'Content-Type: application/json' -d '{"updates":[{"group":"redis","key":"host","value":{"type":"string","str_value":"10.0.0.1"}},{"group":"redis","key":"port","value":{"type":"int","int_value":6379}}]}' >/dev/null || { echo "FAIL draft"; exit 1; }
R=$(api -X POST "$LEADER/api/v1/projects/order-service/branches/dev/publish" -H 'Content-Type: application/json' -d '{"comment":"dev","request_id":"r1"}') || { echo "FAIL publish"; exit 1; }
echo "  publish resp: $R"
echo "$R" | grep -q '"version":2' || { echo "FAIL: expect version 2"; exit 1; }

echo "== 5. 验证复制：节点3 读到 version 2 =="
sleep 1
C=$(api -sf "$H3/api/v1/projects/order-service/branches/dev/config") || { echo "FAIL node3 config"; exit 1; }
echo "  node3 config: $C"
echo "$C" | grep -q '10.0.0.1' && echo "  复制到节点3 ✅" || { echo "FAIL: not replicated"; exit 1; }

echo "== 6. kill 节点2 =="
PID2=$(pgrep -f 'node-id 2' | head -1)
[ -n "$PID2" ] && kill "$PID2" && echo "  killed node2 (pid $PID2)"
sleep 1

echo "== 7. 重新发现 leader 并继续写入 =="
LEADER=""
for H in "$H1" "$H3"; do
  L=$(wait_leader "$H"); [ -n "$L" ] && LEADER="$L" && break
done
[ -n "$LEADER" ] || { echo "FAIL: no leader after kill"; exit 1; }
echo "  new leader: $LEADER"

api -sf -X POST "$LEADER/api/v1/projects" -H 'Content-Type: application/json' -d '{"name":"svc-b"}' >/dev/null || { echo "FAIL: write after kill"; exit 1; }
sleep 1
VISIBLE=0
for H in "$H1" "$H3"; do
api -sf "$H/api/v1/projects" 2>/dev/null | grep -q 'svc-b' && VISIBLE=1
done
[ "$VISIBLE" = "1" ] && echo "  kill 后写入成功且存活节点可见 ✅" || { echo "FAIL: svc-b not visible"; exit 1; }

echo
echo "======== M1 集群演示全部通过 ========"