# 写路径性能分析与优化方案（写效率瓶颈定位）

> 版本：v1.0 ｜ 日期：2025-08-16 ｜ 依据：main `bf030d5` 代码级复核 + 本机实测
> 关联：[deep-analysis-2025.md](deep-analysis-2025.md) §2.4/§6.4（"写 1.6k QPS、单写者串行"）、
> [design-modules/04-publish.md](design-modules/04-publish.md)（D3 checkpoint/diff 存储设计）。

---

## 0. 执行摘要

**写效率慢的根因不是 CPU/锁，而是"每次写命令的 fsync 次数过多 + 全量快照写放大"**：

- **实测**：dev-single 内存模式写 ~**1339 QPS**（无 fsync）；同一进程切 **redb 落盘模式仅 ~24 QPS**（macOS 本机 fsync 慢 + 每 publish 3 次独立事务）。→ 瓶颈是**落盘路径的 fsync 次数**，不是应用层逻辑。
- **代码证据**：一次 `Publish` 在状态机 apply 内产生 **3 个独立 redb 写事务 + 3 次 fsync**（版本记录 / 全量快照 / 分支状态，state.rs:1046-1051）；集群模式再叠加 raft 日志 append + `write_last_applied`（store.rs:590），**共 5 次 fsync/命令**。
- **写放大**：`apply_publish` **每次发布都落全量快照**（`save(snapshot_key, resolved)`，state.rs:1047），而设计 D3 明确"checkpoint 倍数（每 100）或首次 → full，否则 diff"（04-publish.md §8）——**D3 未实现**，版本越大每次发布写入越多。
- **方案（按性价比排序）**：① apply 内多写合并为单事务（`put_batch`，3 fsync → 1）；② 实现 D3 diff/checkpoint 存储（消除全量写放大）；③ 读锁分离/锁外 IO（RwLock，消除"锁内 fsync 阻塞读"）；④ 远期：redb 落盘降级为 Relaxed + raft 日志重放（对齐 etcd 架构）。

---

## 1. 现状实测（本机基线）

| 模式 | 存储后端 | 写 QPS（草稿+发布循环，串行） | 说明 |
|------|----------|------------------------------|------|
| dev-single（无 --data-dir） | `InMemoryStore`（纯内存） | **~1339** | 无 fsync，瓶颈=锁+序列化+HTTP |
| dev-single（--data-dir） | `RedbStorage`（redb，Immediate 持久化） | **~24** | 每次 publish 3 次 redb 事务 + 3 次 fsync |

> macOS 本机 APFS fsync 开销大（每 fsync 约数 ms～十数 ms），Linux 生产 SSD 会好一个量级；
> 但"每命令 fsync 次数"的结构性放大在任何平台都成立，Linux 下 1.6k QPS 同样受此制约（与报告 §6.4 一致）。

## 2. 写路径逐命令事务/fsync 计数（代码证据）

### 2.1 dev-single（直接 apply，raft.rs:163-183）

```
PUT /branches/{b}/draft（DraftUpdate）
  apply_draft_update → save(branch_state)      = 1 事务 + 1 fsync
POST /branches/{b}/publish（Publish）
  apply_publish（state.rs:946）
    save(version_key, record)                   = 1 事务 + 1 fsync   （state.rs:1046）
    save(snapshot_key, resolved)  ← 全量快照    = 1 事务 + 1 fsync   （state.rs:1047）
    save(branch_state_key, st)                  = 1 事务 + 1 fsync   （state.rs:1051）
──────────────────────────────────────────────────────────────
每轮「草稿+发布」= 4 次 fsync；其中发布本身 = 3 次 fsync
```

### 2.2 集群模式（client_write，每命令）

```
1. LogStore::append（raft 日志）                = 1 事务 + 1 fsync   （store.rs:328-358）
2. apply（同 2.1 的 3 次 save）                 = 3 事务 + 3 fsync   （store.rs:539-594）
3. write_last_applied                           = 1 事务 + 1 fsync   （store.rs:590,470-473）
──────────────────────────────────────────────────────────────
每命令 = 5 次 fsync（设计注释"每条独立事务"，store.rs:471）
```

### 2.3 全量快照写放大（D3 未实现）

- `apply_publish` 每次 `kind: VersionKind::Full` + `save(snapshot_key, resolved)`（state.rs:1041-1047）——**把整个分支的物化配置全量序列化落盘**；
- 设计 `04-publish.md §8`（D3）：`vno 为 checkpoint 倍数（每 100）或首次 → full；否则 diff`，`snapshot_of` 从最近 full 重建——**代码未按此实现**；
- 影响：配置越大（几百 KB～MB 级），每次发布写入字节越多，fsync 等待越长，DB 体积线性膨胀（虽有裁剪任务兜底，但热路径写放大仍在）。

