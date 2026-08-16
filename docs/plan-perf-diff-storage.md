# 开发计划：方案② D3 checkpoint/diff 存储

> 依据：[design/perf-diff-storage.md](design/perf-diff-storage.md)

## 任务分解

| # | 任务 | 文件 | 验收 |
|---|------|------|------|
| 1 | keys.rs 新增 `diff_key(pid, branch, vno)` | dsh-core/src/keys.rs | 编译 |
| 2 | `write_version_snapshot` 封装（checkpoint 判定 + full/diff 写入） | dsh-core/src/state.rs | 编译 |
| 3 | 4 个调用点迁移（apply_publish / apply_publish_structure / apply_rollback / cascade_to_project） | dsh-core/src/state.rs | 编译 |
| 4 | `snapshot_of` 改造（checkpoint 基座 + diff 链重建 + apply_diff） | dsh-core/src/state.rs | 编译 |
| 5 | `prune_versions` 裁剪适配（diff key + checkpoint 边界） | dsh-core/src/state.rs | 编译 |
| 6 | `rewrap_deks` 扩展扫描 diff key | dsh-core/src/state.rs | 编译 |
| 7 | 测试 T1-T6（checkpoint 布局/重建/回滚/级联/裁剪/DEK） | dsh-core/tests/state_machine.rs + src 内部 | 全绿 |
| 8 | 全量测试 + e2e + 大配置写字节对比 | 命令行 | 达标 |
| 9 | 更新 perf-write-path.md 方案②状态 | docs | 标记完成 |

## 里程碑

- M1（1-4）：存储布局 + 重建，`cargo test -p dsh-core` 绿
- M2（5-6）：裁剪/DEK 适配
- M3（7-8）：测试 + 验证
- M4（9）：文档收尾

## 风险

- diff 链重建正确性（含 secret 密文 diff）：T2/T6 覆盖
- 裁剪破坏 checkpoint 基座：T5 覆盖 + 边界策略
- 与方案① pending 读合并交互（snapshot_of 走 load_merged）：既有 T4/T5 回归
