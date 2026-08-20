# 模块 01 —— 数据模型与状态机核心（dsh-core）

> 依据：design-v2 §3、schema/storage.v1.schema.json、design-v3 §4/§5
> 版本：v1.0 ｜ 状态：开发就绪

## 1. 职责与边界
- 职责：数据模型（实体 + KV 键构造）、校验器（item 校验/引用解析/循环检测）、
  结构一致性不变量（I3）维护逻辑、diff 计算。
- 不做：Raft、存储 IO、网络、加密算法本身（secret 值以 `Value::Secret` 透传，密文格式见模块 07）。

## 2. 核心类型（Rust）

```
pub struct Project   { pub id: ProjectId, pub name: String, pub created_at: i64 }
pub struct Branch    { pub name: BranchName }                       // 默认 dev/test/prod
pub struct ItemDef   { pub key: String, pub ty: ValueType, pub required: bool,
                       pub secret: bool, pub validate: Option<ValidationRule> }
pub struct GroupDef  { pub name: String, pub items: Vec<ItemDef> }
pub struct Structure { pub version: u64, pub groups: Vec<GroupDef> }
pub struct StructureDraft { pub base_version: u64, pub groups: Vec<GroupDef> }

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Value {
    String(String), Int(i64), Float(f64), Bool(bool), Json(String),
    Array(Vec<String>), Secret(SecretBox),   // SecretBox = 密文载体（模块 07 定义）
}

pub struct DraftValue  { pub value: Value, pub updated_at: i64 }
pub struct BranchState { pub active_version: u64, pub structure_version: u64,
                         pub last_request_id: Option<String>,
                         pub value_draft: BTreeMap<String, BTreeMap<String, DraftValue>> }
pub struct VersionRecord { pub no: u64, pub structure_version: u64, pub created_at: i64,
                           pub operator: String, pub comment: String,
                           pub rollback_of: Option<u64>, pub kind: VersionKind,
                           pub snapshot_ref: Option<String>, pub diff_ref: Option<String> }
pub struct DiffEntry { pub group: String, pub key: String, pub kind: ChangeKind,
                       pub new_value: Option<Value> }

pub enum ChangeKind { Upsert, Delete }
pub enum EventType { ValuePublish, StructurePublish, SharedCascade, Rollback }
pub struct PublishEvent { pub version: u64, pub ty: EventType,
                          pub structure_version: u64, pub comment: String,
                          pub request_id: String, pub changes: Vec<DiffEntry> }
```

## 3. 状态机命令与结果（apply 输入/输出）
所有写操作建模为 Command；apply 返回 `ApplyOutcome`（确定性副作用 = 发布事件列表）。

```
pub enum Command {
    ProjectCreate { name: String },
    ProjectDelete { id: ProjectId },
    BranchCreate { project: ProjectId, name: BranchName },
    BranchDelete { project: ProjectId, name: BranchName },
    DraftUpdate   { project: ProjectId, branch: BranchName,
                    updates: Vec<DraftUpdateItem>, deletes: Vec<(String, String)> },
    Publish       { project: ProjectId, branch: BranchName,
                    comment: String, request_id: String },
    PublishStructure { project: ProjectId, comment: String, request_id: String },
    Rollback      { project: ProjectId, branch: BranchName, to_version: u64,
                    comment: String, request_id: String },
    SharedDraftUpdate { item: SharedItem },
    SharedPublish { comment: String, request_id: String },
    SharedDelete  { key: String },           // 删除共享项（被项目结构引用 → 拒绝）；引用已内嵌 ItemDef.shared_ref
    RefUnbind     { project: ProjectId, group: String, item_key: Option<String> },
    Promote       { project: ProjectId, from: BranchName, to: BranchName,
                    items: Option<Vec<(String, String)>>, force: bool },
    SessionLogin  { token_hash: String, expires_at: i64, device_id: String },
    SessionLogout {}, SessionForceLogout {}, SessionHeartbeat { expires_at: i64 },
    SetPassword   { must_change: bool },
}
pub enum ApplyOutcome { Ok(Vec<PublishEvent>), Err(Error) }  // 无部分生效
```

## 4. 键构造（KV 布局常量，与 storage schema 对齐）

```
pub const K_PROJECT: &str = "p/";            // p/{id}
pub const K_STRUCT:  &str = "/struct";       // p/{id}/struct
pub const K_STRUCT_DRAFT: &str = "/struct-draft";
pub const K_BRANCH:  &str = "/b/";           // p/{id}/b/{branch}
pub const K_STATE:   &str = "/state";
pub const K_VERSION: &str = "/v/";           // p/{id}/b/{branch}/v/{no}
pub const K_REF:     &str = "/refs/";        // p/{id}/refs/{group}/{key?}
pub const K_SHARED:  &str = "sh/";           // sh/{group}/{key}
pub const K_SHARED_DRAFT: &str = "sh-draft/";
pub const K_SESSION: &str = "sess/admin";
pub const K_AUDIT:   &str = "audit/";
pub const K_IDX_PNAME: &str = "idx/pname/";  // 项目名唯一索引
pub const K_IDX_REF: &str = "idx/ref/";      // sh/{g}/{k} → 引用方列表（级联反查）
```

## 5. 校验器（Validator）

| 输入 | 规则 |
|------|------|
| item 值 | 类型匹配；string ≤64KB；validate 规则（正则/范围） |
| 结构 | 分组/item 名唯一；key 合法标识符；TOML 表达力约束（design-v2 §8.2） |
| 引用 | ItemDef.shared_ref 目标存在且类型一致（结构保存/发布双校验）；级联走结构扫描（无独立索引） |
| 发布 | required 未填 → 阻断（policy=block）→ ERR_PUBLISH_BLOCKED |
| 限额 | design-v2 §3.4 限额表 → ERR_LIMIT_EXCEEDED |

## 6. diff 计算（同结构按 key）

```
pub fn compute_diff(old: &SnapshotMap, new: &SnapshotMap) -> Vec<DiffEntry>
// 遍历结构顺序输出 upsert/delete；O(变更项)；secret 值比较用密文（不解密）
```

## 7. 结构一致性（I3）
- 结构单点存储（p/{id}/struct）；PublishStructure 一次性更新全部分支版本号。
- 提供 `assert_structure_consistent(state)` 测试助手：任意操作后全部分支结构恒等。

## 8. 错误处理
- 所有函数返回 `Result<_, Error>`；错误映射 ErrorKind（模块 00 约定）。
- 命令 apply 内不做 IO/日志/时间（D16）——时间戳由调用方注入（`now_ms` 参数）。

## 9. 测试要点（对应 design-v3 §5）
- CORE-001 结构草稿发布后全分支结构恒等 ｜ CORE-002 新建分支继承 ｜ CORE-003 草稿隔离
- 校验器属性测试：随机非法输入不 panic、错误码正确
- diff 属性测试：old → diff → 应用 → 等于 new

## 10. 任务清单
□ 实体类型 + serde 序列化（对照 storage schema） □ Command/ApplyOutcome □ KV 键构造
□ Validator（类型/规则/引用/环/限额） □ compute_diff □ 结构一致性助手
□ 单元测试（§9） □ 错误类型统一（ErrorKind 全量） □ rustdoc 示例
