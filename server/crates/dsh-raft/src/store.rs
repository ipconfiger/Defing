//! Raft 日志/状态机/快照存储（模块 03 §5）。
//! - LogStore：raft-log / raft-meta 表（经 RedbStorage::raw_db）
#![allow(clippy::result_large_err, clippy::type_complexity)] // openraft StorageError/RPCError 的 Err 变体较大（上游类型）

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use dsh_core::error::Error as DshError;
use dsh_core::model::PublishEvent;
use dsh_core::StateMachine;
use dsh_storage::{TBL_RAFT_LOG, TBL_RAFT_META, TBL_SNAPSHOTS};
use openraft::storage::{
    LogState, RaftLogReader, RaftLogStorage, RaftSnapshotBuilder, RaftStateMachine,
};
use openraft::{
    Entry, EntryPayload, ErrorSubject, ErrorVerb, LeaderId, LogId, OptionalSend, Snapshot,
    SnapshotMeta, StorageError, StorageIOError, StoredMembership, Vote,
};
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};
use serde::{Deserialize, Serialize};

use crate::types::{NodeId, NodeInfo, TypeConfig};

type ErrOf = StorageError<NodeId>;

pub type EntryOf = Entry<TypeConfig>;
pub type LogIdOf = LogId<NodeId>;

// 公共存储句柄：共享 redb（dsh_storage::RedbStorage::raw_db）
pub type DbHandle = std::sync::Arc<redb::Database>;

// ---------------- meta 键（raft-meta 表） ----------------

const META_VOTE: &[u8] = b"vote";
const META_LAST_PURGED: &[u8] = b"last_purged";
const META_LAST_APPLIED: &[u8] = b"last_applied";
const META_MEMBERSHIP: &[u8] = b"membership";

// ---------------- 快照持久化（snapshots 表；B5） ----------------

const SNAP_META_KEY: &[u8] = b"meta";
const SNAP_DATA_KEY: &[u8] = b"data";

fn persist_snapshot(
    db: &DbHandle,
    meta: &SnapshotMeta<NodeId, NodeInfo>,
    data: &[u8],
) -> Result<(), ErrOf> {
    let meta_raw = ser(meta)?;
    // meta+data 同一事务原子提交（Durability 默认 Immediate：commit 返回即已 fsync）
    let txn = db
        .begin_write()
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e))?;
    {
        let mut table = txn
            .open_table(TBL_SNAPSHOTS)
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e))?;
        table
            .insert(SNAP_META_KEY, meta_raw.as_slice())
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e))?;
        table
            .insert(SNAP_DATA_KEY, data)
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e))?;
    }
    txn.commit()
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e))
}

fn load_persisted_snapshot(
    db: &DbHandle,
) -> Result<Option<(SnapshotMeta<NodeId, NodeInfo>, Vec<u8>)>, ErrOf> {
    let txn = db
        .begin_read()
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e))?;
    let table = txn
        .open_table(TBL_SNAPSHOTS)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e))?;
    let meta_raw = table
        .get(SNAP_META_KEY)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e))?;
    let data = table
        .get(SNAP_DATA_KEY)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e))?;
    match (meta_raw, data) {
        (Some(m), Some(d)) => Ok(Some((de(m.value())?, d.value().to_vec()))),
        _ => Ok(None),
    }
}

fn log_key(index: u64) -> [u8; 8] {
    index.to_be_bytes()
}

fn log_index_from_key(key: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&key[..8]);
    u64::from_be_bytes(b)
}

fn ser<T: Serialize>(v: &T) -> Result<Vec<u8>, ErrOf> {
    serde_json::to_vec(v).map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e.to_string()))
}

fn de<T: for<'de> Deserialize<'de>>(raw: &[u8]) -> Result<T, ErrOf> {
    serde_json::from_slice(raw)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e.to_string()))
}

