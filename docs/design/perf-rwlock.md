# 设计文档：RwLock 读写分离（perf 方案③）

> 状态：待审核 ｜ 日期：2025-08-16 ｜ 依据：[perf-write-path.md](../perf-write-path.md) 方案③
> 目标：消除"全局 Mutex 串行化读写 + 锁内 fsync 阻塞读"——读走读锁（并发），写走写锁（独占）；
> 写 fsync 期间读不被阻塞，读并发提升。为方案④（日志重放 + Relaxed）预留架构空间。

---

## 1. 现状与问题（代码证据）

- `StateMachine` 被 `Arc<Mutex<StateMachine>>` 包裹，散布在 dsh-api（`app.sm`）、dsh-publish、dsh-observability、dsh-raft；
- **全部 48+ 处 `sm.lock()` 互斥**：读接口（get_config/snapshot/render/list/audit）与写接口（apply）同一把锁；
- `StateMachineStore::apply` 持锁期间做 redb 写事务 + fsync（store.rs:547-590）→ **写 fsync 阻塞所有读**；
- 报告 §2.4 指出："全局单把 Mutex 串行化读写；读多写少场景无并发读；锁内 IO 隐患"。

## 2. 目标

| 场景 | 现状 | 目标 |
|------|------|------|
| 读并发 | 互斥（单读） | **并发读**（RwLock 读锁） |
| 写 fsync 期间 | 阻塞所有读 | 读不被阻塞（redb MVCC 读事务天然支持） |
| 写吞吐 | 单写者（Raft 语义，不变） | 不变 |

**收益定位**：写 QPS 提升有限（写本身 Raft 串行）；核心收益是**读延迟稳定性**（写 fsync 不再卡读）
与**读并发吞吐**（当前 35k 为单锁上限）。方案①/②已把写 fsync 从 3 次降到 1 次，锁内 IO 时长已大幅缩短；
方案③在此基础上解除读写互斥。

## 3. 设计

### 3.1 锁类型替换

```rust
// 现状
pub struct ApiState { pub sm: Arc<Mutex<StateMachine>>, ... }
// 目标
pub struct ApiState { pub sm: Arc<RwLock<StateMachine>>, ... }
```

涉及 crate：dsh-api（ApiState/各 handler）、dsh-publish（PublishService.sm）、
dsh-observability（Observability.sm）、dsh-raft（StateMachineStore.sm + raft.rs write_command 签名）。

### 3.2 调用点分类（核心工作）

逐一审计 ~48 处 `lock()`，分为三类：

| 类别 | 判定 | 目标 API |
|------|------|----------|
| **读** | 仅调用 `&self` 方法（get_config/snapshot_of/get_branch_state/list_*/version_history/get_audit/get_structure/get_shared…） | `sm.read()` |
| **写** | 调用 `apply()` / `apply_inner()` / 需要 `&mut self` | `sm.write()` |
| **特殊** | 读-改-写复合（如 login 的 get_session→logout→login 序列、reveal 校验、draft 保存前读结构） | 按实际需要读锁或写锁（**不能读锁升级写锁**，RwLock 无升级语义 → 复合操作直接写锁或拆两步） |

**风险点**：
- `std::sync::RwLock` 的 `read()` 返回 `RwLockReadGuard`，`write()` 返回 `RwLockWriteGuard`；
  现有代码大量 `let sm = app.sm.lock()?;` 的 guard 类型推断会自动适配，但**所有 `.lock()` 调用点需逐个改为 `.read()` 或 `.write()`**；
- **死锁风险**：RwLock 写锁持有时再读（同线程）→ 死锁；审计每个写路径是否内部又调读；
- `poisoning` 语义：Mutex 中毒后 `.lock()` 返回 Err；RwLock 的 `read()/write()` 同样返回 PoisonError → 现有 `lock_err`/`map_err` 处理保留。

### 3.3 锁外 IO（事件广播后移）

`StateMachineStore::apply` 中事件广播（`events.send`）在写锁内——移至解锁后：

```rust
// 现状（store.rs:562-581）：持写锁期间 send
match sm.apply(cmd, now_ms) { Ok(events) => { for e in &events { self.events.send(...) } ... } }
// 目标：apply 返回 events 后，先收集，解锁后统一广播
```

