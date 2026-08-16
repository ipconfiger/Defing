# 设计文档：灰度发布（G0 设计先行 · 含流程图）

> 状态：**G2 已实现**（2025-08-16 设计定稿；2025-08-16 G2 核心状态机落地）｜ 代码基线：main `8686999`
> 本文档用**流程图 + 时序图 + 请求/响应示例**直白说明灰度是如何实现的。
> 抽象设计决策（D17-D23）见文末附录；正文先讲"怎么跑起来"。
> G2 实现落档：`docs/plan-gray-g2.md`（13 任务全闭环）、roadmap-p4.md §1.3 标记 ✅。

---

## 0. 一句话理解

**在同一个分支里放两个版本（稳定版 + 灰度版），用"客户端是谁"来决定把哪个版本发给它。**
灰度版不是新分支，而是分支里的一个"影子版本"——发布时生成、提升时转正、回滚时删除。

---

## 1. 三个核心概念（先记住这三个东西）

| 概念 | 是什么 | 存在哪 |
|------|--------|--------|
| **稳定版** `active_version` | 当前所有客户端在用的版本号（现在的机制，没变） | `BranchState.active_version` |
| **灰度版** `gray_seq` | 一个"备胎"配置，只有被规则命中的客户端才看得到（独立前缀存储） | `BranchState.gray_seq` |
| **灰度规则** `gray_rule` | 描述"哪些客户端算灰度用户"的规则（标签/IP/百分比） | `BranchState.gray_rule` |

> 关键：这三个都是**状态机数据**，跟着 Raft 复制到所有节点——任何节点都知道灰度规则和版本号。

---

## 2. 完整流程图（三张图看懂）

### 图 1：灰度发布的完整生命周期

```
                        ┌─────────────────────────────┐
                        │       管理员（UI/API）        │
                        └──────────────┬──────────────┘
                                       │
        ┌──────────────────────────────┼──────────────────────────────┐
        │                              │                              │
        ▼                              ▼                              ▼
  [1. GrayPublish]              [2. GrayPromote]              [3. GrayAbort]
  发布灰度版                      灰度转正                       灰度下量
        │                              │                              │
        ▼                              ▼                              ▼
  固化草稿 → 灰度序号 +1             active_version = 原灰度内容       gray_seq = 0
  gray_seq = M                      gray_seq = 0                     gray_rule = None
  gray_rule = 规则                  gray_rule = None
        │                              │                              │
        ▼                              ▼                              ▼
  灰度用户（命中规则）读到 M         全部客户端切到 M（新稳定版）       全部客户端回落到稳定版
  普通用户读到 N（不受影响）         灰度客户端收到补发事件重拉          灰度客户端收到事件重拉
```

### 图 2：客户端拉配置时，服务器怎么决定发哪个版本（数据面核心）

```
客户端请求 GET /v1/projects/p/branches/prod/snapshot
        │
        ▼
携带身份：X-Dsh-Instance: web-1  X-Dsh-Labels: zone=cn-north-1
        │
        ▼
┌──────────────────────────────────────────────┐
│ resolve_version(project, branch, client_ctx) │  ← 纯函数，不碰状态机写入
│                                              │
│  1. gray_seq > 0 吗？                    │
│     ├─ 否 → 返回 active_version  ──────────► │
│     └─ 是 ↓                                  │
│  2. gray_rule 命中吗？                       │
│     ├─ 标签：客户端 labels 包含规则的标签？   │
│     │   zone=cn-north-1 命中 → 是 ↓          │
│     ├─ IP：客户端 IP 在规则 IP 段内？         │
│     ├─ 百分比：hash(instance_id)%100 < 10？  │
│     ├─ 任一命中 → 读灰度快照 ──────────►  │
│     └─ 全不命中 → 返回 active_version ─────► │
└──────────────────────────┬───────────────────┘
                           │
                           ▼
                读取该版本的配置快照（复用现有 snapshot_of）
                           │
                           ▼
                响应带 gray: true/false + resolved_version
```

### 图 3：发布后的内存/存储状态（以 prod 分支为例）

