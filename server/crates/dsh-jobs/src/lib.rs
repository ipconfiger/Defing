//! 后台任务（模块 11）：版本裁剪等。任务仅在 leader 节点执行。

use std::sync::{Arc, RwLock};

use dsh_core::StateMachine;
use dsh_crypto::Cipher;
use tokio::sync::watch;

/// 任务执行上下文。
pub struct JobCtx {
    pub is_leader: Arc<watch::Receiver<bool>>,
}

pub trait Job: Send + Sync {
    fn name(&self) -> &'static str;
    fn interval(&self) -> std::time::Duration;
    fn run(&self, sm: &RwLock<StateMachine>) -> Result<(), String>;
}

/// 版本裁剪：每分支保留最近 keep 个版本 + 活动版本。
pub struct VersionRetention {
    pub keep: usize,
}

impl Job for VersionRetention {
    fn name(&self) -> &'static str {
        "version-retention"
    }

    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(60)
    }

    fn run(&self, sm: &RwLock<StateMachine>) -> Result<(), String> {
        let guard = sm.write().map_err(|e| e.to_string())?;
        let projects = guard.list_projects().map_err(|e| e.to_string())?;
        for p in projects {
            for b in guard.list_branches(&p.id).map_err(|e| e.to_string())? {
                let removed = guard
                    .prune_versions(&p.id, &b, self.keep)
                    .map_err(|e| e.to_string())?;
                if removed > 0 {
                    tracing::info!("pruned {removed} versions of {}/{}", p.id, b);
                }
            }
        }
        Ok(())
    }
}

/// 审计保留：仅保留最近 keep 条（对齐 design-v2：审计保留 100k 条或 30 天）。
pub struct AuditRetention {
    pub keep: usize,
}

impl Job for AuditRetention {
    fn name(&self) -> &'static str {
        "audit-retention"
    }

    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(3600)
    }

    fn run(&self, sm: &RwLock<StateMachine>) -> Result<(), String> {
        let guard = sm.write().map_err(|e| e.to_string())?;
        let removed = guard.prune_audit(self.keep).map_err(|e| e.to_string())?;
        if removed > 0 {
            tracing::info!("pruned {removed} audit entries (keep {})", self.keep);
        }
        Ok(())
    }
}

/// DEK 重包（B6）：轮换主密钥后把全部 secret 密文的 edek 重包到当前 KEK（数据不重加密）。
/// 仅重包 `dek_v < 当前代际` 的密文（已最新则跳过，幂等）。
pub struct RewrapDeks {
    pub cipher: Arc<Cipher>,
}

impl Job for RewrapDeks {
    fn name(&self) -> &'static str {
        "rewrap-deks"
    }

    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(300)
    }

    fn run(&self, sm: &RwLock<StateMachine>) -> Result<(), String> {
        let cipher = self.cipher.clone();
        let gen = cipher.keyring().generation();
        let guard = sm.write().map_err(|e| e.to_string())?;
        let count = guard
            .rewrap_deks(&|ct| {
                if ct.dek_v >= gen {
                    None
                } else {
                    Some(
                        cipher
                            .rewrap_dek(ct)
                            .map_err(|e| dsh_core::Error::internal(format!("rewrap: {e}"))),
                    )
                }
            })
            .map_err(|e| e.to_string())?;
        if count > 0 {
            tracing::info!("rewrapped {count} secret DEKs to KEK generation {gen}");
        }
        Ok(())
    }
}

/// 调度器：按间隔运行任务（仅 leader）。
#[derive(Default)]
pub struct JobScheduler {
    jobs: Vec<Box<dyn Job>>,
}

