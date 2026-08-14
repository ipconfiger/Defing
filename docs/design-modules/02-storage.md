# 模块 02 —— RocksDB 封装（dsh-storage）

> 依据：design-v2 §3.2/§13、schema/storage.v1.schema.json
> 版本：v1.0 ｜ 状态：开发就绪

## 1. 职责与边界
- 职责：RocksDB 打开/配置、前缀读写、批量写、列族划分、快照导出（备份）、openraft 日志/快照存储。
- 不做：业务键语义（由 dsh-core 提供键构造）；不做分布式一致性（上层 Raft 保证）。

## 2. 列族划分
| 列族 | 内容 |
|------|------|
| default | 状态机 KV（p/、sh/、idx/、sess/、audit/） |
| raft-log | openraft 日志条目（key = log_index BE u64） |
| raft-meta | openraft 元数据（term/vote/commit/snapshot 指针） |
| snapshots | 快照数据块（备份/追赶） |

## 3. Storage trait（应用层唯一入口）

```
pub trait Storage: Send + Sync {
    fn get(&self, cf: Cf, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn get_prefix(&self, cf: Cf, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    fn put(&self, cf: Cf, key: &[u8], value: &[u8]) -> Result<()>;
    fn put_batch(&self, cf: Cf, pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<()>;
    fn delete(&self, cf: Cf, key: &[u8]) -> Result<()>;
    fn delete_prefix(&self, cf: Cf, prefix: &[u8]) -> Result<()>;
    fn flush(&self) -> Result<()>;
    fn checkpoint(&self, path: &Path) -> Result<()>;   // 备份/快照源
}
```

## 4. 实现要点（RocksDb impl）
- 打开选项：create_if_missing、atomic_flush=true、WAL 默认开启；`--storage-dir`（默认 ./data）。
- 批量写：单次 WriteBatch 提交（配合 Raft apply 原子性）。
- 迭代器：前缀扫描用 prefix_extractor（FixedPrefixTransform）提升性能。
- 并发：RocksDB 线程安全；Storage 包 Arc<RocksDb>；读路径可并发。
- 快照：Checkpoint::create 用于备份与 Raft 快照源（避免拷贝不一致）。

## 5. Raft 日志/元数据存储（openraft 需要）
- RaftLogStorage：raft-log / raft-meta 列族；实现 openraft::RaftLogStorage：
  append/apply_id/read 日志条目、get_log_state（last_purged_log_id / last_log_id）、
  snapshot（返回 rocksdb checkpoint 句柄）。
- （具体 trait 签名以 openraft 版本为准，M1 固定版本后核对。）

## 6. 备份与恢复
- 备份：`dsh admin snapshot` → checkpoint(path)；归档建议：快照 + WAL。
- 恢复：新节点 `--data-dir` 指向恢复目录 + 身份文件 → 正常 rejoin。

## 7. 错误处理
- 包装为 `ErrorKind::Storage`；打开失败（权限/损坏）→ 启动失败并给出修复指引。

## 8. 测试要点
- 前缀读写/批量写/删除前缀正确性；checkpoint 后读一致；
- 崩溃恢复：写入后 kill（WAL 未 flush）→ 重开可读；
- 与 dsh-core 键构造联测：序列化往返（用 storage schema 的 golden JSON）。

## 9. 任务清单
□ Cargo 依赖（rust-rocksdb 固定版本） □ 列族初始化与 schema_version 检查
□ Storage trait + RocksDb impl □ RaftLogStorage □ checkpoint/备份
□ 崩溃恢复测试 □ golden 序列化测试（对照 schema/storage.v1.schema.json）
