//! 可观测性（模块 10）：审计落库（AuditLog）、Prometheus 指标、就绪判断。
//! 说明：审计条目经 Raft 状态机落库（audit/{seq}，集群一致）；指标为文本格式输出。

use std::sync::{Arc, Mutex};

use dsh_core::command::Command;
use dsh_core::model::PublishEvent;
use dsh_core::StateMachine;
use dsh_raft::RaftHandle;

/// 审计落库器（B4）：写入经状态机（集群经 Raft 复制，集群一致）。尽力而为。
#[derive(Clone)]
pub struct AuditLog {
    sm: Arc<Mutex<StateMachine>>,
    raft: Option<RaftHandle>,
}

impl AuditLog {
    pub fn new(sm: Arc<Mutex<StateMachine>>, raft: Option<RaftHandle>) -> Self {
        Self { sm, raft }
    }

    /// 追加审计条目（action 使用 schema AuditEntry 枚举；失败仅告警）。
    pub async fn append(
        &self,
        action: &str,
        project: Option<String>,
        branch: Option<String>,
        version: Option<u64>,
        request_id: Option<String>,
        detail: serde_json::Value,
    ) {
        let cmd = Command::AuditAppend {
            entry: dsh_core::AuditEntry {
                seq: 0, // 由状态机分配
                ts: now_ms(),
                operator: "admin".into(),
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
                let mut sm = self.sm.lock().expect("sm lock");
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

/// Prometheus 文本指标（模块 10 §3 子集：项目/分支/版本/共享/审计/会话/主密钥/raft）。
pub fn metrics_text(
    sm: &Mutex<StateMachine>,
    raft: Option<&RaftHandle>,
    session_active: bool,
    master_key_ok: bool,
) -> String {
    let guard = sm.lock().expect("sm lock");
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
        .get_audit(None, None, 1)
        .map(|v| v.first().map(|e| e.seq).unwrap_or(0))
        .unwrap_or(0);
    out.push_str("# HELP dsh_audit_entries 审计条目数\n");
    out.push_str("# TYPE dsh_audit_entries gauge\n");
    out.push_str(&format!("dsh_audit_entries {audits}\n"));

    out.push_str("# HELP dsh_session_active 管理员会话是否活动（0/1）\n");
    out.push_str("# TYPE dsh_session_active gauge\n");
    out.push_str(&format!("dsh_session_active {}\n", session_active as u8));
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
        let sm = Mutex::new(StateMachine::new(Box::new(InMemoryStore::new())));
        let text = metrics_text(&sm, None, false, true);
        assert!(text.contains("dsh_projects 0"));
        assert!(text.contains("dsh_session_active 0"));
        assert!(text.contains("dsh_master_key_ok 1"));
        assert!(text.contains("dsh_versions 0"));
    }

    #[test]
    fn ready_dev_single() {
        assert!(is_ready(None));
    }
}
