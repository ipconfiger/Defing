# 研究报告：静态成员表（seed map）建群 vs 当前 join 式组网

> 研究日期：2026-08-17
> 问题：现在 dsh 用 `--bootstrap`（单节点）+ `--join`（其余节点）建群，节点加入后需再 `promote`
> 为 voter。是否应该改为 `--bootstrap=ip1:port1,ip2:port2,ip3:port3` 直接传入整个集群 map、
> 启动后直接投票选举？重启场景下哪种更高效？
> 范围：只做研究与方案对比，不涉及实现。

---

## 0. 结论先行（TL;DR）

1. **重启效率：两种模型没有差别。** 关键事实：join 式组网在幂等修复后，**重启根本不走 join**——
   节点有持久化状态（raft-meta 非空）时直接 resume（auto-rejoin），不发起任何 join RPC。
   静态 seed map 的重启路径（读持久化成员表 → 恢复 → 选举）与它**完全一致**。
   所谓"重启时 join 低效"主要是旧版崩溃循环行为（坑 C3）的印象，已被修复。
2. **静态 seed map 的真正收益在"首次建群"**：并行启动、无"node1 必须先起来"的串行依赖、
   无 learner→voter 两阶段、无 promote 步骤、全部节点直接成为 voter 参与选举。
   3 节点场景绝对时间差是毫秒~秒级，真正的价值是**运维简单 + 少一类崩溃 bug + 对齐 k8s StatefulSet 部署模式**。
3. **可行性：openraft 原生支持，改动量很小。** openraft 官方文档明确：
   `Raft::initialize(全量 map)` 在多节点**用相同 map 同时调用是安全的**（选举协议保证收敛）；
   用不同 map 是**非法的 split-brain**；在已初始化节点上调用会报错且安全。
   dsh 已有 `initialize_single`（就是 `raft.initialize([(node_id, node)])`），扩展为全量 map 即可。
4. **代价与约束**：map 一致是硬要求（配置漂移 = split-brain，etcd 用 cluster-ID 检测并拒绝，事故案例很多）；
   动态扩缩容仍需要 add_learner + promote（openraft 成员变更语义决定），**join 机制不能删**，只是从
   "唯一建群路径"降级为"动态加节点路径"。
5. **建议：seed map 与 join 并存（混合模型）**——新增 `--bootstrap-peers "1@node1:8385,2@node2:8385,3@node3:8385"`
   作为初始建群路径，保留 join 作为扩容路径。consul 的 `bootstrap_expect` + `retry-join` 就是这种混合。

---

## 1. 现状：join 式组网的生命周期（代码级）

### 1.1 启动时序（当前实现）

```
node1 --bootstrap                → raft.initialize([node1]) 单节点建群 → 唯一 voter + leader
node2 --join http://node1:8384   → POST /api/v1/cluster/join
                                    → leader add_learner(2)（成员变更①，node2 变 learner）
                                    → leader 向 node2 复制日志（追平）
node3 同 node2
管理员 promote node2/3           → POST /api/v1/cluster/promote
                                    → leader change_membership([1,2,3])（成员变更②，learner→voter）
任何节点重启（有状态）            → 忽略 --bootstrap/--join，直接 resume（幂等修复后）
```

代码依据（`server/crates/dsh-cli/src/main.rs`）：
- `dsh_raft::initialize_single(&raft, node_id, node_info)` = `raft.initialize(BTreeMap::from([(node_id, node)]))`；
- `join_cluster` POST 到目标节点 `/api/v1/cluster/join`，命中 leader 才成功；
- 幂等初始化分支：`has_persisted_state()`（raft-meta 非空）→ 忽略 `--join`/`--bootstrap` 直接 resume。

### 1.2 结构性约束：为什么 dsh 必须两阶段（join + promote）

openraft 0.9.25 源码（`membership.rs` `ensure_valid`）：
> "Every voter has a corresponding Node" → `ensure_voter_nodes()` 失败返回 `LearnerNotFound`。

即 **`change_membership` 把某节点提升为 voter 的前提是它已作为 learner 存在于成员表中（带 NodeInfo）**。
所以 dsh 的"先 join 成 learner、再 promote 成 voter"不是设计冗余，而是 openraft 成员变更语义的硬要求。
这带来两个操作成本：
1. 两次成员变更（join + promote），各需 leader 在线 + quorum 提交；
2. promote 要求 learner 已入成员表（追平/复制由 leader 后台进行），存在"promote 时 learner 未追平"的时序窗口。

