# 设计文档：写路径事务合并（perf 方案①）

> 状态：待审核 ｜ 日期：2025-08-16 ｜ 依据：[perf-write-path.md](../perf-write-path.md) 方案①
> 目标：把一次写命令（Publish/Rollback/SharedPublish/结构发布等）的多次独立 redb 写事务
> 合并为单事务单 fsync；为方案②（D3 diff 存储）与方案③（RwLock）铺路。

---

## 1. 现状与问题（代码证据）

- `StateMachine` 内写操作走自由函数 `save(&*self.store, ...)`（state.rs:41-44），每个 key 一次 `store.put()`；
- `RedbStorage::put` 每次独立 `begin_write + commit`（dsh-storage lib.rs:151-162），commit 即 fsync（Immediate）；
- 一次 `Publish` 的 `apply_publish` 产生 3 次 save（state.rs:1046/1047/1051）→ **3 次 fsync**；
- 集群模式叠加 raft 日志 append + `write_last_applied`（store.rs:590）→ **5 次 fsync/命令**；
- 实测：dev-single 内存模式 ~1339 QPS；redb 落盘模式 ~24 QPS（macOS，fsync 主导）。

## 2. 目标

| 路径 | 现状 fsync/命令 | 目标 fsync/命令 |
|------|----------------|----------------|
| dev-single publish | 3 | **1** |
| 集群 publish | 5（日志 1 + 状态 3 + last_applied 1） | **2**（日志 1 + 状态含 last_applied 1） |

P0-1（本期）：状态机内部多次写合并（3 → 1）。
P0-2（评估项）：last_applied 并入状态事务（集群 3 → 2，需跨表事务，风险高，单独评审）。

## 3. 关键约束：写后读依赖（必须处理）

合并为"命令末统一落盘"后，**同一命令内先写后读的路径必须读到未提交的写**。逐命令审计（state.rs）：

| 路径 | 写后读依赖 | 说明 |
|------|-----------|------|
| `apply_publish`（946） | ❌ 无 | save 之间无读 |
| `apply_publish_structure`（781） | ❌ 无 | 循环内 save 后不再读该 key |
| `apply_rollback`（1068） | ❌ 无 | 读旧版本在写之前 |
| `apply_shared_publish`（1216） | ✅ **有** | 级联 `cascade_to_project`（1322-1358）先读 branch_state/snapshot_of，后写新版本；同一命令内多个共享项级联到同一分支时，第二次级联必须读到第一次写入的 `active_version`，否则版本号冲突 |
| `apply_audit_append`（1725） | ✅ **有** | 读 `K_AUDIT_SEQ` → +1 → 写 `audit/seq` + `audit/{seq}`；同一命令内两次 append 必须读到递增后的 seq |
| `apply_branch_create`（668） | ❌ 无 | source 物化读旧数据 |
| 会话/PA/轮换 | ❌ 无 | 单 key 读写 |

**结论**：不能简单"收集写、最后 put_batch"——必须在 StateMachine 层做**写缓冲 + 读合并**（pending 优先），
命令结束时 flush（一次事务）。这正是"命令级事务"语义，同时天然满足 raft 批量 apply 的跨命令可见性
（逐命令 flush，后续命令读已提交值）。

## 4. 设计

### 4.1 Store trait 扩展（dsh-storage）

```rust
pub trait Store: Send + Sync {
    // …现有 get/put/delete/get_prefix/put_batch/flush 不变…
    /// 批量写 + 批量删（单事务，原子提交；redb 一次 fsync；内存实现循环）。
    fn write_batch(
        &self,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<(), Error>;
}
```

- `RedbStorage`：单 `begin_write` 事务内对 `TBL_STATE` 逐条 insert/remove，一次 commit（lib.rs 现有 put_batch 模式扩展，加 deletes）；
- `InMemoryStore`：先删后插（保持 put 覆盖语义），循环操作内存 map。

### 4.2 StateMachine 写缓冲 + 读合并（dsh-core）

```rust
pub struct StateMachine {
    store: Box<dyn Store>,
    // 命令级写缓冲（apply 期间非空；命令结束 flush 或 abort 清空）
    pending_puts: Vec<(Vec<u8>, Vec<u8>)>,
    pending_deletes: Vec<Vec<u8>>,
}
```

私有方法（替换自由函数，调用点机械迁移）：

```rust
fn save<T: Serialize>(&mut self, key: &str, value: &T) -> Result<(), Error>   // 写缓冲
fn delete(&mut self, key: &str) -> Result<(), Error>                          // 删缓冲
fn load<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, Error>    // 读合并：pending 优先（删→None；插→最新值），miss 走 store
fn get_prefix(&self, prefix: &str) -> Result<KeyValuePairs, Error>            // 读合并：store 结果 + pending 插入，剔除 pending 删除，BTreeMap 保序
```

**apply 包装（唯一入口，保持签名不变）**：

```rust
pub fn apply(&mut self, cmd: &Command, now_ms: i64) -> ApplyOutcome {
    self.pending_puts.clear();
    self.pending_deletes.clear();
    let result = self.apply_inner(cmd, now_ms);      // 现有 apply 主体改名，内部 save/load/delete/get_prefix 改用新方法
    match result {
        Ok(events) => {
            self.flush_pending()?;                   // write_batch(puts, deletes)；失败 → internal error
            Ok(events)
        }
        Err(e) => { self.pending_puts.clear(); self.pending_deletes.clear(); Err(e) }
    }
}
```

