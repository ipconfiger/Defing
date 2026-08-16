# 开发计划：方案① 写事务合并（perf-write-batch）

> 依据：[design/perf-write-batch.md](design/perf-write-batch.md) ｜ 目标：3 fsync → 1（dev-single）/ 5 → 3（集群）

## 任务分解

| # | 任务 | 文件 | 验收 |
|---|------|------|------|
| 1 | `Store` trait 增加 `write_batch(puts, deletes)` | dsh-storage/src/lib.rs（trait 定义在 dsh-core/src/store.rs） | 编译通过 |
| 2 | `RedbStorage::write_batch` 单事务实现 | dsh-storage/src/lib.rs | 单事务原子提交 |
| 3 | `InMemoryStore::write_batch` 实现 | dsh-core/src/store.rs | 先删后插，内存一致 |
| 4 | `StateMachine` 增加 pending 缓冲字段 + `save/delete/load/get_prefix` 方法（读合并） | dsh-core/src/state.rs | 编译通过 |
| 5 | `apply` 改 `apply_inner` + 包装 flush/abort | dsh-core/src/state.rs | 语义等价 |
| 6 | 迁移全部调用点（save/delete/load/get_prefix → self 方法），rewrap_deks/restore_all 排除 | dsh-core/src/state.rs | 机械迁移无错漏 |
| 7 | 测试 T1/T4/T5 新增 + T2/T3 回归确认 | dsh-core/tests/state_machine.rs | 全绿 |
| 8 | 跑全量测试 + e2e 脚本 | 命令行 | 全绿 |
| 9 | 性能对比（redb 落盘模式 QPS ≥ 2×） | scripts/bench.sh + 手测 | 达标 |
| 10 | 更新 perf-write-path.md 方案①状态 | docs/perf-write-path.md | 标记完成 |

## 里程碑

- M1（任务 1-3）：Store 层就绪，`cargo test -p dsh-storage -p dsh-core` 绿
- M2（任务 4-6）：StateMachine 缓冲 + 迁移，`cargo test --workspace` 绿
- M3（任务 7-9）：测试 + 性能验证
- M4（任务 10）：文档收尾

## 风险与缓解

- 迁移错漏（30+20+14 处）：全量测试兜底 + 逐函数 grep 复核
- 读合并边界（前缀/逆序覆盖）：T5 单测覆盖
- flush 失败语义：返回 internal error，raft 重放兜底
