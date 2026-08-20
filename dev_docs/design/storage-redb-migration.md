# 设计文档：存储层迁移 rocksdb → redb

状态: v2（已吸收 Oracle 交叉审核 B1/B2 阻塞与 N1-N7 建议，待复审）
日期: 2026-08-16
范围: dsh-storage / dsh-raft / dsh-cli / dsh-core(零改动) / 文档
关联调研: redb 4.1.0（docs.rs/CHANGELOG 验证）、仓库存储层侦察（exp-1）

## 1. 背景与目标

rocksdb（rust-rocksdb 0.23，C++ librocksdb-sys）在本机编译问题严重：bindgen/libclang 头文件注入、zstd 汇编 CFLAGS 冲突、编译 40s/12MB 产物。目标：**全链路改用纯 Rust redb 4.1**，彻底移除 C++ 工具链依赖，同时保持 Store trait 与 Raft 存储语义不变。

### 迁移决策（已确认）

| 决策点 | 结论 |
| --- | --- |
| 迁移范围 | rocksdb **全部**使用点：状态机 KV + Raft 日志/元数据/快照 + checkpoint 备份 |
| 数据兼容 | **不做数据迁移**（dev 阶段产品，无存量部署），`--data-dir` 换新目录布局，旧 rocksdb 目录直接失效 |
| redb 版本 | 4.1.0（v3 磁盘格式，全新 create 即可，无 upgrade 负担） |
| 混合部署 | 不支持（集群内所有节点同步升级） |

## 2. 现状与问题（侦察结论）

Store trait（dsh-core/src/store.rs:12-24）是干净的 6 方法抽象（get/put/delete/get_prefix/put_batch/flush），状态机只用前 4 个。但 rocksdb 泄漏到 trait 之外：

| 泄漏点 | 位置 | 内容 |
| --- | --- | --- |
| Raft 日志/元数据存储 | dsh-raft/src/store.rs:28,145-373 | `DbHandle=Arc<rocksdb::DB>`，LogStore/StateMachineStore 直接操作 raft-log/raft-meta CF，WriteBatch/flush_wal |
| 快照持久化 | dsh-raft/src/store.rs:47-76 | persist/load 到 snapshots CF |
| 重启恢复判断 | dsh-cli/main.rs:455-462 | `cf_handle("raft-meta")+raw_iterator` |
| checkpoint 备份 | dsh-storage/src/lib.rs:124-130 | rocksdb 专有 Checkpoint API |
| CF 布局 | dsh-storage/src/lib.rs:12-15 | 4 CF：default/raft-log/raft-meta/snapshots 单库 |

## 3. 目标架构

### 3.1 dsh-storage 重写为 redb 多表存储

```
RedbStorage {
  db: Arc<redb::Database>,
}
4 张 TableDefinition<&[u8], &[u8]>：
  STATE      = "state"       （原 CF_STATE）
  RAFT_LOG   = "raft-log"    （原 CF_RAFT_LOG）
  RAFT_META  = "raft-meta"   （原 CF_RAFT_META）
  SNAPSHOTS  = "snapshots"   （原 CF_SNAPSHOTS）
```

- 文件布局：`{data_dir}/dsh.redb` 单文件（原 rocksdb 目录 `db/` 弃用）。
- 打开：`Database::builder().set_repair_callback(..).create(path)`（对已存在合法库等价 open，首次运行初始化——语义正好覆盖两种情况；repair callback 只能经 Builder 配置，N2a）。
- **eager 建表（B1）**：`open` 内用一个写事务显式 `open_table` 建 4 表后 commit。原因：`WriteTransaction::open_table` 是 lazy 建表，但 `ReadTransaction::open_table` 对不存在的表返回 `TableError::TableDoesNotExist`——不预建则全新 `--data-dir` 下 `get`/`get_prefix`/重启恢复读路径在首写前全部报错。
- `check_integrity`（N2b/c）：仅在 `open` 内、`Arc::new(db)` 之前调用（该方法要求无存活事务且需 `&mut`）；官方注明 quite slow，**仅当 repair callback 触发时才调用**，不作为每次启动的例行步骤。
- 表定义单一来源（N5）：4 个 `TableDefinition<&[u8], &[u8]>` 常量定义在 dsh-storage 并 pub，dsh-raft 引用（redb 持久化校验 type_name，跨 crate 各自定义同名同型有漂移风险）。同一写事务内同名表二次 open 报 `TableAlreadyOpen`——truncate 类操作采用「range 收集 key → drop 迭代器 → 再 remove」模式。
- `RedbStorage: Clone`（内部仅 `Arc<Database>`，derive），对接 cli 的 `Box<dyn Store>` 由调用方包装；`create_dir_all(data_dir)` 保留（redb create 不建父目录，N6）。

