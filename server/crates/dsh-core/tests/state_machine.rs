//! 状态机流程测试（M1）：CRUD / 结构发布 / 值草稿 / 发布 / GetConfig / 幂等 / 隔离。

use dsh_core::command::{Command, DraftUpdateItem};
use dsh_core::model::*;
use dsh_core::{ErrorKind, InMemoryStore, StateMachine, Value};

fn sm() -> StateMachine {
    StateMachine::new(Box::new(InMemoryStore::new()))
}

fn redis_structure() -> Vec<GroupDef> {
    vec![GroupDef {
        name: "redis".into(),
        items: vec![
            ItemDef {
                key: "host".into(),
                ty: ValueType::String,
                required: true,
                secret: false,
                validate: None,
            },
            ItemDef {
                key: "port".into(),
                ty: ValueType::Int,
                required: false,
                secret: false,
                validate: None,
            },
            ItemDef {
                key: "password".into(),
                ty: ValueType::Secret,
                required: false,
                secret: true,
                validate: None,
            },
        ],
    }]
}

fn setup(s: &mut StateMachine) -> (ProjectId, Vec<BranchName>) {
    assert!(s
        .apply(
            &Command::ProjectCreate {
                name: "order-service".into(),
                operator: String::new(),
            },
            1
        )
        .is_ok());
    let pid: ProjectId = "order-service".into();
    let branches = s.list_branches(&pid).unwrap();
    // 默认 dev/test/prod
    assert_eq!(branches.len(), 3);
    // 结构草稿 + 发布
    assert!(s
        .apply(
            &Command::StructureDraftSet {
                project: pid.clone(),
                base_version: 1,
                groups: redis_structure(),
                operator: String::new(),
            },
            2,
        )
        .is_ok());
    let events = s
        .apply(
            &Command::PublishStructure {
                project: pid.clone(),
                comment: "init".into(),
                request_id: "s1".into(),
                operator: String::new(),
            },
            3,
        )
        .unwrap();
    assert_eq!(events.len(), 3); // 全部分支版本推进
    (pid, branches)
}

#[test]
fn full_flow_dev_publish() {
    let mut s = sm();
    let (pid, branches) = setup(&mut s);

    // 草稿编辑 dev
    assert!(s
        .apply(
            &Command::DraftUpdate {
                project: pid.clone(),
                branch: BranchName("dev".into()),
                updates: vec![
                    DraftUpdateItem {
                        group: "redis".into(),
                        key: "host".into(),
                        value: Value::String("127.0.0.1".into())
                    },
                    DraftUpdateItem {
                        group: "redis".into(),
                        key: "port".into(),
                        value: Value::Int(6379)
                    },
                ],
                deletes: vec![],
                operator: String::new(),
            },
            4,
        )
        .is_ok());

    // 草稿隔离（I4）：发布前 GetConfig 不变（结构发布后的版本，值仍为空）
    let before = s.get_config(&pid, &BranchName("dev".into()), 0).unwrap();
    assert_eq!(before.version, 1);
    assert!(before.groups.is_empty());

    // 发布 dev
    let events = s
        .apply(
            &Command::Publish {
                project: pid.clone(),
                branch: BranchName("dev".into()),
                comment: "dev host".into(),
                request_id: "r1".into(),
                operator: String::new(),
            },
            5,
        )
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].ty, EventType::ValuePublish);
    assert_eq!(events[0].version, 2);

    // GetConfig 读到新版本（M1 验收核心）
    let after = s.get_config(&pid, &BranchName("dev".into()), 0).unwrap();
    assert_eq!(after.version, 2);
    assert_eq!(
        after.groups["redis"]["host"],
        Value::String("127.0.0.1".into())
    );
    assert_eq!(after.groups["redis"]["port"], Value::Int(6379));

    // 其他分支不受影响（仍为结构发布版本 1，空值）
    let test = s.get_config(&pid, &BranchName("test".into()), 0).unwrap();
    assert_eq!(test.version, 1);
    assert!(test.groups.is_empty());
    assert_eq!(branches.len(), 3);
}

