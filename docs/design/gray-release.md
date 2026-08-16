# 设计文档：灰度发布（G0 设计先行 · 含流程图）

> 状态：待审核 ｜ 日期：2025-08-16 ｜ 代码基线：main `4377f13`
> 本文档用**流程图 + 时序图 + 请求/响应示例**直白说明灰度是如何实现的。
> 抽象设计决策（D17-D23）见文末附录；正文先讲"怎么跑起来"。

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
  ⑥ 发事件 { gray_seq:1, ty:gray_publish }

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
  ④ 发事件 { version:新active, ty:gray_promote, gray:true }
     ← gray:true 标记是给"原来在灰度里"的客户端：它们收到后知道要重拉

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
  ② 发事件 { ty:gray_abort }

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

## 6. 实现清单（代码要改哪些地方）

| 层 | 文件 | 改什么 |
|----|------|--------|
| 模型 | `model.rs` | `BranchState` 加 `gray_seq`/`gray_rule`（serde default）；新增 `GrayRule` 结构 |
| 命令 | `command.rs` | **纯新增** `GrayPublish`/`GrayAbort`/`GrayPromote` 三个变体（旧命令不动，Raft wire 兼容） |
| 状态机 | `state.rs` | 三个 apply 方法 + `resolve_version`/`rule_matches`/`fnv1a_hash`（读路径纯函数） |
| 存储 | `keys.rs` | 新增 `gray_snap_key(pid, branch, seq)`（独立前缀 gray-snap/，不与 v/ 冲突） |
| API | `lib.rs` | 4 个管理端点 + snapshot/render/watch 解析身份头 |
| proto | `config.v1.proto` | `GetConfigRequest`/`WatchRequest` 加 `instance_id`/`labels` 字段（向后兼容）；`WatchEvent` 加 `gray` 标记 |
| SDK | 三语言 | 加 `instance_id`/`labels` 配置项 + 上报 + watch 按 gray 过滤 |
| UI | `admin/app.js` | 灰度 tab（规则编辑/状态/一键提升/一键回滚） |

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

## 明确不做（本期）

- 流量治理/动态路由（网关职责）；
- 自动回滚决策（只留 `dsh_gray_active` 指标 + 钩子）；
- 多级灰度（A/B/C，双版本封顶）；
- K8s 原生灰度（上层平台对接，本设计提供数据面能力）。