### 3.2 Store trait 实现（状态机侧，零 trait 改动）

- `get/put/delete`：单 key 单事务。**Durability 无需显式设置（N1）**：WriteTransaction 默认即 `Durability::Immediate`（commit 返回时 fsync）。
  - 读写路径分离：get 走 `begin_read`（MVCC 无锁），put/delete 走 write txn。
- `get_prefix(prefix)`：`table.range::<&[u8]>(prefix..)` + `starts_with` break（redb 标准前缀扫描惯用法），收集 `Vec<(Vec<u8>,Vec<u8>)>`——与现 trait 返回类型一致（物化收集，不返回迭代器，trait 不变）。
- `put_batch(pairs)`：单 WriteTransaction 逐条 insert 后一次 commit（原子）。
- `flush()`：redb 每次 Immediate commit 已 fsync——实现为 no-op（保留 trait 默认语义）。

### 3.3 Raft 存储层（dsh-raft/src/store.rs 重写）

保持 openraft 的 Adapater 形状不变，内部改 redb：

- **LogStore**：
  - `raft-log` 表：append 用单 WriteTransaction 批量 insert（等价原 WriteBatch）；`flush_wal(true)` 语义由 `Durability::Immediate` commit 承担。
  - 日志条目序列化格式**不变**（serde_json），key 仍为日志索引的 big-endian u64 字节。
  - `raw_iterator_cf` → `RAFT_LOG.iter()`（升序，redb Range 是 DoubleEndedIterator 可 rev 取 last）。
- **StateMachineStore**：`raft-meta` 表 get/put，语义 1:1 映射。
- **快照持久化**：`snapshots` 表 put/get；快照记录（SnapshotMeta+chunk）序列化不变。
- **重启恢复判断**（dsh-cli/main.rs）：改为读 `raft-meta` 表 last key，或直接复用 StateMachineStore 现成方法（消除 cli 层 redb 直接依赖——cli 不 import redb，只调 dsh-storage/dsh-raft 的封装方法）。

### 3.4 备份（替代 rocksdb Checkpoint）

redb 4.x 无官方 backup API。方案（B2 具体化）：**持有未提交的 WriteTransaction 贯穿整个 copy 窗口 + 文件级 copy**：

- `create_backup(dest)`：`db.begin_write()` 拿到写事务后**不 commit、不写入任何数据，仅持有**（redb 单写者语义：写事务进行期间其他 begin_write 阻塞——这同时互斥状态机写与 raft 日志写，是 redb 下唯一的进程内全局写互斥点）→ copy `{data_dir}/dsh.redb` → `dest/dsh.redb` → drop guard（drop 即 abort，无副作用）。
- **执行线程**：copy 的文件 I/O 放 `tokio::task::spawn_blocking`（begin_write 是阻塞调用，copy 大文件期间所有 raft 写/心跳被挂起——按 §8.2 接受，但不能占死 tokio worker）。
- 一致性论证：写事务挂起期间文件不再变更，copy 得到精确的某一致提交点；跨进程二次 open 同一文件会被 `DatabaseAlreadyOpen` 拒绝，故进程内持锁 copy 是唯一路径。
- 恢复 = 停进程 + 替换文件（运维手册写明）。

## 4. API 映射表（实现对照）

| rocksdb 用法 | redb 4.1 替代 |
| --- | --- |
| `DB::open_cf(dir, [4 CFs])` | `Database::builder().set_repair_callback(..).create({dir}/dsh.redb)` + open 内写事务 **eager 建 4 表**（§3.1，读路径要求） |
| `get_cf/put_cf/delete_cf` | `begin_(read/write)` → `open_table(DEF)` → `get/insert/remove` → `commit` |
| `WriteBatch` + write_cf | 同一 WriteTransaction 内多 insert，一次 commit |
| `raw_iterator_cf + seek` | `iter()` / `range(..)`（有序遍历，item 是 Result 需 `?`） |
| `flush_cf` / `flush_wal(true)` | `Durability::Immediate` commit（默认） |
| `checkpoint::Checkpoint` | 文件级 copy（§3.4） |
| `set_atomic_flush` | 无对应——4 表分事务提交，raft-log 与 state 写入本就不同事务（与现状等价） |

## 5. 错误处理

- dsh-storage 定义 `StorageError` 适配（或复用 dsh-core::Error）：`redb::Error`/`TableError`/`DatabaseError`/`CommitError` → `From` 转换到 dsh-core `ErrorKind::Storage`（如无则新增 Internal 映射）。
- corruption：redb 打开时自动修复 + `set_repair_callback` 记日志；`check_integrity()` 仅在 repair callback 触发时调用（且须在 `Arc::new(db)` 之前，§3.1——不作为每次启动的例行步骤）。
- 禁 unwrap（红线）：所有 redb Result 显式处理。

