#!/usr/bin/env bash
# 重启追赶专项调试：3 节点 → kill leader → 重启 → 观察 node1 完整日志
set -u
BIN=/home/alex/Projects/Defing/server/target/debug/dsh
W=$(mktemp -d /tmp/dsh-rt.XXXXXX)
cleanup() { pkill -x dsh 2>/dev/null || true; }
trap cleanup EXIT

for id in 1 2 3; do
  if [ $id = 1 ]; then FLAG="--bootstrap"; else FLAG="--join http://127.0.0.1:8611"; fi
  $BIN --node-id $id --http-addr 127.0.0.1:861$id --raft-addr 127.0.0.1:871$id --grpc-addr 127.0.0.1:881$id --data-dir $W/n$id --admin-password admin123 $FLAG >$W/n$id.log 2>&1 &
done
sleep 3
# 集群级单会话（I7）：登录一次，token 全集群共享
declare -A T
T[1]=$(curl -sf -X POST http://127.0.0.1:8611/api/v1/login -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
for id in 2 3; do T[$id]=${T[1]}; done
for id in 2 3; do
  curl -s -H "Authorization: Bearer ${T[1]}" -X POST http://127.0.0.1:8611/api/v1/cluster/promote -H 'Content-Type: application/json' -d "{\"node_id\":$id}" >/dev/null
  sleep 1
done
curl -s -H "Authorization: Bearer ${T[1]}" -X POST http://127.0.0.1:8611/api/v1/projects -H 'Content-Type: application/json' -d '{"name":"pre-kill"}' >/dev/null
sleep 1

P1=$(pgrep -f 'node-id 1 ' | head -1)
kill -9 $P1
echo "killed node1 ($P1)"
sleep 3

NEWL=""
for id in 2 3; do
  M=$(curl -s -H "Authorization: Bearer ${T[$id]}" http://127.0.0.1:861$id/api/v1/cluster/members 2>/dev/null)
  echo "$M" | grep -q "\"current_leader\":$id" && NEWL=$id
done
echo "new leader: node$NEWL"
curl -s -H "Authorization: Bearer ${T[$NEWL]}" -X POST http://127.0.0.1:861$NEWL/api/v1/projects -H 'Content-Type: application/json' -d '{"name":"after-kill"}' >/dev/null
echo "write after kill ok"

$BIN --node-id 1 --http-addr 127.0.0.1:8611 --raft-addr 127.0.0.1:8711 --grpc-addr 127.0.0.1:8811 --data-dir $W/n1 --admin-password admin123 >$W/n1r.log 2>&1 &
sleep 10

echo "--- node1 members after restart ---"
# 会话持久化（B2）：重启后旧 token 仍有效，直接复用
T1R=${T[1]}
curl -s -H "Authorization: Bearer $T1R" http://127.0.0.1:8611/api/v1/cluster/members 2>/dev/null; echo
echo "--- node1 projects ---"
curl -s -H "Authorization: Bearer $T1R" http://127.0.0.1:8611/api/v1/projects 2>/dev/null; echo
echo "--- node1 restart log (last 50) ---"
tail -50 $W/n1r.log