fn io_err<NID: openraft::NodeId>(
    subject: ErrorSubject<NID>,
    verb: ErrorVerb,
    msg: impl std::fmt::Display,
) -> StorageError<NID> {
    let e = std::io::Error::other(msg.to_string());
    StorageError::IO {
        source: StorageIOError::new(subject, verb, openraft::AnyError::new(&e)),
    }
}

fn storage_err(e: DshError) -> ErrOf {
    io_err(ErrorSubject::Store, ErrorVerb::Write, e.to_string())
}

// ---------------- raft-meta 表读写（LogStore / StateMachineStore 共用） ----------------

fn meta_get(db: &DbHandle, key: &[u8]) -> Result<Option<Vec<u8>>, ErrOf> {
    let txn = db
        .begin_read()
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e))?;
    let table = txn
        .open_table(TBL_RAFT_META)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e))?;
    match table
        .get(key)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e))?
    {
        Some(guard) => Ok(Some(guard.value().to_vec())),
        None => Ok(None),
    }
}

/// 写 raft-meta 单键（单事务提交；Durability 默认 Immediate，commit 即 fsync）。
fn meta_put(db: &DbHandle, key: &[u8], value: &[u8]) -> Result<(), ErrOf> {
    let txn = db
        .begin_write()
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e))?;
    let mut table = txn
        .open_table(TBL_RAFT_META)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e))?;
    table
        .insert(key, value)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e))?;
    drop(table); // 表句柄先于 commit 释放（commit 消费事务）
    txn.commit()
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e))
}

/// 共享的日志读取逻辑（LogReader 与 LogStore 复用）：[start, end] 闭区间。
fn read_entries(db: &DbHandle, start: u64, end: u64) -> Result<Vec<EntryOf>, ErrOf> {
    let txn = db
        .begin_read()
        .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Read, e))?;
    let table = txn
        .open_table(TBL_RAFT_LOG)
        .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Read, e))?;
    let start_key = log_key(start);
    let mut out = Vec::new();
    for row in table
        .range(start_key.as_slice()..)
        .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Read, e))?
    {
        let (k, v) = row.map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Read, e))?;
        if log_index_from_key(k.value()) > end {
            break;
        }
        out.push(de(v.value())?);
    }
    Ok(out)
}

/// 最后一条日志（升序表尾部）。空表返回 None。
fn last_log_entry(db: &DbHandle) -> Result<Option<EntryOf>, ErrOf> {
    let txn = db
        .begin_read()
        .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Read, e))?;
    let table = txn
        .open_table(TBL_RAFT_LOG)
        .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Read, e))?;
    // 先绑定局部再匹配：避免尾表达式的 AccessGuard 临时值活得比 table 久
    let last = table
        .last()
        .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Read, e))?;
    match last {
        Some((_, v)) => Ok(Some(de(v.value())?)),
        None => Ok(None),
    }
}

/// 在写事务内收集 range 命中的日志索引（BE u64 key → u64）。
/// 表句柄随本函数结束 drop——之后可在同一事务重新 open_table 执行删除
/// （同一事务内同名表二次 open_table 会报 TableAlreadyOpen，设计 §3.1）。
fn collect_log_indexes<'k, R>(txn: &redb::WriteTransaction, range: R) -> Result<Vec<u64>, ErrOf>
where
    R: std::ops::RangeBounds<&'k [u8]> + 'k,
{
    let table = txn
        .open_table(TBL_RAFT_LOG)
        .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))?;
    let mut out = Vec::new();
    for row in table
        .range(range)
        .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))?
    {
        let (k, _) = row.map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))?;
        out.push(log_index_from_key(k.value()));
    }
    Ok(out)
}

/// 在写事务内逐条删除日志（调用前须已 drop 收集阶段的表句柄，见 collect_log_indexes）。
fn remove_log_indexes(txn: &redb::WriteTransaction, indexes: Vec<u64>) -> Result<(), ErrOf> {
    let mut table = txn
        .open_table(TBL_RAFT_LOG)
        .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))?;
    for idx in indexes {
        table
            .remove(log_key(idx).as_slice())
            .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))?;
    }
    Ok(())
}

