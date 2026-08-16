//! 确定性状态机（模块 01/04）：命令 apply + 读取。
//! 约定：apply 不读墙钟/不 IO/不日志（D16）；时间戳由调用方注入 now_ms。
//! M1 范围：项目/分支 CRUD、结构草稿与结构发布、值草稿、值发布、GetConfig（版本快照全量存储）。

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::command::Command;
use crate::diff::compute_diff;
use crate::error::{Error, ErrorKind};
use crate::keys::*;
use crate::limits::*;
use crate::model::*;
use crate::store::{KeyValuePairs, Store};
use crate::validator;

/// GetConfig 返回的配置快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub project: String,
    pub branch: String,
    pub version: u64,
    pub structure_version: u64,
    pub groups: BTreeMap<String, BTreeMap<String, Value>>,
}

/// 版本存储 checkpoint 间隔（perf 方案② D3）：每 N 版本存 full 快照，其余存 diff。
/// 与 design-modules/04-publish.md §8 一致；改小可降低重建成本但增加存储，改大反之。
pub const CHECKPOINT_INTERVAL: u64 = 100;

/// apply 结果：成功产出的事件列表（确定性副作用，供 watch 扇出）。
pub type ApplyOutcome = Result<Vec<PublishEvent>, Error>;

fn load<T: DeserializeOwned>(store: &dyn Store, key: &str) -> Result<Option<T>, Error> {
    match store.get(key.as_bytes())? {
        Some(raw) => serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| Error::internal(format!("corrupt value at {key}: {e}"))),
        None => Ok(None),
    }
}

fn save<T: Serialize>(store: &dyn Store, key: &str, value: &T) -> Result<(), Error> {
    let raw = serde_json::to_vec(value).map_err(|e| Error::internal(format!("serialize: {e}")))?;
    store.put(key.as_bytes(), &raw)
}

/// 项目名合法性（[a-z0-9][a-z0-9-]{0,127}）。
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PROJECT_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name.as_bytes()[0] != b'-'
        && name.as_bytes()[name.len() - 1] != b'-'
}

/// 分支名合法性（[a-z0-9][a-z0-9-]{0,63}）。
fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name.as_bytes()[0] != b'-'
        && name.as_bytes()[name.len() - 1] != b'-'
}

/// 命令级写缓冲操作（perf 方案①）：统一序列保证"最后一次操作决定"语义。
#[derive(Debug, Clone)]
enum PendingOp {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

/// 确定性状态机。
pub struct StateMachine {
    store: Box<dyn Store>,
    /// 命令级写缓冲（perf 方案①：apply 期间收集写操作，命令末统一 write_batch 单事务提交）。
    /// apply 开始清空、命令成功 flush、失败 abort。非 apply 路径（快照安装/后台任务）不使用。
    pending_ops: Vec<PendingOp>,
}

impl StateMachine {
    pub fn new(store: Box<dyn Store>) -> Self {
        Self {
            store,
            pending_ops: Vec::new(),
        }
    }

    // ---------------- 命令级写缓冲（perf 方案①） ----------------

    /// 写缓冲 put：apply 期间收集；无 pending（非 apply 路径）时直写 store。
    fn put_pending(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.pending_ops
            .push(PendingOp::Put(key.to_vec(), value.to_vec()));
        Ok(())
    }

    /// 写缓冲 delete：apply 期间收集。
    fn delete_pending(&mut self, key: &[u8]) -> Result<(), Error> {
        self.pending_ops.push(PendingOp::Delete(key.to_vec()));
        Ok(())
    }

    /// 读合并 get：pending 逆序找 key（最后一次操作决定），miss 走 store。
    fn get_merged(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        for op in self.pending_ops.iter().rev() {
            match op {
                PendingOp::Put(k, v) if k.as_slice() == key => return Ok(Some(v.clone())),
                PendingOp::Delete(k) if k.as_slice() == key => return Ok(None),
                _ => {}
            }
        }
        self.store.get(key)
    }