```
发布前：                                 灰度发布后：
┌─────────────────────┐                ┌─────────────────────┐
│ BranchState (prod)  │                │ BranchState (prod)  │
│  active_version = 5 │                │  active_version = 5 │  ← 没变！
│  gray_seq = 0       │                │  gray_seq = 1       │  ← 灰度序号（独立空间）
│  gray_rule = None   │                │  gray_rule = {labels │
└─────────────────────┘                │   :[{zone:cn-north}]}│
                                       └─────────────────────┘
版本存储：
  v/5  → 快照（稳定版内容，方案② diff/checkpoint）
  gray-snap/p/prod/1 → 灰度快照（草稿物化；独立前缀，不与 v/ 冲突）
```
> **为什么用独立灰度序号 gray_seq 而非 active_version+1**（审核 Q1 修正）：
> 若 gray_version = active_version + 1，灰度期间管理员普通发布会把 active_version 也推进到
> 同一数字 → 两个指针指向同号快照，互相覆盖。灰度用**独立前缀 + 独立递增序号**，与稳定版本号
> 空间完全隔离，任意交错安全。

---

## 3. 三个命令的详细实现（每个命令做什么、改哪些字段）

### 命令 1：GrayPublish（发布灰度版）

```
请求：POST /api/v1/projects/p/branches/prod/gray-publish
      {"rule":{"match_labels":[{"zone":"cn-north-1"}]},
       "comment":"先给华北发新配置","request_id":"g1"}

服务器 apply_gray_publish 做的事：
  ① 校验草稿非空、结构存在（和普通发布一样的校验）
  ② 把草稿物化成配置快照（含共享引用解析）
  ③ gray_seq = gray_seq + 1（分支级独立灰度序号，不与 active_version 冲突）
  ④ 灰度快照存 gray-snap/{pid}/{branch}/{gray_seq}（复用方案② diff/checkpoint 逻辑）
  ⑤ 写 BranchState：gray_seq=新值, gray_rule=规则, 清空草稿
  ⑥ 发事件 { version:active_version, ty:ValuePublish, gray:true, gray_seq:1 }
     ← 复用既有 EventType（ValuePublish），加 gray:bool 字段（serde default）——
        不新增枚举值（否则新节点写的灰度 VersionRecord 进快照后旧节点装快照反序列化失败，Q3）

结果：
  - 稳定版（5）原封不动，普通客户端照常用
  - 命中规则的客户端（zone=cn-north-1）现在读到 6
```

### 命令 2：GrayPromote（灰度转正）

```
请求：POST /api/v1/projects/p/branches/prod/gray-promote
      {"comment":"华北验证通过，全量","request_id":"g2"}

服务器 apply_gray_promote 做的事：
  ① 校验 gray_seq > 0（没有灰度就报错）
  ② 读取灰度快照内容 → 写入新的 active_version（active = max+1，走方案② write_version_snapshot）
  ③ gray_seq = 0, gray_rule = None（清掉灰度）
  ④ 发事件 { version:新active, ty:ValuePublish, gray:true, gray_seq:0 }
     ← gray:true 标记 + 携带新 active 版本号：灰度客户端收到后无条件重拉（Q4 修订：
        该事件永不被 version>last 过滤，且 abort 也必须带回落版本号）

结果：
  - 所有客户端现在都读到新稳定版 6
  - 灰度客户端收到补发事件 → SDK 自动重拉全量（解决漏收问题）
```

### 命令 3：GrayAbort（灰度下量/回滚）

```
请求：POST /api/v1/projects/p/branches/prod/gray-abort
      {"comment":"有问题，撤回","request_id":"g3"}

服务器 apply_gray_abort 做的事：
  ① gray_seq = 0, gray_rule = None
  ② 发事件 { version:active_version, ty:ValuePublish, gray:true, gray_seq:0 }
     ← 必须携带回落版本号（active_version）：灰度客户端据此重拉稳定版（Q4 修订）

结果：
  - 所有客户端（含灰度客户端）回落到稳定版 5
  - 灰度版本 6 的快照还在（历史可查），但不再被任何客户端解析到
```

---

## 4. 时序图：一次完整的灰度发布流程（端到端）

```
管理员          Defing 集群            华北客户端          华南客户端
  │               │                      │                   │
  │ GrayPublish   │                      │                   │
  ├──────────────►│  固化草稿→灰度版本6   │                   │
  │               │  gray_rule=华北标签   │                   │
  │               ├─────────────────────►│                   │
  │               │  (Raft 复制到各节点)  │                   │
  │               │                      │                   │
  │               │◄─── 拉配置 ──────────┤                   │
  │               │   X-Dsh-Labels:      │                   │
  │               │   zone=cn-north-1    │                   │
  │               │── 命中 → 返回版本6 ──►│  读到新配置 ✅     │
  │               │                      │                   │
  │               │◄─── 拉配置 ──────────┼───────────────────┤
  │               │   X-Dsh-Labels:      │                   │
  │               │   zone=cn-south-1    │                   │
  │               │── 未命中 → 返回版本5 ─┼──────────────────►│  读到旧配置 ✅
  │               │                      │                   │
  │ GrayPromote   │                      │                   │
  ├──────────────►│  active_version=6    │                   │
  │               │  gray 清空           │                   │
  │               ├──── 补发事件 ────────►│  SDK 重拉 → 版本6  │
  │               ├──────────────────────┼──────────────────►│  现在也读到 6
```