// ---------------- LogStore ----------------

/// Raft 日志存储（raft-log 表：key = 8B BE index）。
pub struct LogStore {
    db: DbHandle,
}

impl LogStore {
    pub fn new(db: DbHandle) -> Self {
        Self { db }
    }
}

/// 日志读取器（只读，独立于 LogStore 的写路径）。
pub struct LogReader {
    db: DbHandle,
}

impl LogReader {
    pub fn new(db: DbHandle) -> Self {
        Self { db }
    }
}

impl RaftLogReader<TypeConfig> for LogReader {
    async fn try_get_log_entries<RB: std::ops::RangeBounds<u64> + Clone + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<EntryOf>, ErrOf> {
        let start = match range.start_bound() {
            std::ops::Bound::Included(i) => *i,
            std::ops::Bound::Excluded(i) => *i + 1,
            std::ops::Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            std::ops::Bound::Included(i) => *i,
            std::ops::Bound::Excluded(i) => *i - 1,
            std::ops::Bound::Unbounded => u64::MAX,
        };
        read_entries(&self.db, start, end)
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: std::ops::RangeBounds<u64> + Clone + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<EntryOf>, ErrOf> {
        let start = match range.start_bound() {
            std::ops::Bound::Included(i) => *i,
            std::ops::Bound::Excluded(i) => *i + 1,
            std::ops::Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            std::ops::Bound::Included(i) => *i,
            std::ops::Bound::Excluded(i) => *i - 1,
            std::ops::Bound::Unbounded => u64::MAX,
        };
        read_entries(&self.db, start, end)
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = LogReader;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, ErrOf> {
        let last_purged = meta_get(&self.db, META_LAST_PURGED)?
            .map(|raw| de::<u64>(&raw))
            .transpose()?
            .map(|idx| LogId {
                leader_id: LeaderId::new(0, NodeId::MAX),
                index: idx,
            });
        let last_log = last_log_entry(&self.db)?.map(|e| e.log_id);
        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id: last_log,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        LogReader::new(self.db.clone())
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), ErrOf> {
        let raw = ser(vote)?;
        meta_put(&self.db, META_VOTE, &raw)
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, ErrOf> {
        meta_get(&self.db, META_VOTE)?
            .map(|raw| de(&raw))
            .transpose()
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: openraft::storage::LogFlushed<TypeConfig>,
    ) -> Result<(), ErrOf>
    where
        I: IntoIterator<Item = EntryOf> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        // 单写事务批量 insert 后一次 commit：等价原 WriteBatch+flush_wal(true)
        // （WriteTransaction 默认 Durability::Immediate，commit 返回即已 fsync）
        let txn = self
            .db
            .begin_write()
            .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))?;
        {
            let mut table = txn
                .open_table(TBL_RAFT_LOG)
                .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))?;
            for e in entries {
                let raw = ser(&e)?;
                table
                    .insert(log_key(e.log_id.index).as_slice(), raw.as_slice())
                    .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))?;
            }
        }
        txn.commit()
            .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))?;
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), ErrOf> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))?;
        let from = log_key(log_id.index);
        let indexes = collect_log_indexes(&txn, from.as_slice()..)?;
        remove_log_indexes(&txn, indexes)?;
        txn.commit()
            .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), ErrOf> {
        // BE 定宽 key：..=idx 的字典序范围恰为索引 <= idx 的全部日志
        let txn = self
            .db
            .begin_write()
            .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))?;
        let upto = log_key(log_id.index);
        let indexes = collect_log_indexes(&txn, ..=upto.as_slice())?;
        remove_log_indexes(&txn, indexes)?;
        // 日志清理与 last_purged 同一事务原子提交（redb 多表事务原生支持）
        let mut meta_table = txn
            .open_table(TBL_RAFT_META)
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e))?;
        meta_table
            .insert(META_LAST_PURGED, ser(&log_id.index)?.as_slice())
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e))?;
        drop(meta_table); // 表句柄先于 commit 释放（commit 消费事务）
        txn.commit()
            .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e))
    }
}