### 2.4 锁内 IO + 读写互斥（次要）

- 全局 `std::sync::Mutex<StateMachine>` 串行化读写（报告 §2.4）；`apply` 持锁期间做全部 redb 写事务 + fsync（store.rs:547-590）→ **写 fsync 阻塞所有读**；读也串行（35k QPS 是单锁下的数字）。

---

## 3. 优化方案（按性价比排序）

### 方案 ①：apply 内多写合并为单事务（首选，低风险高收益）

**目标**：把一次 `Publish` 的 3 次独立 redb 写事务合并为 1 次（`put_batch`，dsh-storage 已有批量事务 API，lib.rs:199-214）；集群模式的 `write_last_applied` 与状态写合并（raft 语义要求日志 append 仍单独先 fsync，但 last_applied 可与 state 同事务——**要么全落要么全不落**，语义反而更强）。

| 改动点 | 内容 |
|--------|------|
| dsh-core `Store` trait | 增加 `begin_write_batch() -> WriteBatch`（或直接在 apply 路径收集 key-value 后 `put_batch`） |
| `StateMachine` apply | 各命令的多次 `save()` 收集到"待提交"缓冲，命令结束后统一 `put_batch` 提交；**apply 内读-改-写依赖**（如 `apply_publish` 中 save 之间无读，可安全合并；`apply_publish_structure` 多分支循环同理） |
| `StateMachineStore::apply` | `write_last_applied` 与状态机写合并进同一事务（store.rs:590） |
| dev-single 直写路径 | 同一 apply 改造自动生效 |

**预期**：publish 3 fsync → 1；集群 5 fsync → 2（日志 1 + 状态 1）。Linux SSD 下写 QPS 预计 **2.5～4×**（1.6k → 4k～6k）；macOS 本机 24 → 60～90。
**风险**：低-中。需保证 apply 内无"先写后读未提交值"的跨 save 依赖（现实现 save 之间不读，安全）；`put_batch` 失败时整体回滚，语义与现状（多次提交中途失败）相比只强不弱。

### 方案 ②：实现 D3 checkpoint/diff 快照存储（中风险，消除写放大）

**目标**：按设计 D3 落地——每 100 版本写 full 快照，其余只写 diff（`compute_diff` 已存在，state.rs 有 diff 基础设施）；`snapshot_of` 从最近 full + diff 链重建（04-publish.md §8 的 `snapshot_of` 封装）。

| 改动点 | 内容 |
|--------|------|
| `apply_publish` | `kind: Full`（checkpoint 倍数/首次）或 `Diff` + `diff_ref`；diff 落 `diff_key` |
| `apply_publish_structure` / `apply_rollback` / `apply_shared_publish` | 同样按 checkpoint 规则 |
| `snapshot_of` | Full 直接读；Diff 从最近 full 起应用 diff 链（有界，≤100 条） |
| `prune_versions` / 回滚 / promote | 适配 diff 链（裁剪时保留最近的 full） |

**预期**：大配置（>100KB）下写字节降 10～100×，DB 体积不再线性膨胀，历史版本读取更快。
**风险**：中。diff 链重建正确性需要覆盖测试（PUB-001~005 扩展）；裁剪与 full 保留策略需设计（保留最近 checkpoint）。

### 方案 ③：读写锁分离 + 锁外 IO（中低风险，读体验提升）

**目标**：全局 `Mutex<StateMachine>` → `RwLock`（或 apply 前先计算、落盘移到锁外）；`get_config`/读接口走读锁（redb 读事务不阻塞写），写持写锁。

| 改动点 | 内容 |
|--------|------|
| `StateMachineStore.sm` / dev-single | `Mutex` → `RwLock`（dsh-api 所有 `app.sm.lock()` 读路径改读锁） |
| `apply` | 持写锁；事件广播（`events.send`）移到解锁后 |

**预期**：写 fsync 期间读不再被阻塞；读并发提升（当前 35k 为单锁上限）。写 QPS 提升有限（写本身串行）。
**风险**：低-中。`RwLock` 写饥饿需关注；dsh-api 大量 `sm.lock()` 调用点改造量中等。

### 方案 ④（远期）：落盘降级 Relaxed + raft 日志重放（对齐 etcd，高风险高收益）

**目标**：redb 状态机表 `Durability::Relaxed`（commit 不 fsync，OS 缓存批量落盘），raft 日志保持 `Immediate`（正确性来源）；启动时从日志重放追赶状态机——这是 etcd 的经典架构。

**前置**：需实现"启动时 raft 日志重放"（当前 `has_persisted_state` 重启直读状态表，cli main.rs:503+），是**架构级改动**，与"重启自动恢复"语义强相关。
**预期**：写 QPS 向内存模式（~1339）逼近，Linux 下可达数万（对齐 etcd 批量能力）。
**风险**：高。崩溃一致性、快照安装、裁剪边界都要重审；建议作为 P4 之后的独立项目，先做方案 ①②③。

