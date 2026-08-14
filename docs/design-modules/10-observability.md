# 模块 10 —— 可观测性（dsh-observability）

> 依据：design-v2 §12、design-v3 §5（WCH 指标）、schema/storage.v1.schema.json（AuditEntry）
> 版本：v1.0 ｜ 状态：开发就绪

## 1. 职责与边界
- 职责：健康检查、Prometheus 指标、结构化日志、审计日志、OpenTelemetry 接口（企业版）。
- 不做：业务逻辑；存储（审计落库经 dsh-storage，模块 02）。

## 2. 健康检查
- /healthz：进程存活（恒 200）。
- /readyz：已加入集群（raft 状态非 Learner/未初始化 → 503）；读模式可服务。

## 3. 指标（命名遵循 prometheus 惯例，前缀 dsh_）

| 指标 | 类型 | 说明 |
|------|------|------|
| dsh_raft_role / dsh_raft_leader / dsh_raft_term / dsh_raft_committed_index | gauge | 共识 |
| dsh_raft_snapshot_size_bytes | gauge | 快照 |
| dsh_api_qps / dsh_api_latency_seconds | counter/histogram | 分桶 0.5..1000ms，label{grpc,http,method} |
| dsh_watch_conns / dsh_watch_events_total / dsh_watch_dropped_total | gauge/counter | 订阅 |
| dsh_publish_total / dsh_rollback_total / dsh_publish_blocked_total | counter | 发布 |
| dsh_versions_total / dsh_drafts_pending / dsh_shared_refs_total | gauge | 数据 |
| dsh_storage_bytes / dsh_session_active / dsh_master_key_ok | gauge | 资源/安全 |

## 4. 结构化日志
- 字段：ts, level, request_id, operator, action, project, branch, version, msg。
- 配置：DSH_LOG（级别）、DSH_LOG_JSON=1。

## 5. 审计（AuditSink）

```
pub trait AuditSink: Send + Sync {
    fn write(&self, entry: AuditEntry) -> Result<()>;   // 落库 audit/{seq}（模块 02）
}
pub struct AuditEntry {
    pub seq: u64, pub ts: i64, pub operator: String, pub action: AuditAction,
    pub project: Option<String>, pub branch: Option<String>, pub version: Option<u64>,
    pub request_id: Option<String>, pub detail: serde_json::Value,
}
// AuditAction 枚举：login/logout/force_logout/set_password/project_create/project_delete/
// branch_create/branch_delete/draft_update/publish/structure_publish/shared_publish/
// rollback/promote/decrypt/export/cluster_join/cluster_promote/cluster_remove/rotate_master_key
```

## 6. OpenTelemetry（企业版接口预留）
- 可选启用：OTLP 导出；trace 贯穿 gRPC/HTTP handler；不阻塞主链路。

## 7. 测试要点
- 指标正确性（发布后 publish_total 递增）；审计写库后可查询（audit API）；
- readyz 状态切换（未加入集群 503）。

## 8. 任务清单
□ Metrics 注册与导出（/metrics，模块 05 挂载） □ healthz/readyz
□ 日志初始化（tracing + JSON） □ AuditSink + 审计 API（模块 05）
□ 指标埋点（raft/api/watch/publish/storage） □ OpenTelemetry 可选接入