#[test]
fn publish_is_idempotent_by_request_id() {
    let mut s = sm();
    let (pid, _) = setup(&mut s);
    let b = BranchName("dev".into());
    s.apply(
        &Command::DraftUpdate {
            project: pid.clone(),
            branch: b.clone(),
            updates: vec![DraftUpdateItem {
                group: "redis".into(),
                key: "host".into(),
                value: Value::String("x".into()),
            }],
            deletes: vec![],
        
                operator: String::new(),
            },
        4,
    )
    .unwrap();
    let cmd = Command::Publish {
        project: pid.clone(),
        branch: b.clone(),
        comment: "c".into(),
        request_id: "r9".into(),
    
                operator: String::new(),
            };
    let first = s.apply(&cmd, 5).unwrap();
    assert_eq!(first.len(), 1);
    // 同 request_id 重放 → 不重复生效（I10）
    let second = s.apply(&cmd, 6).unwrap();
    assert!(second.is_empty());
    let snap = s.get_config(&pid, &b, 0).unwrap();
    assert_eq!(snap.version, 2);
}

#[test]
fn required_unset_blocks_publish() {
    let mut s = sm();
    let (pid, _) = setup(&mut s);
    let b = BranchName("dev".into());
    s.apply(
        &Command::DraftUpdate {
            project: pid.clone(),
            branch: b.clone(),
            updates: vec![DraftUpdateItem {
                group: "redis".into(),
                key: "port".into(),
                value: Value::Int(6379),
            }],
            deletes: vec![],
        
                operator: String::new(),
            },
        4,
    )
    .unwrap();
    let err = s
        .apply(
            &Command::Publish {
                project: pid.clone(),
                branch: b.clone(),
                comment: "c".into(),
                request_id: "r2".into(),
                operator: String::new(),
            },
            5,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::PublishBlocked);
    // 未发布：版本不变
    assert_eq!(s.get_config(&pid, &b, 0).unwrap().version, 1);
}

#[test]
fn no_draft_publish_errors() {
    let mut s = sm();
    let (pid, _) = setup(&mut s);
    let err = s
        .apply(
            &Command::Publish {
                project: pid.clone(),
                branch: BranchName("test".into()),
                comment: "c".into(),
                request_id: "r3".into(),
                operator: String::new(),
            },
            5,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoDraft);
}

#[test]
fn branch_inherits_structure_and_values() {
    let mut s = sm();
    let (pid, _) = setup(&mut s);
    // dev 发布一些值
    s.apply(
        &Command::DraftUpdate {
            project: pid.clone(),
            branch: BranchName("dev".into()),
            updates: vec![DraftUpdateItem {
                group: "redis".into(),
                key: "host".into(),
                value: Value::String("10.0.0.1".into()),
            }],
            deletes: vec![],
        
                operator: String::new(),
            },
        4,
    )
    .unwrap();
    s.apply(
        &Command::Publish {
            project: pid.clone(),
            branch: BranchName("dev".into()),
            comment: "c".into(),
            request_id: "r4".into(),
        
                operator: String::new(),
            },
        5,
    )
    .unwrap();
    // 新分支从 dev 继承活动版本值到草稿
    assert!(s
        .apply(
            &Command::BranchCreate {
                project: pid.clone(),
                name: "gray".into(),
                source: Some(BranchName("dev".into())),
                operator: String::new(),
            },
            6
        )
        .is_ok());
    let st = s
        .get_branch_state(&pid, &BranchName("gray".into()))
        .unwrap()
        .unwrap();
    assert_eq!(st.structure_version, 2);
    assert_eq!(
        st.value_draft["redis"]["host"].value,
        Value::String("10.0.0.1".into())
    );
}

#[test]
fn draft_update_validates_unknown_item_and_type() {
    let mut s = sm();
    let (pid, _) = setup(&mut s);
    // 未知 item
    let err = s
        .apply(
            &Command::DraftUpdate {
                project: pid.clone(),
                branch: BranchName("dev".into()),
                updates: vec![DraftUpdateItem {
                    group: "redis".into(),
                    key: "nope".into(),
                    value: Value::String("x".into()),
                }],
                deletes: vec![],
                operator: String::new(),
            },
            4,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Validation);
    // 类型不匹配
    let err = s
        .apply(
            &Command::DraftUpdate {
                project: pid.clone(),
                branch: BranchName("dev".into()),
                updates: vec![DraftUpdateItem {
                    group: "redis".into(),
                    key: "port".into(),
                    value: Value::String("abc".into()),
                }],
                deletes: vec![],
                operator: String::new(),
            },
            4,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Validation);
}