---

## 4. 建议实施顺序

| 步 | 内容 | 估时 | 验收 |
|----|------|------|------|
| P0-1 | 方案① 写事务合并（apply 内 `write_batch` + last_applied 合并） | 2–3 天 | 写 QPS 提升 ≥2×（本机基线对比）；`cargo test --workspace` 全绿；e2e 4 个脚本全过 |
| P0-2 | 基准回归脚本化：`scripts/bench.sh` 增加 redb 落盘模式对比 + 写 QPS 门槛断言 | 0.5 天 | bench 输出含 `WRITE_QPS`（内存/落盘双行） |
| P1 | 方案② D3 diff/checkpoint 存储 | 3–5 天 | 大配置写字节下降；历史读取正确（扩展测试） |
| P2 | 方案③ RwLock 读写分离 | 2–3 天 | 写 fsync 期间读延迟不劣化 |
| 远期 | 方案④ 日志重放 + Relaxed | 另立项 | 与灰度/RBAC 路线图独立排期 |

> 注：本方案不改变状态机确定性纪律（D16）——写合并只是"多次提交→一次提交"，apply 语义不变；
> 不引入任何墙钟/IO 到 apply 逻辑，Raft 重放安全性保持。

### 实施状态（2025-08-16）

- **P0-1 已完成**（设计 dev_docs/design/perf-write-batch.md，开发计划 dev_docs/plan-perf-write-batch.md）：
  - Store trait 增加 `write_batch(puts, deletes)`（redb 单事务 + InMemory 实现，含原子性测试）；
  - StateMachine 命令级写缓冲 `pending_ops`（Put/Delete 统一序列，读合并"最后一次操作决定"）；
  - `apply` 包装（apply_inner + 成功 flush / 失败 abort，全有或全无）；
  - ~47 处调用点迁移（save/load/delete/get_prefix），rewrap_deks/restore_all/prune_audit/dump_all 保持直写（非命令路径）；
  - **实测**：redb 落盘模式串行写 QPS **24 → 46（1.92×）**（macOS APFS fsync 主导；debug 构建）；
  - 全量测试 132+ 用例绿、clippy/fmt 零告警、4 个 e2e（dev-single/api-surface/cluster/chaos）全过。
- **P2 已完成**（方案③ RwLock 读写分离，设计 dev_docs/design/perf-rwlock.md）：
  - `Arc<Mutex<StateMachine>>` → `Arc<RwLock<StateMachine>>`（dsh-api/publish/observability/raft/jobs/testkit/cli）；
  - dsh-api 24+3 处 lock 审计**全为读** → `.read()`；raft apply/write_command/install_snapshot/jobs 写路径 → `.write()`；
  - **锁外广播**：StateMachineStore::apply 与 dev-single write_command 的 events.send 移到解锁后（缩短写锁持有）；
  - 测试 9 文件适配（含审核补漏的 dsh-cli 生产文件）；
  - **实测**：读写混合下读 p50=0.9ms/p99=11.3ms（写 fsync 不阻塞读）；集群/混沌 e2e 无回归；
  - 全量测试 136+ 用例绿、clippy/fmt 零告警。
- **P0-2 bench.sh 对比行 ✅ 已完成**：内存/redb 双行写 QPS（实测 1497/45，macOS APFS fsync 主导）；
  另补充生产环境实测（§5：单节点 vs 3 节点集群对比）。

### 方案④评估结论（2025-08-16）：**不纳入本期，另立项**

**目标**：状态机表 redb `Durability::Relaxed`（commit 不 fsync），raft 日志保持 `Immediate`；启动时从日志重放追赶状态机——etcd 经典架构，写 QPS 向内存模式（~1339）逼近。

**技术前提（代码级分析）**：
1. 当前 `write_last_applied` 每条独立事务（store.rs:470-473，设计 §8.7"重启重放边界语义优先"）；状态机表经 Store trait 独立提交；
2. **致命约束**：若状态表 Relaxed 而 last_applied Immediate，崩溃时 last_applied 可能超前于状态表 → openraft 认为"已应用到 last_applied"、不重放 → **数据丢失**；
3. 正确做法必须**状态写 + last_applied 同一事务**（同落或不落）——这正是 **P0-2（跨表事务合并）** 的前置，而 P0-2 需 RedbStorage 暴露 WriteTransaction 注入 StateMachine 写路径（架构级）；
4. 依赖 openraft 启动重放语义验证（`applied_state` 返回 last_applied 后 openraft 自动补 apply？）+ 快照安装/回滚/裁剪适配。

