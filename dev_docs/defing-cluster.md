# Defing (dsh) 多节点集群部署踩坑记录

> 落档日期：2026-08-17
> 场景：为 ipconfiger/Defing（Rust 分布式配置文档服务，产出二进制 dsh）在测试机 172.16.48.71
> 部署 3 节点 Raft 集群（docker compose）。本文件记录过程中遇到的所有坑、根因与解法，供复现/排障。

---

## 0. 背景与最终拓扑

- 项目：Defing（单二进制 dsh，Raft 集群 + Admin UI + SDK）
- 集群：3 节点，数据卷 n1/n2/n3，容器内地址用**服务名**
- 建群方式（最终）：**静态成员表 `--bootstrap-peers`**（全员 voter，无需 join/promote）；
  bootstrap+join 保留为动态扩容路径（见 §2.2）
- 宿主端口：18384/28384/38384 → 各节点 HTTP API（宿主 8384 已被 ru_deployer 占用，故用 18xxx 段）
- 凭据：`--admin-password`（默认 changeme）、`--join-token`/`--raft-token`（默认 demo，生产须强随机）
- compose 文件：仓库 `dev_docs/docker-compose.yml.demo`（已修复的最终版）→ 测试机 `/opt/ru_deployer/dsh-cluster/`
- 镜像：根 Dockerfile 构建的 `dsh:latest`（ubuntu:24.04 + COPY server/target/release/dsh）

启动命令：`docker compose -p dsh-cluster -f docker-compose.yml up -d`；
seed 建群后**无需 promote**（全员 voter）；bootstrap+join 方式才需
`POST /api/v1/cluster/promote {"node_id": 2}`。

---

## 1. 坑清单（按部署阶段）

### 阶段 A：拉取与构建脚本

#### 坑 A1：`cargo: command not found`（exit 127）
- **现象**：`deploy-dsh.sh`（once 包装）触发部署，`Defing_deploy.sh` 执行到 `cargo build --release` 时报
  `/opt/ru_deployer/scripts/Defing_deploy.sh: line 18: cargo: command not found`。
- **根因**：非交互 shell（nohup/supervisor/systemd 触发）的 PATH 不含 rustup 路径
  （`/root/.cargo/bin` 只在交互登录时被 .bash_profile 注入）；且 ssh 远程执行时 PATH 受限。
- **解决**：部署脚本内显式补 PATH：
  ```bash
  export PATH="$HOME/.cargo/bin:$PATH"
  ```
  用 `$HOME` 而非写死 `/root`，本地（alex）与测试机（root）通用。
- **验证**：重跑 once，cargo 正常执行。

#### 坑 A2：`Could not find protoc`（build script 失败，exit 101）
- **现象**：`cargo build --release` 报
  `failed to run custom build command for dsh-api ... Error: Could not find protoc ... apt-get install protobuf-compiler`。
- **根因**：dsh-api 的 build.rs 用 tonic-prost-build 从 `proto/config.v1.proto` 生成代码，编译期需要系统 `protoc`；
  测试机未安装 protobuf 编译器。
- **解决**：`apt-get install -y protobuf-compiler`（protoc 3.21.12）。
- **注意**：Defing 官方 deploy/Dockerfile 的多阶段构建里也显式装了 protobuf-compiler（D-DKR 修复），
  说明这是 Defing 的硬性构建依赖，任何构建环境都缺不了。

### 阶段 B：docker 镜像构建（本地沙箱场景）

#### 坑 B1：`docker buildx/activity: read-only file system`
- **现象**：docker build 失败：
  `ERROR: failed to update builder last activity time: open /home/alex/.docker/buildx/activity/...: read-only file system`。
- **根因**：执行环境沙箱只允许写工作区，`/home/alex/.docker`（buildx 活动记录目录）在沙箱外只读。
  测试机 root 全权限无此问题。
- **解决**：把 docker 配置目录重定向到可写位置：
  ```bash
  export DOCKER_CONFIG=/path/to/writable/.docker-tmp
  ```
  buildx 的活动记录改写到 ${DOCKER_CONFIG}/buildx/activity。
- **注意**：这不是 Defing 的问题；若在受限环境中跑 docker build 遇到，先怀疑配置目录可写性。

### 阶段 C：集群 docker-compose（重点，全是容器化特有的坑）