#[test]
fn duplicate_project_conflicts() {
    let mut s = sm();
    s.apply(&Command::ProjectCreate { name: "p1".into(),
                operator: String::new(),
            }, 1)
        .unwrap();
    let err = s
        .apply(&Command::ProjectCreate { name: "p1".into(),
                operator: String::new(),
            }, 2)
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
}

#[test]
fn project_delete_removes_everything() {
    let mut s = sm();
    let (pid, _) = setup(&mut s);
    s.apply(&Command::ProjectDelete { id: pid.clone(),
                operator: String::new(),
            }, 10)
        .unwrap();
    assert!(s.get_project(&pid).unwrap().is_none());
    assert!(s.list_projects().unwrap().is_empty());
    assert!(s.list_branches(&pid).unwrap().is_empty());
    assert!(s.get_structure(&pid).unwrap().is_none());
}

#[test]
fn branch_delete_guards_published() {
    let mut s = sm();
    let (pid, _) = setup(&mut s);
    // 结构发布后 active_version=1 → 不可删
    let err = s
        .apply(
            &Command::BranchDelete {
                project: pid.clone(),
                name: BranchName("test".into()),
                operator: String::new(),
            },
            5,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
}

// ---------------- M2：回滚（I6/I9） ----------------

#[test]
fn rollback_creates_new_version_with_old_content() {
    let mut s = sm();
    let (pid, _) = setup(&mut s);
    let b = BranchName("dev".into());
    // 发布 v2（结构发布 v1）
    s.apply(
        &Command::DraftUpdate {
            project: pid.clone(),
            branch: b.clone(),
            updates: vec![DraftUpdateItem {
                group: "redis".into(),
                key: "host".into(),
                value: Value::String("10.0.0.9".into()),
            }],
            deletes: vec![],
        
                operator: String::new(),
            },
        4,
    )
    .unwrap();
    s.apply(
        &Command::Publish {
            project: pid.clone(),
            branch: b.clone(),
            comment: "v2".into(),
            request_id: "r1".into(),
        
                operator: String::new(),
            },
        5,
    )
    .unwrap();
    assert_eq!(s.get_config(&pid, &b, 0).unwrap().version, 2);

    // 回滚到 v1
    let events = s
        .apply(
            &Command::Rollback {
                project: pid.clone(),
                branch: b.clone(),
                to_version: 1,
                comment: "rollback".into(),
                request_id: "rb1".into(),
                operator: String::new(),
            },
            6,
        )
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].ty, EventType::Rollback);
    assert_eq!(events[0].version, 3);

    // v3 内容 = v1 内容（host 空）
    let snap = s.get_config(&pid, &b, 0).unwrap();
    assert_eq!(snap.version, 3);
    assert!(snap.groups.is_empty());

    // 历史记录 rollback_of=1
    let rec = s.get_version_record(&pid, &b, 3).unwrap().unwrap();
    assert_eq!(rec.rollback_of, Some(1));

    // 幂等：同 request_id 不重复
    let again = s
        .apply(
            &Command::Rollback {
                project: pid.clone(),
                branch: b.clone(),
                to_version: 1,
                comment: "x".into(),
                request_id: "rb1".into(),
                operator: String::new(),
            },
            7,
        )
        .unwrap();
    assert!(again.is_empty());
    assert_eq!(s.get_config(&pid, &b, 0).unwrap().version, 3);
}

#[test]
fn rollback_invalid_version_rejected() {
    let mut s = sm();
    let (pid, _) = setup(&mut s);
    let b = BranchName("dev".into());
    let err = s
        .apply(
            &Command::Rollback {
                project: pid.clone(),
                branch: b.clone(),
                to_version: 1,
                comment: "x".into(),
                request_id: "r".into(),
                operator: String::new(),
            },
            5,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Validation); // to_version >= active(1)
    let err = s
        .apply(
            &Command::Rollback {
                project: pid.clone(),
                branch: b.clone(),
                to_version: 99,
                comment: "x".into(),
                request_id: "r".into(),
                operator: String::new(),
            },
            5,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Validation); // 超出活动版本范围
}