impl JobScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, job: impl Job + 'static) {
        self.jobs.push(Box::new(job));
    }

    pub fn spawn(self, sm: Arc<RwLock<StateMachine>>, is_leader: watch::Receiver<bool>) {
        for job in self.jobs {
            let sm = sm.clone();
            let is_leader = is_leader.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(job.interval());
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    if !*is_leader.borrow() {
                        continue;
                    }
                    if let Err(e) = job.run(&sm) {
                        tracing::warn!("job {} failed: {e}", job.name());
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::command::{Command, DraftUpdateItem};
    use dsh_core::model::BranchName;
    use dsh_core::InMemoryStore;

    #[test]
    fn retention_keeps_active_and_recent() {
        let mut sm = StateMachine::new(Box::new(InMemoryStore::new()));
        sm.apply(
            &Command::ProjectCreate {
                name: "p".into(),
                operator: String::new(),
                ts: 0,
            },
            1,
        )
        .unwrap();
        sm.apply(
            &Command::StructureDraftSet {
                project: "p".into(),
                base_version: 1,
                groups: vec![dsh_core::model::GroupDef {
                    name: "g".into(),
                    items: vec![dsh_core::model::ItemDef {
                        key: "k".into(),
                        ty: dsh_core::model::ValueType::String,
                        required: true,
                        secret: false,
                        validate: None,
                    }],
                }],

                operator: String::new(),
            },
            2,
        )
        .unwrap();
        sm.apply(
            &Command::PublishStructure {
                project: "p".into(),
                comment: "s".into(),
                request_id: "s1".into(),

                operator: String::new(),
                ts: 0,
            },
            3,
        )
        .unwrap();
        // 发布 5 个版本（v2..v6）
        for i in 0..5 {
            sm.apply(
                &Command::DraftUpdate {
                    project: "p".into(),
                    branch: BranchName("dev".into()),
                    updates: vec![DraftUpdateItem {
                        group: "g".into(),
                        key: "k".into(),
                        value: dsh_core::model::Value::String(format!("v{i}")),
                    }],
                    deletes: vec![],

                    operator: String::new(),
                    ts: 0,
                    expected_draft_rev: None,
                },
                10 + i,
            )
            .unwrap();
            sm.apply(
                &Command::Publish {
                    project: "p".into(),
                    branch: BranchName("dev".into()),
                    comment: "c".into(),
                    request_id: format!("r{i}"),

                    operator: String::new(),
                    ts: 0,
                },
                20 + i,
            )
            .unwrap();
        }
        let total = sm
            .version_history(&"p".into(), &BranchName("dev".into()))
            .unwrap()
            .len();
        assert!(total >= 6); // 结构 v1 + 5 次发布

        // perf 方案② D3：diff 链完整性优先——版本数小于 checkpoint 间隔时基座为 v1，
        // 裁剪不删任何版本（removed=0 是保守正确行为）；活动版本始终可读。
        let removed = sm
            .prune_versions(&"p".into(), &BranchName("dev".into()), 2)
            .unwrap();
        // 6 个版本全在 v1 基座 + diff 链内 → 不裁剪
        assert_eq!(removed, 0, "小版本数 diff 链必须完整保留");
        // 活动版本仍可读
        let cfg = sm
            .get_config(&"p".into(), &BranchName("dev".into()), 0)
            .unwrap();
        assert_eq!(cfg.version, 6);
        // 跨 checkpoint 裁剪：发布到 v250 后 keep=10 → 删除 v<200 且保留 v200 基座
        for i in 0..244 {
            sm.apply(
                &Command::DraftUpdate {
                    project: "p".into(),
                    branch: BranchName("dev".into()),
                    updates: vec![DraftUpdateItem {
                        group: "g".into(),
                        key: "k".into(),
                        value: dsh_core::model::Value::String(format!("x{i}")),
                    }],
                    deletes: vec![],
                    operator: String::new(),
                    ts: 0,
                    expected_draft_rev: None,
                },
                100 + i,
            )
            .unwrap();
            sm.apply(
                &Command::Publish {
                    project: "p".into(),
                    branch: BranchName("dev".into()),
                    comment: "c".into(),
                    request_id: format!("rx{i}"),
                    operator: String::new(),
                    ts: 0,
                },
                200 + i,
            )
            .unwrap();
        }
        let removed2 = sm
            .prune_versions(&"p".into(), &BranchName("dev".into()), 10)
            .unwrap();
        assert!(removed2 > 0, "跨 checkpoint 裁剪应删除旧版本");
        let hist2 = sm
            .version_history(&"p".into(), &BranchName("dev".into()))
            .unwrap();
        // 保留 v200 基座 + v241..v250 + 活动版本，总 <= 61
        assert!(hist2.len() <= 61, "裁剪后历史应受限: {}", hist2.len());
        // 活动版本（v250）仍可读且内容正确
        let cfg2 = sm
            .get_config(&"p".into(), &BranchName("dev".into()), 0)
            .unwrap();
        assert_eq!(cfg2.version, 250);
        assert_eq!(
            cfg2.groups["g"]["k"],
            dsh_core::model::Value::String("x243".into())
        );
    }
}

#[cfg(test)]
mod rewrap_tests {
    use super::*;
    use dsh_core::command::{Command, DraftUpdateItem};
    use dsh_core::model::{BranchName, GroupDef, ItemDef, SharedItem, Value, ValueType};
    use dsh_core::InMemoryStore;

    #[test]
    fn rewrap_job_bumps_generation_and_keeps_data() {
        let cipher = Arc::new(Cipher::new([1u8; 32]));
        let mut sm = StateMachine::new(Box::new(InMemoryStore::new()));
        sm.apply(
            &Command::ProjectCreate {
                name: "p".into(),
                operator: String::new(),
                ts: 0,
            },
            1,
        )
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

                operator: String::new(),
            },
            2,
        )
        .unwrap();
        sm.apply(
            &Command::PublishStructure {
                project: "p".into(),
                comment: "s".into(),
                request_id: "s1".into(),

                operator: String::new(),
                ts: 0,
            },
            3,
        )
        .unwrap();
        let ct = cipher.encrypt_secret(b"job-secret").unwrap();
        sm.apply(
            &Command::DraftUpdate {
                project: "p".into(),
                branch: BranchName("dev".into()),
                updates: vec![
                    DraftUpdateItem {
                        group: "g".into(),
                        key: "host".into(),
                        value: Value::String("h".into()),
                    },
                    DraftUpdateItem {
                        group: "g".into(),
                        key: "pass".into(),
                        value: Value::Secret(ct),
                    },
                ],
                deletes: vec![],

                operator: String::new(),
                ts: 0,
                expected_draft_rev: None,
            },
            4,
        )
        .unwrap();
        sm.apply(
            &Command::Publish {
                project: "p".into(),
                branch: BranchName("dev".into()),
                comment: "v".into(),
                request_id: "r1".into(),

                operator: String::new(),
                ts: 0,
            },
            5,
        )
        .unwrap();
        // 共享项 secret（代际 1）
        sm.apply(
            &Command::SharedDraftUpdate {
                item: SharedItem {
                    group: "g".into(),
                    key: "tok".into(),
                    ty: ValueType::Secret,
                    secret: true,
                    required: false,
                    value: Value::Secret(cipher.encrypt_secret(b"shared-job").unwrap()),
                    version: 0,
                },

                operator: String::new(),
            },
            6,
        )
        .unwrap();
        sm.apply(
            &Command::SharedPublish {
                comment: "c".into(),
                request_id: "sp".into(),

                operator: String::new(),
                ts: 0,
            },
            7,
        )
        .unwrap();

        // 轮换：KEK 2 成为当前 → 任务重包代际 1 的密文
        cipher.rotate_master_key([2u8; 32]);
        let job = RewrapDeks {
            cipher: cipher.clone(),
        };
        let sm_mutex = RwLock::new(sm);
        job.run(&sm_mutex).unwrap();

        let guard = sm_mutex.read().unwrap();
        let cfg = guard
            .get_config(&"p".into(), &BranchName("dev".into()), 0)
            .unwrap();
        match cfg.groups.get("g").unwrap().get("pass").unwrap() {
            Value::Secret(ct2) => {
                assert_eq!(ct2.dek_v, 2, "快照 secret 已重包到新代际");
                assert_eq!(cipher.decrypt_secret(ct2).unwrap(), b"job-secret");
            }
            _ => panic!("expected secret"),
        }
    }
}