// ---------------- StateMachineStore ----------------

/// 状态机 + 快照（Clone 共享同一实例）。
#[derive(Clone)]
pub struct StateMachineStore {
    /// 状态机（共享：apply 与读共用锁）
    pub sm: Arc<Mutex<StateMachine>>,
    db: DbHandle,
    /// 内存中的当前快照（M1：不跨重启持久化）
    current_snapshot: Arc<tokio::sync::Mutex<Option<(SnapshotMeta<NodeId, NodeInfo>, Vec<u8>)>>>,
    /// 发布事件广播（集群 watch：apply 时向本地订阅者推送）
    events: tokio::sync::broadcast::Sender<PublishEvent>,
    /// 主密钥轮换钩子：apply 到 `Command::RotateMasterKey` 成功时调用（更新本地 Cipher keyring + 持久化 ring 文件）。
    /// 状态机本身不落数据（确定性），此钩子是集群一致的密钥轮换副作用出口。
    rotation_hook: Option<Arc<dyn Fn(Vec<u8>) + Send + Sync>>,
}

impl StateMachineStore {
    pub fn new(sm: Arc<Mutex<StateMachine>>, db: DbHandle) -> Self {
        Self::new_with_rotation(sm, db, None)
    }

    /// 构造状态机存储并挂载主密钥轮换钩子（集群模式由 dsh-cli 传入；dev-single 走 handler 本地逻辑，不挂）。
    pub fn new_with_rotation(
        sm: Arc<Mutex<StateMachine>>,
        db: DbHandle,
        hook: Option<Arc<dyn Fn(Vec<u8>) + Send + Sync>>,
    ) -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        Self {
            sm,
            db,
            current_snapshot: Arc::new(tokio::sync::Mutex::new(None)),
            events: tx,
            rotation_hook: hook,
        }
    }

    /// 订阅发布事件（集群 watch 用）。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PublishEvent> {
        self.events.subscribe()
    }

    /// 重启恢复判断（dsh-cli）：raft-meta 表非空即存在已落盘的 Raft 状态
    /// （vote / last_applied / last_purged / membership 任一存在即 true）。
    pub fn has_persisted_state(&self) -> bool {
        match self.raft_meta_is_empty() {
            Ok(empty) => !empty,
            Err(e) => {
                // 读失败按「无持久化状态」处理并告警——该方法仅用于重启恢复判断
                tracing::warn!("has_persisted_state: {e}");
                false
            }
        }
    }

    fn raft_meta_is_empty(&self) -> Result<bool, ErrOf> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e))?;
        let table = txn
            .open_table(TBL_RAFT_META)
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e))?;
        table
            .is_empty()
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e))
    }

    fn read_last_applied(&self) -> Result<Option<LogIdOf>, ErrOf> {
        meta_get(&self.db, META_LAST_APPLIED)?
            .map(|raw| de(&raw))
            .transpose()
    }

    fn write_last_applied(&self, id: &LogIdOf) -> Result<(), ErrOf> {
        // 每条独立事务（设计 §8.7：重启重放边界语义优先，不合并为批末单写）
        meta_put(&self.db, META_LAST_APPLIED, &ser(id)?)
    }

    fn read_membership(&self) -> Result<StoredMembership<NodeId, NodeInfo>, ErrOf> {
        match meta_get(&self.db, META_MEMBERSHIP)? {
            Some(raw) => de(&raw),
            None => Ok(StoredMembership::default()),
        }
    }

    fn write_membership(&self, m: &StoredMembership<NodeId, NodeInfo>) -> Result<(), ErrOf> {
        meta_put(&self.db, META_MEMBERSHIP, &ser(m)?)
    }
}

pub struct SnapshotBuilder {
    inner: Arc<StateMachineStore>,
}

