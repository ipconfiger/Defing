#!/usr/bin/env bash
# M4 混沌测试：leader 击杀 → 重新选举 → 继续写入 → 重启追赶；follower 击杀重启追赶。
set -u
BIN=${BIN:-/home/alex/Projects/Defing/server/target/debug/defing}
WORK=$(mktemp -d /tmp/dsh-chaos.XXXXXX)
PIDS=""

cleanup() {
  for p in $PIDS; do kill "$p" 2>/dev/null || true; done
  pkill -x defing 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

HA1=127.0.0.1:8611; HA2=127.0.0.1:8612; HA3=127.0.0.1:8613
H1=http://$HA1; H2=http://$HA2; H3=http://$HA3

start_node() {
  local id=$1 http=$2 raft=$3 data=$4; shift 4
  $BIN --node-id "$id" --http-addr "$http" --raft-addr "$raft" --grpc-addr "127.0.0.1:88$id" \
       --data-dir "$data" --admin-password admin123 --allow-no-master-key --join-token demo --raft-token demo "$@" >"$WORK/n$id.log" 2>&1 &
  PIDS="$PIDS $!"
}

wait_ready() { for i in $(seq 1 50); do curl -sf "$1/healthz" >/dev/null 2>&1 && return 0; sleep 0.2; done; return 1; }

tok_of() { local host=$(echo "$1" | cut -d'/' -f3); cat "$WORK/tok_"$host 2>/dev/null; }
node_id_of() { curl -sf -H "Authorization: Bearer $(tok_of "$1")" "$1/api/v1/cluster/members" 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)['node_id'])"; }
leader_of()  { curl -sf -H "Authorization: Bearer $(tok_of "$1")" "$1/api/v1/cluster/members" 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)['current_leader'])"; }

login() { curl -sf -X POST "$1/api/v1/login" -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])"; }
api() { curl -sf -H "Authorization: Bearer $1" "${@:2}"; }

echo "== 1. 3 节点集群 =="
start_node 1 "$HA1" 127.0.0.1:8711 "$WORK/n1" --bootstrap
sleep 1
start_node 2 "$HA2" 127.0.0.1:8712 "$WORK/n2" --join "$H1"
start_node 3 "$HA3" 127.0.0.1:8713 "$WORK/n3" --join "$H1"
sleep 2
for H in "$H1" "$H2" "$H3"; do wait_ready "$H" || { echo "FAIL ready $H"; exit 1; }; done
# 集群级单会话（I7）：登录一次（自动转发至 leader），token 全集群共享
T1=$(login "$H1")
for H in "$H1" "$H2" "$H3"; do
  HOST=$(echo "$H" | cut -d'/' -f3)
  echo "$T1" > "$WORK/tok_"$HOST
done
T2=$(tok_of "$H2"); T3=$(tok_of "$H3")
api "$T1" -X POST "$H1/api/v1/cluster/promote" -H 'Content-Type: application/json' -d '{"node_id":2}' >/dev/null
sleep 1
api "$T1" -X POST "$H1/api/v1/cluster/promote" -H 'Content-Type: application/json' -d '{"node_id":3}' >/dev/null
sleep 2
echo "  3 nodes up, promoted, logged in"

echo "== 2. 写入 v2 =="
api "$T1" -X POST "$H1/api/v1/projects" -H 'Content-Type: application/json' -d '{"name":"chaos-app"}' >/dev/null
api "$T1" -X PUT "$H1/api/v1/projects/chaos-app/structure-draft" -H 'Content-Type: application/json' -d '{"base_version":1,"groups":[{"name":"redis","items":[{"key":"host","type":"string","required":true}]}]}' >/dev/null
api "$T1" -X POST "$H1/api/v1/projects/chaos-app/structure-draft/publish" -H 'Content-Type: application/json' -d '{"comment":"s","request_id":"s1"}' >/dev/null
api "$T1" -X PUT "$H1/api/v1/projects/chaos-app/branches/dev/draft" -H 'Content-Type: application/json' -d '{"updates":[{"group":"redis","key":"host","value":{"type":"string","str_value":"1.1.1.1"}}]}' >/dev/null
api "$T1" -X POST "$H1/api/v1/projects/chaos-app/branches/dev/publish" -H 'Content-Type: application/json' -d '{"comment":"v2","request_id":"r1"}' >/dev/null
echo "  v2 written"

echo "== 3. 击杀 leader =="
LEADER_ID=""; LEADER_HTTP=""
for H in "$H1" "$H2" "$H3"; do
  NID=$(node_id_of "$H"); LID=$(leader_of "$H")
  if [ -n "$NID" ] && [ "$NID" = "$LID" ]; then LEADER_ID=$NID; LEADER_HTTP="$H"; fi
