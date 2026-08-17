#!/usr/bin/env bash
# seed map（--bootstrap-peers）静态成员表建群 e2e：
#   1) 启动前校验（A1 三段式必填 / A3 通配地址拒绝 / A3 raft_addr 查重）
#   2) B1：单节点 + seed 无 quorum → 15s 后出现"长时间未确认 leader"提示
#   3) 三节点并行 seed 建群 → 直接选举，全员 voter（无需 join/promote）
#   4) 写 leader + 三节点复制一致
#   5) kill node2 → 相同 seed 重启 → resume（无 WARN）且数据可读
#   6) A2：不一致 seed 重启 → WARN 差异明细 + 仍 resume
set -u
BIN=${BIN:-/home/alex/Projects/Defing/server/target/debug/dsh}
WORK=$(mktemp -d /tmp/dsh-seed-demo.XXXXXX)
PIDS=""

cleanup() {
  for p in $PIDS; do kill "$p" 2>/dev/null || true; done
  pkill -x dsh 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

H1=http://127.0.0.1:8611
H2=http://127.0.0.1:8612
H3=http://127.0.0.1:8613
SEED="1@127.0.0.1:8711@127.0.0.1:8611,2@127.0.0.1:8712@127.0.0.1:8612,3@127.0.0.1:8713@127.0.0.1:8613"
COMMON="--admin-password admin123 --allow-no-master-key --join-token demo --raft-token demo"

fail() { echo "FAIL: $1"; exit 1; }

start_node() { # $1=node_id $2=data_dir $3..=flags
  local id=$1 data=$2; shift 2
  $BIN --node-id "$id" --http-addr 127.0.0.1:861$id --raft-addr 127.0.0.1:871$id --grpc-addr 127.0.0.1:881$id \
       --data-dir "$data" $COMMON "$@" >"$WORK/n$id.log" 2>&1 &
  PIDS="$PIDS $!"
}

wait_ready() { # $1=http
  for i in $(seq 1 60); do
    curl -sf "$1/healthz" >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  fail "$1 not ready (log: $(tail -3 "$WORK"/*.log))"
}

api() { # 带 token 的 curl（登录一次，集群级会话）
  curl -sf -H "Authorization: Bearer $TOK" "$@"
}

echo "== 1. 启动前校验（A1/A3）=="
# A1：两段式缺 http_addr → 拒绝
$BIN --node-id 1 --http-addr 127.0.0.1:8611 --raft-addr 127.0.0.1:8711 --grpc-addr 127.0.0.1:8811 --data-dir "$WORK/x1" $COMMON --bootstrap-peers "1@127.0.0.1:8711" >"$WORK/x1.log" 2>&1
[ $? -ne 0 ] || fail "两段式 seed 应启动失败"
grep -q "缺少 http_addr" "$WORK/x1.log" || fail "A1: 缺 缺少 http_addr 提示"
echo "  A1 两段式拒绝 ✅"
# A3：raft_addr 0.0.0.0 → 拒绝
$BIN --node-id 1 --http-addr 127.0.0.1:8611 --raft-addr 127.0.0.1:8711 --grpc-addr 127.0.0.1:8811 --data-dir "$WORK/x2" $COMMON --bootstrap-peers "1@0.0.0.0:8711@127.0.0.1:8611" >"$WORK/x2.log" 2>&1
[ $? -ne 0 ] || fail "0.0.0.0 seed 应启动失败"
grep -q "不可路由通配地址" "$WORK/x2.log" || fail "A3: 缺 通配地址 提示"
echo "  A3 通配地址拒绝 ✅"
# A3：raft_addr 重复 → 拒绝
$BIN --node-id 1 --http-addr 127.0.0.1:8611 --raft-addr 127.0.0.1:8711 --grpc-addr 127.0.0.1:8811 --data-dir "$WORK/x3" $COMMON --bootstrap-peers "1@127.0.0.1:8711@127.0.0.1:8611,2@127.0.0.1:8711@127.0.0.1:8612" >"$WORK/x3.log" 2>&1
[ $? -ne 0 ] || fail "重复 raft_addr 应启动失败"
grep -q "raft_addr 重复" "$WORK/x3.log" || fail "A3: 缺 重复 提示"
echo "  A3 raft_addr 查重 ✅"

echo "== 2. B1：单节点 + seed 无 quorum → 15s 后无 leader 提示 =="
start_node 1 "$WORK/w1" --bootstrap-peers "$SEED"
sleep 18
grep -q "长时间未确认 leader" "$WORK/n1.log" || fail "B1: 无 quorum 提示未出现"
echo "  B1 无 leader 提示 ✅"
kill "${PIDS##* }" 2>/dev/null; sleep 1

echo "== 3. 三节点并行 seed 建群（全员 voter，无需 promote）=="
for id in 1 2 3; do
  start_node "$id" "$WORK/w2/n$id" --bootstrap-peers "$SEED"
done
wait_ready "$H1"; wait_ready "$H2"; wait_ready "$H3"
TOK=$(curl -sf -X POST "$H1/api/v1/login" -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
M=$(api "$H1/api/v1/cluster/members")
echo "$M" | python3 -c "
import json,sys
d=json.load(sys.stdin)
voters=[m['node_id'] for m in d['members'] if m['is_voter']]
assert d['current_leader'] is not None and str(d['current_leader']) != 'null', d
assert sorted(map(int,voters))==[1,2,3], voters
print('  leader', d['current_leader'], 'voters', voters)
" || fail "seed 建群应全员 voter 且有 leader（members: $M）"

echo "== 4. 写 leader + 三节点复制 =="
LEADER="http://127.0.0.1:861$(api "$H1/api/v1/cluster/members" | python3 -c "import json,sys; print(json.load(sys.stdin)['current_leader'])")"
api -X POST "$LEADER/api/v1/projects" -H 'Content-Type: application/json' -d '{"name":"seed-proj"}' >/dev/null || fail "create project"
sleep 2
for H in "$H1" "$H2" "$H3"; do
  api -sf "$H/api/v1/projects" | grep -q 'seed-proj' || fail "seed-proj 未复制到 $H"
done
echo "  三节点读回一致 ✅"

echo "== 5. kill node2 → 相同 seed 重启 → resume 无 WARN =="
P2=$(pgrep -f 'node-id 2 ' | head -1)
[ -n "$P2" ] && kill -9 "$P2" && sleep 1
start_node 2 "$WORK/w2/n2" --bootstrap-peers "$SEED"
wait_ready "$H2"
grep -c "WARNING: --bootstrap-peers 与集群当前成员表不一致" "$WORK/n2.log" | grep -q '^0$' || fail "相同 seed 重启不应 WARN"
grep -q "ignoring --bootstrap-peers and resuming" "$WORK/n2.log" || fail "相同 seed 重启应 resume"
api -sf "$H2/api/v1/projects" | grep -q 'seed-proj' || fail "node2 重启后数据不可读"
echo "  resume 无 WARN + 数据可读 ✅"

echo "== 6. A2：不一致 seed 重启 → WARN 明细 + 仍 resume =="
BADSEED="1@127.0.0.1:8711@127.0.0.1:8611,2@127.0.0.1:8712@127.0.0.1:8612,4@127.0.0.1:8714@127.0.0.1:8614"
P2=$(pgrep -f 'node-id 2 ' | head -1)
[ -n "$P2" ] && kill -9 "$P2" && sleep 1
start_node 2 "$WORK/w2/n2" --bootstrap-peers "$BADSEED"
wait_ready "$H2"
grep -q "WARNING: --bootstrap-peers 与集群当前成员表不一致" "$WORK/n2.log" || fail "A2: 不一致 seed 应 WARN"
grep -q "seed 含 node 4" "$WORK/n2.log" || fail "A2: WARN 应含 node4 差异明细"
grep -q "集群成员表含 node 3" "$WORK/n2.log" || fail "A2: WARN 应含 node3 差异明细"
grep -q "ignoring --bootstrap-peers and resuming" "$WORK/n2.log" || fail "A2: WARN 后仍应 resume"
api -sf "$H2/api/v1/cluster/members" >/dev/null || fail "A2: WARN 后节点不可用"
echo "  WARN 明细 + 仍 resume + 节点可用 ✅"

echo
echo "======== seed map 建群 e2e 全部通过 ========"
