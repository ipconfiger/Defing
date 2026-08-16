//! 状态机写命令（Raft 日志载荷；确定性 apply，模块 01 §3）。
//! operator 字段（审计身份）：空串 = 旧客户端/全局管理员 → 状态机落 "admin"；
//! 项目管理员为 "pa:{username}"。全部 `#[serde(default)]` 保证旧日志重放兼容。

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
        #[serde(default)]
        operator: String,
        /// 墙钟 ms（API 层注入；0 = 回退 apply 的 now_ms 参数，旧日志重放兼容）
        #[serde(default)]
        ts: i64,
    },
    ProjectDelete {
        id: ProjectId,
        #[serde(default)]
        operator: String,
    },
    /// source：可选，从该分支的活动版本值物化出初始值草稿（缺省为空草稿）。
    BranchCreate {
        project: ProjectId,
        name: BranchName,
        source: Option<BranchName>,
        #[serde(default)]
        operator: String,
        /// 墙钟 ms（API 层注入；0 = 回退 apply 的 now_ms 参数，旧日志重放兼容）
        #[serde(default)]
        ts: i64,
    },
    BranchDelete {
        project: ProjectId,
        name: BranchName,
        #[serde(default)]
        operator: String,
    },
    /// 整体替换结构草稿；base_version 必须等于当前已发布结构版本。
    StructureDraftSet {
        project: ProjectId,
        base_version: u64,
        groups: Vec<GroupDef>,
        #[serde(default)]
        operator: String,
    },
    /// 发布结构草稿：对全部分支同时生效（I3/I5）。
    PublishStructure {
        project: ProjectId,
        comment: String,
        request_id: String,
        #[serde(default)]
        operator: String,
        /// 墙钟 ms（API 层注入；0 = 回退 apply 的 now_ms 参数，旧日志重放兼容）
        #[serde(default)]
        ts: i64,
    },
    /// 更新分支值草稿（不生效，I4）。
    DraftUpdate {
        project: ProjectId,
        branch: BranchName,
        updates: Vec<DraftUpdateItem>,
        /// 待删除 item："group/key"
        deletes: Vec<(String, String)>,
        #[serde(default)]
        operator: String,
        /// 墙钟 ms（API 层注入；0 = 回退 apply 的 now_ms 参数，旧日志重放兼容）
        #[serde(default)]
        ts: i64,
    },
    /// 发布分支版本（原子：固化草稿→版本→指针→diff→事件；幂等 I10）。
    Publish {
        project: ProjectId,
        branch: BranchName,
        comment: String,
        request_id: String,
        #[serde(default)]
        operator: String,
        /// 墙钟 ms（API 层注入；0 = 回退 apply 的 now_ms 参数，旧日志重放兼容）
        #[serde(default)]
        ts: i64,
    },
    /// 回滚：基于历史版本内容创建新版本（历史不可变，I6/I9）。
    Rollback {
        project: ProjectId,
        branch: BranchName,
        to_version: u64,
        comment: String,
        request_id: String,
        #[serde(default)]
        operator: String,
        /// 墙钟 ms（API 层注入；0 = 回退 apply 的 now_ms 参数，旧日志重放兼容）
        #[serde(default)]
        ts: i64,
    },
    /// 更新共享项草稿（写共享草稿，发布后生效）。
    SharedDraftUpdate {
        item: SharedItem,
        #[serde(default)]
        operator: String,
    },
    /// 发布共享项（auto 级联引用它的所有项目分支；原子）。
    SharedPublish {
        comment: String,
        request_id: String,
        #[serde(default)]
        operator: String,
        /// 墙钟 ms（API 层注入；0 = 回退 apply 的 now_ms 参数，旧日志重放兼容）
        #[serde(default)]
        ts: i64,
    },
    /// 绑定项目 item → 共享项。
    RefBind {
        project: ProjectId,
        binding: RefBinding,
        #[serde(default)]
        operator: String,
    },
    /// 解绑。
    RefUnbind {
        project: ProjectId,
        group: String,
        item_key: Option<String>,
        #[serde(default)]
        operator: String,
    },
    /// 管理员登录（I7）：token 哈希入库；已有活动会话 → ERR_SESSION_IN_USE。
    /// 密码校验在 API 层（admin_password 是节点配置，不进状态机）。
    /// 注意：全局管理员会话命令保持原状（Raft wire 兼容），项目管理员用 Pa* 变体。
    SessionLogin {
        token_hash: String,
        issued_at: i64,
        expires_at: Option<i64>,
    },
    /// 登出：清除会话（幂等）。
    SessionLogout,
    /// 心跳续期：更新 expires_at；无会话 → ERR_SESSION_EXPIRED。
    SessionHeartbeat { expires_at: Option<i64> },
    /// 创建项目管理员账号（项目须存在；用户名 [A-Za-z0-9_-]{2,64} 且 ≠ "admin"）。
    ProjectAdminCreate {
        project: ProjectId,
        username: String,
        salt: String,
        password_hash: String,
        /// 墙钟 ms（API 层注入；0 = 回退 apply 的 now_ms 参数，旧日志重放兼容）
        #[serde(default)]
        ts: i64,
    },
    /// 删除项目管理员账号（级联删除其会话）。
    ProjectAdminDelete { username: String },
    /// 修改项目管理员密码（级联删除其会话，需重新登录）。
    ProjectAdminSetPassword {
        username: String,
        salt: String,
        password_hash: String,
    },
    /// 项目管理员登录：写 sess/pa/{username}；该账号已有会话 → ERR_SESSION_IN_USE
    /// （只判 is_some，不读墙钟，保证 Raft 重放确定性）。
    PaSessionLogin {
        username: String,
        token_hash: String,
        issued_at: i64,
        expires_at: Option<i64>,
        device_id: String,
    },
    /// 项目管理员登出（幂等）。
    PaSessionLogout { username: String },
    /// 项目管理员心跳续期（None = 永不过期，语义同 SessionHeartbeat）。
    PaSessionHeartbeat {
        username: String,
        expires_at: Option<i64>,
    },
    /// 修改管理员密码（哈希落状态机，集群一致；登录优先用它校验，回退节点配置）。
    AdminSetPassword { password_hash: String },
    /// 审计落库（seq 由状态机单调分配并覆写；经 Raft 复制，集群一致）。
    AuditAppend { entry: crate::model::AuditEntry },
    /// 主密钥轮换（集群一致）：新 KEK 经 Raft 复制到全部节点；各节点 apply 时更新本地 keyring 并持久化 ring 文件。
    /// F7b：新命令 `kek` 置空、`kek_enc` 携带「当前 KEK 自加密的新 KEK」（Raft 日志无明文）；
    /// 旧日志仅含 `kek` 明文（`#[serde(default)]` 兼容重放），钩子实现方按字段选择解密路径。
    RotateMasterKey {
        /// 明文新 KEK（32B；旧日志路径，新命令置空）
        #[serde(default)]
        kek: Vec<u8>,
        /// 自加密的新 KEK（AES-256-GCM，用提交时刻的当前 KEK 加密；F7b）
        #[serde(default)]
        kek_enc: Vec<u8>,
    },
}