done
[ -n "$LEADER_ID" ] || { echo "FAIL: no leader"; exit 1; }
echo "  leader = node $LEADER_ID ($LEADER_HTTP)"
KILLED_LOG="$WORK/n$LEADER_ID.log"
PID=$(pgrep -f "node-id $LEADER_ID" | head -1)
[ -n "$PID" ] && kill -9 "$PID" && echo "  killed node $LEADER_ID (SIGKILL pid $PID)"
sleep 2

echo "== 4. 幸存节点重新选举并继续写入 =="
NEW_LEADER=""
for H in "$H1" "$H2" "$H3"; do
  [ "$H" = "$LEADER_HTTP" ] && continue
  for i in $(seq 1 30); do
    NID=$(node_id_of "$H" 2>/dev/null); LID=$(leader_of "$H" 2>/dev/null)
    [ -n "$NID" ] && [ "$NID" = "$LID" ] && NEW_LEADER="$H" && break
    sleep 0.3
  done
  [ -n "$NEW_LEADER" ] && break
done
[ -n "$NEW_LEADER" ] || { echo "FAIL: no re-election"; exit 1; }
echo "  new leader: $NEW_LEADER"
NT=$T2; [ "$NEW_LEADER" = "$H3" ] && NT=$T3
api "$NT" -X POST "$NEW_LEADER/api/v1/projects" -H 'Content-Type: application/json' -d '{"name":"survivor-write"}' >/dev/null && echo "  击杀 leader 后写入成功 ✅" || { echo "FAIL: write after leader kill"; exit 1; }

echo "== 5. 重启被击杀节点（同 data-dir）→ 追赶 =="
HA_VAR="HA$LEADER_ID"; RADDR="${!HA_VAR}"
H_VAR="H$LEADER_ID"; RH="${!H_VAR}"
start_node "$LEADER_ID" "$RADDR" "127.0.0.1:871$LEADER_ID" "$WORK/n$LEADER_ID"
sleep 4
wait_ready "$RH" || { echo "FAIL: restarted node not ready"; tail -5 "$KILLED_LOG"; exit 1; }
# 会话在 Raft 日志中持久化：重启后旧 token 仍有效（B2），直接复用
RT=$(tok_of "$RH")
SN=$(api "$RT" -sf "$RH/api/v1/projects" 2>/dev/null | grep -c survivor-write)
C=$(curl -sf "$RH/v1/projects/survivor-write/branches/dev/snapshot" 2>/dev/null || api "$RT" -sf "$RH/api/v1/projects/survivor-write/branches/dev/config" 2>/dev/null)
SEEN=0
for i in $(seq 1 40); do
  api "$RT" -sf "$RH/api/v1/projects" 2>/dev/null | grep -q survivor-write && SEEN=1 && break
  sleep 0.5
done
[ "$SEEN" = "1" ] && echo "  重启节点数据追赶一致 ✅（含宕机期间写入）" || { echo "  FAIL: 重启节点未追赶"; echo "--- $KILLED_LOG tail ---"; tail -12 "$KILLED_LOG"; api "$RT" -sf "$RH/api/v1/projects"; }

echo "== 6. follower 击杀 + 重启追赶 =="
FID=""
for id in 1 2 3; do
  [ "$id" != "$LEADER_ID" ] && FID=$id && break
done
FPID=$(pgrep -f "node-id $FID" | head -1)
[ -n "$FPID" ] && kill -9 "$FPID" && echo "  killed follower node $FID"
sleep 1
api "$NT" -X POST "$NEW_LEADER/api/v1/projects" -H 'Content-Type: application/json' -d '{"name":"during-down"}' >/dev/null
HA_VAR="HA$FID"; RADDR="${!HA_VAR}"
H_VAR="H$FID"; FH="${!H_VAR}"
start_node "$FID" "$RADDR" "127.0.0.1:871$FID" "$WORK/n$FID"
sleep 4
wait_ready "$FH" || { echo "FAIL: follower restart not ready"; exit 1; }
# 会话持久化：复用旧 token
FT=$(tok_of "$FH")
SEEN2=0
for i in $(seq 1 40); do
  api "$FT" -sf "$FH/api/v1/projects" 2>/dev/null | grep -q during-down && SEEN2=1 && break
  sleep 0.5
done
[ "$SEEN2" = "1" ] && echo "  follower 重启后追赶一致 ✅（含宕机期间写入）" || echo "  WARN: follower 未看到宕机期间数据"

echo
echo "======== M4 混沌测试完成 ========"