---

## 5. 客户端身份从哪来（SDK 怎么上报）

| 身份字段 | 从哪拿 | 稳定吗 |
|----------|--------|--------|
| `instance_id` | SDK 配置（如 Pod 名/主机名/部署单元 ID） | ✅ 稳定（容器重建不变） |
| `labels` | SDK 配置（如 zone/svc/version 标签） | ✅ 稳定 |
| `ip` | 服务端从对端 socket 解析（客户端不用传） | ⚠️ 容器重建会变（兜底用） |

传输方式：
```
HTTP：  GET /v1/projects/p/branches/prod/snapshot
        X-Dsh-Instance: web-1
        X-Dsh-Labels: zone=cn-north-1,svc=checkout

gRPC：  GetConfigRequest { project, branch, instance_id, labels }
        （ip 由服务端从 gRPC 对端地址拿）
```

---

## 5.5 watch 语义（Q4 审核修订——重点）

**现状问题**：现有 watch 按 `e.version > last` 去重（SSE 与 gRPC 同），灰度事件若只加 gray 标记，
`promote/abort` 事件会被该过滤直接滤掉——gray 标记根本没机会被读到，正是 D22 要防的"灰→全量漏收"。

**修订后的 watch 契约（二选一，推荐 a）**：

```
方案 a（服务端按身份投递，推荐）：
  watch 连接注册 instance_id/labels（WatchRequest 加字段）
  服务端按该客户端的 resolve 结果决定是否投递事件
    - 灰度客户端：只收 gray:true 事件 + promote 补发事件
    - 稳定客户端：只收 gray:false 事件
  对 after_version 重放同样生效（重放基于 version_history，按 resolve 过滤）

方案 b（客户端契约，实现简单但语义靠 SDK 保证）：
  gray 标记事件永不按版本过滤、无条件重拉
  SDK 缓存版本只取自快照响应（不取事件版本）
  abort/promote 必须携带回落/新版本号
```

**事件字段（Q3 修订）**：
```
PublishEvent / VersionRecord：加 #[serde(default)] gray: bool
  - 灰度事件 gray=true（复用既有 EventType，如 ValuePublish）
  - 不新增 EventType 枚举值（否则新节点写灰度 VersionRecord 进快照，
    旧节点装快照反序列化失败——快照传输面的兼容破口）
WatchEvent（proto）：加 bool gray = 8（向后兼容）
```

**SDK 契约（配合方案 b 兜底）**：
```
- watch 回调收到 gray:true 事件 → 无条件重拉全量（无论版本号）
- 缓存版本号只从 snapshot 响应更新（不信任事件版本）
- abort 事件携带回落版本号 → SDK 据此重拉稳定版
```

## 5.6 数据面调用点（Q6 审核明确）

**需传 client_ctx 做灰度解析（仅 3 处）**：
```
1. HTTP snapshot    GET /v1/projects/{p}/branches/{b}/snapshot   （lib.rs:1790 附近）
2. gRPC get_config  GetConfigRequest                             （grpc.rs:142）
3. gRPC get_item    GetItemRequest                                （grpc.rs:161，必须同样 resolve！）
```

**明确绕过灰度解析（管理面/历史）**：
```
- render/reveal（lib.rs:1925）：version=0 解析稳定 active（管理员看稳定客户端所见）；
  要看灰度明文须显式传灰度版本号（灰度记录在版本历史中）
- branch_diff/compare：按显式版本对比，不涉及灰度解析
- testkit/jobs 内部调用：绕过（内部确定性路径）
```

**响应补充**：snapshot 响应加 `"gray": true/false` + `"resolved_version"`（客户端可见自己处于哪个版本）。

---

## 6. 实现清单（代码要改哪些地方）

> ✅ = G2 已落地（2025-08-16）；⬜ = 后续阶段（G3/G4）。