### 1.3 首启的串行依赖

- node2/3 的 `--join` 目标写死为 node1，且 compose 里 `depends_on: node1: service_healthy`：
  建群**必须 node1 先 bootstrap 并成为 leader**（node1 是唯一 voter，必然当选），node2/3 才能加入。
- 若 node1 启动失败/被 kill，全新 node2/3 无法建群（join 30s 超时退出，restart 策略重试等待）。
- 这是 join 模型固有的"引导节点依赖"，seed map 模型不存在此问题。

---

## 2. 静态 seed map 模型（`--bootstrap=1@ip1:8385,2@ip2:8385,3@ip3:8385`）

### 2.1 工作机制

1. 每个节点启动参数携带**全量成员表**（node-id → raft 地址）；
2. 空数据目录（首启）→ 调用 `raft.initialize(全量 map)`，**所有节点同时成为 voter**，直接进入选举；
3. 已有数据（重启）→ resume（与现在完全一致，seed 参数被忽略）。

openraft 官方文档（`cluster_formation`）对此的权威表述：
> - `Raft::initialize(membership)` 会写入 index=0 的初始成员表日志并立即生效，转入 Candidate 开始竞选；
> - 在多节点**同时调用且 map 相同 → 安全**（投票协议保证一致性收敛）；
> - **map 不同 → ILLEGAL，导致未定义状态（split-brain）**；
> - 在已初始化节点上调用 → 返回错误且安全。

### 2.2 首启分区的安全性分析（同 map 前提）

- 3 节点首启被分区为 1+2：1 侧无法凑齐 quorum（需 2 票）→ 选不出 leader；2 侧选出 leader 并提交空白日志；
  分区恢复后，1 侧节点看到更高 term 的 leader、且 map 相同 → 以 follower 身份追日志收敛。**无 split-brain**。
- 唯一真正的危险是 **map 不一致**：两个"各自眼中的集群"各自有 quorum，各选各的 leader → 永久分裂。
  （etcd 用随机 cluster-ID 握手检测此类情况，见 §3。）

### 2.3 效率对比（冷启动 3 节点）

| 维度 | join 式（现状） | seed map 式 |
|---|---|---|
| 节点启动顺序 | node1 先行（bootstrap+选举），node2/3 等 node1 healthy | 三节点并行，无依赖 |
| 建群动作 | 2×join RPC + 2×成员变更（add_learner + change_membership）+ 1×promote | 1×initialize（全员 voter）+ 1 轮选举 |
| promote 步骤 | 有（人工/自动化，且依赖 learner 已入成员表） | 无（初始即 voter） |
| 端到端时长 | node1→node2→promote→node3→promote 的串行链 | 并行，约一个选举超时（dev_config 300–600ms）内完成 |
| 首启失败模式 | node1 挂 → 全新集群无法建群 | 任意 2/3 在线即可建群（quorum） |

注：对 3 节点小集群、空日志，join/promote 每次只是毫秒级 HTTP RTT + 成员变更提交，**绝对时间差很小**；
差异主要体现在"少两个运维步骤、少一种串行依赖、少一类崩溃窗口"。

### 2.4 重启/故障恢复场景（用户最关心的问题）

| 场景 | join 式 | seed map 式 | 差异 |
|---|---|---|---|
| 正常重启（有数据） | resume，无 join RPC | resume | **无差异** |
| crash 后拉起（有数据） | resume，自动追日志 | 同 | **无差异** |
| 数据目录被清空/重建（替换节点） | 需 join + promote 重新入群 | 启动即 voter，经选举+复制追赶入群 | seed 少一步（join RPC），但都需要 leader 在线复制日志 |
| 全集群同时重启 | 各自 resume + 选举 | 同 | **无差异** |

**结论：重启效率上 seed map 没有任何优势，因为 join 式重启本来就不再 join**（幂等 resume 已消除重启时的
join 调用）。seed map 的优势全部集中在"首次建群"与"运维语义"。

---

## 3. 业界对照（研究结论）