#### 坑 C1：NodeInfo 地址为 `0.0.0.0` → 跨节点会话/数据复制全部失效
- **现象**（首版 compose 照搬官方示例 `--http-addr 0.0.0.0:8384 --raft-addr 0.0.0.0:8385`）：
  - 节点启动正常、healthz 200、leader 选举正常、promote 也返回 `{"voters":[1,2,3]}`；
  - 但 **node2/3 读任何数据都报 `ERR_SESSION_EXPIRED`**（"需要管理员会话"），node1 正常；
  - `GET /api/v1/cluster/members` 显示三节点 `http_addr` 全是 `0.0.0.0:8384`。
- **根因**：`--http-addr`/`--raft-addr` 参数同时承担"监听地址"和"上报给集群的 NodeInfo 地址"。
  `0.0.0.0` 作为 NodeInfo 时，leader 向 follower 发 Raft 复制请求、跨节点转发会话校验时，
  在 leader 容器内解析 `0.0.0.0` 即指向自己 → 复制/转发全部走错节点。官方示例面向单机多进程
  （127.0.0.1 可路由），容器化后必须上报**容器内可路由地址**。
- **解决**：容器内改用服务名地址（compose 网络内可解析、可路由）：
  ```yaml
  command: dsh --node-id 2 --join http://node1:8384 --http-addr node2:8384 --raft-addr node2:8385 ...
  ```
  即 node1→`node1:8384/8385`、node2→`node2:...`、node3→`node3:...`。
- **验证**：members 显示 `node1:8384 / node2:8384 / node3:8384`，三节点读回配置一致（version 一致）、
  同一 admin token 三节点通用。
- **附加观察**：promote 只传 `{"node_id":2}` 也能把节点变为 voter（`{"voters":[1,2,3]}`），
  但不会修正 NodeInfo 地址；docker 场景务必从源头（--http-addr/--raft-addr）保证地址可路由，
  不要指望 promote 传 http_addr/raft_addr 补救（实测传了也没更新 members 显示）。

#### 坑 C2：healthcheck 探测 `127.0.0.1` 失败 → 节点依赖启动卡死
- **现象**：`--http-addr node1:8384`（服务名）后，node1 一直 `health: starting`，
  node2/3 因 `depends_on: node1: condition: service_healthy` 永远等不到 node1 healthy。
- **根因**：bind 到服务名（解析为容器 IP `172.x.x.x`）后，服务**不再监听 127.0.0.1**；
  而 healthcheck 用 `exec 3<>/dev/tcp/127.0.0.1/8384` 探测 loopback → 连不上。
- **解决**：healthcheck 探测各自的**服务名**（容器内 /etc/hosts 可解析）：
  ```yaml
  test: ["CMD", "bash", "-c", "exec 3<>/dev/tcp/node1/8384"]   # node2/3 分别用 node2/node3
  ```
- **验证**：三节点快速进入 healthy，依赖链正常启动。
- **通用教训**：容器内服务 bind 到具体 IP（服务名/DNS）时，容器内自检（healthcheck/本地 curl）也要用同一地址，
  不能用 127.0.0.1。

#### 坑 C3：固定 `--join` 参数 + 已有数据 → 重启崩溃 `join timed out`
- **现象**：`docker restart dsh-cluster-node2-1` 后 node2 退出（exit 1），日志：
  `Error: "join timed out (no leader responded)"`（期间短暂 `become leader id=2` 后 shutdown）。
- **根因**（代码级）：dsh-cli 的 join 流程向 leader POST `/api/v1/cluster/join`；leader 侧
  `cluster_join` 对「已在集群成员表中的 node_id」返回 **409**（F14 防重复 node_id / 防地址劫持）。
  node2 数据卷已有 raft 状态（成员表含自身），重启后固定命令里的 `--join` 再走一遍 → 409 →
  `join_cluster` 每 300ms 重试、30s 超时 → 进程退出。期间本节点 raft 实例按持久化 voter 身份
  短暂竞选（`become leader id=2`），但 raft RPC 服务要在 join 之后才 bind，无法与集群通信。
- **解决**（根治，判断下沉到二进制）：把「首次初始化」与「重启恢复」的区分交给 dsh 进程自身，
  用 raft-meta 表（持久化状态的权威信号）判断，而不是 shell 判目录：
  1. **dsh-cli**：启动时已有持久化状态 → **忽略 `--bootstrap`/`--join`，直接 resume**
     （日志 `node {id} has persisted state; ignoring --join and resuming (auto-rejoin)`）；
  2. **dsh-cli**：join 收到 409（已在集群）→ 视为幂等成功，停止重试、resume 追赶；
  3. **dsh-api**：`/cluster/join` 对「已存在但仍是 learner」的 node_id 幂等成功
     （openraft `add_learner` 本身对已存在节点就是幂等 re-add，可刷新 NodeInfo）；
     仅对「已是 voter」保留 409（防劫持，且 voter 重启恢复本就不需要 join）；
  4. **leader 跟随**：join 命中 follower 时返回 428 + `leader_hint`（与写路径同约定），
     dsh-cli 跟随 hint 切换到真实 leader 重试——leader 切换后无需人工改 `--join` 指向。
  由此 compose/k8s 的启动命令可以**静态书写**（每次启动同一参数），无需条件包装。