| 层 | 文件 | 改什么 | 状态 |
|----|------|--------|------|
| 模型 | `model.rs` | `BranchState` 加 `gray_seq`/`gray_rule`（serde default）；新增 `GrayRule`/`LabelSelector`；`PublishEvent`/`VersionRecord` 加 `gray: bool`（serde default，Q3） | ✅ |
| 命令 | `command.rs` | **纯新增** `GrayPublish`/`GrayAbort`/`GrayPromote` 三个变体（旧命令不动，Raft wire 兼容） | ✅ |
| 状态机 | `state.rs` | 三个 apply 方法 + `resolve_version`/`rule_matches`/`fnv1a_hash`/`ip_in_cidr`（读路径纯函数）+ 结构发布×灰度双号 bump（D23）+ `gray_snapshot_of` + `rewrap_deks` 覆盖灰度快照 | ✅ |
| 存储 | `keys.rs` | 新增 `gray_snap_key(pid, branch, seq)`（独立前缀 gray-snap/，不与 v/ 冲突） | ✅ |
| API | `lib.rs` | 4 个管理端点 + snapshot/render/watch 解析身份头 + snapshot 响应 `gray`/`resolved_version` | ⬜ G4（身份注入 ⬜ G3） |
| proto | `config.v1.proto` | `GetConfigRequest`/`WatchRequest` 加 `instance_id`/`labels` 字段（向后兼容）；`WatchEvent` 加 `gray` 标记 | ⬜ G3 |
| SDK | 三语言 | 加 `instance_id`/`labels` 配置项 + 上报 + watch 按 gray 过滤 | ⬜ G3/G4 |
| UI | `admin/app.js` | 灰度 tab（规则编辑/状态/一键提升/一键回滚） | ⬜ G4 |

> G2 实现要点（与设计逐条对齐）：promote 的 `next = max(active, gray)+1` 单调分配器（Q1）；
> 灰度快照存**全量 SnapshotMap**（非 diff 链——仅当前灰度一个活跃快照，读路径直接命中）；
> 结构发布灰度活跃时 `stable_next = max+1`、`gray_next = stable_next+1` 分配两个不同号（Q1/D23）；
> abort 不产生新版本，事件携带回落版本号 = active_version（Q4）；
> 事件 `changes`：publish 为 diff(active, gray)（稳定→灰度的增量），promote 为 diff(old, gray)，abort 为空。
> 已知限制（后续阶段处理）：gray-snap/ 快照随灰度发布累积（当前仅分支删除级联清理，回收策略留待 G4+）；
> watch 数据面按身份投递属 G3（G2 保证事件字段与重放 gray 标记正确）。

---

## 7. 为什么这样实现是安全的（不破坏现有成果）

1. **纯新增命令变体**（沿用多会话 B1/N10 纪律）——旧节点/旧日志完全兼容，混合版本集群不分裂；
2. **selector 求值在读取路径**（`resolve_version` 是纯函数）——apply 不读墙钟/请求，D16 确定性保持，Raft 重放一致；
3. **灰度版本复用方案② diff/checkpoint 存储**——不引入新的写放大，灰度发布也是 1 次 write_batch；
4. **写性能不受影响**——灰度命令是新命令（低频管理操作），数据面只是多一次整数/字符串比较；
5. **回滚是一键的**——GrayAbort 摘指针，所有客户端立即回落，无需重新发布旧版本。

---

## 附录：决策记录（D17-D23）

| 决策 | 结论 | 一句话理由 |
|------|------|-----------|
| D17 灰度模型 | 分支内双版本（非独立分支） | 独立分支无法按实例定向，且破坏"分支=环境"语义 |
| D18 身份稳定键 | instance_id 优先 > labels > IP | 容器重建 IP 变，分桶漂移（roadmap 风险 2） |
| D19 规则形态 | 标签 OR 集 + IP 段 + 百分比 | 覆盖定向灰度 + 放量灰度两种模式 |
| D20 解析位置 | 数据面读路径求值，apply 只存规则 | D16：apply 不读请求，规则是状态机数据 |
| D21 生命周期 | publish → 观察 → promote/abort | 转正与下量都是显式命令，可审计 |
| D22 watch 语义 | 事件带 gray 标记；promote 补发全量 | 防"灰→全量"切换漏收（roadmap 风险 1） |
| D23 结构发布 × 灰度 | 灰度版本同步 bump 不失效 | 灰度期间结构演进不中断观察 |

