# 设计文档：G1 发布策略地基（D1 收尾——三旋钮）

> 状态：**已完成**（2025-08-16）｜ 基线：main `679430e`（G5 已落地）
> 验收：三旋钮 e2e 全过（warn 放行+warnings / manual 不级联+下次物化 / linear 读）；core 测试 2 项；workspace 31 套件全绿
> 前置：design-v2.md §2.5/§4.6/§4.7（read-mode / publish-policy / shared-cascade 定义）
> 一句话：**把三个发布/读取策略旋钮做成命令行参数——校验 warn 放行、共享发布手动级联、线性读门控。**

---

## 0. 现状与缺口

| 旋钮 | 现状 | 缺口 |
|------|------|------|
| `--publish-policy` | 无参数；`apply_*` 内 `validate_publish` 失败恒报 `ERR_PUBLISH_BLOCKED`（state.rs materialize_resolved） | warn 模式（校验失败继续发布 + 记录明细） |
| `--shared-cascade` | 无参数；`apply_shared_publish` 恒 auto 级联（state.rs:1533） | manual 模式（只更共享版本，引用分支下次发布物化） |
| `--read-mode` | 无参数；全部读直接读本地 sm（恒 stale 语义） | linear 模式（ReadIndex 门控，读已提交） |

**关键约束（D16 确定性）**：publish-policy 与 shared-cascade 影响 **apply 结果**（warn 放行 / 不级联）。
若它们是节点配置，集群内配置不一致 → 同一日志重放结果分歧 → Raft 分裂。
**解法：把策略编码进命令（日志）**——API 层按节点配置生成命令时注入策略字段，follower 重放
从日志读同一策略 → 确定性由日志序保证。

---

## 1. 决策（D35-D37）

### D35：`--publish-policy=block|warn` —— 策略进发布命令

```rust
// dsh-core model
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PublishPolicy { #[default] Block, Warn }

// command.rs —— 纯新增字段（#[serde(default)]=Block，旧日志/旧节点兼容：
// serde 缺省忽略未知字段，旧节点读新日志安全）
Command::Publish        { …, #[serde(default)] policy: PublishPolicy }
Command::GrayPublish    { …, #[serde(default)] policy: PublishPolicy }
Command::SharedPublish  { …, #[serde(default)] policy: PublishPolicy }
Command::PublishStructure{ …, #[serde(default)] policy: PublishPolicy }
```

- **apply 语义**：`materialize_resolved`（值/灰度发布共用）与结构/共享发布校验处——
  `errs 非空 && policy==Block` → `ERR_PUBLISH_BLOCKED`（现状不变）；
  `errs 非空 && policy==Warn` → **跳过校验继续发布**，errs 记入响应/审计 detail；
- **API 层**：PublishService 各发布方法从 CLI 配置读 policy 注入命令；
- **确定性**：policy 在命令（日志）中，全节点重放一致 ✓；
- **审计/响应**：warn 发布时 audit detail 带 `validation_warnings: errs`；发布响应带 `warnings`。

