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

/// Prometheus 文本指标（基础指标；完整指标见模块 10 §3）。
pub fn metrics_text(sm: &Mutex<StateMachine>) -> String {
    let guard = sm.lock().expect("sm lock");
    let projects = guard.list_projects().map(|p| p.len()).unwrap_or(0);
    let mut out = String::new();
    out.push_str("# HELP dsh_projects 项目数\n");
    out.push_str("# TYPE dsh_projects gauge\n");
    out.push_str(&format!("dsh_projects {projects}\n"));
    out.push_str("# HELP dsh_versions 分支活动版本总和\n");
    out.push_str("# TYPE dsh_versions gauge\n");
    let mut versions = 0u64;
    if let Ok(projects_list) = guard.list_projects() {
        for p in projects_list {
            if let Ok(branches) = guard.list_branches(&p.id) {
                for b in branches {
                    if let Ok(Some(st)) = guard.get_branch_state(&p.id, &b) {
                        versions += st.active_version;
                    }
                }
            }
        }
    }
    out.push_str(&format!("dsh_versions {versions}\n"));
    out
}

/// 集群就绪：raft 有日志（已初始化）即就绪；dev-single 恒就绪。
pub fn is_ready(raft: Option<&RaftHandle>) -> bool {
    match raft {
        Some(raft) => raft.metrics().borrow().last_log_index.is_some(),
        None => true,
    }
}

/// 集群成员概要（/api/v1/cluster/members 数据源）。
pub fn cluster_members_json(raft: Option<&RaftHandle>, node_id: Option<u64>) -> serde_json::Value {
    let raft = match raft {
        Some(r) => r,
        None => {
            return serde_json::json!({
                "node_id": node_id,
                "current_leader": null,
                "state": "dev-single",
            })
        }
    };
    let m = raft.metrics().borrow().clone();
    serde_json::json!({
        "node_id": node_id,
        "current_leader": m.current_leader,
        "state": format!("{:?}", m.state),
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
        let text = metrics_text(&sm);
        assert!(text.contains("dsh_projects 0"));
        assert!(text.contains("dsh_versions 0"));
    }

    #[test]
    fn ready_dev_single() {
        assert!(is_ready(None));
    }
}
