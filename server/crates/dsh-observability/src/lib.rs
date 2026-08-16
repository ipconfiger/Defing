//! 可观测性（模块 10）：审计落库（AuditLog）、Prometheus 指标、就绪判断。
//! 说明：审计条目经 Raft 状态机落库（audit/{seq}，集群一致）；指标为文本格式输出。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use dsh_core::command::Command;
use dsh_core::model::PublishEvent;
use dsh_core::StateMachine;
use dsh_raft::RaftHandle;

/// 进程内 HTTP 计数（G5/D32：API middleware 自增；节点本地视图，非状态机数据）。
/// 自动回滚钩子（D33 LocalHttp5xxProbe）与 /metrics 均读此计数。
pub static HTTP_REQUESTS: AtomicU64 = AtomicU64::new(0);
pub static HTTP_5XX: AtomicU64 = AtomicU64::new(0);

/// 读取 HTTP 计数 (requests, 5xx)。
pub fn http_counters() -> (u64, u64) {
    (
        HTTP_REQUESTS.load(Ordering::Relaxed),
        HTTP_5XX.load(Ordering::Relaxed),
    )
}

/// 重置 HTTP 计数（测试用）。
pub fn reset_http_counters() {
    HTTP_REQUESTS.store(0, Ordering::Relaxed);
    HTTP_5XX.store(0, Ordering::Relaxed);
}

/// 审计落库器（B4）：写入经状态机（集群经 Raft 复制，集群一致）。尽力而为。
#[derive(Clone)]
pub struct AuditLog {
    sm: Arc<RwLock<StateMachine>>,
    raft: Option<RaftHandle>,
}

impl AuditLog {
    pub fn new(sm: Arc<RwLock<StateMachine>>, raft: Option<RaftHandle>) -> Self {
        Self { sm, raft }
    }