// ---------------- M2：共享库 + 引用 + 级联（R6） ----------------

fn publish_shared(s: &mut StateMachine, group: &str, key: &str, value: Value, request_id: &str) {
    s.apply(
        &Command::SharedDraftUpdate {
            item: SharedItem {
                group: group.into(),
                key: key.into(),
                ty: value.value_type(),
                secret: false,
                required: false,
                value,
                version: 0,
            },
        
                operator: String::new(),
            },
        10,
    )
    .unwrap();
    s.apply(
        &Command::SharedPublish {
            comment: "shared".into(),
            request_id: request_id.into(),
        
                operator: String::new(),
            },
        11,
    )
    .unwrap();
}

#[test]
fn shared_cascade_updates_referencing_branches() {
    let mut s = sm();
    // 先发布共享项
    publish_shared(
        &mut s,
        "infra",
        "db_host",
        Value::String("db.internal".into()),
        "sp1",
    );

    // 项目结构含 db/pwd item，绑定引用
    let (pid, _) = setup(&mut s); // setup 发布结构 v2（redis 组）
                                  // 再发布结构：加 db 组
    s.apply(
        &Command::StructureDraftSet {
            project: pid.clone(),
            base_version: 2,
            groups: vec![
                dsh_core::model::GroupDef {
                    name: "redis".into(),
                    items: vec![
                        ItemDef {
                            key: "host".into(),
                            ty: ValueType::String,
                            required: true,
                            secret: false,
                            validate: None,
                        },
                        ItemDef {
                            key: "port".into(),
                            ty: ValueType::Int,
                            required: false,
                            secret: false,
                            validate: None,
                        },
                        ItemDef {
                            key: "password".into(),
                            ty: ValueType::Secret,
                            required: false,
                            secret: true,
                            validate: None,
                        },
                    ],
                },
                dsh_core::model::GroupDef {
                    name: "db".into(),
                    items: vec![ItemDef {
                        key: "host".into(),
                        ty: ValueType::String,
                        required: false,
                        secret: false,
                        validate: None,
                    }],
                },
            ],
        
                operator: String::new(),
            },
        12,
    )
    .unwrap();
    s.apply(
        &Command::PublishStructure {
            project: pid.clone(),
            comment: "add db".into(),
            request_id: "s2".into(),
        
                operator: String::new(),
            },
        13,
    )
    .unwrap();

    // RefBind：db/host → infra/db_host
    s.apply(
        &Command::RefBind {
            project: pid.clone(),
            binding: RefBinding {
                group: "db".into(),
                item_key: Some("host".into()),
                shared_group: "infra".into(),
                shared_key: "db_host".into(),
            },
        
                operator: String::new(),
            },
        14,
    )
    .unwrap();

    // 分支发布（dev：只填 redis/host）→ db/host 由共享物化
    s.apply(
        &Command::DraftUpdate {
            project: pid.clone(),
            branch: BranchName("dev".into()),
            updates: vec![DraftUpdateItem {
                group: "redis".into(),
                key: "host".into(),
                value: Value::String("127.0.0.1".into()),
            }],
            deletes: vec![],
        
                operator: String::new(),
            },
        15,
    )
    .unwrap();
    let before = s
        .get_branch_state(&pid, &BranchName("dev".into()))
        .unwrap()
        .unwrap()
        .active_version;
    s.apply(
        &Command::Publish {
            project: pid.clone(),
            branch: BranchName("dev".into()),
            comment: "dev".into(),
            request_id: "r1".into(),
        
                operator: String::new(),
            },
        16,
    )
    .unwrap();
    let dev_ver = s.get_config(&pid, &BranchName("dev".into()), 0).unwrap();
    assert_eq!(
        dev_ver.groups["db"]["host"],
        Value::String("db.internal".into())
    );

    // 共享项变更 → 级联：dev 分支版本推进，值更新
    let after_publish_ver = s
        .get_branch_state(&pid, &BranchName("dev".into()))
        .unwrap()
        .unwrap()
        .active_version;
    assert!(after_publish_ver > before);
    publish_shared(
        &mut s,
        "infra",
        "db_host",
        Value::String("db.internal.2".into()),
        "sp2",
    );
    let dev_after = s.get_config(&pid, &BranchName("dev".into()), 0).unwrap();
    assert_eq!(
        dev_after.groups["db"]["host"],
        Value::String("db.internal.2".into())
    );
    // 事件类型 SharedCascade
    let hist = s.version_history(&pid, &BranchName("dev".into())).unwrap();
    assert!(hist.len() >= 3);
}

