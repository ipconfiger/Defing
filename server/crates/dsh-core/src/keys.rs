//! KV 键构造（对齐 design-v2 §3.2 前缀布局，模块 01 §4）。

use crate::model::{BranchName, ProjectId};

pub const K_PROJECT: &str = "p/";
pub const K_STRUCT: &str = "/struct";
pub const K_STRUCT_DRAFT: &str = "/struct-draft";
pub const K_BRANCH: &str = "/b/";
pub const K_STATE: &str = "/state";
pub const K_VERSION: &str = "/v/";
pub const K_REF: &str = "/refs/";
pub const K_SHARED: &str = "sh/";
pub const K_SHARED_DRAFT: &str = "sh-draft/";
pub const K_SESSION: &str = "sess/admin";
/// 管理员密码哈希（set-password 落状态机，集群一致；登录时优先于节点配置校验）。
pub const K_ADMIN_PW: &str = "sess/admin-pw";
pub const K_AUDIT: &str = "audit/";
/// 审计 seq 计数键（位于 audit/ 前缀内；get_prefix 扫描时按 20 位数字后缀区分条目）。
pub const K_AUDIT_SEQ: &str = "audit/seq";
pub const K_IDX_PNAME: &str = "idx/pname/";
pub const K_IDX_REF: &str = "idx/ref/";
/// 组级引用反查索引：idx/refg/{shared_group}/{project}/{group} → "1"
pub const K_IDX_REFG: &str = "idx/refg/";

pub fn project_key(id: &ProjectId) -> String {
    format!("{K_PROJECT}{}", id.as_str())
}
pub fn struct_key(id: &ProjectId) -> String {
    format!("{K_PROJECT}{}{K_STRUCT}", id.as_str())
}
pub fn struct_draft_key(id: &ProjectId) -> String {
    format!("{K_PROJECT}{}{K_STRUCT_DRAFT}", id.as_str())
}
pub fn branch_state_key(id: &ProjectId, branch: &BranchName) -> String {
    format!(
        "{K_PROJECT}{}{K_BRANCH}{}{K_STATE}",
        id.as_str(),
        branch.as_str()
    )
}
pub fn version_key(id: &ProjectId, branch: &BranchName, no: u64) -> String {
    format!(
        "{K_PROJECT}{}{K_BRANCH}{}{K_VERSION}{no}",
        id.as_str(),
        branch.as_str()
    )
}
/// 版本值快照（M1：每版本存全量；M2 起按 checkpoint 规则存 diff）。
pub fn snapshot_key(id: &ProjectId, branch: &BranchName, no: u64) -> String {
    format!(
        "{K_PROJECT}{}{K_BRANCH}{}{K_VERSION}{no}/snap",
        id.as_str(),
        branch.as_str()
    )
}
pub fn branch_prefix(id: &ProjectId, branch: &BranchName) -> String {
    format!("{K_PROJECT}{}{K_BRANCH}{}", id.as_str(), branch.as_str())
}
pub fn ref_key(id: &ProjectId, group: &str, item_key: Option<&str>) -> String {
    match item_key {
        Some(k) => format!("{K_PROJECT}{}{K_REF}{group}/{k}", id.as_str()),
        None => format!("{K_PROJECT}{}{K_REF}{group}", id.as_str()),
    }
}
pub fn shared_key(group: &str, key: &str) -> String {
    format!("{K_SHARED}{group}/{key}")
}
/// 共享组前缀（组级引用按共享组扫描已发布项）。
pub fn shared_prefix(group: &str) -> String {
    format!("{K_SHARED}{group}/")
}
pub fn shared_draft_key(group: &str, key: &str) -> String {
    format!("{K_SHARED_DRAFT}{group}/{key}")
}
pub fn session_key() -> &'static str {
    K_SESSION
}
pub fn audit_key(seq: u64) -> String {
    format!("{K_AUDIT}{seq:020}")
}
pub fn idx_pname(name: &str) -> String {
    format!("{K_IDX_PNAME}{name}")
}
pub fn idx_ref(shared_group: &str, shared_key: &str) -> String {
    format!("{K_IDX_REF}{shared_group}/{shared_key}")
}
/// 组级引用反查索引（整组绑定共享组 SG）。
pub fn group_ref_index_key(shared_group: &str, project: &ProjectId, group: &str) -> String {
    format!("{K_IDX_REFG}{shared_group}/{}/{group}", project.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shapes() {
        let id: ProjectId = "order-service".into();
        let b: BranchName = "prod".into();
        assert_eq!(project_key(&id), "p/order-service");
        assert_eq!(struct_key(&id), "p/order-service/struct");
        assert_eq!(branch_state_key(&id, &b), "p/order-service/b/prod/state");
        assert_eq!(version_key(&id, &b, 12), "p/order-service/b/prod/v/12");
        assert_eq!(
            snapshot_key(&id, &b, 12),
            "p/order-service/b/prod/v/12/snap"
        );
        assert_eq!(shared_key("redis", "host"), "sh/redis/host");
        assert_eq!(idx_pname("order-service"), "idx/pname/order-service");
        assert_eq!(audit_key(7), "audit/00000000000000000007");
    }
}