## 6. 测试计划（TDD 先行）

**dsh-storage 单测（重写现有 3 个 + 新增）**
- `open_put_get_prefix`：建库/读写/前缀扫描（含空前缀=全表、不存在前缀、边界 key）——**空库直接 get_prefix 不报 TableDoesNotExist（B1 回归）**
- `crash_reopen_keeps_data`：Immediate 提交 → drop → 重开 → 数据在
- `backup_file_copy`：备份文件可被 `Database::open` 打开且数据完整（替代 checkpoint_works）
- 新增：`put_batch` 原子性；多表隔离（state 写入不污染 raft-log）
- **Raft 存储等价测试（N4a 补齐）**：LogStore append/read/last 之外，**truncate/purge/get_log_state 必测**（append→truncate→get_log_state；purge 后 last_purged 更新——范围翻译+多表事务最易错）；StateMachineStore 快照 persist/load roundtrip

**既有测试回归（零改动预期）**
- dsh-core 全部（state_machine/project_admin/model_serde——InMemoryStore 不受影响）
- dsh-api 全部（http_project_admin/grpc_data_plane——InMemoryStore）
- dsh-raft 3 个集成测试（cluster/snapshot_persist/forward_hint）——改用 RedbStorage 后跑通即证明 raft 层等价
- dsh-publish/testkit/observability/jobs

**端到端**：宿主机拉进程（禁容器编译）——dev-single 内存态 + `--data-dir` 持久态重启数据保留 + 集群 3 节点冒烟（deploy/docker-compose 用新二进制）。

## 7. 开发计划（波次划分，可并发）

| 波次 | 任务 | 文件 | 依赖 |
| --- | --- | --- | --- |
| W1-a | dsh-storage 重写：RedbStorage + Store impl + 备份 + 单测 | dsh-storage/* | 无 |
| W1-b | dsh-raft store.rs 重写：LogStore/StateMachineStore/快照（redb 版） | dsh-raft/src/store.rs | W1-a 的 raw_db 接口（可先按约定接口并行写，W2 联调） |
| W2 | dsh-cli 接线 + 移除 rocksdb 依赖（两处 Cargo.toml）+ Cargo.lock 更新 | dsh-cli/main.rs, Cargo.toml | W1-a+W1-b |
| W3 | 全量回归（单测+集成+e2e）+ 文档（README 构建说明简化、AGENTS 环境要求清理） | 全仓 | W2 |

- W1-a/W1-b 并发（接口约定先行：`RedbStorage: Clone` + `raw_db() -> Arc<redb::Database>`、表常量 dsh-storage pub 单一来源）。
- 验收（N4b 补 CI 门禁）：`cargo check --workspace --all-targets` 零 error；`cargo test --workspace`（raft 集成测试宿主机 rocksdb 环境问题随迁移消失，应可全绿）；`cargo fmt --check` + `cargo clippy --all-targets --all-features -- -D warnings`（仓库 CI 强制）；`rg rocksdb server/` 仅剩注释/文档。
- `OpenOptions.max_open_files` 语义失效（N6a）：3 个 raft 集成测试的 OpenOptions 构造同步清理。

## 8. 风险与权衡

1. **性能**：redb 批量写弱于 rocksdb（1595ms vs 451ms），但 dsh 写路径=Raft 复制后的 apply（单线程、低频），读多写少正中 redb 甜蜜点；不做性能优化预留。
2. **备份窗口持锁**：低写入频率可接受；文档明示「备份期间写入会延迟」。
3. **单文件体积**：无压缩，uncompacted 体积大于 rocksdb；提供 `compact()` 调用（停机维护或低峰定时，本期只在文档说明，不自动调度）。
4. **redb bug 面**：4.1.0 修复了 savepoint/并发系列问题；本设计不使用 savepoint，只用基础事务+range，避开复杂特性。
5. **数据不迁移**：dev 产品决策；`--data-dir` 内旧 `db/` 目录被忽略，新 `dsh.redb` 并存（不删用户文件）。
6. **单值 3 GiB 上限（N3）**：redb `MAX_PAIR_LENGTH ≈ 3.75GiB`；快照是单 key 存全量 dump，超限返回 `ValueTooLarge`——dev 配置中心远够用，文档记录。
7. **apply 每条目一 fsync（N7）**：现 per-entry `write_last_applied` 在 redb Immediate 下=每条独立事务+fsync，吞吐低于 rocksdb WAL 追加一个量级。本期维持 per-entry（重启重放边界语义等价优先，`sm.apply` 幂等性未论证前不合并为批末单写）。

## 9. 明确不做（本期）

- rocksdb→redb 数据迁移工具；自动 compaction 调度；ReadOnlyDatabase 运维模式；性能基准对比测试；多文件分表。