#[test]
fn ref_requires_published_shared() {
    let mut s = sm();
    let (pid, _) = setup(&mut s);
    let err = s
        .apply(
            &Command::RefBind {
                project: pid.clone(),
                binding: RefBinding {
                    group: "redis".into(),
                    item_key: Some("host".into()),
                    shared_group: "infra".into(),
                    shared_key: "nope".into(),
                },
                operator: String::new(),
            },
            12,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Validation);
}

// ---------------- 会话（I7 单管理员） ----------------

#[test]
fn session_login_logout_heartbeat() {
    let mut s = sm();
    let token = "tok-abc123";
    let hash = dsh_core::token_hash(token);
    // 登录成功 → 会话入库（只存哈希）
    s.apply(
        &Command::SessionLogin {
            token_hash: hash.clone(),
            issued_at: 1000,
            expires_at: Some(1000 + 86_400_000),
        },
        1,
    )
    .unwrap();
    let sess = s.get_session().unwrap().expect("session exists");
    assert_eq!(sess.token_hash, hash);
    assert_ne!(sess.token_hash, token); // 明文不落库
    assert_eq!(sess.expires_at, Some(1000 + 86_400_000));

    // 二次登录 → ERR_SESSION_IN_USE
    let err = s
        .apply(
            &Command::SessionLogin {
                token_hash: dsh_core::token_hash("tok-other"),
                issued_at: 2000,
                expires_at: None,
            },
            2,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::SessionInUse);

    // 心跳续期
    s.apply(
        &Command::SessionHeartbeat {
            expires_at: Some(3000),
        },
        3,
    )
    .unwrap();
    assert_eq!(s.get_session().unwrap().unwrap().expires_at, Some(3000));

    // 登出 → 会话清除；重复登出幂等
    s.apply(&Command::SessionLogout, 4).unwrap();
    assert!(s.get_session().unwrap().is_none());
    assert!(s.apply(&Command::SessionLogout, 5).is_ok());
}

#[test]
fn session_heartbeat_without_login_expired() {
    let mut s = sm();
    let err = s
        .apply(&Command::SessionHeartbeat { expires_at: None }, 1)
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::SessionExpired);
}

#[test]
fn session_token_hash_is_deterministic_and_distinct() {
    assert_eq!(dsh_core::token_hash("abc"), dsh_core::token_hash("abc"));
    assert_ne!(dsh_core::token_hash("abc"), dsh_core::token_hash("abd"));
    assert_eq!(dsh_core::token_hash("abc").len(), 64); // SHA-256 hex
}

// ---------------- 审计（B4：落库 audit/{seq}） ----------------

fn audit_cmd(action: &str, ts: i64) -> Command {
    Command::AuditAppend {
        entry: AuditEntry {
            seq: 0, // 状态机分配
            ts,
            operator: "admin".into(),
            action: action.into(),
            project: Some("order-service".into()),
            branch: Some("dev".into()),
            version: Some(3),
            request_id: Some("r-1".into()),
            detail: serde_json::json!({ "n": 1 }),
        },
    }
}

#[test]
fn audit_append_seq_monotonic_and_queryable() {
    let mut s = sm();
    s.apply(&audit_cmd("publish", 1000), 1).unwrap();
    s.apply(&audit_cmd("rollback", 2000), 2).unwrap();
    s.apply(&audit_cmd("publish", 3000), 3).unwrap();

    // 全量（新 → 旧）
    let all = s.get_audit(None, None, 100).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].seq, 3);
    assert_eq!(all[0].action, "publish");
    assert_eq!(all[2].seq, 1);

    // action 过滤
    let pubs = s.get_audit(Some("publish"), None, 100).unwrap();
    assert_eq!(pubs.len(), 2);
    assert!(pubs.iter().all(|e| e.action == "publish"));

    // since 过滤（ts ≥ since）
    let recent = s.get_audit(None, Some(1500), 100).unwrap();
    assert_eq!(recent.len(), 2);

    // limit 截断
    let limited = s.get_audit(None, None, 2).unwrap();
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0].seq, 3);
}

