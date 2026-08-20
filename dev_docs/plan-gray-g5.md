# 开发计划：G5 百分比放量 + 可观测 + 自动回滚钩子

> 依据：[design/g5-observability.md](design/g5-observability.md)（D31-D34 定稿）
> 目标：算法文档化 + 6 项指标 + 可选自动回滚钩子 + 跨节点一致性验证。

## 任务分解

| # | 任务 | 文件 | 验收 |
|---|------|------|------|
| 1 | HTTP 计数 statics（dsh_http_requests_total / dsh_http_5xx_total）+ accessor/reset | dsh-observability/src/lib.rs | 编译 + 单测 | | ✅ |
| 2 | metrics_text 加灰度指标：dsh_gray_active（扫描）+ gray_publish/promote/abort_total（审计计数）+ HTTP 计数 | dsh-observability/src/lib.rs | 编译 + 单测 | | ✅ |
| 3 | API count_http middleware + build_router layer | dsh-api/src/lib.rs | 编译 + 集成测试 | | ✅ |
| 4 | GrayHealthProbe trait + LocalHttp5xxProbe + spawn_gray_auto_rollback（raft 写路径 + 审计 gray_auto_abort） | dsh-jobs/src/lib.rs + Cargo.toml | 编译 + 单测 | | ✅ |
| 5 | CLI --gray-rollback-threshold / --gray-rollback-interval + 装配 | dsh-cli/src/main.rs | 编译 | | ✅ |
| 6 | 集群一致性测试：3 节点同一 percentage 规则同桶（fnv1a 确定性） | dsh-raft/tests/cluster.rs | 全绿 | | ✅ |
| 7 | e2e：scripts/gray-obs-demo.sh（metrics 6 项断言 + 自动回滚触发） | scripts/ | 退出 0 | | ✅ |
| 8 | gray-release.md 补"百分比分桶算法"章节 | dev_docs/gray-release.md | 文档 | | ✅ |
| 9 | 全量回归 + 文档（roadmap G5 ✅） | 命令行 | 达标 | | ✅ |

## 里程碑

- M1（1-3）：指标 + middleware
- M2（4-5）：自动回滚钩子 + CLI
- M3（6-7）：集群一致性测试 + e2e
- M4（8-9）：算法文档化 + 回归收尾

## 关键纪律

- **D16**：自动回滚是后台任务（非 apply），abort 命令经状态机确定性 apply；
- **仅 leader**：沿用 is_leader watch 门控；
- **默认禁用**：threshold=0 不 spawn，无行为变化（向后兼容）；
- **审计留痕**：自动回滚 action="gray_auto_abort" 与手动 gray_abort 区分。

## 风险

- 阈值误判 → 默认禁用 + 审计；- 审计计数随保留裁剪 → 语义注明；
- cluster 测试耗时 → 复用 bootstrap 框架。