impl RaftSnapshotBuilder<TypeConfig> for SnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, ErrOf> {
        let pairs = {
            let sm =
                self.inner.sm.lock().map_err(|e| {
                    io_err(ErrorSubject::StateMachine, ErrorVerb::Write, e.to_string())
                })?;
            sm.dump_all().map_err(storage_err)?
        };
        let last_applied = self.inner.read_last_applied()?;
        let membership = self.inner.read_membership()?;
        let data = serde_json::to_vec(&pairs)
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e.to_string()))?;
        let snapshot_id = format!(
            "{}-{}",
            last_applied
                .as_ref()
                .map(|l| l.index.to_string())
                .unwrap_or_default(),
            data.len()
        );
        let meta = SnapshotMeta {
            last_log_id: last_applied,
            last_membership: membership,
            snapshot_id,
        };
        let snapshot = Snapshot {
            meta: meta.clone(),
            snapshot: Box::new(Cursor::new(data.clone())),
        };
        // 落盘（重启后 get_current_snapshot 可从盘恢复，无需重新从 leader 拉全量）
        persist_snapshot(&self.inner.db, &meta, &data)?;
        *self.inner.current_snapshot.lock().await = Some((meta, data));
        Ok(snapshot)
    }
}

impl RaftStateMachine<TypeConfig> for StateMachineStore {
    type SnapshotBuilder = SnapshotBuilder;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogIdOf>, StoredMembership<NodeId, NodeInfo>), ErrOf> {
        let last_applied = self.read_last_applied()?;
        let membership = self.read_membership()?;
        Ok((last_applied, membership))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<Result<u64, DshError>>, ErrOf>
    where
        I: IntoIterator<Item = EntryOf> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let mut sm = self
            .sm
            .lock()
            .map_err(|e| io_err(ErrorSubject::StateMachine, ErrorVerb::Write, e.to_string()))?;
        let mut responses = Vec::new();
        for entry in entries {
            let log_id = entry.log_id;
            if let EntryPayload::Membership(m) = &entry.payload {
                let stored = StoredMembership::new(Some(log_id), m.clone());
                self.write_membership(&stored)?;
            }
            let mut resp = Ok(0u64);
            if let EntryPayload::Normal(cmd) = &entry.payload {
                // 确定性时间：用日志序号（避免墙钟导致跨节点状态发散，D16）
                let now_ms = log_id.index as i64;
                match sm.apply(cmd, now_ms) {
                    Ok(events) => {
                        resp = Ok(events.first().map(|e| e.version).unwrap_or(0));
                        // 事件广播（watch）：所有节点本地 apply 时推送，语义一致
                        for e in &events {
                            let _ = self.events.send(e.clone());
                        }
                        // 主密钥轮换副作用（更新本地 keyring + 持久化 ring 文件）：
                        // 状态机 apply 成功后才触发（钩子幂等，重放/多节点安全）。
                        if let dsh_core::command::Command::RotateMasterKey { kek } = cmd {
                            if let Some(h) = &self.rotation_hook {
                                h(kek.clone());
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("apply command failed (logged but state unchanged): {e}");
                        // 错误随 Raft 客户端响应返回（不再吞掉）
                        resp = Err(e);
                    }
                }
            }
            self.write_last_applied(&log_id)?;
            responses.push(resp);
        }
        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        SnapshotBuilder {
            inner: Arc::new(StateMachineStore {
                sm: self.sm.clone(),
                db: self.db.clone(),
                current_snapshot: Arc::new(tokio::sync::Mutex::new(None)),
                events: self.events.clone(),
                rotation_hook: self.rotation_hook.clone(),
            }),
        }
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Cursor<Vec<u8>>>, ErrOf> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, NodeInfo>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), ErrOf> {
        let data = snapshot.into_inner();
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = serde_json::from_slice(&data)
            .map_err(|e| io_err(ErrorSubject::StateMachine, ErrorVerb::Read, e.to_string()))?;
        {
            let sm = self
                .sm
                .lock()
                .map_err(|e| io_err(ErrorSubject::StateMachine, ErrorVerb::Write, e.to_string()))?;
            sm.restore_all(&pairs).map_err(storage_err)?;
        }
        if let Some(last_log_id) = &meta.last_log_id {
            self.write_last_applied(last_log_id)?;
        }
        self.write_membership(&meta.last_membership)?;
        // 安装的快照也落盘（重启后仍可用）
        persist_snapshot(&self.db, meta, &data)?;
        *self.current_snapshot.lock().await = Some((meta.clone(), data));
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<TypeConfig>>, ErrOf> {
        let guard = self.current_snapshot.lock().await;
        if let Some((meta, data)) = guard.as_ref() {
            return Ok(Some(Snapshot {
                meta: meta.clone(),
                snapshot: Box::new(Cursor::new(data.clone())),
            }));
        }
        drop(guard);
        // 重启后内存为空 → 读盘（B5：快照跨重启持久化）
        match load_persisted_snapshot(&self.db)? {
            Some((meta, data)) => Ok(Some(Snapshot {
                meta,
                snapshot: Box::new(Cursor::new(data)),
            })),
            None => Ok(None),
        }
    }
}

// ---------------- 内嵌单测（设计 §6 N4a：范围翻译+多表事务最易错路径） ----------------

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_storage::RedbStorage;
    use openraft::storage::RaftLogStorageExt;

    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("dsh-raft-store-{tag}-{}-{n}", std::process::id()))
    }

    fn open_storage(dir: &std::path::Path) -> RedbStorage {
        RedbStorage::open(&dir.display().to_string()).unwrap()
    }

    fn blank(index: u64) -> EntryOf {
        Entry {
            log_id: LogId {
                leader_id: LeaderId::new(1, 1),
                index,
            },
            payload: EntryPayload::Blank,
        }
    }

    fn truncate_at(index: u64) -> LogId<NodeId> {
        LogId {
            leader_id: LeaderId::new(1, 1),
            index,
        }
    }

    /// (last_purged, last_log) 索引对。
    async fn log_state_of(store: &mut LogStore) -> (Option<u64>, Option<u64>) {
        let st = store.get_log_state().await.unwrap();
        (
            st.last_purged_log_id.map(|l| l.index),
            st.last_log_id.map(|l| l.index),
        )
    }

    async fn indexes_of(store: &mut LogStore, range: std::ops::RangeInclusive<u64>) -> Vec<u64> {
        store
            .try_get_log_entries(range)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.log_id.index)
            .collect()
    }

    #[tokio::test]
    async fn append_truncate_get_log_state_consistent() {
        let dir = tmpdir("trunc");
        let _ = std::fs::remove_dir_all(&dir);
        let storage = open_storage(&dir);
        let mut store = LogStore::new(storage.raw_db());

        store
            .blocking_append([blank(1), blank(2), blank(3)])
            .await
            .unwrap();
        assert_eq!(log_state_of(&mut store).await, (None, Some(3)));
        assert_eq!(indexes_of(&mut store, 1..=3).await, vec![1, 2, 3]);

        // 截断 index >= 2
        store.truncate(truncate_at(2)).await.unwrap();
        assert_eq!(log_state_of(&mut store).await, (None, Some(1)));
        assert_eq!(indexes_of(&mut store, 1..=3).await, vec![1]);

        // 截到头：last_log_id 变 None
        store.truncate(truncate_at(1)).await.unwrap();
        assert_eq!(log_state_of(&mut store).await, (None, None));
        assert!(indexes_of(&mut store, 1..=3).await.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn purge_updates_last_purged_and_clears_prefix() {
        let dir = tmpdir("purge");
        let _ = std::fs::remove_dir_all(&dir);
        let storage = open_storage(&dir);
        let mut store = LogStore::new(storage.raw_db());

        store.blocking_append((1..=5).map(blank)).await.unwrap();
        store.purge(truncate_at(3)).await.unwrap();

        // last_purged=3；日志仅剩 4、5
        assert_eq!(log_state_of(&mut store).await, (Some(3), Some(5)));
        assert_eq!(indexes_of(&mut store, 1..=u64::MAX).await, vec![4, 5]);

        // 重复 purge（空 range）幂等
        store.purge(truncate_at(3)).await.unwrap();
        assert_eq!(log_state_of(&mut store).await, (Some(3), Some(5)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn snapshot_persist_load_roundtrip() {
        let dir = tmpdir("snap");
        let _ = std::fs::remove_dir_all(&dir);
        let storage = open_storage(&dir);

        // 安装快照（install_snapshot 内部落盘到 snapshots 表）
        let data = serde_json::to_vec(&vec![(b"k".to_vec(), b"v".to_vec())]).unwrap();
        let meta = SnapshotMeta {
            last_log_id: Some(LogId {
                leader_id: LeaderId::new(1, 1),
                index: 5,
            }),
            last_membership: StoredMembership::default(),
            snapshot_id: "5-2".to_string(),
        };
        {
            let sm = Arc::new(Mutex::new(StateMachine::new(Box::new(storage.clone()))));
            let mut store = StateMachineStore::new(sm, storage.raw_db());
            store
                .install_snapshot(&meta, Box::new(Cursor::new(data.clone())))
                .await
                .unwrap();
        }

        // 「重启」：全新 StateMachineStore（内存快照为空）→ 从盘恢复
        let sm = Arc::new(Mutex::new(StateMachine::new(Box::new(storage.clone()))));
        let mut revived = StateMachineStore::new(sm, storage.raw_db());
        let snap = revived
            .get_current_snapshot()
            .await
            .unwrap()
            .expect("snapshot should persist (B5)");
        assert_eq!(snap.meta.snapshot_id, meta.snapshot_id);
        assert_eq!(snap.meta.last_log_id, meta.last_log_id);
        assert_eq!(snap.snapshot.into_inner(), data);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn has_persisted_state_tracks_raft_meta() {
        let dir = tmpdir("meta");
        let _ = std::fs::remove_dir_all(&dir);
        let storage = open_storage(&dir);
        let sm = Arc::new(Mutex::new(StateMachine::new(Box::new(storage.clone()))));
        let mut store = StateMachineStore::new(sm, storage.raw_db());

        assert!(
            !store.has_persisted_state(),
            "fresh store: no persisted state"
        );
        // apply 一条空白日志 → last_applied 落盘（per-entry 独立事务）
        store.apply([blank(1)]).await.unwrap();
        assert!(store.has_persisted_state());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rotation_hook_fires_on_rotate_command_only() {
        let dir = tmpdir("rot");
        let _ = std::fs::remove_dir_all(&dir);
        let storage = open_storage(&dir);
        let sm = Arc::new(Mutex::new(StateMachine::new(Box::new(storage.clone()))));

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let hook_calls = calls.clone();
        let hook_received = received.clone();
        let mut store = StateMachineStore::new_with_rotation(
            sm.clone(),
            storage.raw_db(),
            Some(Arc::new(move |kek: Vec<u8>| {
                hook_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                hook_received.lock().unwrap().push(kek);
            })),
        );

        // RotateMasterKey 条目 → 钩子被调用且收到正确 kek
        let kek = vec![1u8; 32];
        let rotate = Entry {
            log_id: LogId {
                leader_id: LeaderId::new(1, 1),
                index: 1,
            },
            payload: EntryPayload::Normal(dsh_core::command::Command::RotateMasterKey {
                kek: kek.clone(),
            }),
        };
        store.apply([rotate]).await.unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "hook must fire exactly once for RotateMasterKey"
        );
        assert_eq!(
            *received.lock().unwrap(),
            vec![kek],
            "hook must receive the exact new KEK"
        );

        // 普通命令 → 钩子不再被调用
        let normal = Entry {
            log_id: LogId {
                leader_id: LeaderId::new(1, 1),
                index: 2,
            },
            payload: EntryPayload::Normal(dsh_core::command::Command::SessionLogout),
        };
        store.apply([normal]).await.unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "hook must NOT fire for non-rotate commands"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
