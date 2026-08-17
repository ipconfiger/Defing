# 开发计划：G1 发布策略地基（D1 收尾——三旋钮）

> 依据：[design/g1-policy.md](design/g1-policy.md)（D35-D37 定稿）
> 目标：publish-policy（block/warn）+ shared-cascade（auto/manual）+ read-mode（linear/stale）三旋钮。

## 任务分解

| # | 任务 | 文件 | 验收 |
|---|------|------|------|
| 1 | 枚举：PublishPolicy / SharedCascadeMode / ReadMode（serde lowercase + Default） | dsh-core/src/model.rs | 编译 | | ✅ |
| 2 | 4 发布命令加 `#[serde(default)] policy`；SharedPublish 加 `#[serde(default)] cascade` | dsh-core/src/command.rs | 编译 + 旧日志兼容（serde 忽略未知字段） | | ✅ |
| 3 | materialize_resolved 接受 policy：warn 跳过校验继续；结构发布同；apply_shared_publish manual 跳过级联 | dsh-core/src/state.rs | 编译 | | ✅ |
| 4 | PublishService 各发布方法注入 policy/cascade；PublishOutcome 带 warnings | dsh-publish/src/lib.rs | 编译 | | ✅ |
| 5 | ApiState.read_mode（pub 默认 Linear）+ linearized_read()；读 handler 接线 | dsh-api/src/lib.rs + grpc.rs | 编译 | | ✅ |
| 6 | CLI 三参数 → PublishService/ApiState 注入 | dsh-cli/src/main.rs | 编译 | | ✅ |
| 7 | core 测试：warn 放行 + 审计 warnings / manual 不级联 + 下次发布物化 / 默认 block 回归 | dsh-core/tests/state_machine.rs | 全绿 | | ✅ |
| 8 | 集群测试：linear 读一致性（follower ReadIndex 后读） | dsh-raft/tests/cluster.rs | 全绿 | | ✅ |
| 9 | e2e：scripts/g1-policy-demo.sh（三旋钮断言） | scripts/ | 退出 0 | | ✅ |
| 10 | 全量回归 + 文档（roadmap G1 ✅） | 命令行 | 达标 | | ✅ |

## 里程碑

- M1（1-3）：模型 + 命令 + apply 语义
- M2（4-6）：PublishService + 读门控 + CLI
- M3（7-9）：测试 + e2e
- M4（10）：回归 + 文档

## 关键纪律

- **D16**：策略编码进命令（日志）→ 全节点重放一致；读模式不产生日志无此约束；
- **B1/N10**：只加 `#[serde(default)]` 字段，不新增变体、不改既有字段语义（默认=现状行为）；
- **默认=现状**：block/auto/linear 即当前行为，未配置参数时零变化。

## 风险

- 命令字段扩展兼容 → serde default + 忽略未知字段（测试覆盖旧形状反序列化）；
- ReadIndex 无 quorum → 读 503；- manual 物化语义 → 文档明示 + 测试覆盖。