- **验证**：`docker restart node2` 后日志出现 `ignoring --join and resuming`，
  自动恢复为 voter、数据读回正常、集群不中断（leader 不变）。
- **通用教训**：有状态容器重启 = 复用数据卷重放；初始化参数（join/bootstrap）必须**幂等**。
  不要用 shell 判目录的临时手段（见坑 C4），应在二进制内用「持久化状态」这一权威信号判断。

#### 坑 C4：shell 判空包装（`ls -A /data`）在「首次 join 即崩溃」时误判
- **现象**：node2 首次 `--join` 期间崩溃（如 node1 未就绪 join 30s 超时退出、或中途被 kill），
  容器被 restart 策略拉起后**仍然崩溃**，日志：
  `Error: 集群模式需要 --bootstrap、--join 或已有数据目录`。
- **根因**：`RedbStorage::open` 打开数据目录会**立即创建 `dsh.redb` 并 eager 预建全部表**，
  但此时 raft-meta 仍为空。`ls -A /data` 看到文件 → 包装脚本以为「已有数据」→ 不传 `--join` →
  dsh 发现 `!bootstrap && !join && !has_state` → 报错退出 → 崩溃循环。
  即：**「数据目录有文件」≠「有 raft 状态」**；shell 判空与二进制内部的
  `has_persisted_state()`（读 raft-meta 表）在崩溃窗口不一致。
- **解决**：弃用 shell 包装，改用坑 C3 的二进制幂等逻辑——dsh 按 raft-meta 判据：
  无状态（含刚 crash 未收到任何日志的场景）→ 重跑 join；有状态 → resume。两种情况都正确。
- **通用教训**：初始化判据必须与运行时的权威状态一致；shell 文件探测是脆弱代理，
  容器编排（compose/k8s）的启动命令应保持静态、由应用自身做幂等。

---

## 2. 最终 compose 关键片段（含全部修复）

### 2.1 建群方式一：静态成员表（`--bootstrap-peers`，推荐）

初始建群推荐直接传整个集群成员表（研究/设计见 `dev_docs/research-cluster-bootstrap.md`）：
所有节点传**完全相同**的 `node_id@raft_addr@http_addr` 列表（**三段式必填**），并行启动后
直接选举，全员 voter，**无需 join 与 promote**。openraft 语义：同 map 并发 `initialize` 安全——
先到者完成首写，其余节点收到良性 `NotAllowed` 后经复制追平。

```yaml
services:
  node1:
    image: dsh:latest
    environment:
      DSH_BOOTSTRAP_PEERS: "1@node1:8385@node1:8384,2@node2:8385@node2:8384,3@node3:8385@node3:8384"
    command: dsh --node-id 1 --bootstrap-peers $${DSH_BOOTSTRAP_PEERS} --http-addr node1:8384 --raft-addr node1:8385 --data-dir /data --admin-password ${DSH_ADMIN_PASSWORD:-changeme} --join-token ${DSH_CLUSTER_TOKEN:-demo} --raft-token ${DSH_CLUSTER_TOKEN:-demo}
    volumes: [n1:/data]
    ports: ["18384:8384"]
    healthcheck:
      test: ["CMD", "bash", "-c", "exec 3<>/dev/tcp/node1/8384"]
      interval: 5s
      timeout: 3s
      retries: 30
      start_period: 5s
  # node2/3 同构：--node-id 2/3、--http-addr/--raft-addr 用 node2/node3、healthcheck 探 node2/node3；
  # 成员表 DSH_BOOTSTRAP_PEERS 三个服务完全相同（含全部 3 节点）。
volumes: { n1: {}, n2: {}, n3: {} }
```

要点：
- **三段式必填**（`node_id@raft_addr@http_addr`）：http_addr 是 leader 重定向/join 跟随/登录转发的
  依据，缺失会静默降级；条目校验还包括 raft/http 地址各自不得重复、拒绝 `0.0.0.0`/`::`
  不可路由通配地址（坑 C1）、端口须为 1-65535 数值；