---

## 附录三：百分比分桶算法（G5 文档化，D33 配套）

**语义**：`fnv1a_hash(instance_id) % 100 < percentage` 命中灰度（`GrayRule.percentage` 为 0-100）。

**算法**（`StateMachine::fnv1a_hash`，32 位 FNV-1a，纯函数）：

```
h = 0x811c9dc5
for b in instance_id.as_bytes():        // UTF-8 字节序
    h ^= b
    h = (h * 0x01000193) mod 2^32
命中 ⇔ h % 100 < percentage
```

**确定性论证（跨节点同桶）**：
1. `fnv1a_hash` 是纯函数（无墙钟/随机/IO）——同一 instance_id 恒同哈希；
2. 规则（percentage）是**状态机数据**，经 Raft 复制到全部节点——各节点读到同一规则；
3. `rule_matches` 求值次序固定（labels → IP → percent），percent 判据仅依赖 (hash, pct)；
4. 结论：集群任意节点对同一 instance_id 解析结果逐位一致（G5 集群测试
   `gray_percentage_consistent_across_nodes` 3 节点实测验证）。

**边界与约束**：
- 无身份（instance_id 空）永不进灰度（Q2 门闩在 percent 判据之前）——空串哈希恒恒定，
  禁止参与分桶；
- 容器重建时 instance_id 不变 → 分桶稳定；IP 漂移不影响 percent 桶（D18）；
- 调整 percentage 会立即改变放量面（规则是活数据）；50 → 51 只多出哈希余数=50 的桶；
- 分布式一致性无需额外共识：分桶在读取路径按同一状态机数据求值，天然一致。

## 明确不做（本期）

- 流量治理/动态路由（网关职责）；
- 自动回滚决策（只留 `dsh_gray_active` 指标 + 钩子）；
- 多级灰度（A/B/C，双版本封顶）；
- K8s 原生灰度（上层平台对接，本设计提供数据面能力）。

---

## 附录二：高精度审核修订记录（2025-08-16，子代理 Q1-Q6）

| # | 审核问题 | 严重度 | 处理 |
|---|---------|--------|------|
| Q1 | **版本号冲突**：gray=active+1 与普通发布共用单调基底 → 双指针同号、快照互相覆盖（含结构发布撞号） | 🔴 阻塞 | ✅ 修订：**独立灰度序号 gray_seq + 独立前缀 gray-snap/**；实现时用分支级单调分配器 `next = max(active, gray)+1`，结构发布灰度活跃时一次分配两个不同号；灰度命令补 I10 幂等（last_request_id） |
| Q2 | **确定性两缺口**：无身份客户端未定义（空串哈希恒恒定）；求值顺序未固定 | 🟠 修订 | ✅ 补"**无身份永不进灰度**"（instance_id 空 → 直接 stable）；labels/IP/percent 按**固定次序**求值（labels → IP → percent），纯函数 |
| Q3 | **新增 EventType 枚举值破坏快照兼容**：新节点写灰度 VersionRecord 进快照，旧节点装快照反序列化失败 | 🔴 阻塞 | ✅ 修订：灰度事件**复用既有 EventType**（ValuePublish），`PublishEvent`/`VersionRecord` 加 `#[serde(default)] gray: bool`；proto `WatchEvent` 加 `bool gray=8`（向后兼容） |
| Q4 | **watch 漏收**：现有 `version > last` 过滤会滤掉 promote/abort 事件，gray 标记根本没机会被读；abort 连版本号都没带 | 🔴 阻塞 | ✅ 修订（§5.5）：推荐方案 a（watch 连接注册身份、服务端按 resolve 投递）；方案 b 兜底（gray 事件永不按版本过滤 + SDK 缓存版本只取快照响应 + abort 携带回落版本号） |
| Q5 | **prune 裁掉灰度快照**：prune_versions 只保 `no==active_version`，灰度期间普通发布会把 gray 指向的快照裁掉 → 灰度客户端 NotFound | 🟠 修订 | ✅ 修订：保留条件加 `\|\| no == gray_version`（Abort 后历史可查同样依赖） |
| Q6 | **数据面调用点未明确**：get_item 走同一解析必须 resolve；reveal/对比应绕过 | 🟡 修订 | ✅ 明确（§5.6）：仅 HTTP snapshot / gRPC get_config / gRPC get_item 三处传身份；render/reveal/branch_diff 绕过；snapshot 响应加 `gray`/`resolved_version` 字段 |
