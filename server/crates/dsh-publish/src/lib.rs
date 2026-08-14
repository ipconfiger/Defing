//! 发布编排（模块 04）：提交前 secret 加密（I8）、发布/回滚/结构发布/草稿更新的写路径封装。
//! 确定性 apply 仍在 dsh-core 状态机；本模块负责 API 层到状态机之间的发布域逻辑。

use std::sync::{Arc, Mutex};

use dsh_core::command::{Command, DraftUpdateItem};
use dsh_core::error::Error;
use dsh_core::model::{BranchName, DiffEntry, ProjectId, PublishEvent, Value, ValueType};
use dsh_core::StateMachine;
use dsh_crypto::Cipher;
use dsh_raft::RaftHandle;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 发布结果（handler 直接序列化）。
#[derive(Debug, Clone)]
pub struct PublishOutcome {
    pub version: u64,
    pub changes: Vec<DiffEntry>,
}

/// 结构发布结果。
#[derive(Debug, Clone)]
pub struct StructurePublishOutcome {
    pub affected: Vec<(String, u64)>,
}

/// 发布编排服务。
#[derive(Clone)]
pub struct PublishService {
    sm: Arc<Mutex<StateMachine>>,
    cipher: Option<Arc<Cipher>>,
    raft: Option<RaftHandle>,
    events_tx: Option<tokio::sync::broadcast::Sender<PublishEvent>>,
}

impl PublishService {
    pub fn new(
        sm: Arc<Mutex<StateMachine>>,
        cipher: Option<Arc<Cipher>>,
        raft: Option<RaftHandle>,
        events_tx: Option<tokio::sync::broadcast::Sender<PublishEvent>>,
    ) -> Self {
        Self {
            sm,
            cipher,
            raft,
            events_tx,
        }
    }

    /// 通用写（dev-single 直 apply；集群经 Raft client_write，含 leader 转发提示）。
    async fn write(&self, cmd: &Command, now_ms: i64) -> Result<dsh_raft::WriteOutcome, Error> {
        dsh_raft::write_command(
            &self.sm,
            self.raft.as_ref(),
            cmd,
            now_ms,
            self.events_tx.as_ref(),
        )
        .await
    }