#[test]
fn audit_persists_and_prunes() {
    let mut s = sm();
    for i in 0..5 {
        s.apply(&audit_cmd("publish", 1000 + i), 10 + i).unwrap();
    }
    // 保留最近 2 条
    let removed = s.prune_audit(2).unwrap();
    assert_eq!(removed, 3);
    let all = s.get_audit(None, None, 100).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].seq, 5);
    assert_eq!(all[1].seq, 4);
    // 再剪一次：已达标不再删
    assert_eq!(s.prune_audit(2).unwrap(), 0);
}

#[test]
fn audit_seq_counter_survives_restore() {
    // 模拟快照 dump/restore：seq 计数键随状态导出，恢复后继续递增
    let mut s = sm();
    s.apply(&audit_cmd("login", 1), 1).unwrap();
    let pairs = s.dump_all().unwrap();
    let mut s2 = StateMachine::new(Box::new(InMemoryStore::new()));
    s2.restore_all(&pairs).unwrap();
    s2.apply(&audit_cmd("logout", 2), 2).unwrap();
    let all = s2.get_audit(None, None, 100).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].seq, 2);
}

// ---------------- B3：组级引用（item_key=None 整组绑定共享组） ----------------

/// 建项目（结构 redis 组：host/port）+ 发布共享组 infra（host/port）。
fn group_ref_setup(s: &mut StateMachine) -> ProjectId {
    let (pid, _) = setup(s); // redis 组：host 必填 / port / password(secret)
    publish_shared(
        s,
        "infra",
        "host",
        Value::String("sh-host.old".into()),
        "gh1",
    );
    publish_shared(s, "infra", "port", Value::Int(6000), "gh2");
    pid
}

#[test]
fn group_ref_bind_requires_matching_shared_item() {
    let mut s = sm();
    let (pid, _) = setup(&mut s);
    // 共享组 infra 没有任何已发布项 → 组级绑定被拒
    let err = s
        .apply(
            &Command::RefBind {
                project: pid,
                binding: RefBinding {
                    group: "redis".into(),
                    item_key: None,
                    shared_group: "infra".into(),
                    shared_key: "redis".into(),
                },
                operator: String::new(),
            },
            20,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Validation);
}

#[test]
fn group_ref_materializes_matching_items_at_publish() {
    let mut s = sm();
    let pid = group_ref_setup(&mut s);
    // 组级绑定：整组 redis ← 共享组 infra
    s.apply(
        &Command::RefBind {
            project: pid.clone(),
            binding: RefBinding {
                group: "redis".into(),
                item_key: None,
                shared_group: "infra".into(),
                shared_key: "redis".into(),
            },
        
                operator: String::new(),
            },
        30,
    )
    .unwrap();
    // 草稿只显式设置 host → 发布后 host=草稿值，port=共享值（整组按结构 item key 匹配）
    s.apply(
        &Command::DraftUpdate {
            project: pid.clone(),
            branch: BranchName("dev".into()),
            updates: vec![DraftUpdateItem {
                group: "redis".into(),
                key: "host".into(),
                value: Value::String("local-host".into()),
            }],
            deletes: vec![],
        
                operator: String::new(),
            },
        31,
    )
    .unwrap();
    s.apply(
        &Command::Publish {
            project: pid.clone(),
            branch: BranchName("dev".into()),
            comment: "g1".into(),
            request_id: "gr1".into(),
        
                operator: String::new(),
            },
        32,
    )
    .unwrap();
    let cfg = s.get_config(&pid, &BranchName("dev".into()), 0).unwrap();
    let redis = cfg.groups.get("redis").unwrap();
    assert_eq!(
        redis.get("host").unwrap(),
        &Value::String("local-host".into()),
        "草稿显式值优先"
    );
    assert_eq!(
        redis.get("port").unwrap(),
        &Value::Int(6000),
        "未显式 item 由组级共享物化"
    );
    // password 不在共享组 infra → 不物化（结构 item 但无匹配共享项）
    assert!(!redis.contains_key("password"));
}