- 成员表内**本节点**的 raft_addr/http_addr 必须与 `--raft-addr`/`--http-addr` 一致，否则启动即报错；
- 成员表**只用于初始建群**：已有状态（重启/crash 恢复）自动 resume，忽略本参数；
  若 seed 与集群当前成员表不一致（seed 过期 / 想用 config 加节点等误操作），启动会
  **WARN 并给出差异明细，但不会覆盖**——运行期成员变更只能走 `--join`/promote/remove-node
  （成员表是共识复制数据，单节点本地覆盖会与集群分叉）；
- 后续扩缩容仍用 `--join` + promote + remove-node（见 2.2）。

### 2.2 建群方式二：bootstrap + join（保留，用于动态扩容）

```yaml
  node1:
    command: dsh --node-id 1 --bootstrap --http-addr node1:8384 --raft-addr node1:8385 --data-dir /data ...
  node2/3:
    command: dsh --node-id 2 --join http://node1:8384 --http-addr node2:8384 --raft-addr node2:8385 --data-dir /data ...
  # 加入后需 promote 为 voter
```

三条铁律（容器化集群）：
1. **监听地址 ≠ 上报地址**：NodeInfo 必须可路由（服务名/具体 IP），不能用 0.0.0.0。
2. **自检用服务名**：healthcheck/本地探活用 127.0.0.1 的前提是服务真监听 loopback。
3. **初始化参数幂等化**：bootstrap/join/bootstrap-peers 由 dsh 自身按 raft-meta 判断
   （已有状态自动 resume，无状态才执行初始化），compose/k8s 启动命令静态书写即可；
   不要用 shell 判目录条件化传参（坑 C4）。

---

## 3. 其他部署相关注意（非坑，但易踩）

- **构建依赖**：Defing server 编译必须 protoc（proto 生成），构建镜像/机器先装 protobuf-compiler。
- **once 部署不写 DB 历史**：ru_deployer 的 `--once` 手动部署不落 SQLite 部署记录（与轮询模式不同），
  查 `history` 看不到属正常。
- **GitHub public 仓库无需 token**：git_host=github.com 的仓库不配 token 时走无认证裸 URL clone。
- **端口规划**：宿主 8384 已被 ru_deployer 使用，集群 HTTP 映射到 18xxx 段；raft 端口（8385）不映射，
  集群内部经 compose 网络互通即可。
- **凭据安全**：`--join-token`/`--raft-token` 生产环境务必强随机（README F3 强制项），
  `--admin-password` 同理；本测试用默认值仅限测试环境。
- **seed map 一致性是硬约束**：`--bootstrap-peers` 三节点必须传完全相同的值（不一致 = split-brain，
  启动校验只能保证本节点条目与本地参数一致，无法发现他节点配置漂移）；k8s StatefulSet 应从
  ordinal 模板生成、compose 用同一环境变量注入。

---

## 4. 验证清单（部署完成后应全部通过）

```bash
# 1. 三节点 healthy
docker compose -p dsh-cluster ps --format '{{.Name}} {{.Status}}'
# 2. healthz 全 200
for p in 18384 28384 38384; do curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:$p/healthz; done
# 3. 登录（seed map 建群后全员 voter，无需 promote；bootstrap+join 方式才需要）
TOK=$(curl -s -X POST http://127.0.0.1:18384/api/v1/login -H 'Content-Type: application/json' -d '{"password":"changeme"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
curl -s -X POST http://127.0.0.1:18384/api/v1/cluster/promote -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' -d '{"node_id":2}'   # 仅 join 方式
curl -s -X POST http://127.0.0.1:18384/api/v1/cluster/promote -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' -d '{"node_id":3}'   # 仅 join 方式
# 4. members：三节点 voter，http_addr 为 node1/node2/node3:8384
curl -s -H "Authorization: Bearer $TOK" http://127.0.0.1:18384/api/v1/cluster/members
# 5. 写读 + 三节点复制（写入 leader 后各节点读回 version 一致）
# 6. 容错：docker restart 任一节点 → 日志出现 "ignoring --join/--bootstrap-peers and resuming"，自动恢复 voter 且数据可读
# 7. 崩溃恢复：docker kill -s KILL <node2>（模拟 crash）→ restart 策略拉起 → 自动恢复，集群不中断
# 8. 首次 join 即崩溃：node1 停止时拉起全新节点（--join node1）→ join 30s 超时退出属预期，
#    重启策略重试即可；node1 恢复后该节点正常 join（数据目录虽有 dsh.redb 文件但无 raft 状态，dsh 仍会重跑 join）
# 9. seed 建群校验：成员表内本节点 raft_addr 与 --raft-addr 不一致 → 启动即报错退出（配置漂移兜底）
```
