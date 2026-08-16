# 开发计划：G2 灰度核心状态机

> 依据：[design/gray-release.md](design/gray-release.md)（G0 已完成，审核 Q1-Q6 闭环）
> 目标：分支内双版本（稳定 + 灰度）状态机支持——3 个灰度命令 + 独立灰度快照存储 + 解析纯函数。

## 任务分解

| # | 任务 | 文件 | 验收 |
|---|------|------|------|
| 1 | `GrayRule` 模型（match_labels / ip_cidrs / percentage）+ `BranchState.gray_seq`/`gray_rule`（serde default） | dsh-core/src/model.rs | 编译 |
| 2 | `gray_snap_key(pid, branch, seq)` 独立前缀 `gray-snap/` | dsh-core/src/keys.rs | 编译 |
| 3 | 3 新命令变体 `GrayPublish`/`GrayAbort`/`GrayPromote`（纯新增，旧变体不动） | dsh-core/src/command.rs | 编译 |
| 4 | apply_gray_publish：固化草稿→灰度快照（复用 write_version_snapshot 逻辑，写 gray-snap/）+ gray_seq 递增 + I10 幂等 + 事件（既有 EventType + gray:bool） | dsh-core/src/state.rs | 编译 |
| 5 | apply_gray_promote：读灰度快照→写新 active_version（next=max(active,gray)+1）+ 清灰度 + 事件带 gray:true + 回落版本号 | dsh-core/src/state.rs | 编译 |
| 6 | apply_gray_abort：清灰度 + 事件带回落版本号 | dsh-core/src/state.rs | 编译 |
| 7 | `resolve_version`/`rule_matches`/`fnv1a_hash` 读路径纯函数（固定求值次序 labels→IP→percent；无身份不进灰度） | dsh-core/src/state.rs | 编译 |
| 8 | apply_publish_structure：灰度活跃时灰度快照同步 bump（分配不同号）+ 事件 | dsh-core/src/state.rs | 编译 |
| 9 | prune_versions：保留条件加 `\|\| no == gray_seq 指向`（Q5） | dsh-core/src/state.rs | 编译 |
| 10 | 事件/版本记录加 `gray: bool`（serde default，不新增 EventType 枚举值，Q3） | dsh-core/src/model.rs | 编译 |
| 11 | 测试 T1-T8（灰度发布/解析三路/转正/下量/幂等/结构发布×gray/无身份不进灰度/保留策略） | dsh-core/tests/state_machine.rs | 全绿 |
| 12 | 全量测试 + e2e + clippy/fmt | 命令行 | 达标 |
| 13 | 更新 gray-release.md / roadmap-p4.md 状态 | docs | 标记 G2 完成 |

## 里程碑

- M1（1-3）：模型 + 命令 + key，编译
- M2（4-9）：状态机 apply + 解析 + 结构发布 + 保留策略
- M3（10-12）：事件 gray 字段 + 测试 + 回归
- M4（13）：文档收尾

## 关键纪律（来自设计/审核）

- **纯新增命令变体**（B1/N10）——旧节点/旧日志不分裂；
- **灰度序号独立**（gray_seq + gray-snap/ 前缀）——不与 active_version 版本号空间冲突（Q1）；
- **复用既有 EventType + gray:bool**（serde default）——防旧节点装快照反序列化失败（Q3）；
- **apply 不读墙钟/请求**——规则是状态机数据，selector 求值在读取路径（D16/D20）；
- **无身份永不进灰度**（Q2）；求值次序固定 labels→IP→percent（纯函数）。

## 风险

- 结构发布灰度同步 bump 的版本分配（Q1 双号）——T7 覆盖；
- watch 漏收（Q4）属 G3 数据面阶段，G2 只保证事件字段正确（gray:bool + 版本号）；
- 灰度快照的 checkpoint/diff 复用——与方案②交互，T1/T4 覆盖。
