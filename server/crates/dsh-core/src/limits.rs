//! 限额（design-v2 §3.4 限额表，默认值）。
//! 注：当前未提供启动参数覆盖（O4 修正——原注释"均可在启动参数覆盖"不实）。
//! MAX_PROJECTS/MAX_BRANCHES_PER_PROJECT/MAX_GROUPS_PER_PROJECT/MAX_ITEMS_PER_PROJECT
//! 由状态机 apply 路径强制；CHECKPOINT_INTERVAL 为 diff 版本 checkpoint 的设计常量
//! （当前版本恒存 Full 快照，尚未启用）。

pub const MAX_PROJECTS: usize = 10_000;
pub const MAX_BRANCHES_PER_PROJECT: usize = 100;
pub const MAX_GROUPS_PER_PROJECT: usize = 500;
pub const MAX_ITEMS_PER_PROJECT: usize = 10_000;
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_ARRAY_ELEMENT_BYTES: usize = 8 * 1024;
pub const MAX_DIFF_BYTES: usize = 1024 * 1024;
pub const MAX_COMMENT_BYTES: usize = 500;
pub const MAX_KEY_BYTES: usize = 128;
pub const MAX_DESC_BYTES: usize = 200; // 描述字段（ItemDef/SharedItem 助记，不渲染）
pub const MAX_GROUP_NAME_BYTES: usize = 128;
pub const MAX_PROJECT_NAME_BYTES: usize = 128;
pub const CHECKPOINT_INTERVAL: u64 = 100; // 每 100 版写全量快照（D3）