**注意**：`apply` 是同步方法（`sm.write()` 持锁调用），events 收集后需在锁外 send——
需要把 guard 的 drop 与 send 分离（作用域化：`{ let mut sm = self.sm.write()?; ... sm.apply(...) }` 后再 send）。
dev-single 直写路径（raft.rs:175）同理。

### 3.4 影响面

| 位置 | 改动 |
|------|------|
| dsh-api/src/lib.rs | `ApiState.sm` 类型 + ~25 处 `lock()` → read/write 分类 |
| dsh-publish/src/lib.rs | `PublishService.sm` 类型 + 方法内 lock（encrypt_secret_updates 读结构→写 draft 前置） |
| dsh-observability/src/lib.rs | `sm` 类型 + metrics 读 |
| dsh-raft/src/store.rs | `StateMachineStore.sm` 类型 + apply 持写锁 + 事件锁外广播 |
| dsh-raft/src/raft.rs | `write_command` 签名（dev-single 直写路径） |
| dsh-testkit / dsh-jobs / 测试 | `Mutex<StateMachine>` → `RwLock<StateMachine>`（testkit 的 seed 用 `sm.lock().map_err(...)`） |

## 4. 测试计划

| 用例 | 断言 |
|------|------|
| T1 全量回归 | `cargo test --workspace` 全绿（既有 136+ 用例是对锁改造的正确性护栏） |
| T2 读写并发正确性 | 并发读（get_config ×N）期间执行写（publish），读结果一致（不 panic/不死锁） |
| T3 事件广播不丢 | 锁外广播后 watch 仍收到全部事件（既有 watch 测试回归） |
| T4 e2e | dev-single / cluster / chaos / api-surface 全过 |

## 5. 验收标准

1. `cargo test --workspace` 全绿；clippy/fmt 零告警；
2. 4 个 e2e 脚本全过；
3. 写 fsync 期间读延迟不劣化（bench 扩展：并发读+写混合）；
4. 确定性保持（apply 逻辑零改动，仅锁类型/广播时机变化）。

## 6. 明确不做（本期）

- 分片锁/每项目锁（Raft 单写者语义下无收益，复杂度高）；
- parking_lot RwLock（std 已够用，避免新依赖）；
- 方案④（Relaxed + 日志重放）——本方案为其铺路（锁外 IO 已就位）。

## 7. 审核修订记录（2025-08-16，子代理 Q1-Q5）

| # | 审核问题 | 结论/处理 |
|---|---------|----------|
| Q1 | dsh-api 24 处 lock 读写分类 | **全部为读**（比设计预想简单）——统一 `.read()`；promote/login/pa_login/publish_shared 的写走 `app.write()`（独立锁，读锁块已闭合），无升级死锁 |
| Q2 | 死锁风险 | **无**：所有 apply 持写锁调 `sm.apply`（内部只读自身字段，不重新拿 self.sm）；锁序一致（外层 sm → 内层 store） |
| Q3 | poison 语义 | **兼容**：`lock_err`/`unwrap_or_else(into_inner)`/match 模式均适用 RwLock guard；纯读站点的 `let mut` 产生 unused_mut 警告需清理 |
| Q4 | 锁外广播 | **可行**：apply 无内部 await，作用域化后 events（owned Vec）锁外 send；接受"状态先于事件可见"窗口；rotation_hook（ring 文件 IO）仍留写锁内（低频） |
| Q5 | 测试适配 | 9 个文件：testkit、raft store 内嵌测试、cluster/forward_hint/snapshot_persist、grpc_data_plane/http_project_admin、dsh-cli（**生产文件，设计漏列**）、observability/publish 测试 |

**实现状态**：生产代码完成（dsh-api 24 读锁 + grpc.rs 3 读锁、dsh-raft apply/write_command 写锁+锁外广播、
dsh-publish/observability/testkit/jobs/cli 适配）；测试文件 9 处适配完成；锁外广播在 StateMachineStore::apply
与 dev-single write_command 双路径落地。全量回归中。