### D36：`--shared-cascade=auto|manual` —— 模式进 SharedPublish 命令

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SharedCascadeMode { #[default] Auto, Manual }

Command::SharedPublish { …, #[serde(default)] cascade: SharedCascadeMode }
```

- **apply 语义**：`apply_shared_publish` 级联循环 `if cascade == Auto`（现状）→ 引用分支版本推进；
  `Manual` → **只更新共享库版本**，不级联；
- **引用分支"下次发布物化"**：现状 `materialize_resolved`（Publish/GrayPublish 时）已读当前
  共享值补全引用——manual 模式下引用分支下次发布天然物化最新共享值，无需额外逻辑 ✓；
- **确定性**：cascade 在命令中 ✓。

### D37：`--read-mode=linear|stale` —— ReadIndex 门控（读不产生日志，节点配置即可）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadMode { #[default] Stale, Linear }   // 修订：默认 Stale（见下）
// ApiState 加 pub read_mode: ReadMode（pub 字段，main 构造后设置，不破坏构造函数签名）
```

- **默认 Stale（修订）**：本地直接读（现状，零破坏）。design-v2 草案的 "linear 默认" 落空原因：
  linear 需要读转发链路（见下），默认启用会破坏多节点集群下既有 SDK 的 follower 读
  （读请求变成 428 重定向，SDK 尚无读跟随逻辑）；
- **linear（显式开启）**：读前 `raft.ensure_linearizable()`（openraft 0.9 ReadIndex）。
  **openraft 0.9 无 follower 侧 ReadIndex**——follower 上 `ensure_linearizable` 返回
  `CheckIsLeaderError::ForwardToLeader`。处理：**复用写路径重定向机制**——
  返回 `ERR_LEADER_REDIRECT`（HTTP 428）+ leader_hint = leader http_addr，客户端跟随；
  leader 上 ensure_linearizable 通过后本地读；dev-single（无 raft）恒满足；
- **覆盖入口**（全部同步读）：HTTP snapshot / render_config / admin_config / branch_diff /
  gRPC get_config / get_item；watch 是事件流（apply 后广播 + 已提交重放），不适用；
- **实现**：`ApiState::linearized_read()` 辅助 + `leader_http_hint()`（从
  ForwardToLeader 提取 leader http）；各读 handler 开头调用。

---

## 2. 代码改动清单

| 文件 | 改动 |
|------|------|
| `dsh-core/src/model.rs` | `PublishPolicy` / `SharedCascadeMode` / `ReadMode` 枚举 |
| `dsh-core/src/command.rs` | 4 个发布命令加 `#[serde(default)] policy`；SharedPublish 加 `#[serde(default)] cascade` |
| `dsh-core/src/state.rs` | `materialize_resolved` 接受 policy（warn 跳过校验）；`apply_publish_structure`/`apply_shared_publish` 校验/级联按命令字段分支 |
| `dsh-publish/src/lib.rs` | 各发布方法加 policy/cascade 参数（从配置注入）；PublishOutcome 带 warnings |
| `dsh-api/src/lib.rs` | ApiState.read_mode（pub 默认 Linear）；`linearized_read()`；各读 handler 接线；warn 发布审计 detail/响应 warnings |
| `dsh-api/src/grpc.rs` | get_config/get_item 开头 linearized_read |
| `dsh-cli/src/main.rs` | `--publish-policy` / `--shared-cascade` / `--read-mode` 参数 → PublishService/ApiState 注入 |
| 测试 | core（warn 放行 / manual 不级联 / 默认 block）；集群（linear 读一致性）；e2e（三旋钮断言） |
| `docs/roadmap-p4.md` / `plan-gray-g1.md` / `g1-policy.md` | 状态标记 |

**明确不做（本期）**：单 item 回滚（D1 旁注）；发布锁（D4 旁注）；read-mode=linear 的 leader 转发
（SDK 侧已有 ERR_LEADER_REDIRECT 跟随，ReadIndex 足够）；warn 模式下的部分发布
（warn = 全量继续，非跳过坏项）。

---

## 3. 验收标准

- core：warn 发布（含缺失 required）成功 + 审计带 warnings；manual 共享发布不级联 + 引用分支
  下次发布物化新值；默认 block 行为不变（回归）；
- 集群：linear 模式下 follower 读返回已提交数据（与 leader 一致）；
- e2e：三旋钮命令行 + 行为断言全过；workspace + clippy/fmt + contract 全绿；CI 8/8。

## 4. 风险

| 风险 | 对策 |
|------|------|
| 策略进命令改既有命令字段（B1/N10 纪律） | 纯加 `#[serde(default)]` 字段：旧日志反序列化取默认、serde 忽略未知字段——旧节点安全；不新增变体 |
| warn 放行导致缺项发布 | 设计语义即"运维知情放行"；审计 detail 留痕 + 响应 warnings 可见 |
| ReadIndex 失败（无 quorum） | 线性读返回错误（503 语义）；stale 模式可绕过（运维选择） |
| manual 模式下引用分支读到旧值 | 语义即"下次发布物化"；文档明示 |
