# 模块 11 —— 后台任务（dsh-jobs）

> 依据：design-v2 §4.9、模块 07（轮换）
> 版本：v1.0 ｜ 状态：开发就绪

## 1. 职责与边界
- 职责：版本裁剪、DEK 重包、会话清扫、审计清理。周期性调度，幂等，不阻塞写路径。
- 不做：实时业务逻辑；跨节点协调（任务只在 leader 节点执行，避免重复）。

## 2. 调度模型

```
pub struct JobScheduler { raft: RaftHandle, registry: Vec<Job> }
pub trait Job: Send + Sync {
    fn name(&self) -> &'static str;
    fn interval(&self) -> Duration;
    async fn run(&self, ctx: &JobCtx) -> Result<()>;   // ctx: 仅 leader 执行判定 + 存储
}
// 调度：tokio interval；每次 run 前检查 raft.is_leader()，非 leader 跳过
```

## 3. 任务定义

| 任务 | 周期 | 逻辑 | 幂等/安全 |
|------|------|------|-----------|
| VersionRetention | 1h | 按 --version-retention-count/days 删除历史版本；保 checkpoint 保底（最近 1 个之后） | 只删历史不动活动版本；每分支独立事务 |
| DekRewrap | 轮换触发 | 遍历 secret 值 rewrap_dek（模块 07）更新 edek | 游标续跑；重包前后解密结果一致（验证） |
| SessionSweep | 5min | 删除过期 sess/admin | 单会话可用性兜底（I7） |
| AuditCleanup | 24h | 按 --audit-retention 裁剪 audit/{seq} | 前缀删除 |

## 4. 实现要点
- 版本裁剪：从各分支版本链尾部（早于活动版本且超保留策略）批量 delete；每分支一个事务。
- DEK 重包：以游标（last_processed_key）遍历状态机 secret 值；每批提交；中断可续。
- 通过 Raft 写（或仅 leader 本地写？）——**裁剪/清理通过 Raft 写**（保持所有节点一致）；
  DEK 重包只改密文元数据，也走 Raft 写。性能可接受（低频批量）。

## 5. 测试要点
- 裁剪：构造 100+ 版本 → 裁剪 → 历史被删、活动与 checkpoint 保底保留；
- 重包：轮换后 DekRewrap → 所有 secret 可解、edek 均为新 KEK；
- 会话清扫：过期会话被清除后可重新登录（I7 恢复路径）。

## 6. 任务清单
□ JobScheduler（leader 判定 + interval） □ VersionRetention □ DekRewrap（游标续跑）
□ SessionSweep □ AuditCleanup □ 各任务幂等与安全测试