#[test]
fn group_ref_cascade_on_shared_publish() {
    let mut s = sm();
    let pid = group_ref_setup(&mut s);
    s.apply(
        &Command::RefBind {
            project: pid.clone(),
            binding: RefBinding {
                group: "redis".into(),
                item_key: None,
                shared_group: "infra".into(),
                shared_key: "redis".into(),
            },
        
                operator: String::new(),
            },
        40,
    )
    .unwrap();
    // 共享发布 infra/port 新值 → 组级级联：引用项目的分支版本推进
    publish_shared(&mut s, "infra", "port", Value::Int(7000), "gh3");
    let cfg = s.get_config(&pid, &BranchName("dev".into()), 0).unwrap();
    assert_eq!(
        cfg.version, 2,
        "共享发布后活动版本推进（结构 v1 + 级联 v2）"
    );
    assert_eq!(
        cfg.groups.get("redis").unwrap().get("port").unwrap(),
        &Value::Int(7000),
        "组级级联更新匹配 item"
    );
    // 其他分支（test/prod）同样级联
    let t = s.get_config(&pid, &BranchName("test".into()), 0).unwrap();
    assert_eq!(
        t.groups.get("redis").unwrap().get("port").unwrap(),
        &Value::Int(7000)
    );
}

#[test]
fn group_ref_unbind_stops_materialization() {
    let mut s = sm();
    let pid = group_ref_setup(&mut s);
    s.apply(
        &Command::RefBind {
            project: pid.clone(),
            binding: RefBinding {
                group: "redis".into(),
                item_key: None,
                shared_group: "infra".into(),
                shared_key: "redis".into(),
            },
        
                operator: String::new(),
            },
        50,
    )
    .unwrap();
    s.apply(
        &Command::RefUnbind {
            project: pid.clone(),
            group: "redis".into(),
            item_key: None,
        
                operator: String::new(),
            },
        51,
    )
    .unwrap();
    // 发布（草稿只设 host）→ port 不再物化（但 required host 已设，可发布）
    s.apply(
        &Command::DraftUpdate {
            project: pid.clone(),
            branch: BranchName("dev".into()),
            updates: vec![DraftUpdateItem {
                group: "redis".into(),
                key: "host".into(),
                value: Value::String("h".into()),
            }],
            deletes: vec![],
        
                operator: String::new(),
            },
        52,
    )
    .unwrap();
    s.apply(
        &Command::Publish {
            project: pid.clone(),
            branch: BranchName("dev".into()),
            comment: "g2".into(),
            request_id: "gr2".into(),
        
                operator: String::new(),
            },
        53,
    )
    .unwrap();
    let cfg = s.get_config(&pid, &BranchName("dev".into()), 0).unwrap();
    assert!(
        !cfg.groups.get("redis").unwrap().contains_key("port"),
        "解绑后不再物化共享值"
    );
}

// ---------------- B6：DEK 重包（rewrap_deks） ----------------

fn fake_ct(dek_v: u64) -> Value {
    Value::Secret(Ciphertext {
        enc: "aes-256-gcm".into(),
        v: 1,
        dek_v,
        nonce: "n".into(),
        ct: "c".into(),
        edek: "e".into(),
        edek_nonce: "en".into(),
    })
}