**flush 失败语义**：`write_batch` 失败 → 返回 internal error（同现状"storage 错误罕见"的处理水位）；
pending 清空，下次 apply 重新开始（raft 重放兜底，与现状部分落盘相比是增强而非劣化）。

**读合并细节**：
- `load`：`pending_deletes.contains(key)` → None；`pending_puts` 逆序找 key（后写覆盖）→ 命中即返回；否则 `store.get`；
- `get_prefix`：以 `store.get_prefix(prefix)` 为基，合并 `pending_puts` 中前缀命中的 key（覆盖），剔除 `pending_deletes` 命中 key，收集为 `BTreeMap` 保证字典序（与 store 前缀扫描一致）；
- **`snapshot_of` 内部直读必须改走读合并**（审核发现）：级联 `cascade_to_project`（1322-1358）在同一命令内先 `save(snapshot_key(vno), new_snap)`、当第二个共享项级联到同一分支时又 `snapshot_of(active_version=vno)` 读取刚写的快照（1330 行）——直读 store 会读不到 pending 快照而报 not found。同理 `get_version_record`/`version_history` 走 `load`/`get_prefix` 读合并后无副作用（无 pending 时行为等同直读）。

### 4.3 调用点迁移清单

state.rs 内全部写/读调用改为方法：
- `save(&*self.store, ...)` × ~30 处 → `self.save(...)`
- `self.store.delete(...)` × ~14 处 → `self.delete(...)`
- `load(&*self.store, ...)` × ~20 处 → `self.load(...)`
- `self.store.get_prefix(...)` × ~10 处 → `self.get_prefix(...)`
- `self.store.put(...)`（restore_all:353）→ 快照安装路径，**保持直写 store**（不经缓冲；restore_all 是整库恢复，非命令语义）
- `self.store.get(...)`（snapshot_of:301）→ **改走读合并**（`self.load` 语义；级联写后读场景必须，见上）；restore_all:353 的 `put` 保持直写

**风险**：纯机械迁移，错漏由 130 测试 + e2e 兜底；`restore_all`/`dump_all` 明确排除在缓冲外。

### 4.4 dev-single 与集群共用

- dev-single（raft.rs:175 `guard.apply`）：apply 内自动 flush → 3 fsync → 1；
- 集群（store.rs:562 `sm.apply`）：同样自动 flush → 状态 3 → 1；`write_last_applied` 维持独立事务（P0-2 再评估）；
- `dsh-jobs`（DEK 重包/裁剪/审计保留）与 `rewrap_deks`：走 `store.put`/`save` 直写路径，**不经 apply 缓冲**（后台任务非命令语义，逐 key 提交保持现状；`rewrap_deks` 内 save 改为 `self.save` 会引入缓冲但从不 flush → 必须**保持 store 直写**或提供显式 flush。**决策：rewrap_deks 保持自由函数 save（store 直写）**，不迁移）。

## 5. 测试计划

| 用例 | 断言 |
|------|------|
| T1 单命令多写合并（state_machine.rs 新增） | `Publish` 后 store 中 version/snapshot/branch_state 三者一致（与现状等价的断言已有，补一条"apply 后可直接读新版本"） |
| T2 级联写后读（回归，已有 SHR-001 类） | 同一命令两个共享项级联同一分支：两次版本号递增不冲突（**必须过**，读合并的关键回归） |
| T3 审计 seq 递增（回归） | 同命令多次 AuditAppend：seq 单调递增、无覆盖（**必须过**） |
| T4 失败回滚（新增） | 命令 apply 失败（如 publish 无草稿）：pending 清空、store 无部分写（对比旧实现） |
| T5 read-merge 单测（新增） | `load`/`get_prefix` 对 pending 覆盖/删除的合并正确性（含逆序覆盖、前缀边界） |
| T6 既有 130 测试全绿 + e2e（dev-single-demo / cluster-demo / chaos / api-surface / sdk 契约） | 无回归 |
| T7 性能对比（bench） | redb 落盘模式写 QPS ≥ 2× 基线（24 → ≥48） |

## 6. 验收标准

1. `cargo test --workspace` 全绿（含新增 T1/T4/T5）；
2. 4 个 e2e 脚本全过；
3. redb 落盘模式写 QPS ≥ 2× 基线（scripts/bench.sh 增加落盘模式对比行）；
4. 不破坏 D16 确定性（apply 逻辑零墙钟/零 IO 新增；仅落盘时机后移且命令内可见性等价）；
5. 文档更新：perf-write-path.md 方案①标记完成。

## 7. 明确不做（本期）

- last_applied 跨表合并（P0-2）：需 RedbStorage 暴露 WriteTransaction 注入 StateMachine 写路径，风险高，单独评审；
- raft 日志 append 与状态写合并：日志必须先于状态可见（raft 语义），不可合并；
- 方案②③④：另立方案文档，本设计为其铺路（write_batch 是方案② checkpoint 的基础设施）。
