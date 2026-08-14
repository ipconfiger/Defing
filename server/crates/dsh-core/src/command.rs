//! 状态机写命令（Raft 日志载荷；确定性 apply，模块 01 §3）。

use serde::{Deserialize, Serialize};

use crate::model::{BranchName, GroupDef, ProjectId, RefBinding, SharedItem, Value};

/// 值草稿更新条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DraftUpdateItem {
    pub group: String,
    pub key: String,
    pub value: Value,
}

/// 状态机写命令（M1 子集；M2 追加 Rollback/SharedPublish/RefBind/Promote/会话命令）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    ProjectCreate {
        name: String,
    },
    ProjectDelete {
        id: ProjectId,
    },
    /// source：可选，从该分支的活动版本值物化出初始值草稿（缺省为空草稿）。
    BranchCreate {
        project: ProjectId,
        name: BranchName,
        source: Option<BranchName>,
    },
    BranchDelete {
        project: ProjectId,
        name: BranchName,
    },
    /// 整体替换结构草稿；base_version 必须等于当前已发布结构版本。
    StructureDraftSet {
        project: ProjectId,
        base_version: u64,
        groups: Vec<GroupDef>,
    },
    /// 发布结构草稿：对全部分支同时生效（I3/I5）。
    PublishStructure {
        project: ProjectId,
        comment: String,
        request_id: String,
    },
    /// 更新分支值草稿（不生效，I4）。
    DraftUpdate {
        project: ProjectId,
        branch: BranchName,
        updates: Vec<DraftUpdateItem>,
        /// 待删除 item："group/key"
        deletes: Vec<(String, String)>,
    },
    /// 发布分支版本（原子：固化草稿→版本→指针→diff→事件；幂等 I10）。
    Publish {
        project: ProjectId,
        branch: BranchName,
        comment: String,
        request_id: String,
    },
    /// 回滚：基于历史版本内容创建新版本（历史不可变，I6/I9）。
    Rollback {
        project: ProjectId,
        branch: BranchName,
        to_version: u64,
        comment: String,
        request_id: String,
    },
    /// 更新共享项草稿（写共享草稿，发布后生效）。
    SharedDraftUpdate {
        item: SharedItem,
    },
    /// 发布共享项（auto 级联引用它的所有项目分支；原子）。
    SharedPublish {
        comment: String,
        request_id: String,
    },
    /// 绑定项目 item → 共享项。
    RefBind {
        project: ProjectId,
        binding: RefBinding,
    },
    /// 解绑。
    RefUnbind {
        project: ProjectId,
        group: String,
        item_key: Option<String>,
    },
    /// 管理员登录（I7）：token 哈希入库；已有活动会话 → ERR_SESSION_IN_USE。
    /// 密码校验在 API 层（admin_password 是节点配置，不进状态机）。
    SessionLogin {
        token_hash: String,
        issued_at: i64,
        expires_at: Option<i64>,
    },
    /// 登出：清除会话（幂等）。
    SessionLogout,
    /// 心跳续期：更新 expires_at；无会话 → ERR_SESSION_EXPIRED。
    SessionHeartbeat {
        expires_at: Option<i64>,
    },
    /// 修改管理员密码（哈希落状态机，集群一致；登录优先用它校验，回退节点配置）。
    AdminSetPassword {
        password_hash: String,
    },
    /// 审计落库（seq 由状态机单调分配并覆写；经 Raft 复制，集群一致）。
    AuditAppend {
        entry: crate::model::AuditEntry,
    },
}