    /// 追加审计条目（action 使用 schema AuditEntry 枚举；失败仅告警）。
    /// operator：操作者身份（"admin" / "pa:{username}"），由 API 层传入。
    #[allow(clippy::too_many_arguments)]
    pub async fn append(
        &self,
        action: &str,
        project: Option<String>,
        branch: Option<String>,
        version: Option<u64>,
        request_id: Option<String>,
        detail: serde_json::Value,
        operator: &str,
    ) {
        let cmd = Command::AuditAppend {
            entry: dsh_core::AuditEntry {
                seq: 0, // 由状态机分配
                ts: now_ms(),
                operator: if operator.is_empty() {
                    "admin".into()
                } else {
                    operator.into()
                },
                action: action.into(),
                project,
                branch,
                version,
                request_id,
                detail,
            },
        };
        let res = match &self.raft {
            None => {
                // F13：审计尽力而为——锁中毒仅告警不 panic
                let mut sm = match self.sm.write() {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!("audit append: state machine lock poisoned: {e:?}");
                        return;
                    }
                };
                sm.apply(&cmd, now_ms())
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
            Some(raft) => {
                match dsh_raft::client_write(raft, cmd, std::time::Duration::from_secs(5)).await {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            }
        };
        if let Err(e) = res {
            tracing::warn!("audit append failed ({action}): {e}");
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Prometheus 文本指标（模块 10 §3 子集：项目/分支/版本/共享/审计/主密钥/raft）。
/// 注：不含会话活动指标——`dsh_session_active` 是会话存在性 oracle（S7），已移除。
pub fn metrics_text(
    sm: &RwLock<StateMachine>,
    raft: Option<&RaftHandle>,
    master_key_ok: bool,
) -> String {
    // F13：指标为只读，锁中毒时取内部值继续（PoisonError::into_inner）
    let guard = sm.read().unwrap_or_else(|e| e.into_inner());
    let projects = guard.list_projects().map(|p| p.len()).unwrap_or(0);
    let mut out = String::new();
    out.push_str("# HELP dsh_projects 项目数\n");
    out.push_str("# TYPE dsh_projects gauge\n");
    out.push_str(&format!("dsh_projects {projects}\n"));

    let mut branches = 0u64;
    let mut versions = 0u64;
    if let Ok(projects_list) = guard.list_projects() {
        for p in projects_list {
            if let Ok(bs) = guard.list_branches(&p.id) {
                branches += bs.len() as u64;
                for b in bs {
                    if let Ok(Some(st)) = guard.get_branch_state(&p.id, &b) {
                        versions += st.active_version;
                    }
                }
            }
        }
    }
    out.push_str("# HELP dsh_branches 分支总数\n");
    out.push_str("# TYPE dsh_branches gauge\n");
    out.push_str(&format!("dsh_branches {branches}\n"));
    out.push_str("# HELP dsh_versions 分支活动版本总和\n");
    out.push_str("# TYPE dsh_versions gauge\n");
    out.push_str(&format!("dsh_versions {versions}\n"));

    let shared = guard.list_shared_published().map(|v| v.len()).unwrap_or(0);
    let drafts = guard.list_shared_drafts().map(|v| v.len()).unwrap_or(0);
    out.push_str("# HELP dsh_shared_items 已发布共享项数\n");
    out.push_str("# TYPE dsh_shared_items gauge\n");
    out.push_str(&format!("dsh_shared_items {shared}\n"));
    out.push_str("# HELP dsh_shared_drafts 共享草稿数\n");
    out.push_str("# TYPE dsh_shared_drafts gauge\n");
    out.push_str(&format!("dsh_shared_drafts {drafts}\n"));

    let audits = guard
        .get_audit(None, None, None, 1)
        .map(|v| v.first().map(|e| e.seq).unwrap_or(0))
        .unwrap_or(0);
    out.push_str("# HELP dsh_audit_entries 审计条目数\n");
    out.push_str("# TYPE dsh_audit_entries gauge\n");
    out.push_str(&format!("dsh_audit_entries {audits}\n"));

    // G5/D31：灰度指标——活跃灰度分支数（扫描）+ 灰度命令累计（审计计数，集群一致）
    let mut gray_active = 0u64;
    if let Ok(projects_list) = guard.list_projects() {
        for p in projects_list {
            if let Ok(bs) = guard.list_branches(&p.id) {
                for b in bs {
                    if let Ok(Some(st)) = guard.get_branch_state(&p.id, &b) {
                        if st.gray_seq > 0 {
                            gray_active += 1;
                        }
                    }
                }
            }
        }
    }
    out.push_str("# HELP dsh_gray_active 活跃灰度分支数（gray_seq>0）\n");
    out.push_str("# TYPE dsh_gray_active gauge\n");
    out.push_str(&format!("dsh_gray_active {gray_active}\n"));
    for (action, name) in [
        ("gray_publish", "dsh_gray_publish_total"),
        ("gray_promote", "dsh_gray_promote_total"),
        ("gray_abort", "dsh_gray_abort_total"),
    ] {
        // 审计保留策略会裁剪旧条目 → counter 回落属正常（指标语义注明：当前审计窗口内累计）
        let n = guard
            .get_audit(Some(action), None, None, 1_000_000)
            .map(|v| v.len())
            .unwrap_or(0);
        out.push_str(&format!("# HELP {name} 灰度{action}累计（审计窗口内）\n"));
        out.push_str(&format!("# TYPE {name} counter\n"));
        out.push_str(&format!("{name} {n}\n"));
    }

    // G5/D31：进程内 HTTP 计数（节点本地视图；自动回滚信号源）
    let (http_reqs, http_5xx) = http_counters();
    out.push_str("# HELP dsh_http_requests_total HTTP 请求总数（进程内）\n");
    out.push_str("# TYPE dsh_http_requests_total counter\n");
    out.push_str(&format!("dsh_http_requests_total {http_reqs}\n"));
    out.push_str("# HELP dsh_http_5xx_total HTTP 5xx 响应数（进程内）\n");
    out.push_str("# TYPE dsh_http_5xx_total counter\n");
    out.push_str(&format!("dsh_http_5xx_total {http_5xx}\n"));

    out.push_str("# HELP dsh_master_key_ok 主密钥是否就绪（0/1）\n");
    out.push_str("# TYPE dsh_master_key_ok gauge\n");
    out.push_str(&format!("dsh_master_key_ok {}\n", master_key_ok as u8));

    match raft {
        Some(raft) => {
            let m = raft.metrics().borrow().clone();
            // openraft ServerState: Leader=2 / Follower=1 / Learner=0 / Candidate=3
            let role = match &m.state {
                dsh_raft::openraft::ServerState::Leader => 2,
                dsh_raft::openraft::ServerState::Follower => 1,
                dsh_raft::openraft::ServerState::Learner => 0,
                dsh_raft::openraft::ServerState::Candidate => 3,
                _ => 0,
            };
            let term = m.current_term;
            let committed = m.last_log_index.unwrap_or(0);
            out.push_str(
                "# HELP dsh_raft_role 节点角色（0=learner 1=follower 2=leader 3=candidate）\n",
            );
            out.push_str("# TYPE dsh_raft_role gauge\n");
            out.push_str(&format!("dsh_raft_role {role}\n"));
            out.push_str("# HELP dsh_raft_term 当前任期\n");
            out.push_str("# TYPE dsh_raft_term gauge\n");
            out.push_str(&format!("dsh_raft_term {term}\n"));
            out.push_str("# HELP dsh_raft_committed_index 已提交日志索引\n");
            out.push_str("# TYPE dsh_raft_committed_index gauge\n");
            out.push_str(&format!("dsh_raft_committed_index {committed}\n"));
        }
        None => {
            out.push_str("# HELP dsh_raft_role 节点角色（0=dev-single）\n");
            out.push_str("# TYPE dsh_raft_role gauge\n");
            out.push_str("dsh_raft_role 0\n");
        }
    }
    out
}

/// 集群就绪：raft 有日志（已初始化）即就绪；dev-single 恒就绪。
pub fn is_ready(raft: Option<&RaftHandle>) -> bool {
    match raft {
        Some(raft) => raft.metrics().borrow().last_log_index.is_some(),
        None => true,
    }
}

/// 集群成员概要（/api/v1/cluster/members 数据源；members 数组对齐 openapi Member schema）。
pub fn cluster_members_json(raft: Option<&RaftHandle>, node_id: Option<u64>) -> serde_json::Value {
    let raft = match raft {
        Some(r) => r,
        None => {
            return serde_json::json!({
                "node_id": node_id,
                "current_leader": null,
                "state": "dev-single",
                "members": [],
            })
        }
    };
    let m = raft.metrics().borrow().clone();
    let leader = m.current_leader;
    let voter_ids: Vec<u64> = m.membership_config.membership().voter_ids().collect();
    let members: Vec<serde_json::Value> = m
        .membership_config
        .membership()
        .nodes()
        .map(|(id, n)| {
            serde_json::json!({
                "node_id": id.to_string(),
                "grpc_addr": n.grpc_addr,
                "http_addr": n.http_addr,
                "is_leader": Some(*id) == leader,
                "is_voter": voter_ids.contains(id),
            })
        })
        .collect();
    serde_json::json!({
        "node_id": node_id,
        "current_leader": leader,
        "state": format!("{:?}", m.state),
        "members": members,
    })
}

/// 订阅发布事件（供 watch hub 转发；与 dsh-watch 解耦的事件通道封装）。
pub type EventReceiver = tokio::sync::broadcast::Receiver<PublishEvent>;

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::InMemoryStore;

    #[test]
    fn metrics_contains_gauges() {
        reset_http_counters();
        let sm = RwLock::new(StateMachine::new(Box::new(InMemoryStore::new())));
        let text = metrics_text(&sm, None, true);
        assert!(text.contains("dsh_projects 0"));
        assert!(text.contains("dsh_master_key_ok 1"));
        assert!(text.contains("dsh_versions 0"));
        // G5：灰度 + HTTP 指标存在且初始为 0
        assert!(text.contains("dsh_gray_active 0"));
        assert!(text.contains("dsh_gray_publish_total 0"));
        assert!(text.contains("dsh_gray_promote_total 0"));
        assert!(text.contains("dsh_gray_abort_total 0"));
        assert!(text.contains("dsh_http_requests_total 0"));
        assert!(text.contains("dsh_http_5xx_total 0"));
        // S7：会话存在性指标已移除（信息泄露 oracle）
        assert!(!text.contains("dsh_session_active"));
    }

    #[test]
    fn http_counters_roundtrip() {
        reset_http_counters();
        assert_eq!(http_counters(), (0, 0));
        HTTP_REQUESTS.fetch_add(7, Ordering::Relaxed);
        HTTP_5XX.fetch_add(2, Ordering::Relaxed);
        assert_eq!(http_counters(), (7, 2));
        reset_http_counters();
        assert_eq!(http_counters(), (0, 0));
    }

    #[test]
    fn ready_dev_single() {
        assert!(is_ready(None));
    }
}