| 系统 | 建群模型 | 动态成员 | 关键点 |
|---|---|---|---|
| etcd | `--initial-cluster` 静态 map + `--initial-cluster-state=new/existing` | `member add/remove` | 官方要求 advertise peer URL **稳定**；用随机 cluster-ID 拒绝非同簇节点；[discovery 模式仅用于初始建群](https://apache.googlesource.com/cloudstack-kubernetes-provider/+show/b13b4a31891ea31a105db83bf019224b9407aa9e/vendor/github.com/coreos/etcd/Documentation/op-guide/clustering.md#2)。多节点自举配置不一致/数据目录损坏会触发 [cluster ID mismatch 事故](https://github.com/etcd-io/etcd/issues/12361#1)（[排障案例](https://docs.veertu.com/anka/anka-build-cloud/troubleshooting/etcd/etcd-cluster-id-mismatch/#scenario)、[gardener etcd-druid #361](https://github.com/gardener/etcd-druid/issues/361#1)） |
| ZooKeeper | 静态 `server.N=host:2888:3888` + `myid` 文件 | 动态重配 | 配置即身份；quorum 规则天然防分区双主（少数侧选不出 leader） |
| Kafka KRaft | `controller.quorum.voters=1@host:9093,...` 静态 map | [KIP-853 动态成员](https://cwiki.apache.org/confluence/download/export/pdfexport-20241214-141224-0401-2910/217391519_76944394dcfc413bbd957d67e08b6332-141224-0401-2911.pdf#7#3) | 静态 map 仅在存储格式化（建群）时生效；运行期成员变更走 API |
| Consul | `-bootstrap-expect=N` + `-retry-join`（**混合模型**） | 天然 join | 指定节点先 bootstrap 单节点集群，其余节点经 gossip 发现并 join——**和 dsh 现模型一致，只是 join 目标自动发现**（[官方 bootstrap 指南](https://developer.hashicorp.com/consul/docs/v1.10.x/install/bootstrapping)） |
| openraft（dsh 底层） | [官方文档：`initialize(全量 map)` 多节点同 map 同时调用安全](https://docs.rs/openraft/0.9.1/x86_64-apple-darwin/openraft/docs/cluster_control/cluster_formation/index.html#1) | `add_learner` + `change_membership`（[dynamic-membership 指南](https://github.com/databendlabs/openraft/blob/fd1df83f90f7dbd2c04b60597837ec9efa0ad211/guide/src/dynamic-membership.md#1)） | dsh 的底层能力已具备 |
| dsh 现状 | `--bootstrap` 单节点 + `--join` + 人工 promote | join + promote | 两阶段建群、node1 依赖 |

横向结论：
- **静态 map** 是"固定规模 + 稳定地址"场景（k8s StatefulSet、compose 服务名）的行业标准做法
  （etcd/ZK/KRaft 全部如此）；
- **join** 是"动态扩容 + 地址未知/漂移"场景的做法；
- **混合模型**（静态建群 + join 扩容）有先例（consul），且恰好对应 dsh"初始固定 3 节点、后续可能扩缩容"的诉求。

---

## 4. 建议方案（设计概要，仅方案不实现）

### 4.1 目标

- 初始建群走静态 map（并行、无 promote、无 node1 依赖），对齐 k8s StatefulSet 部署模式；
- 动态加节点/缩容仍走现有 join + promote + remove-node；
- 重启语义不变（resume），seed 参数在已有数据时被忽略（复用现有幂等分支）。

### 4.2 参数设计

```text
--bootstrap-peers "1@node1:8385@node1:8384,2@node2:8385@node2:8384,3@node3:8385@node3:8384"
  格式：node_id@raft_addr@http_addr 逗号分隔（**三段式必填**，见 §6.2 A1 修订原因）；
  raft_addr/http_addr 用容器内可路由地址（服务名/DNS），与本节点 --raft-addr/--http-addr 一致。
  语义：仅当本节点数据目录为空（无持久化状态）时生效；
        已有状态 → 忽略并 resume（与现有 --join 幂等行为一致）。
```

compose 用法（node1/2/3 三份配置改为同一环境变量，命令全部静态化）：

```yaml
environment:
  DSH_BOOTSTRAP_PEERS: "1@node1:8385,2@node2:8385,3@node3:8385"
command: dsh --node-id 1 --bootstrap-peers ${DSH_BOOTSTRAP_PEERS} ...   # 2/3 同理，仅 node-id 不同
```

k8s StatefulSet 用法（ordinal 模板生成，天然一致）：

```yaml
env:
- name: DSH_BOOTSTRAP_PEERS
  value: "0@$(hostname)-0.svc:8385,1@...-1.svc:8385,2@...-2.svc:8385"   # 由模板/init 容器生成
```

### 4.3 启动逻辑（main.rs 集群模式，在现有幂等分支上扩展）

```text
has_state = raft-meta 非空
if has_state → resume（现有逻辑，seed/join 参数均被忽略）
elif --bootstrap-peers 给定:
    解析 map → 校验：本节点 node-id 必须在 map 中；map 中本节点 raft_addr 与本地 --raft-addr 一致
    raft.initialize(全量 map)   // openraft：同 map 多节点同时调用安全
    可选 --bootstrap-expect=N：等 N 个节点在线再 initialize（防首启极端分区，最小实现可不做）
elif --bootstrap → initialize([self])（保留兼容）
elif --join → join_cluster（保留，扩容路径）
```

### 4.4 风险与缓解

| 风险 | 说明 | 缓解 |
|---|---|---|
| 配置漂移（map 不一致）→ split-brain | openraft 官方认定的唯一非法用法；etcd 同类事故多 | ① 部署模板保证一致（StatefulSet ordinal / 同一环境变量）；② 启动时可选校验：本地 map 与集群已提交成员表比对，不一致拒绝启动（etcd `initial-cluster-state=existing` 的思路）；③ 文档强调"建群后修改 map 无效，成员变更走 API" |
| 首启分区双群 | 同 map 时 quorum 规则已防住（§2.2），无需额外处理 | 若想更稳：`--bootstrap-expect` 等 majority 在线（[swarmkit 同类诉求](https://github.com/moby/swarmkit/issues/853#1)） |
| 动态扩缩容仍需要 join | openraft 要求新 voter 先为 learner，seed map 只解决初始建群 | 保留现有 join + promote + remove-node 不动 |
| 无 join-token 的建群路径 | seed 模式下知道 map 即可尝试 initialize | initialize 只在空数据目录有效 + 已初始化节点拒绝 + raft-token 校验 raft RPC；与 join-token 威胁模型等价（都需要集群凭据） |
| 地址变更 | 成员表是复制日志而非配置，运行期改地址仍需成员变更 | 文档明确：地址（服务名/DNS）必须稳定，变更走 remove-node + 重新 join（两模型相同） |

### 4.5 收益/代价总账

收益：
- 首启并行、无 node1 串行依赖、任意 2/3 在线即可建群；
- 消灭 promote 步骤（运维少一步）与"promote 时 learner 未追平"的时序窗口；
- join 式建群特有的崩溃 bug 类别（坑 C3/C4：`--join` 重启 409 循环、`ls -A` 判空误判）从根上消失
  （seed 模式下重启/首启都不再走 join 路径）；
- 与 k8s StatefulSet 部署模式完全对齐（etcd/ZK 在 k8s 上的标准做法）。

代价：
- 配置一致性约束（可用模板 + 启动校验兜底）；
- 代码上新增一个参数分支 + map 解析/校验（改动集中在 dsh-cli，约几十行）；join/promote 代码保留；
- 需要更新文档（defing-cluster.md、demo compose）与部署示例。

---

## 5. 对原始问题的直接回答

1. **"重启时 join 会不会低效？"** —— 现状重启根本不走 join（幂等 resume），所以不存在"重启时 join 的开销"；
   换成 seed map 后重启路径也完全一样。**这个效率担忧对重启场景不成立。**
2. **"`--bootstrap=ip1:port1,...` 直接投票选举会不会更高效？"** —— 对**首次建群**更高效且更简单：
   并行启动、全员 voter 直接选举、无 promote。但绝对时间差异很小（毫秒~秒级），
   主要收益是运维简单、健壮性和部署模式对齐，不是"更快"。
3. **值得做吗？** —— 值得，作为**与 join 并存的初始建群路径**（混合模型）。
   openraft 原生支持、dsh 已有 `initialize_single` 可扩展、改动量小、能根除一类部署坑；
   同时保留 join 用于动态扩缩容。核心前提是**部署时保证所有节点 map 一致**。

---

## 6. 实施状态（2026-08-17 已落地）

按本报告 §4 实现，改动与验证：

| 项 | 内容 |
|---|---|
| CLI 参数 | `--bootstrap-peers "1@node1:8385@node1:8384,2@node2:8385@node2:8384,3@node3:8385@node3:8384"`（**`node_id@raft_addr@http_addr` 三段式必填**，与 `--bootstrap`/`--join` 互斥） |
| 启动逻辑 | 空数据目录 + seed → 校验（三段式必填、raft/http 地址查重、拒绝 0.0.0.0/:: 通配、端口 1-65535、本节点在 map 中且地址与本地参数一致，违规启动即报错）→ 所有节点并行 `initialize(全量 map)`；已有状态 → resume（忽略 seed；seed 与持久化成员表不一致时 **WARN 差异明细，不覆盖不阻断**） |
| dsh-raft | 新增 `initialize_cluster`：把 openraft 的 `NotAllowed` 视为良性（建群已由他节点完成，本节点经复制追平），其余错误透传 |
| 测试 | dsh-cli 解析/校验单测 4 项；dsh-raft 新增 `three_node_static_map_bootstrap`（同 map 并发 initialize → 选举 → 写读复制）；真实二进制冒烟：3 节点并行建群、全员 voter 无 promote、写 leader 三节点复制一致、kill -9 后同命令重启 resume、配置不一致启动报错 |
| 文档 | `docs/docker-compose.yml.demo` 与 `docs/defing-cluster.md` §2 以 seed map 为主推建群方式，join 保留为动态扩容路径 |
| 可观测性（B1） | 集群模式新增"长时间无 leader"周期提示（15s 宽限后每 10s 一次，每段失联一次）：seed 建群 quorum 未达成 / 多数派不可达时不再静默空转 |
| 回归脚本（B2） | `scripts/seed-demo.sh` 固化全部 seed 场景（A1/A3 拒绝、B1 提示、三节点建群、复制、同/异 seed 重启），已接入 CI e2e job |
| README（B3） | README「集群（3 节点）」补 seed 建群为方式一，join 降为方式二 |

### 6.1 实测修正（相对 §2.1 的重要发现）

openraft 0.9.25 的"同 map 多节点同时 `initialize` 安全"在实践中是**先到者首写、其余节点收到
`NotAllowed` 良性错误**：某个节点若在自身 `initialize` 前已收到同群节点的竞选投票请求（vote 被
置为非 (0,0)），openraft 会拒绝其写入 index-0 成员表（前置条件 vote==(0,0)）。这**不是 bug**：
失败节点保持安全，随后经 leader 复制追平成员表成为 voter（与 join 后 learner 追赶同路径）。
实现按此语义处理：忽略 `NotAllowed`，`initialize_cluster` 返回是否由本节点完成首写（仅用于日志）。

### 6.2 深度审核修正（A1/A2/A3，2026-08-17）

对实现做二次深度审核后补的三项闭环：

- **A1（强制三段式）**：`http_addr` 段从"可选"改为**必填**——缺省时成员表 http 为空会静默破坏
  写路径重定向（hint 空）、登录转发（跳过转发→follower 登录 504）、join 428 跟随（无法换目标），
  三种都是隐性故障且无合法用例，故解析期直接报错。
- **A3（地址校验强化）**：seed 条目新增 raft/http 地址**各自查重**（两节点共用同一 raft 地址 =
  复制目标冲突）与**拒绝 0.0.0.0/:: 通配**（坑 C1）、端口 1-65535 数值校验。
- **A2（seed vs 持久化成员表一致性）**：有状态节点启动时直接读存储层持久化成员表
  （`StateMachineStore::persisted_membership`，不用 `raft.metrics()`——Raft::new 后 metrics 可能
  尚未发布，异步延迟会导致漏报）比对 seed，不一致 **WARN 差异明细、不覆盖不阻断**。
  "不覆盖"是共识系统的硬约束：成员表是复制日志的一部分，单节点本地覆盖会与集群分叉
  （对应 etcd `--initial-cluster-state=existing` + cluster ID 拒绝的语义）；运行期成员变更
  只能走 join/promote/remove-node，推倒重建先清卷再以 seed 建群。实现细节：
  - 差异明细含三类：seed 有而集群无（提示走 join）、集群有而 seed 无（提示 seed 过期）、
    共同节点地址不一致（提示配置漂移）；
  - 持久化成员表为空（崩溃于追平前）时跳过比对，避免瞬态误报；
  - 冒烟验证：相同 seed 重启无 WARN；seed 改成 {1,2,4} 重启 → WARN 明细 + 仍 resume + 数据可读。