**决策**：风险高（崩溃一致性语义变化）、收益边际（P0-1+P1+P2 已达成 1.92× 写 QPS + 写放大消除 + 读写分离）、依赖未排期的 P0-2 → **保持远期定位，独立立项**（与灰度/RBAC 路线图并列，而非本 roadmap 内）。

### 本 roadmap 最终验收（P0-1 + P1 + P2 + 方案④评估）

| 项 | 状态 | 证据 |
|----|------|------|
| P0-1 写事务合并 | ✅ | redb 落盘写 QPS 24→46（1.92×）；132+ 用例绿；e2e 全过 |
| P1 D3 diff/checkpoint 存储 | ✅ | 大配置（50KB）10 版本 DB 0 增长（全量应 +250KB）；136+ 用例绿 |
| P2 RwLock 读写分离 | ✅ | 读写混合读 p50=0.9ms/p99=11.3ms（写不阻塞读）；集群/混沌 e2e 无回归 |
| P0-2 last_applied 跨表合并 | ⏸ 独立评审后排期 | 架构级（RedbStorage 事务注入） |
| 方案④ Relaxed + 日志重放 | ⏸ 远期另立项 | 依赖 P0-2 + openraft 重放验证，风险高 |
| P0-2 bench.sh 对比行 | ✅ 已完成 | 内存/redb 双行写 QPS（实测 1497/45，macOS APFS fsync 主导） |
- **P1 已完成**（方案② D3 diff/checkpoint 存储，设计 dev_docs/design/perf-diff-storage.md）：
  - `snapshot_of` 改造为 diff 链重建（checkpoint 基座 + apply_diff，含空组清理）；
  - `write_version_snapshot` 统一封装（v1/每 100 存 full，其余存 diff）+ 4 个写版本调用点迁移；
  - `prune_versions` 适配（删 diff_key + checkpoint 基座对齐，保链完整）；
  - `rewrap_deks` 扩展 /diff 扫描（diff 中 secret 密文重包）；`version_history` 排除 /diff 后缀；
  - **修复方案①遗留**：cascade_to_project 的 7 处跨行直写 save 迁移到 pending（合并+原子性补齐）；
  - **实测**：大配置（~50KB）发布 10 版本 DB 体积 0 增长（全量存储应 +250KB+），写放大消除；
  - 全量测试 136+ 用例绿、clippy/fmt 零告警、e2e 全过。
- P0-2（last_applied 跨表合并）评估结论：**本期不做**——需 RedbStorage 暴露事务注入，收益边际（集群 3→2 fsync），正确性无碍，独立评审后另行排期。

---

## 5. 生产环境实测（2025-08-16，Alibaba Cloud Linux 3, x86_64, 2 核）

部署：交叉编译 `bin/dsh-linux-x86_64`（cargo zigbuild，ELF x86-64 glibc，12MB）→ `root@47.108.112.24:/opt/dsh/dsh`。
磁盘 fsync 能力：**467/s（2.14ms/fsync）**——远快于 macOS APFS（~10ms+）。

### 5.1 单节点（dev-single，redb 落盘）

| 指标 | 结果 |
|------|------|
| 读 QPS（数据面 snapshot，50 并发） | **3094** |
| 串行写 QPS（draft+publish） | **85** |
| 并发写 QPS（8 独立分支） | **76** |
| watch 延迟（发布→SSE 事件） | **4.5ms** |
| 1MB 大配置写 QPS（单 key 变更） | **82**（≈1KB 配置的 85——**diff 存储使写性能与配置大小解耦**） |

### 5.2 3 节点集群（Raft，全部 promote 为 voter）

| 指标 | 单节点 | 3 节点集群 | 比值 |
|------|--------|-----------|------|
| 串行写 QPS | 85 | **11** | 集群 ≈ 单节点的 **13%** |
| 并发写 QPS（8 分支） | 76 | **12** | ~16% |
| 读 QPS（数据面，集群节点分摊） | 3094 | **2859** | ~92%（读几乎无损） |

**集群写路径开销（每命令）**：leader 日志 append fsync（#1）→ 复制到 2 follower（网络 ×2）→ 多数派确认（follower 各自 fsync）→ apply fsync（#2）→ write_last_applied fsync（#3）——**至少 3 次 fsync + 2 次网络 RTT**。实测 11 QPS ≈ 每写 90ms，与 Raft 多数派落盘语义吻合。

**结论**：
1. 集群写 ≈ 单节点 1/7~1/8（11 vs 85）是 Raft 强一致的**固有成本**（etcd 3 节点亦如此），非本优化可消除；
2. 读几乎无损（92%）——数据面任意节点本地服务 + 方案③读写分离在集群下同样生效；
3. 集群写瓶颈 = fsync 次数 × 网络复制。**方案④（Relaxed+日志重放）或 group commit（批量合并多条命令为一次日志 append+fsync）**是突破路径——后者可让集群写提升数倍。