    /// 读合并 get_prefix：store 结果 + pending 操作（按序应用），BTreeMap 保字典序。
    fn get_prefix_merged(&self, prefix: &[u8]) -> Result<KeyValuePairs, Error> {
        let mut out: std::collections::BTreeMap<Vec<u8>, Vec<u8>> =
            self.store.get_prefix(prefix)?.into_iter().collect();
        for op in &self.pending_ops {
            match op {
                PendingOp::Put(k, v) => {
                    if k.starts_with(prefix) {
                        out.insert(k.clone(), v.clone());
                    }
                }
                PendingOp::Delete(k) => {
                    if k.starts_with(prefix) {
                        out.remove(k);
                    }
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    /// 命令末统一落盘：单事务 write_batch（puts + deletes）。
    fn flush_pending(&mut self) -> Result<(), Error> {
        if self.pending_ops.is_empty() {
            return Ok(());
        }
        // 操作序列 → puts/deletes（写缓冲内允许同 key 多操作，write_batch 先删后插自洽）
        let ops = std::mem::take(&mut self.pending_ops);
        let mut puts = Vec::new();
        let mut deletes = Vec::new();
        for op in ops {
            match op {
                PendingOp::Put(k, v) => puts.push((k, v)),
                PendingOp::Delete(k) => deletes.push(k),
            }
        }
        self.store.write_batch(&puts, &deletes)
    }

    /// 命令内读（写后读可见：pending 优先）——apply 路径统一入口。
    fn load_merged<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, Error> {
        match self.get_merged(key.as_bytes())? {
            Some(raw) => serde_json::from_slice(&raw)
                .map(Some)
                .map_err(|e| Error::internal(format!("corrupt value at {key}: {e}"))),
            None => Ok(None),
        }
    }

    /// 命令内写（写缓冲）——apply 路径统一入口。
    fn save_pending<T: Serialize>(&mut self, key: &str, value: &T) -> Result<(), Error> {
        let raw =
            serde_json::to_vec(value).map_err(|e| Error::internal(format!("serialize: {e}")))?;
        self.put_pending(key.as_bytes(), &raw)
    }

    // ---------------- 读取 ----------------

    pub fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, Error> {
        self.load_merged(&project_key(id))
    }

    pub fn list_projects(&self) -> Result<Vec<Project>, Error> {
        let rows = self.get_prefix_merged(b"p/")?;
        let mut out = Vec::new();
        for (k, v) in rows {
            let ks = String::from_utf8_lossy(&k);
            let rest = &ks[K_PROJECT.len()..];
            if rest.contains('/') {
                continue; // 子键（struct/branch/...）跳过
            }
            if let Ok(p) = serde_json::from_slice::<Project>(&v) {
                out.push(p);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn get_structure(&self, id: &ProjectId) -> Result<Option<Structure>, Error> {
        self.load_merged(&struct_key(id))
    }

    pub fn get_structure_draft(&self, id: &ProjectId) -> Result<Option<StructureDraft>, Error> {
        self.load_merged(&struct_draft_key(id))
    }

    pub fn get_branch_state(
        &self,
        id: &ProjectId,
        branch: &BranchName,
    ) -> Result<Option<BranchState>, Error> {
        self.load_merged(&branch_state_key(id, branch))
    }

    /// 读取当前活动会话（I7；无会话返回 None）。
    pub fn get_session(&self) -> Result<Option<AdminSession>, Error> {
        self.load_merged(session_key())
    }

    /// 审计查询：按 action 过滤、since（ts ≥ since，墙钟 ms）过滤、按 seq 倒序、limit 截断。
    pub fn get_audit(
        &self,
        action: Option<&str>,
        project: Option<&str>,
        since: Option<i64>,
        limit: usize,
    ) -> Result<Vec<AuditEntry>, Error> {
        let rows = self.get_prefix_merged(K_AUDIT.as_bytes())?;
        let mut out = Vec::new();
        for (k, v) in rows {
            let ks = String::from_utf8_lossy(&k);
            let Some(rest) = ks.strip_prefix(K_AUDIT) else {
                continue;
            };
            // 跳过计数键 "seq"（非 20 位数字后缀）
            if rest.parse::<u64>().is_err() {
                continue;
            }
            if let Ok(e) = serde_json::from_slice::<AuditEntry>(&v) {
                if let Some(a) = action {
                    if e.action != a {
                        continue;
                    }
                }
                if let Some(p) = project {
                    if e.project.as_deref() != Some(p) {
                        continue;
                    }
                }
                if let Some(s) = since {
                    if e.ts < s {
                        continue;
                    }
                }
                out.push(e);
            }
        }
        out.sort_by_key(|b| std::cmp::Reverse(b.seq)); // 新 → 旧
        if out.len() > limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    /// 审计保留：仅保留最近 keep 条（后台任务用；keep=0 清空全部）。
    pub fn prune_audit(&self, keep: usize) -> Result<usize, Error> {
        let rows = self.get_prefix_merged(K_AUDIT.as_bytes())?;
        let mut seqs: Vec<u64> = Vec::new();
        for (k, _) in rows {
            let ks = String::from_utf8_lossy(&k);
            if let Some(rest) = ks.strip_prefix(K_AUDIT) {
                if let Ok(seq) = rest.parse::<u64>() {
                    seqs.push(seq);
                }
            }
        }
        seqs.sort_unstable();
        let total = seqs.len();
        if total <= keep {
            return Ok(0);
        }
        let mut removed = 0;
        for seq in seqs.into_iter().take(total - keep) {
            self.store.delete(audit_key(seq).as_bytes())?;
            removed += 1;
        }
        Ok(removed)
    }

    /// DEK 重包（B6）：扫描全部存储中的 secret 密文，用 `f` 逐个重写（轮换后台任务用）。
    /// `f` 返回 None = 跳过（如代际已最新）；返回 Some(新密文) = 写回。
    /// 覆盖：版本快照（…/snap）、版本 diff（…/diff，perf 方案② D3）、
    /// 共享项（sh/、sh-draft/）、分支草稿（…/b/{branch}/state）。
    pub fn rewrap_deks(
        &self,
        f: &dyn Fn(&Ciphertext) -> Option<Result<Ciphertext, Error>>,
    ) -> Result<usize, Error> {
        let rows = self.get_prefix_merged(b"")?;
        let mut rewrapped = 0usize;
        for (k, v) in rows {
            let ks = String::from_utf8_lossy(&k);
            let key = ks.as_ref();
            if key.ends_with("/snap") {
                let mut snap: SnapshotMap = match serde_json::from_slice(&v) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if Self::rewrap_snapshot(&mut snap, f)? {
                    save(&*self.store, key, &snap)?;
                    rewrapped += 1;
                }
            } else if key.ends_with("/diff") {
                // perf 方案② D3：diff 中 Upsert 条目的 new_value 可能含 Secret 密文
                let mut diff: Vec<DiffEntry> = match serde_json::from_slice(&v) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                let mut changed = false;
                for entry in diff.iter_mut() {
                    if let ChangeKind::Upsert = entry.kind {
                        if let Some(nv) = &mut entry.new_value {
                            if Self::rewrap_value(nv, f)? {
                                changed = true;
                            }
                        }
                    }
                    // Delete 条目 new_value=None，天然跳过
                }
                if changed {
                    save(&*self.store, key, &diff)?;
                    rewrapped += 1;
                }
            } else if key.starts_with(K_SHARED) || key.starts_with(K_SHARED_DRAFT) {
                let mut item: SharedItem = match serde_json::from_slice(&v) {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                if Self::rewrap_value(&mut item.value, f)? {
                    save(&*self.store, key, &item)?;
                    rewrapped += 1;
                }
            } else if let Some(rest) = key.strip_prefix(K_PROJECT) {
                // p/{pid}/b/{branch}/state —— 草稿值
                if rest.contains(K_BRANCH) && key.ends_with(K_STATE) {
                    let mut st: BranchState = match serde_json::from_slice(&v) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let mut changed = false;
                    for items in st.value_draft.values_mut() {
                        for dv in items.values_mut() {
                            if Self::rewrap_value(&mut dv.value, f)? {
                                changed = true;
                            }
                        }
                    }
                    if changed {
                        save(&*self.store, key, &st)?;
                        rewrapped += 1;
                    }
                }
            }
        }
        Ok(rewrapped)
    }

    fn rewrap_snapshot(
        snap: &mut SnapshotMap,
        f: &dyn Fn(&Ciphertext) -> Option<Result<Ciphertext, Error>>,
    ) -> Result<bool, Error> {
        let mut changed = false;
        for items in snap.values_mut() {
            for v in items.values_mut() {
                if Self::rewrap_value(v, f)? {
                    changed = true;
                }
            }
        }
        Ok(changed)
    }

    fn rewrap_value(
        v: &mut Value,
        f: &dyn Fn(&Ciphertext) -> Option<Result<Ciphertext, Error>>,
    ) -> Result<bool, Error> {
        if let Value::Secret(ct) = v {
            if let Some(res) = f(ct) {
                *v = Value::Secret(res?);
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub fn list_branches(&self, id: &ProjectId) -> Result<Vec<BranchName>, Error> {
        let prefix = format!("{K_PROJECT}{}{K_BRANCH}", id.as_str());
        let rows = self.get_prefix_merged(prefix.as_bytes())?;
        let mut out = Vec::new();
        for (k, _) in rows {
            let ks = String::from_utf8_lossy(&k);
            let rest = &ks[prefix.len()..];
            if let Some(pos) = rest.find('/') {
                let name = &rest[..pos];
                if !name.is_empty() {
                    out.push(BranchName(name.to_string()));
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// 读取某版本的值快照（perf 方案② D3：checkpoint 版本存 full，其余存 diff，读时重建）。
    /// 定位最近 checkpoint（含自身）作为基座，从基座 + 1 应用到目标版本。
    pub fn snapshot_of(
        &self,
        id: &ProjectId,
        branch: &BranchName,
        version: u64,
    ) -> Result<SnapshotMap, Error> {
        // 边界：version 必须 ≥1（调用方保证：get_config 对 version=0 解析 active_version）
        if version == 0 {
            return Err(Error::not_found(format!("version 0 of {branch}")));
        }
        // 最近 checkpoint 基座（向下取整；v=1 恒 full）
        let start = if version.is_multiple_of(CHECKPOINT_INTERVAL) {
            version // 自身即 checkpoint：直接读 full，0 个 diff 应用
        } else {
            let base = ((version - 1) / CHECKPOINT_INTERVAL) * CHECKPOINT_INTERVAL;
            if base == 0 {
                1
            } else {
                base
            }
        };
        let base_key = snapshot_key(id, branch, start);
        let mut snap: SnapshotMap = match self.get_merged(base_key.as_bytes())? {
            Some(raw) => serde_json::from_slice(&raw)
                .map_err(|e| Error::internal(format!("corrupt snapshot {base_key}: {e}")))?,
            None => {
                // 兼容旧数据/裁剪后基座缺失：退化直读目标版本（旧版全量存储或兜底）
                let fallback = snapshot_key(id, branch, version);
                return match self.get_merged(fallback.as_bytes())? {
                    Some(raw) => serde_json::from_slice(&raw)
                        .map_err(|e| Error::internal(format!("corrupt snapshot {fallback}: {e}"))),
                    None => Err(Error::not_found(format!("version {version} of {branch}"))),
                };
            }
        };
        for v in (start + 1)..=version {
            if v % CHECKPOINT_INTERVAL == 0 {
                // checkpoint 版本存 full：直接替换基座
                let cp_key = snapshot_key(id, branch, v);
                snap = match self.get_merged(cp_key.as_bytes())? {
                    Some(raw) => serde_json::from_slice(&raw)
                        .map_err(|e| Error::internal(format!("corrupt snapshot {cp_key}: {e}")))?,
                    None => {
                        return Err(Error::not_found(format!("snapshot {v} of {branch}")));
                    }
                };
            } else {
                let dk = diff_key(id, branch, v);
                let diff: Vec<DiffEntry> = match self.get_merged(dk.as_bytes())? {
                    Some(raw) => serde_json::from_slice(&raw)
                        .map_err(|e| Error::internal(format!("corrupt diff {dk}: {e}")))?,
                    None => {
                        // 旧版本（升级前全量存储）无 diff_key：退化直读目标版本全量
                        let fallback = snapshot_key(id, branch, v);
                        return match self.get_merged(fallback.as_bytes())? {
                            Some(raw) => serde_json::from_slice(&raw).map_err(|e| {
                                Error::internal(format!("corrupt snapshot {fallback}: {e}"))
                            }),
                            None => Err(Error::not_found(format!("version {v} of {branch}"))),
                        };
                    }
                };
                Self::apply_diff(&mut snap, &diff);
            }
        }
        Ok(snap)
    }

    /// 应用 diff 到快照（确定性：BTreeMap 有序；Upsert 写、Delete 删）。
    /// Delete 删除 item 后若组变空则移除组（与全量快照的空组语义一致，避免残留空组）。
    fn apply_diff(snap: &mut SnapshotMap, diff: &[DiffEntry]) {
        for entry in diff {
            match entry.kind {
                ChangeKind::Upsert => {
                    if let Some(v) = &entry.new_value {
                        snap.entry(entry.group.clone())
                            .or_default()
                            .insert(entry.key.clone(), v.clone());
                    }
                }
                ChangeKind::Delete => {
                    if let Some(items) = snap.get_mut(&entry.group) {
                        items.remove(&entry.key);
                        if items.is_empty() {
                            snap.remove(&entry.group);
                        }
                    }
                }
            }
        }
    }

    /// 写版本快照（perf 方案② D3）：checkpoint（每 100 或首次）存 full，其余存 diff。
    /// 需同时传入 old 快照（compute_diff 的输入）；`record.kind` 会被覆写为 Full/Diff。
    fn write_version_snapshot(
        &mut self,
        id: &ProjectId,
        branch: &BranchName,
        vno: u64,
        old: &SnapshotMap,
        new: &SnapshotMap,
        record: &mut VersionRecord,
    ) -> Result<(), Error> {
        let is_checkpoint = vno == 1 || vno.is_multiple_of(CHECKPOINT_INTERVAL);
        if is_checkpoint {
            record.kind = VersionKind::Full;
            self.save_pending(&snapshot_key(id, branch, vno), new)?;
        } else {
            record.kind = VersionKind::Diff;
            let diff = compute_diff(old, new);
            self.save_pending(&diff_key(id, branch, vno), &diff)?;
        }
        self.save_pending(&version_key(id, branch, vno), record)
    }

    pub fn get_version_record(
        &self,
        id: &ProjectId,
        branch: &BranchName,
        no: u64,
    ) -> Result<Option<VersionRecord>, Error> {
        self.load_merged(&version_key(id, branch, no))
    }

    pub fn version_history(
        &self,
        id: &ProjectId,
        branch: &BranchName,
    ) -> Result<Vec<VersionRecord>, Error> {
        let prefix = format!(
            "{K_PROJECT}{}{K_BRANCH}{}{K_VERSION}",
            id.as_str(),
            branch.as_str()
        );
        let rows = self.get_prefix_merged(prefix.as_bytes())?;
        let mut out = Vec::new();
        for (k, v) in rows {
            let ks = String::from_utf8_lossy(&k);
            // 跳过快照与 diff 后缀（perf 方案② D3：snap/diff 与 version 同前缀）
            if ks.ends_with("/snap") || ks.ends_with("/diff") {
                continue;
            }
            if let Ok(r) = serde_json::from_slice::<VersionRecord>(&v) {
                out.push(r);
            }
        }
        out.sort_by_key(|r| r.no);
        Ok(out)
    }

    /// 导出全部状态（快照构建用）。
    pub fn dump_all(&self) -> Result<crate::store::KeyValuePairs, Error> {
        self.get_prefix_merged(b"")
    }

    /// 清空并恢复全部状态（快照安装用）。
    pub fn restore_all(&self, pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<(), Error> {
        for (k, _) in self.get_prefix_merged(b"")? {
            self.store.delete(&k)?;
        }
        for (k, v) in pairs {
            self.store.put(k, v)?;
        }
        Ok(())
    }

    /// 版本裁剪：保留活动版本 + 最近 keep 个版本，删除更早的历史（后台任务用）。
    /// 版本裁剪：保留活动版本 + 最近 keep 个版本，删除更早的历史（后台任务用）。
    /// perf 方案② D3：同时删除 diff_key；且删除下限对齐到"最近保留 checkpoint 之前"——
    /// 保证最新保留版本是 checkpoint（full 基座），其后的 diff 链可完整重建。
    pub fn prune_versions(
        &self,
        project: &ProjectId,
        branch: &BranchName,
        keep: usize,
    ) -> Result<usize, Error> {
        let st = self
            .get_branch_state(project, branch)?
            .ok_or_else(|| Error::not_found(format!("branch {branch}")))?;
        let hist = self.version_history(project, branch)?; // 升序
        let total = hist.len();
        if total <= keep {
            return Ok(0);
        }
        // 目标：保留最近 keep 个版本。若裁剪导致最新保留版本不是 checkpoint，
        // 则额外保留其 checkpoint 基座（否则 diff 链断裂、历史全部不可读）。
        // 最新保留版本号 = total - keep（1-based 第 total-keep 个）；其基座 = 该版本向下取整到 checkpoint。
        let newest_kept_no = hist[total - keep - 1].no;
        let mut keep_from = newest_kept_no; // 语义上保留 >= keep_from 的版本
        if !newest_kept_no.is_multiple_of(CHECKPOINT_INTERVAL) && newest_kept_no != 1 {
            // 向下对齐到最近 checkpoint（含）——额外保留基座
            keep_from = ((newest_kept_no - 1) / CHECKPOINT_INTERVAL) * CHECKPOINT_INTERVAL;
            if keep_from == 0 {
                keep_from = 1;
            }
        }
        let mut removed = 0;
        for rec in hist.iter().take(total) {
            let no = rec.no;
            if no >= keep_from || no == st.active_version {
                continue; // 保留区间或活动版本
            }
            self.store
                .delete(version_key(project, branch, no).as_bytes())?;
            // 该版本可能存 full（checkpoint）或 diff——两个 key 都尝试删除（幂等）
            self.store
                .delete(snapshot_key(project, branch, no).as_bytes())?;
            self.store
                .delete(diff_key(project, branch, no).as_bytes())?;
            removed += 1;
        }
        Ok(removed)
    }

    /// GetConfig：version=0 取活动版本。
    pub fn get_config(
        &self,
        id: &ProjectId,
        branch: &BranchName,
        version: u64,
    ) -> Result<ConfigSnapshot, Error> {
        let st = self
            .get_branch_state(id, branch)?
            .ok_or_else(|| Error::not_found(format!("branch {branch} of {id}")))?;
        let vno = if version == 0 {
            st.active_version
        } else {
            version
        };
        if vno == 0 {
            return Err(Error::not_found("no published version yet"));
        }
        let snap = self.snapshot_of(id, branch, vno)?;
        let structure = self.get_structure(id)?.unwrap_or(Structure {
            version: 0,
            groups: vec![],
        });
        Ok(ConfigSnapshot {
            project: id.to_string(),
            branch: branch.to_string(),
            version: vno,
            structure_version: structure.version,
            groups: snap,
        })
    }

    // ---------------- apply ----------------

    /// 命令载荷墙钟（API 层注入）；0 = 回退 apply 的 now_ms 参数（旧日志重放兼容）。
    fn eff_ts(ts: &i64, fallback: i64) -> i64 {
        if *ts > 0 {
            *ts
        } else {
            fallback
        }
    }

    /// 应用命令（perf 方案①）：命令级写缓冲——apply 内多次写合并为一次 write_batch 单事务。
    /// 失败时 abort（pending 清空，无部分写）；成功时 flush（一次 fsync）。
    pub fn apply(&mut self, cmd: &Command, now_ms: i64) -> ApplyOutcome {
        self.pending_ops.clear();
        let result = self.apply_inner(cmd, now_ms);
        match result {
            Ok(events) => {
                if let Err(e) = self.flush_pending() {
                    self.pending_ops.clear();
                    return Err(e);
                }
                Ok(events)
            }
            Err(e) => {
                // abort：丢弃未提交写（命令失败无部分生效，语义优于旧逐事务提交）
                self.pending_ops.clear();
                Err(e)
            }
        }
    }

    fn apply_inner(&mut self, cmd: &Command, now_ms: i64) -> ApplyOutcome {
        match cmd {
            Command::ProjectCreate { name, operator, ts } => {
                self.apply_project_create(name, Self::eff_ts(ts, now_ms), operator)
            }
            Command::ProjectDelete { id, operator } => self.apply_project_delete(id, operator),
            Command::BranchCreate {
                project,
                name,
                source,
                operator,
                ts,
            } => self.apply_branch_create(
                project,
                name,
                source.as_ref(),
                Self::eff_ts(ts, now_ms),
                operator,
            ),
            Command::BranchDelete {
                project,
                name,
                operator,
            } => self.apply_branch_delete(project, name, operator),
            Command::StructureDraftSet {
                project,
                base_version,
                groups,
                operator,
            } => self.apply_structure_draft_set(project, *base_version, groups, operator),
            Command::PublishStructure {
                project,
                comment,
                request_id,
                operator,
                ts,
            } => self.apply_publish_structure(
                project,
                comment,
                request_id,
                Self::eff_ts(ts, now_ms),
                operator,
            ),
            Command::DraftUpdate {
                project,
                branch,
                updates,
                deletes,
                operator,
                ts,
            } => self.apply_draft_update(
                project,
                branch,
                updates,
                deletes,
                Self::eff_ts(ts, now_ms),
                operator,
            ),
            Command::Publish {
                project,
                branch,
                comment,
                request_id,
                operator,
                ts,
            } => self.apply_publish(
                project,
                branch,
                comment,
                request_id,
                Self::eff_ts(ts, now_ms),
                operator,
            ),
            Command::Rollback {
                project,
                branch,
                to_version,
                comment,
                request_id,
                operator,
                ts,
            } => self.apply_rollback(
                project,
                branch,
                *to_version,
                comment,
                request_id,
                Self::eff_ts(ts, now_ms),
                operator,
            ),
            Command::SharedDraftUpdate { item, operator } => {
                self.apply_shared_draft_update(item, operator)
            }
            Command::SharedPublish {
                comment,
                request_id,
                operator,
                ts,
            } => self.apply_shared_publish(comment, request_id, Self::eff_ts(ts, now_ms), operator),
            Command::RefBind {
                project,
                binding,
                operator,
            } => self.apply_ref_bind(project, binding, operator),
            Command::RefUnbind {
                project,
                group,
                item_key,
                operator,
            } => self.apply_ref_unbind(project, group, item_key.as_deref(), operator),
            Command::SessionLogin {
                token_hash,
                issued_at,
                expires_at,
            } => self.apply_session_login(token_hash, *issued_at, *expires_at),
            Command::SessionLogout => self.apply_session_logout(),
            Command::SessionHeartbeat { expires_at } => self.apply_session_heartbeat(*expires_at),
            Command::ProjectAdminCreate {
                project,
                username,
                salt,
                password_hash,
                ts,
            } => self.apply_project_admin_create(
                project,
                username,
                salt,
                password_hash,
                Self::eff_ts(ts, now_ms),
            ),
            Command::ProjectAdminDelete { username } => self.apply_project_admin_delete(username),
            Command::ProjectAdminSetPassword {
                username,
                salt,
                password_hash,
            } => self.apply_project_admin_set_password(username, salt, password_hash),
            Command::PaSessionLogin {
                username,
                token_hash,
                issued_at,
                expires_at,
                device_id,
            } => self.apply_pa_session_login(
                username,
                token_hash,
                *issued_at,
                *expires_at,
                device_id,
            ),
            Command::PaSessionLogout { username } => self.apply_pa_session_logout(username),
            Command::PaSessionHeartbeat {
                username,
                expires_at,
            } => self.apply_pa_session_heartbeat(username, *expires_at),
            Command::AdminSetPassword { password_hash } => {
                self.apply_admin_set_password(password_hash)
            }
            Command::AuditAppend { entry } => self.apply_audit_append(entry),
            Command::RotateMasterKey { .. } => self.apply_rotate_master_key(),
        }
    }

    fn apply_project_create(&mut self, name: &str, now_ms: i64, _operator: &str) -> ApplyOutcome {
        if !valid_name(name) {
            return Err(Error::validation(format!("invalid project name: {name:?}")));
        }
        // N2：限额表 MAX_PROJECTS 强制（此前为死常量，未实施）
        if self.list_projects()?.len() >= MAX_PROJECTS {
            return Err(Error::limit_exceeded("too many projects"));
        }
        let id = ProjectId(name.to_string());
        if self.get_project(&id)?.is_some() {
            return Err(Error::conflict(format!("project {name} already exists")));
        }
        let project = Project {
            id: id.clone(),
            name: name.to_string(),
            created_at: now_ms,
        };
        let structure = Structure {
            version: 1,
            groups: vec![],
        };
        self.save_pending(&project_key(&id), &project)?;
        self.save_pending(&idx_pname(name), &"1")?;
        self.save_pending(&struct_key(&id), &structure)?;
        for default_branch in [BranchName::DEV, BranchName::TEST, BranchName::PROD] {
            self.save_pending(
                &branch_state_key(&id, &BranchName(default_branch.to_string())),
                &BranchState::new(1),
            )?;
        }
        Ok(vec![])
    }

    fn apply_project_delete(&mut self, id: &ProjectId, _operator: &str) -> ApplyOutcome {
        let project = self
            .get_project(id)?
            .ok_or_else(|| Error::not_found(format!("project {id}")))?;
        let prefix = project_key(id);
        for (k, _) in self.get_prefix_merged(prefix.as_bytes())? {
            self.delete_pending(&k)?;
        }
        self.delete_pending(idx_pname(&project.name).as_bytes())?;
        // 级联删除该项目全部项目管理员账号及其会话（设计 §5）
        for acct in self.list_project_admins(&id.0)? {
            self.store
                .delete(pa_session_key(&acct.username).as_bytes())?;
            self.store
                .delete(project_admin_key(&acct.username).as_bytes())?;
        }
        // N1：清理孤儿全局引用索引（共享项发布级联扫描会命中已删项目，索引脏数据需一并清除）。
        // 共享 group/key 与项目名/组名均受 valid_key_name 字符集约束（无 `/`），按 `/` 切分可靠。
        // idx/ref/{sg}/{sk}/{project}/{group}/{item_key} → 第 3 段（index 2）为 project
        for (k, _) in self.get_prefix_merged(K_IDX_REF.as_bytes())? {
            let ks = String::from_utf8_lossy(&k);
            if let Some(rest) = ks.strip_prefix(K_IDX_REF) {
                let parts: Vec<&str> = rest.split('/').collect();
                if parts.len() == 5 && parts[2] == id.as_str() {
                    self.delete_pending(&k)?;
                }
            }
        }
        // idx/refg/{sg}/{project}/{group} → 第 2 段（index 1）为 project
        for (k, _) in self.get_prefix_merged(K_IDX_REFG.as_bytes())? {
            let ks = String::from_utf8_lossy(&k);
            if let Some(rest) = ks.strip_prefix(K_IDX_REFG) {
                let parts: Vec<&str> = rest.split('/').collect();
                if parts.len() == 3 && parts[1] == id.as_str() {
                    self.delete_pending(&k)?;
                }
            }
        }
        Ok(vec![])
    }

    fn apply_branch_create(
        &mut self,
        id: &ProjectId,
        name: &BranchName,
        source: Option<&BranchName>,
        now_ms: i64,
        _operator: &str,
    ) -> ApplyOutcome {
        if !valid_branch(name.as_str()) {
            return Err(Error::validation(format!("invalid branch name: {name:?}")));
        }
        self.get_project(id)?
            .ok_or_else(|| Error::not_found(format!("project {id}")))?;
        if self.get_branch_state(id, name)?.is_some() {
            return Err(Error::conflict(format!("branch {name} exists")));
        }
        let branches = self.list_branches(id)?;
        if branches.len() >= MAX_BRANCHES_PER_PROJECT {
            return Err(Error::limit_exceeded("too many branches"));
        }
        let structure = self.get_structure(id)?.unwrap_or(Structure {
            version: 1,
            groups: vec![],
        });
        let mut state = BranchState::new(structure.version);
        if let Some(src) = source {
            let src_state = self
                .get_branch_state(id, src)?
                .ok_or_else(|| Error::validation(format!("source branch {src} not found")))?;
            if src_state.active_version == 0 {
                return Err(Error::validation(format!(
                    "source branch {src} has no published version"
                )));
            }
            let snap = self.snapshot_of(id, src, src_state.active_version)?;
            state.value_draft = snap
                .into_iter()
                .map(|(g, items)| {
                    let m = items
                        .into_iter()
                        .map(|(k, v)| {
                            (
                                k,
                                DraftValue {
                                    value: v,
                                    updated_at: now_ms,
                                },
                            )
                        })
                        .collect();
                    (g, m)
                })
                .collect();
        }
        self.save_pending(&branch_state_key(id, name), &state)?;
        Ok(vec![])
    }

    fn apply_branch_delete(
        &mut self,
        id: &ProjectId,
        name: &BranchName,
        _operator: &str,
    ) -> ApplyOutcome {
        let st = self
            .get_branch_state(id, name)?
            .ok_or_else(|| Error::not_found(format!("branch {name} of {id}")))?;
        if st.active_version > 0 || !st.value_draft.is_empty() {
            return Err(Error::conflict(
                "branch has published versions or pending draft",
            ));
        }
        let prefix = branch_prefix(id, name);
        for (k, _) in self.get_prefix_merged(prefix.as_bytes())? {
            self.delete_pending(&k)?;
        }
        Ok(vec![])
    }

    fn apply_structure_draft_set(
        &mut self,
        id: &ProjectId,
        base_version: u64,
        groups: &[GroupDef],
        _operator: &str,
    ) -> ApplyOutcome {
        let structure = self
            .get_structure(id)?
            .ok_or_else(|| Error::not_found(format!("project {id}")))?;
        if base_version != structure.version {
            return Err(Error::conflict(format!(
                "base_version {base_version} != current structure version {}",
                structure.version
            )));
        }
        let draft_structure = Structure {
            version: base_version,
            groups: groups.to_vec(),
        };
        let errs = validator::validate_structure(&draft_structure);
        if !errs.is_empty() {
            return Err(Error::publish_blocked(
                serde_json::json!({ "errors": errs }),
            ));
        }
        let draft = StructureDraft {
            base_version,
            groups: groups.to_vec(),
        };
        self.save_pending(&struct_draft_key(id), &draft)?;
        Ok(vec![])
    }

    fn apply_publish_structure(
        &mut self,
        id: &ProjectId,
        comment: &str,
        request_id: &str,
        now_ms: i64,
        operator: &str,
    ) -> ApplyOutcome {
        let structure = self
            .get_structure(id)?
            .ok_or_else(|| Error::not_found(format!("project {id}")))?;
        let draft = self
            .get_structure_draft(id)?
            .ok_or_else(|| Error::new(ErrorKind::NoDraft, "no structure draft"))?;
        if draft.base_version != structure.version {
            return Err(Error::conflict("structure draft base_version mismatch"));
        }
        let draft_structure = Structure {
            version: structure.version,
            groups: draft.groups.clone(),
        };
        let errs = validator::validate_structure(&draft_structure);
        if !errs.is_empty() {
            return Err(Error::publish_blocked(
                serde_json::json!({ "errors": errs }),
            ));
        }
        let new_structure = Structure {
            version: structure.version + 1,
            groups: draft.groups.clone(),
        };
        let mut events = Vec::new();
        let branches = self.list_branches(id)?;
        for branch in &branches {
            let mut st = self
                .get_branch_state(id, branch)?
                .ok_or_else(|| Error::internal("branch state missing"))?;
            let vno = st.active_version + 1;
            // 结构发布：值不变（D14 只清理被删 item 的草稿值）
            let values = if st.active_version == 0 {
                SnapshotMap::new()
            } else {
                self.snapshot_of(id, branch, st.active_version)?
            };
            let mut record = VersionRecord {
                no: vno,
                structure_version: new_structure.version,
                created_at: now_ms,
                operator: Self::operator_id(operator),
                comment: comment.to_string(),
                rollback_of: None,
                kind: VersionKind::Full,
                snapshot_ref: None,
                diff_ref: None,
                event_ty: Some(EventType::StructurePublish),
            };
            // 结构发布值不变：old==values==new → diff 恒空（checkpoint 规则仍按 vno）
            self.write_version_snapshot(id, branch, vno, &values, &values, &mut record)?;
            st.active_version = vno;
            st.structure_version = new_structure.version;
            // D14：清理结构发布后不存在的 item 草稿值
            let mut known: BTreeMap<String, BTreeMap<String, ()>> = BTreeMap::new();
            for g in &new_structure.groups {
                for item in &g.items {
                    known
                        .entry(g.name.clone())
                        .or_default()
                        .insert(item.key.clone(), ());
                }
            }
            st.value_draft.retain(|g, items| {
                known.contains_key(g) && {
                    items.retain(|k, _| known[g].contains_key(k));
                    !items.is_empty()
                }
            });
            self.save_pending(&branch_state_key(id, branch), &st)?;
            events.push(PublishEvent {
                project: id.clone(),
                branch: branch.clone(),
                version: vno,
                ty: EventType::StructurePublish,
                structure_version: new_structure.version,
                comment: comment.to_string(),
                request_id: request_id.to_string(),
                changes: vec![],
            });
        }
        self.save_pending(&struct_key(id), &new_structure)?;
        self.delete_pending(struct_draft_key(id).as_bytes())?;
        Ok(events)
    }

    fn apply_draft_update(
        &mut self,
        id: &ProjectId,
        branch: &BranchName,
        updates: &[crate::command::DraftUpdateItem],
        deletes: &[(String, String)],
        now_ms: i64,
        _operator: &str,
    ) -> ApplyOutcome {
        let mut st = self
            .get_branch_state(id, branch)?
            .ok_or_else(|| Error::not_found(format!("branch {branch} of {id}")))?;
        let structure = self
            .get_structure(id)?
            .ok_or_else(|| Error::not_found(format!("project {id}")))?;

        // 建立 group → item 定义索引
        let mut index: BTreeMap<String, BTreeMap<String, &ItemDef>> = BTreeMap::new();
        for g in &structure.groups {
            for item in &g.items {
                index
                    .entry(g.name.clone())
                    .or_default()
                    .insert(item.key.clone(), item);
            }
        }

        let mut total = st.value_draft.values().map(|m| m.len()).sum::<usize>();
        for u in updates {
            let def = index
                .get(&u.group)
                .and_then(|m| m.get(&u.key))
                .ok_or_else(|| Error::validation(format!("unknown item {}/{}", u.group, u.key)))?;
            let errs = validator::validate_value(def, &u.value);
            if !errs.is_empty() {
                return Err(Error::validation(errs.join("; ")));
            }
            let size = serde_json::to_vec(&u.value)
                .map_err(|e| Error::internal(format!("serialize value: {e}")))?
                .len();
            if size > MAX_VALUE_BYTES {
                return Err(Error::limit_exceeded("value too large"));
            }
            let fresh = !st
                .value_draft
                .get(&u.group)
                .is_some_and(|m| m.contains_key(&u.key));
            if fresh {
                total += 1;
                if total > MAX_ITEMS_PER_PROJECT {
                    return Err(Error::limit_exceeded("too many draft items"));
                }
            }
        }
        for (g, key) in deletes {
            if let Some(m) = st.value_draft.get_mut(g) {
                m.remove(key);
            }
        }
        for u in updates {
            st.value_draft.entry(u.group.clone()).or_default().insert(
                u.key.clone(),
                DraftValue {
                    value: u.value.clone(),
                    updated_at: now_ms,
                },
            );
        }
        self.save_pending(&branch_state_key(id, branch), &st)?;
        Ok(vec![])
    }

    fn apply_publish(
        &mut self,
        id: &ProjectId,
        branch: &BranchName,
        comment: &str,
        request_id: &str,
        now_ms: i64,
        operator: &str,
    ) -> ApplyOutcome {
        let mut st = self
            .get_branch_state(id, branch)?
            .ok_or_else(|| Error::not_found(format!("branch {branch} of {id}")))?;
        let structure = self
            .get_structure(id)?
            .ok_or_else(|| Error::not_found(format!("project {id}")))?;

        // 幂等（I10）：同 request_id 直接返回（已生效，不重复）
        if st.last_request_id.as_deref() == Some(request_id) {
            return Ok(vec![]);
        }
        if st.value_draft.is_empty() {
            return Err(Error::new(ErrorKind::NoDraft, "no pending draft"));
        }

        // 完整性校验（M1 policy=block）
        let draft_map: BTreeMap<String, BTreeMap<String, DraftValue>> = st.value_draft.clone();
        let errs = validator::validate_publish(&draft_map, &structure);
        if !errs.is_empty() {
            return Err(Error::publish_blocked(
                serde_json::json!({ "errors": errs }),
            ));
        }

        // 物化：草稿值 + 共享库引用（草稿无值时取共享值）
        let mut resolved: SnapshotMap = draft_map
            .into_iter()
            .map(|(g, items)| {
                let m = items.into_iter().map(|(k, dv)| (k, dv.value)).collect();
                (g, m)
            })
            .collect();
        for binding in self.read_refs_of_project(id)? {
            match binding.item_key.as_deref() {
                Some(key) => {
                    if resolved
                        .get(&binding.group)
                        .is_none_or(|m| !m.contains_key(key))
                    {
                        if let Some(shared) =
                            self.get_shared(&binding.shared_group, &binding.shared_key)?
                        {
                            resolved
                                .entry(binding.group.clone())
                                .or_default()
                                .insert(key.to_string(), shared.value.clone());
                        }
                    }
                }
                // 组级引用（B3）：整组绑定共享组 SG —— 结构组内 item 按 key 取共享项
                None => {
                    let struct_group = structure.groups.iter().find(|g| g.name == binding.group);
                    if let Some(sg) = struct_group {
                        for item in &sg.items {
                            let entry = resolved.get(&binding.group);
                            if entry.is_none_or(|m| !m.contains_key(&item.key)) {
                                if let Some(shared) =
                                    self.get_shared(&binding.shared_group, &item.key)?
                                {
                                    resolved
                                        .entry(binding.group.clone())
                                        .or_default()
                                        .insert(item.key.clone(), shared.value.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        let old = if st.active_version == 0 {
            SnapshotMap::new()
        } else {
            self.snapshot_of(id, branch, st.active_version)?
        };
        let diff = compute_diff(&old, &resolved);

        let vno = st.active_version + 1;
        let mut record = VersionRecord {
            no: vno,
            structure_version: structure.version,
            created_at: now_ms,
            operator: Self::operator_id(operator),
            comment: comment.to_string(),
            rollback_of: None,
            kind: VersionKind::Full,
            snapshot_ref: None,
            diff_ref: None,
            event_ty: Some(EventType::ValuePublish),
        };
        self.write_version_snapshot(id, branch, vno, &old, &resolved, &mut record)?;
        st.active_version = vno;
        st.last_request_id = Some(request_id.to_string());
        st.value_draft.clear();
        self.save_pending(&branch_state_key(id, branch), &st)?;

        Ok(vec![PublishEvent {
            project: id.clone(),
            branch: branch.clone(),
            version: vno,
            ty: EventType::ValuePublish,
            structure_version: structure.version,
            comment: comment.to_string(),
            request_id: request_id.to_string(),
            changes: diff,
        }])
    }

    // ---------------- 回滚（I6/I9） ----------------

    #[allow(clippy::too_many_arguments)]
    fn apply_rollback(
        &mut self,
        project: &ProjectId,
        branch: &BranchName,
        to_version: u64,
        comment: &str,
        request_id: &str,
        now_ms: i64,
        operator: &str,
    ) -> ApplyOutcome {
        let mut st = self
            .get_branch_state(project, branch)?
            .ok_or_else(|| Error::not_found(format!("branch {branch} of {project}")))?;
        // 幂等（I10）
        if st.last_request_id.as_deref() == Some(request_id) {
            return Ok(vec![]);
        }
        if to_version == 0 || to_version >= st.active_version {
            return Err(Error::validation(format!(
                "to_version {to_version} must be 0 < v < active {}",
                st.active_version
            )));
        }
        let snap = self.snapshot_of(project, branch, to_version)?; // 不存在 → NotFound
        let old = if st.active_version == 0 {
            SnapshotMap::new()
        } else {
            self.snapshot_of(project, branch, st.active_version)?
        };
        let diff = compute_diff(&old, &snap);
        let vno = st.active_version + 1;
        let mut record = VersionRecord {
            no: vno,
            structure_version: st.structure_version,
            created_at: now_ms,
            operator: Self::operator_id(operator),
            comment: comment.to_string(),
            rollback_of: Some(to_version),
            kind: VersionKind::Full,
            snapshot_ref: None,
            diff_ref: None,
            event_ty: Some(EventType::Rollback),
        };
        self.write_version_snapshot(project, branch, vno, &old, &snap, &mut record)?;
        st.active_version = vno;
        st.last_request_id = Some(request_id.to_string());
        self.save_pending(&branch_state_key(project, branch), &st)?;
        Ok(vec![PublishEvent {
            project: project.clone(),
            branch: branch.clone(),
            version: vno,
            ty: EventType::Rollback,
            structure_version: st.structure_version,
            comment: comment.to_string(),
            request_id: request_id.to_string(),
            changes: diff,
        }])
    }

    // ---------------- 共享库（R6） ----------------

    fn apply_shared_draft_update(&mut self, item: &SharedItem, _operator: &str) -> ApplyOutcome {
        if item.group.is_empty() || item.key.is_empty() {
            return Err(Error::validation("shared item group/key required"));
        }
        if !validator::valid_key_name(&item.group) || !validator::valid_key_name(&item.key) {
            return Err(Error::validation(
                "shared group/key 须为 1-128 位 [A-Za-z0-9._-]",
            ));
        }
        // F9（状态机兜底，防绕过 API 层校验）：secret 标志与类型一致性——
        // secret 项只能是 Secret 类型（密文）；Secret 类型必须标记 secret=true。
        if item.secret && item.ty != ValueType::Secret {
            return Err(Error::validation("secret 共享项 type 必须为 secret"));
        }
        if !item.secret && item.ty == ValueType::Secret {
            return Err(Error::validation(
                "type=secret 的共享项必须标记 secret=true",
            ));
        }
        let size = serde_json::to_vec(item)
            .map_err(|e| Error::internal(format!("serialize shared: {e}")))?
            .len();
        if size > MAX_VALUE_BYTES {
            return Err(Error::limit_exceeded("shared item too large"));
        }
        self.save_pending(&shared_draft_key(&item.group, &item.key), item)?;
        Ok(vec![])
    }

    /// 管理面访问器：共享草稿列表（GET /api/v1/shared-draft）。
    pub fn list_shared_drafts(&self) -> Result<Vec<SharedItem>, Error> {
        let rows = self.get_prefix_merged(K_SHARED_DRAFT.as_bytes())?;
        let mut out = Vec::new();
        for (_, v) in rows {
            if let Ok(item) = serde_json::from_slice::<SharedItem>(&v) {
                out.push(item);
            }
        }
        out.sort_by(|a, b| {
            (a.group.as_str(), a.key.as_str()).cmp(&(b.group.as_str(), b.key.as_str()))
        });
        Ok(out)
    }

    /// 管理面访问器：已发布共享项列表（GET /api/v1/shared）。
    pub fn list_shared_published(&self) -> Result<Vec<SharedItem>, Error> {
        let rows = self.get_prefix_merged(K_SHARED.as_bytes())?;
        let mut out = Vec::new();
        for (_, v) in rows {
            if let Ok(item) = serde_json::from_slice::<SharedItem>(&v) {
                out.push(item);
            }
        }
        out.sort_by(|a, b| {
            (a.group.as_str(), a.key.as_str()).cmp(&(b.group.as_str(), b.key.as_str()))
        });
        Ok(out)
    }

    /// 管理面访问器：项目引用绑定列表（GET /api/v1/shared/refs?project=）。
    pub fn list_refs(&self, project: &ProjectId) -> Result<Vec<RefBinding>, Error> {
        self.read_refs_of_project(project)
    }

    pub fn get_shared(&self, group: &str, key: &str) -> Result<Option<SharedItem>, Error> {
        self.load_merged(&shared_key(group, key))
    }

    /// 引用索引：idx/ref/{shared_group}/{shared_key}/{project}/{group}/{item_key} → "1"
    fn ref_index_key(
        shared_group: &str,
        shared_key: &str,
        project: &ProjectId,
        group: &str,
        item_key: &str,
    ) -> String {
        format!(
            "{K_IDX_REF}{shared_group}/{shared_key}/{}/{group}/{item_key}",
            project.as_str()
        )
    }

    fn apply_shared_publish(
        &mut self,
        comment: &str,
        request_id: &str,
        now_ms: i64,
        _operator: &str,
    ) -> ApplyOutcome {
        let drafts = self.list_shared_drafts()?;
        if drafts.is_empty() {
            return Err(Error::new(ErrorKind::NoDraft, "no shared draft"));
        }
        let mut events = Vec::new();
        for item in &drafts {
            let prev = self.get_shared(&item.group, &item.key)?;
            let version = prev.as_ref().map(|p| p.version).unwrap_or(0) + 1;
            let published = SharedItem {
                group: item.group.clone(),
                key: item.key.clone(),
                ty: item.ty,
                secret: item.secret,
                required: item.required,
                value: item.value.clone(),
                version,
            };
            self.save_pending(&shared_key(&item.group, &item.key), &published)?;
            self.store
                .delete(shared_draft_key(&item.group, &item.key).as_bytes())?;

            // 级联（auto）：引用该共享项的 (项目, 分支) 版本推进
            let prefix = format!("{K_IDX_REF}{}/{}", item.group, item.key);
            let rows = self.get_prefix_merged(prefix.as_bytes())?;
            for (k, _) in rows {
                let ks = String::from_utf8_lossy(&k);
                let rest = &ks[prefix.len() + 1..]; // {project}/{group}/{item_key}
                let parts: Vec<&str> = rest.split('/').collect();
                if parts.len() != 3 {
                    continue;
                }
                let project = ProjectId(parts[0].to_string());
                let group = parts[1].to_string();
                let key = parts[2].to_string();
                self.cascade_to_project(
                    &project,
                    &group,
                    &key,
                    &item.value,
                    comment,
                    request_id,
                    now_ms,
                    &mut events,
                )?;
            }
            // 组级引用级联（B3）：idx/refg/{shared_group}/{project}/{group} —— 结构组内含该 key 则推进
            let gprefix = format!("{K_IDX_REFG}{}", item.group);
            let grows = self.get_prefix_merged(gprefix.as_bytes())?;
            for (gk, _) in grows {
                let gks = String::from_utf8_lossy(&gk);
                let grest = &gks[gprefix.len() + 1..]; // {project}/{group}
                let gparts: Vec<&str> = grest.split('/').collect();
                if gparts.len() != 2 {
                    continue;
                }
                let project = ProjectId(gparts[0].to_string());
                let group = gparts[1].to_string();
                // 仅当项目结构组包含该共享 key 时级联（整组共享按结构组 item 集合匹配）
                let structure = self.get_structure(&project)?.unwrap_or(Structure {
                    version: 0,
                    groups: vec![],
                });
                let has_key = structure
                    .groups
                    .iter()
                    .any(|g| g.name == group && g.items.iter().any(|i| i.key == item.key));
                if has_key {
                    self.cascade_to_project(
                        &project,
                        &group,
                        &item.key,
                        &item.value,
                        comment,
                        request_id,
                        now_ms,
                        &mut events,
                    )?;
                }
            }
        }
        Ok(events)
    }

    /// 级联单个 (项目, group, key) 的值更新到全部分支（版本推进 + SharedCascade 事件）。
    #[allow(clippy::too_many_arguments)]
    fn cascade_to_project(
        &mut self,
        project: &ProjectId,
        group: &str,
        key: &str,
        value: &Value,
        comment: &str,
        request_id: &str,
        now_ms: i64,
        events: &mut Vec<PublishEvent>,
    ) -> Result<(), Error> {
        for branch in self.list_branches(project)? {
            let mut st = self
                .get_branch_state(project, &branch)?
                .ok_or_else(|| Error::internal("branch state missing"))?;
            let old = if st.active_version == 0 {
                SnapshotMap::new()
            } else {
                self.snapshot_of(project, &branch, st.active_version)?
            };
            let mut new_snap = old.clone();
            new_snap
                .entry(group.to_string())
                .or_default()
                .insert(key.to_string(), value.clone());
            let diff = compute_diff(&old, &new_snap);
            let vno = st.active_version + 1;
            let mut record = VersionRecord {
                no: vno,
                structure_version: st.structure_version,
                created_at: now_ms,
                operator: "shared".into(),
                comment: comment.to_string(),
                rollback_of: None,
                kind: VersionKind::Full,
                snapshot_ref: None,
                diff_ref: None,
                event_ty: Some(EventType::SharedCascade),
            };
            self.write_version_snapshot(project, &branch, vno, &old, &new_snap, &mut record)?;
            st.active_version = vno;
            self.save_pending(&branch_state_key(project, &branch), &st)?;
            events.push(PublishEvent {
                project: project.clone(),
                branch,
                version: vno,
                ty: EventType::SharedCascade,
                structure_version: st.structure_version,
                comment: comment.to_string(),
                request_id: request_id.to_string(),
                changes: diff,
            });
        }
        Ok(())
    }

    fn apply_ref_bind(
        &mut self,
        project: &ProjectId,
        binding: &RefBinding,
        _operator: &str,
    ) -> ApplyOutcome {
        if !validator::valid_key_name(&binding.shared_group)
            || !validator::valid_key_name(&binding.shared_key)
        {
            return Err(Error::validation(
                "shared group/key 须为 1-128 位 [A-Za-z0-9._-]",
            ));
        }
        let structure = self
            .get_structure(project)?
            .ok_or_else(|| Error::not_found(format!("project {project}")))?;
        let group_def = structure
            .groups
            .iter()
            .find(|g| g.name == binding.group)
            .ok_or_else(|| {
                Error::validation(format!("group {} not in project structure", binding.group))
            })?;
        match binding.item_key.as_deref() {
            Some(item_key) => {
                // 校验：结构内存在该 item
                let found = group_def.items.iter().any(|i| i.key == item_key);
                if !found {
                    return Err(Error::validation(format!(
                        "item {}/{} not in project structure",
                        binding.group, item_key
                    )));
                }
                // 校验：共享项已发布存在
                if self
                    .get_shared(&binding.shared_group, &binding.shared_key)?
                    .is_none()
                {
                    return Err(Error::validation(format!(
                        "shared item {}/{} not published",
                        binding.shared_group, binding.shared_key
                    )));
                }
                self.save_pending(&ref_key(project, &binding.group, Some(item_key)), binding)?;
                self.save_pending(
                    &Self::ref_index_key(
                        &binding.shared_group,
                        &binding.shared_key,
                        project,
                        &binding.group,
                        item_key,
                    ),
                    &"1",
                )?;
            }
            None => {
                // 组级引用（B3）：整组绑定共享组 SG —— 结构组内 item 按 key 匹配已发布共享项（≥1 个）
                let struct_keys: std::collections::HashSet<&str> =
                    group_def.items.iter().map(|i| i.key.as_str()).collect();
                let rows = self
                    .store
                    .get_prefix(shared_prefix(&binding.shared_group).as_bytes())?;
                let mut matched = 0usize;
                for (_, v) in rows {
                    if let Ok(item) = serde_json::from_slice::<SharedItem>(&v) {
                        if item.group == binding.shared_group
                            && struct_keys.contains(item.key.as_str())
                        {
                            matched += 1;
                        }
                    }
                }
                if matched == 0 {
                    return Err(Error::validation(format!(
                        "shared group {} has no published item matching structure group {}",
                        binding.shared_group, binding.group
                    )));
                }
                self.save_pending(&ref_key(project, &binding.group, None), binding)?;
                self.save_pending(
                    &group_ref_index_key(&binding.shared_group, project, &binding.group),
                    &"1",
                )?;
            }
        }
        Ok(vec![])
    }

    fn apply_ref_unbind(
        &mut self,
        project: &ProjectId,
        group: &str,
        item_key: Option<&str>,
        _operator: &str,
    ) -> ApplyOutcome {
        match item_key {
            Some(key) => {
                let binding: Option<RefBinding> =
                    self.load_merged(&ref_key(project, group, Some(key)))?;
                if let Some(b) = binding {
                    self.delete_pending(ref_key(project, group, Some(key)).as_bytes())?;
                    self.delete_pending(
                        Self::ref_index_key(&b.shared_group, &b.shared_key, project, group, key)
                            .as_bytes(),
                    )?;
                }
            }
            None => {
                // 组级解绑
                let binding: Option<RefBinding> =
                    self.load_merged(&ref_key(project, group, None))?;
                if let Some(b) = binding {
                    self.delete_pending(ref_key(project, group, None).as_bytes())?;
                    self.delete_pending(
                        group_ref_index_key(&b.shared_group, project, group).as_bytes(),
                    )?;
                }
            }
        }
        Ok(vec![])
    }

    /// 读取项目全部 item 级引用。
    fn read_refs_of_project(&self, project: &ProjectId) -> Result<Vec<RefBinding>, Error> {
        let prefix = format!("{K_PROJECT}{}{K_REF}", project.as_str());
        let rows = self.get_prefix_merged(prefix.as_bytes())?;
        let mut out = Vec::new();
        for (_, v) in rows {
            if let Ok(b) = serde_json::from_slice::<RefBinding>(&v) {
                out.push(b);
            }
        }
        Ok(out)
    }

    // ---------------- 会话（I7 单管理员；状态机内强制） ----------------

    /// 命令 operator 的落库值：空串（旧客户端/全局管理员）→ "admin"。
    fn operator_id(operator: &str) -> String {
        if operator.is_empty() {
            "admin".to_string()
        } else {
            operator.to_string()
        }
    }

    fn apply_session_login(
        &mut self,
        token_hash: &str,
        issued_at: i64,
        expires_at: Option<i64>,
    ) -> ApplyOutcome {
        if self.get_session()?.is_some() {
            return Err(Error::new(ErrorKind::SessionInUse, "已有管理员在线"));
        }
        let session = AdminSession {
            token_hash: token_hash.to_string(),
            issued_at,
            expires_at,
            device_id: "cli".into(),
            principal: Principal::Admin,
        };
        self.save_pending(session_key(), &session)?;
        Ok(vec![])
    }

    fn apply_session_logout(&mut self) -> ApplyOutcome {
        self.delete_pending(session_key().as_bytes())?;
        Ok(vec![])
    }

    fn apply_session_heartbeat(&mut self, expires_at: Option<i64>) -> ApplyOutcome {
        let mut session = self
            .get_session()?
            .ok_or_else(|| Error::new(ErrorKind::SessionExpired, "未登录"))?;
        session.expires_at = expires_at;
        self.save_pending(session_key(), &session)?;
        Ok(vec![])
    }

    // ---------------- 项目管理员（Project Admin）----------------
    // 设计文档 docs/design/project-admin.md §3.1/§6。
    // 会话判定只看 is_some()，不读墙钟（D16 确定性）。

    fn valid_pa_username(name: &str) -> bool {
        !name.is_empty()
            && name != "admin"
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && name.len() >= 2
    }

    fn apply_project_admin_create(
        &mut self,
        project: &ProjectId,
        username: &str,
        salt: &str,
        password_hash: &str,
        now_ms: i64,
    ) -> ApplyOutcome {
        if !Self::valid_pa_username(username) {
            return Err(Error::new(
                ErrorKind::Validation,
                "用户名须为 2-64 位 [A-Za-z0-9_-] 且不可为 admin",
            ));
        }
        if load::<Project>(&*self.store, &project_key(project))?.is_none() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("项目 {project} 不存在"),
            ));
        }
        let key = project_admin_key(username);
        if load::<ProjectAdminAccount>(&*self.store, &key)?.is_some() {
            return Err(Error::new(ErrorKind::Conflict, "账号已存在"));
        }
        let acct = ProjectAdminAccount {
            username: username.to_string(),
            project: project.clone(),
            salt: salt.to_string(),
            password_hash: password_hash.to_string(),
            created_at: now_ms,
        };
        self.save_pending(&key, &acct)?;
        Ok(vec![])
    }

    fn apply_project_admin_delete(&mut self, username: &str) -> ApplyOutcome {
        let key = project_admin_key(username);
        if load::<ProjectAdminAccount>(&*self.store, &key)?.is_none() {
            return Err(Error::new(ErrorKind::NotFound, "账号不存在"));
        }
        self.delete_pending(pa_session_key(username).as_bytes())?;
        self.delete_pending(key.as_bytes())?;
        Ok(vec![])
    }

    fn apply_project_admin_set_password(
        &mut self,
        username: &str,
        salt: &str,
        password_hash: &str,
    ) -> ApplyOutcome {
        let key = project_admin_key(username);
        let Some(mut acct) = load::<ProjectAdminAccount>(&*self.store, &key)? else {
            return Err(Error::new(ErrorKind::NotFound, "账号不存在"));
        };
        acct.salt = salt.to_string();
        acct.password_hash = password_hash.to_string();
        self.save_pending(&key, &acct)?;
        // 改密即时收回会话（权限立即生效）
        self.delete_pending(pa_session_key(username).as_bytes())?;
        Ok(vec![])
    }

    fn apply_pa_session_login(
        &mut self,
        username: &str,
        token_hash: &str,
        issued_at: i64,
        expires_at: Option<i64>,
        device_id: &str,
    ) -> ApplyOutcome {
        let key = pa_session_key(username);
        if load::<AdminSession>(&*self.store, &key)?.is_some() {
            return Err(Error::new(ErrorKind::SessionInUse, "该账号已有会话在线"));
        }
        let Some(acct) = load::<ProjectAdminAccount>(&*self.store, &project_admin_key(username))?
        else {
            return Err(Error::new(ErrorKind::NotFound, "账号不存在"));
        };
        let session = AdminSession {
            token_hash: token_hash.to_string(),
            issued_at,
            expires_at,
            device_id: device_id.to_string(),
            principal: Principal::ProjectAdmin {
                username: username.to_string(),
                project: acct.project.clone(),
            },
        };
        self.save_pending(&key, &session)?;
        Ok(vec![])
    }

    fn apply_pa_session_logout(&mut self, username: &str) -> ApplyOutcome {
        self.delete_pending(pa_session_key(username).as_bytes())?;
        Ok(vec![])
    }

    fn apply_pa_session_heartbeat(
        &mut self,
        username: &str,
        expires_at: Option<i64>,
    ) -> ApplyOutcome {
        let key = pa_session_key(username);
        let Some(mut session) = load::<AdminSession>(&*self.store, &key)? else {
            return Err(Error::new(ErrorKind::SessionExpired, "会话不存在"));
        };
        session.expires_at = expires_at;
        self.save_pending(&key, &session)?;
        Ok(vec![])
    }

    /// 读取项目管理员账号。
    pub fn get_project_admin(&self, username: &str) -> Result<Option<ProjectAdminAccount>, Error> {
        self.load_merged(&project_admin_key(username))
    }

    /// 列出项目全部项目管理员账号（扫 adm/pa/ 前缀过滤，O(账号数)）。
    pub fn list_project_admins(&self, project: &str) -> Result<Vec<ProjectAdminAccount>, Error> {
        let mut out = vec![];
        for (_, raw) in self.get_prefix_merged(K_PA_ACCOUNT.as_bytes())? {
            if let Ok(acct) = serde_json::from_slice::<ProjectAdminAccount>(&raw) {
                if acct.project.0 == project {
                    out.push(acct);
                }
            }
        }
        out.sort_by(|a, b| a.username.cmp(&b.username));
        Ok(out)
    }

    pub fn get_pa_session(&self, username: &str) -> Result<Option<AdminSession>, Error> {
        self.load_merged(&pa_session_key(username))
    }

    fn apply_admin_set_password(&mut self, password_hash: &str) -> ApplyOutcome {
        self.save_pending(K_ADMIN_PW, &password_hash.to_string())?;
        Ok(vec![])
    }

    /// 状态机内管理员密码哈希（set-password 后登录用它校验；未设置时回退节点配置）。
    pub fn get_admin_password_hash(&self) -> Result<Option<String>, Error> {
        self.load_merged(K_ADMIN_PW)
    }

    /// 审计追加：seq 单调分配（audit/seq 计数），条目落 audit/{seq:020}。
    /// 入参 entry.seq 忽略（由状态机分配）。
    fn apply_audit_append(&mut self, entry: &AuditEntry) -> ApplyOutcome {
        let prev: Option<u64> = self.load_merged(K_AUDIT_SEQ)?;
        let seq = prev.unwrap_or(0) + 1;
        let entry = AuditEntry {
            seq,
            ..entry.clone()
        };
        self.save_pending(&audit_key(seq), &entry)?;
        self.save_pending(K_AUDIT_SEQ, &seq)?;
        Ok(vec![])
    }

    /// 密钥轮换：副作用（更新 Cipher/写 ring 文件）由 dsh-raft 的 apply 钩子执行，
    /// 状态机本身不落任何数据（保证确定性，跨节点重放结果一致）。
    fn apply_rotate_master_key(&mut self) -> ApplyOutcome {
        Ok(vec![])
    }
}

/// 会话令牌哈希（SHA-256 hex；明文 token 不落库/不落日志，I7）。
pub fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::InMemoryStore;

    fn sm() -> StateMachine {
        StateMachine::new(Box::new(InMemoryStore::new()))
    }

    fn shared_item(group: &str, key: &str) -> SharedItem {
        SharedItem {
            group: group.into(),
            key: key.into(),
            ty: ValueType::String,
            secret: false,
            required: false,
            value: Value::String("v".into()),
            version: 0,
        }
    }

    #[test]
    fn shared_draft_rejects_dangerous_names() {
        let mut s = sm();
        // `/` 会破坏 sh/{group}/{key} 与 idx/ref 索引分隔 → 级联静默跳过（C3）
        assert!(s
            .apply_shared_draft_update(&shared_item("a/b", "k"), "")
            .is_err());
        assert!(s
            .apply_shared_draft_update(&shared_item("g", "k/x"), "")
            .is_err());
        // HTML/XSS 载荷（S1）、非 ASCII、空白、引号
        for (g, k) in [
            ("<img onerror=alert(1)>", "k"),
            ("g", "<img>"),
            ("配置", "k"),
            ("g", "a b"),
            ("g", "a'b"),
            ("g", "a&b"),
        ] {
            assert!(
                s.apply_shared_draft_update(&shared_item(g, k), "").is_err(),
                "{g:?}/{k:?} must be rejected"
            );
        }
    }

    #[test]
    fn shared_draft_accepts_safe_names() {
        let mut s = sm();
        for (g, k) in [
            ("infra.db", "host_name-1"),
            ("redis", "max_conns"),
            ("g", "k"),
        ] {
            assert!(
                s.apply_shared_draft_update(&shared_item(g, k), "").is_ok(),
                "{g:?}/{k:?} must be accepted"
            );
        }
    }

    #[test]
    fn ref_bind_rejects_dangerous_shared_names() {
        let mut s = sm();
        // 校验发生在项目结构查询之前，无需先建项目
        for (sg, sk) in [
            ("infra/..", "k"),
            ("infra", "<img>"),
            ("配置", "k"),
            ("infra", "a b"),
        ] {
            let b = RefBinding {
                group: "redis".into(),
                item_key: Some("host".into()),
                shared_group: sg.into(),
                shared_key: sk.into(),
            };
            let e = s
                .apply_ref_bind(&ProjectId("p".into()), &b, "")
                .expect_err("must reject");
            assert_eq!(e.kind, ErrorKind::Validation, "{sg:?}/{sk:?}: {e:?}");
        }
    }

    #[test]
    fn ref_bind_accepts_safe_shared_names() {
        let mut s = sm();
        let b = RefBinding {
            group: "redis".into(),
            item_key: Some("host".into()),
            shared_group: "infra.db".into(),
            shared_key: "host-1".into(),
        };
        // 通过字符集校验后走到项目结构查找（项目不存在 → NotFound，而非 Validation）
        let e = s
            .apply_ref_bind(&ProjectId("p".into()), &b, "")
            .expect_err("project missing");
        assert_eq!(e.kind, ErrorKind::NotFound, "{e:?}");
    }

    /// N1 回归：删除项目须清理全局引用索引（idx/ref、idx/refg）中的孤儿条目，
    /// 否则共享项发布级联扫描会永久命中已删项目（脏数据残留）。
    #[test]
    fn project_delete_cleans_orphan_ref_indexes() {
        let mut s = sm();
        let proj = "order-service";
        s.apply(
            &Command::ProjectCreate {
                name: proj.into(),
                operator: String::new(),
                ts: 0,
            },
            1,
        )
        .unwrap();

        // 直接构造 RefBind 已写入的全局引用索引键（item 级 5 段 + 组级 3 段），
        // 模拟删除项目后遗留的孤儿条目。
        let item_idx = format!("{K_IDX_REF}infra/host/{proj}/redis/host");
        let group_idx = format!("{K_IDX_REFG}infra/{proj}/redis");
        s.store.put(item_idx.as_bytes(), b"1").unwrap();
        s.store.put(group_idx.as_bytes(), b"1").unwrap();
        // 对照组：其他项目的索引必须保留。
        let other_item = format!("{K_IDX_REF}infra/host/other-svc/redis/host");
        let other_group = format!("{K_IDX_REFG}infra/other-svc/redis");
        s.store.put(other_item.as_bytes(), b"1").unwrap();
        s.store.put(other_group.as_bytes(), b"1").unwrap();

        s.apply(
            &Command::ProjectDelete {
                id: ProjectId(proj.into()),
                operator: String::new(),
            },
            2,
        )
        .unwrap();

        // 断言：idx/ref/ 与 idx/refg/ 前缀下不存在含该项目 id 的键（与清理逻辑同构的精确匹配）。
        for (k, _) in s.store.get_prefix(K_IDX_REF.as_bytes()).unwrap() {
            let ks = String::from_utf8_lossy(&k);
            if let Some(rest) = ks.strip_prefix(K_IDX_REF) {
                let parts: Vec<&str> = rest.split('/').collect();
                assert!(
                    !(parts.len() == 5 && parts[2] == proj),
                    "orphan idx/ref key survives: {ks}"
                );
            }
        }
        for (k, _) in s.store.get_prefix(K_IDX_REFG.as_bytes()).unwrap() {
            let ks = String::from_utf8_lossy(&k);
            if let Some(rest) = ks.strip_prefix(K_IDX_REFG) {
                let parts: Vec<&str> = rest.split('/').collect();
                assert!(
                    !(parts.len() == 3 && parts[1] == proj),
                    "orphan idx/refg key survives: {ks}"
                );
            }
        }
        // 对照组索引不受影响。
        assert!(s.store.get(other_item.as_bytes()).unwrap().is_some());
        assert!(s.store.get(other_group.as_bytes()).unwrap().is_some());
    }

    /// perf 方案① T5：命令内读合并——pending 覆盖/删除对 load/get_prefix 可见（写后读）。
    #[test]
    fn pending_read_merge_visibility() {
        let mut s = sm();
        // put → merged get 命中 pending（未提交即可见）
        s.put_pending(b"p/x", b"v1").unwrap();
        assert_eq!(s.get_merged(b"p/x").unwrap().unwrap(), b"v1");
        // 覆盖：后写优先（逆序）
        s.put_pending(b"p/x", b"v2").unwrap();
        assert_eq!(s.get_merged(b"p/x").unwrap().unwrap(), b"v2");
        // 删除优先于插入（同 key 先插后删 → None）
        s.delete_pending(b"p/x").unwrap();
        assert_eq!(s.get_merged(b"p/x").unwrap(), None);
        // 先删后插 → 插生效
        s.put_pending(b"p/x", b"v3").unwrap();
        assert_eq!(s.get_merged(b"p/x").unwrap().unwrap(), b"v3");
        // get_prefix 合并：store 基 + pending 插 + pending 删
        s.store.put(b"p/a", b"sa").unwrap();
        s.store.put(b"p/z", b"sz").unwrap();
        s.put_pending(b"p/m", b"pm").unwrap();
        s.delete_pending(b"p/a").unwrap();
        let rows = s.get_prefix_merged(b"p/").unwrap();
        let map: std::collections::BTreeMap<_, _> = rows.into_iter().collect();
        assert_eq!(map.get(b"p/a".as_slice()), None, "pending 删除遮蔽 store");
        assert_eq!(
            map.get(b"p/m".as_slice()).unwrap(),
            b"pm",
            "pending 插入合并"
        );
        assert_eq!(map.get(b"p/z".as_slice()).unwrap(), b"sz", "store 基保留");
        assert_eq!(map.get(b"p/x".as_slice()).unwrap(), b"v3");
        // 前缀边界：prefix "p/m" 只命中自己
        let rows2 = s.get_prefix_merged(b"p/m").unwrap();
        assert_eq!(rows2.len(), 1);
    }

    /// perf 方案① T4：命令失败 → pending abort，store 无部分写。
    #[test]
    fn apply_failure_aborts_pending() {
        let mut s = sm();
        // 建项目 + 结构（正常路径）
        s.apply(
            &Command::ProjectCreate {
                name: "p".into(),
                operator: String::new(),
                ts: 0,
            },
            1,
        )
        .unwrap();
        s.apply(
            &Command::StructureDraftSet {
                project: "p".into(),
                base_version: 1,
                groups: vec![GroupDef {
                    name: "redis".into(),
                    items: vec![ItemDef {
                        key: "host".into(),
                        ty: ValueType::String,
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
        s.apply(
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
        // 无草稿直接发布 → 失败（NoDraft）；不产生版本/快照
        let e = s
            .apply(
                &Command::Publish {
                    project: "p".into(),
                    branch: BranchName("dev".into()),
                    comment: "x".into(),
                    request_id: "r1".into(),
                    operator: String::new(),
                    ts: 0,
                },
                4,
            )
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::NoDraft);
        assert!(s.pending_ops.is_empty(), "失败后 pending 必须清空");
        // store 无版本记录/快照（无部分写）
        let pid: ProjectId = "p".into();
        let dev = BranchName("dev".into());
        assert!(s
            .store
            .get(version_key(&pid, &dev, 4).as_bytes())
            .unwrap()
            .is_none());
        assert!(s
            .store
            .get(snapshot_key(&pid, &dev, 4).as_bytes())
            .unwrap()
            .is_none());
        // 分支仍可正常发布（后续成功路径不受污染）
        s.apply(
            &Command::DraftUpdate {
                project: "p".into(),
                branch: BranchName("dev".into()),
                updates: vec![crate::command::DraftUpdateItem {
                    group: "redis".into(),
                    key: "host".into(),
                    value: Value::String("h".into()),
                }],
                deletes: vec![],
                operator: String::new(),
                ts: 0,
            },
            5,
        )
        .unwrap();
        assert!(s
            .apply(
                &Command::Publish {
                    project: "p".into(),
                    branch: BranchName("dev".into()),
                    comment: "v1".into(),
                    request_id: "r2".into(),
                    operator: String::new(),
                    ts: 0,
                },
                6,
            )
            .is_ok());
    }
}