    /// 提交前加密 secret 项（明文仅存在于 API 输入；状态机只存密文，保证 Raft apply 确定性，I8）。
    pub fn encrypt_secret_updates(
        &self,
        project: &ProjectId,
        updates: &mut [DraftUpdateItem],
    ) -> Result<(), Error> {
        let Some(cipher) = &self.cipher else {
            return Ok(());
        };
        let sm = self.sm.lock().expect("sm lock");
        let structure = sm.get_structure(project)?;
        let secret_keys: std::collections::HashSet<(String, String)> = structure
            .map(|s| {
                s.groups
                    .iter()
                    .flat_map(|g| {
                        g.items
                            .iter()
                            .filter(|it| it.ty == ValueType::Secret)
                            .map(move |it| (g.name.clone(), it.key.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        drop(sm);
        for u in updates.iter_mut() {
            if secret_keys.contains(&(u.group.clone(), u.key.clone())) {
                if let Value::String(plain) = &u.value {
                    let ct = cipher
                        .encrypt_secret(plain.as_bytes())
                        .map_err(|e| Error::internal(format!("encrypt secret: {e}")))?;
                    u.value = Value::Secret(ct);
                }
            }
        }
        Ok(())
    }

    /// 值草稿更新（secret 项自动加密）。
    pub async fn update_draft(
        &self,
        project: &ProjectId,
        branch: &BranchName,
        mut updates: Vec<DraftUpdateItem>,
        deletes: Vec<(String, String)>,
    ) -> Result<(), Error> {
        self.encrypt_secret_updates(project, &mut updates)?;
        self.write(
            &Command::DraftUpdate {
                project: project.clone(),
                branch: branch.clone(),
                updates,
                deletes,
            },
            now_ms(),
        )
        .await
        .map(|_| ())
    }

    /// 发布分支版本（幂等 I10：同 request_id 返回当前活动版本 + 空 changes）。
    pub async fn publish(
        &self,
        project: &ProjectId,
        branch: &BranchName,
        comment: &str,
        request_id: &str,
    ) -> Result<PublishOutcome, Error> {
        let wr = self
            .write(
                &Command::Publish {
                    project: project.clone(),
                    branch: branch.clone(),
                    comment: comment.to_string(),
                    request_id: request_id.to_string(),
                },
                now_ms(),
            )
            .await?;
        let version = if wr.version > 0 {
            wr.version
        } else {
            let sm = self.sm.lock().expect("sm lock");
            sm.get_branch_state(project, branch)?
                .map(|s| s.active_version)
                .unwrap_or(0)
        };
        let changes = wr
            .events
            .first()
            .map(|e| e.changes.clone())
            .unwrap_or_default();
        Ok(PublishOutcome { version, changes })
    }

    /// 回滚（新版本 = 旧版本内容，历史不可变 I6/I9）。
    pub async fn rollback(
        &self,
        project: &ProjectId,
        branch: &BranchName,
        to_version: u64,
        comment: &str,
        request_id: &str,
    ) -> Result<u64, Error> {
        let wr = self
            .write(
                &Command::Rollback {
                    project: project.clone(),
                    branch: branch.clone(),
                    to_version,
                    comment: comment.to_string(),
                    request_id: request_id.to_string(),
                },
                now_ms(),
            )
            .await?;
        if wr.version > 0 {
            return Ok(wr.version);
        }
        let sm = self.sm.lock().expect("sm lock");
        Ok(sm
            .get_branch_state(project, branch)?
            .map(|s| s.active_version)
            .unwrap_or(0))
    }

    /// 发布结构草稿（全部分支同时生效，I3/I5）。
    pub async fn publish_structure(
        &self,
        project: &ProjectId,
        comment: &str,
        request_id: &str,
    ) -> Result<StructurePublishOutcome, Error> {
        let wr = self
            .write(
                &Command::PublishStructure {
                    project: project.clone(),
                    comment: comment.to_string(),
                    request_id: request_id.to_string(),
                },
                now_ms(),
            )
            .await?;
        let affected = wr
            .events
            .iter()
            .map(|e| (e.branch.as_str().to_string(), e.version))
            .collect();
        Ok(StructurePublishOutcome { affected })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::command::Command;
    use dsh_core::model::{GroupDef, ItemDef, ValueType};
    use dsh_core::{InMemoryStore, StateMachine};

    fn sm_with_structure() -> Arc<Mutex<StateMachine>> {
        let mut sm = StateMachine::new(Box::new(InMemoryStore::new()));
        sm.apply(&Command::ProjectCreate { name: "p".into() }, 1)
            .unwrap();
        sm.apply(
            &Command::StructureDraftSet {
                project: "p".into(),
                base_version: 1,
                groups: vec![GroupDef {
                    name: "g".into(),
                    items: vec![
                        ItemDef {
                            key: "host".into(),
                            ty: ValueType::String,
                            required: true,
                            secret: false,
                            validate: None,
                        },
                        ItemDef {
                            key: "pass".into(),
                            ty: ValueType::Secret,
                            required: false,
                            secret: true,
                            validate: None,
                        },
                    ],
                }],
            },
            2,
        )
        .unwrap();
        sm.apply(
            &Command::PublishStructure {
                project: "p".into(),
                comment: "s".into(),
                request_id: "s1".into(),
            },
            3,
        )
        .unwrap();
        Arc::new(Mutex::new(sm))
    }

    #[test]
    fn encrypt_secret_updates_encrypts_only_secret_items() {
        let sm = sm_with_structure();
        let svc = PublishService::new(sm, Some(Arc::new(Cipher::new([7u8; 32]))), None, None);
        let mut updates = vec![
            DraftUpdateItem {
                group: "g".into(),
                key: "host".into(),
                value: Value::String("h".into()),
            },
            DraftUpdateItem {
                group: "g".into(),
                key: "pass".into(),
                value: Value::String("s3cret".into()),
            },
        ];
        svc.encrypt_secret_updates(&ProjectId("p".into()), &mut updates)
            .unwrap();
        assert!(
            matches!(updates[0].value, Value::String(_)),
            "非 secret 项不加密"
        );
        assert!(
            matches!(updates[1].value, Value::Secret(_)),
            "secret 项提交前加密（I8）"
        );
    }

    #[test]
    fn encrypt_without_cipher_is_noop() {
        let sm = sm_with_structure();
        let svc = PublishService::new(sm, None, None, None);
        let mut updates = vec![DraftUpdateItem {
            group: "g".into(),
            key: "pass".into(),
            value: Value::String("plain".into()),
        }];
        svc.encrypt_secret_updates(&ProjectId("p".into()), &mut updates)
            .unwrap();
        assert!(matches!(updates[0].value, Value::String(_)));
    }
}