#[test]
fn rewrap_deks_rewrites_snapshot_shared_and_draft_secrets() {
    let mut s = sm();
    let (pid, _) = setup(&mut s); // redis 组含 password(secret)
                                  // 共享项（secret 值，代际 1）
    s.apply(
        &Command::SharedDraftUpdate {
            item: SharedItem {
                group: "infra".into(),
                key: "token".into(),
                ty: ValueType::Secret,
                secret: true,
                required: false,
                value: fake_ct(1),
                version: 0,
            },
        
                operator: String::new(),
            },
        60,
    )
    .unwrap();
    s.apply(
        &Command::SharedPublish {
            comment: "c".into(),
            request_id: "rw1".into(),
        
                operator: String::new(),
            },
        61,
    )
    .unwrap();
    // 分支发布 secret（快照，代际 1）
    s.apply(
        &Command::DraftUpdate {
            project: pid.clone(),
            branch: BranchName("dev".into()),
            updates: vec![
                DraftUpdateItem {
                    group: "redis".into(),
                    key: "host".into(),
                    value: Value::String("h".into()),
                },
                DraftUpdateItem {
                    group: "redis".into(),
                    key: "password".into(),
                    value: fake_ct(1),
                },
            ],
            deletes: vec![],
        
                operator: String::new(),
            },
        62,
    )
    .unwrap();
    s.apply(
        &Command::Publish {
            project: pid.clone(),
            branch: BranchName("dev".into()),
            comment: "c".into(),
            request_id: "rw2".into(),
        
                operator: String::new(),
            },
        63,
    )
    .unwrap();

    // 重包：代际 < 2 的密文重写为代际 2（模拟轮换后 edek 换新 KEK）
    let count = s
        .rewrap_deks(&|ct| {
            if ct.dek_v >= 2 {
                None
            } else {
                let mut n = ct.clone();
                n.dek_v = 2;
                Some(Ok(n))
            }
        })
        .unwrap();
    assert!(
        count >= 2,
        "快照 + 共享项中的 secret 均被重包，实际 {count}"
    );

    let cfg = s.get_config(&pid, &BranchName("dev".into()), 0).unwrap();
    match cfg.groups.get("redis").unwrap().get("password").unwrap() {
        Value::Secret(ct2) => assert_eq!(ct2.dek_v, 2, "快照 secret 已重包"),
        _ => panic!("expected secret"),
    }
    let rows = s.dump_all().unwrap();
    let sh_row = rows
        .iter()
        .find(|(k, _)| String::from_utf8_lossy(k) == "sh/infra/token")
        .expect("shared item row");
    let shared: SharedItem = serde_json::from_slice(&sh_row.1).unwrap();
    match shared.value {
        Value::Secret(ct2) => assert_eq!(ct2.dek_v, 2, "共享 secret 已重包"),
        _ => panic!("expected secret"),
    }
    // 草稿中的 secret（尚未发布）也重包
    s.apply(
        &Command::DraftUpdate {
            project: pid.clone(),
            branch: BranchName("test".into()),
            updates: vec![DraftUpdateItem {
                group: "redis".into(),
                key: "password".into(),
                value: fake_ct(1),
            }],
            deletes: vec![],
        
                operator: String::new(),
            },
        64,
    )
    .unwrap();
    let count3 = s
        .rewrap_deks(&|ct| {
            if ct.dek_v >= 2 {
                None
            } else {
                let mut n = ct.clone();
                n.dek_v = 2;
                Some(Ok(n))
            }
        })
        .unwrap();
    assert!(count3 >= 1, "草稿 secret 也被重包，实际 {count3}");
    // 幂等：全部已最新 → 0
    let count2 = s
        .rewrap_deks(&|ct| {
            if ct.dek_v >= 2 {
                None
            } else {
                Some(Ok(ct.clone()))
            }
        })
        .unwrap();
    assert_eq!(count2, 0);
}

// ---------------- P3：限额（LIM-001） + 管理员改密 ----------------

#[test]
fn shared_item_over_limit_rejected() {
    let mut s = sm();
    let big = "x".repeat(dsh_core::limits::MAX_VALUE_BYTES + 1);
    let item = dsh_core::model::SharedItem {
        group: "g".into(),
        key: "k".into(),
        ty: dsh_core::model::ValueType::String,
        secret: false,
        required: false,
        value: dsh_core::model::Value::String(big),
        version: 0,
    };
    let err = s
        .apply(&dsh_core::command::Command::SharedDraftUpdate { item,
                operator: String::new(),
            }, 1)
        .unwrap_err();
    assert_eq!(
        err.kind,
        dsh_core::ErrorKind::LimitExceeded,
        "超限额应 ERR_LIMIT_EXCEEDED"
    );
}

#[test]
fn admin_set_password_persists_and_reads() {
    let mut s = sm();
    let hash = "sha256-hex-of-password";
    s.apply(
        &dsh_core::command::Command::AdminSetPassword {
            password_hash: hash.into(),
        },
        1,
    )
    .unwrap();
    assert_eq!(s.get_admin_password_hash().unwrap().as_deref(), Some(hash));
    // 未设置时返回 None（回退节点配置）
    let s2 = sm();
    assert_eq!(s2.get_admin_password_hash().unwrap(), None);
}
