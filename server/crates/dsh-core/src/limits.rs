//! 限额（design-v2 §3.4 限额表，默认值；均可在启动参数覆盖）。

pub const MAX_PROJECTS: usize = 10_000;
pub const MAX_BRANCHES_PER_PROJECT: usize = 100;
pub const MAX_GROUPS_PER_PROJECT: usize = 500;
pub const MAX_ITEMS_PER_PROJECT: usize = 10_000;
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_ARRAY_ELEMENT_BYTES: usize = 8 * 1024;
pub const MAX_DIFF_BYTES: usize = 1024 * 1024;
pub const MAX_COMMENT_BYTES: usize = 500;
pub const MAX_KEY_BYTES: usize = 128;
pub const MAX_GROUP_NAME_BYTES: usize = 128;
pub const MAX_PROJECT_NAME_BYTES: usize = 128;
pub const CHECKPOINT_INTERVAL: u64 = 100; // 每 100 版写全量快照（D3）
