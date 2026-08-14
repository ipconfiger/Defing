//! Raft 日志/状态机/快照存储（模块 03 §5）。
//! - LogStore：raft-log / raft-meta 列族（经 RocksStore::raw）
#![allow(clippy::result_large_err, clippy::type_complexity)] // openraft StorageError/RPCError 的 Err 变体较大（上游类型）

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use dsh_core::error::Error as DshError;
use dsh_core::model::PublishEvent;
use dsh_core::StateMachine;
use openraft::storage::{
    LogState, RaftLogReader, RaftLogStorage, RaftSnapshotBuilder, RaftStateMachine,
};
use openraft::{
    Entry, EntryPayload, ErrorSubject, ErrorVerb, LeaderId, LogId, OptionalSend, Snapshot,
    SnapshotMeta, StorageError, StorageIOError, StoredMembership, Vote,
};
use serde::{Deserialize, Serialize};

use crate::types::{NodeId, NodeInfo, TypeConfig};

type ErrOf = StorageError<NodeId>;

pub type EntryOf = Entry<TypeConfig>;
pub type LogIdOf = LogId<NodeId>;

// 公共存储句柄：共享 rocksdb（dsh-storage::RocksStore::raw）
pub type DbHandle = std::sync::Arc<rocksdb::DBWithThreadMode<rocksdb::SingleThreaded>>;

// ---------------- meta 键（raft-meta 列族） ----------------

const META_VOTE: &[u8] = b"vote";
const META_LAST_PURGED: &[u8] = b"last_purged";
const META_LAST_APPLIED: &[u8] = b"last_applied";
const META_MEMBERSHIP: &[u8] = b"membership";

// ---------------- 快照持久化（snapshots 列族；B5） ----------------

const SNAP_META_KEY: &[u8] = b"meta";
const SNAP_DATA_KEY: &[u8] = b"data";

fn snap_cf_of(db: &DbHandle) -> Result<rocksdb::ColumnFamilyRef<'_>, ErrOf> {
    db.cf_handle("snapshots")
        .ok_or_else(|| io_err(ErrorSubject::Store, ErrorVerb::Read, "missing snapshots cf"))
}

fn persist_snapshot(
    db: &DbHandle,
    meta: &SnapshotMeta<NodeId, NodeInfo>,
    data: &[u8],
) -> Result<(), ErrOf> {
    let cf = snap_cf_of(db)?;
    db.put_cf(&cf, SNAP_META_KEY, ser(meta)?)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e.to_string()))?;
    db.put_cf(&cf, SNAP_DATA_KEY, data)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e.to_string()))?;
    db.flush_cf(&cf)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e.to_string()))?;
    Ok(())
}

fn load_persisted_snapshot(
    db: &DbHandle,
) -> Result<Option<(SnapshotMeta<NodeId, NodeInfo>, Vec<u8>)>, ErrOf> {
    let cf = snap_cf_of(db)?;
    let meta_raw = db
        .get_cf(&cf, SNAP_META_KEY)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e.to_string()))?;
    let data = db
        .get_cf(&cf, SNAP_DATA_KEY)
        .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e.to_string()))?;
    match (meta_raw, data) {
        (Some(m), Some(d)) => Ok(Some((de(&m)?, d))),
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

fn log_cf_of(db: &DbHandle) -> Result<rocksdb::ColumnFamilyRef<'_>, ErrOf> {
    db.cf_handle("raft-log")
        .ok_or_else(|| io_err(ErrorSubject::Logs, ErrorVerb::Read, "missing raft-log cf"))
}

fn meta_cf_of(db: &DbHandle) -> Result<rocksdb::ColumnFamilyRef<'_>, ErrOf> {
    db.cf_handle("raft-meta")
        .ok_or_else(|| io_err(ErrorSubject::Store, ErrorVerb::Read, "missing raft-meta cf"))
}

/// 共享的日志读取逻辑（LogReader 与 LogStore 复用）。
fn read_entries(db: &DbHandle, start: u64, end: u64) -> Result<Vec<EntryOf>, ErrOf> {
    let cf = log_cf_of(db)?;
    let mut iter = db.raw_iterator_cf(&cf);
    iter.seek(log_key(start));
    let mut out = Vec::new();
    while iter.valid() {
        let k = iter.key().map(log_index_from_key).unwrap_or(0);
        if k > end {
            break;
        }
        let v = iter
            .value()
            .ok_or_else(|| io_err(ErrorSubject::LogIndex(k), ErrorVerb::Read, "nil value"))?;
        out.push(de(v)?);
        iter.next();
    }
    Ok(out)
}

// ---------------- LogStore ----------------

/// Raft 日志存储（raft-log 列族：key = 8B BE index）。
pub struct LogStore {
    db: DbHandle,
}

impl LogStore {
    pub fn new(db: DbHandle) -> Self {
        Self { db }
    }

    fn get_meta(&self, key: &[u8]) -> Result<Option<Vec<u8>>, ErrOf> {
        let cf = meta_cf_of(&self.db)?;
        self.db
            .get_cf(&cf, key)
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e.to_string()))
    }

    fn put_meta(&self, key: &[u8], value: &[u8]) -> Result<(), ErrOf> {
        let cf = meta_cf_of(&self.db)?;
        self.db
            .put_cf(&cf, key, value)
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e.to_string()))
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
        let last_purged = self
            .get_meta(META_LAST_PURGED)?
            .map(|raw| de::<u64>(&raw))
            .transpose()?
            .map(|idx| LogId {
                leader_id: LeaderId::new(0, NodeId::MAX),
                index: idx,
            });
        let entries = read_entries(&self.db, 0, u64::MAX)?;
        let last_log = entries.last().map(|e| e.log_id);
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
        self.put_meta(META_VOTE, &raw)
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, ErrOf> {
        self.get_meta(META_VOTE)?.map(|raw| de(&raw)).transpose()
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
        let cf = log_cf_of(&self.db)?;
        let mut batch = rocksdb::WriteBatch::default();
        for e in entries {
            let raw = ser(&e)?;
            batch.put_cf(&cf, log_key(e.log_id.index), raw);
        }
        self.db
            .write(batch)
            .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e.to_string()))?;
        self.db
            .flush_wal(true)
            .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e.to_string()))?;
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), ErrOf> {
        let cf = log_cf_of(&self.db)?;
        let mut iter = self.db.raw_iterator_cf(&cf);
        iter.seek(log_key(log_id.index));
        let mut batch = rocksdb::WriteBatch::default();
        while iter.valid() {
            let k = iter.key().map(|k| k.to_vec()).unwrap_or_default();
            batch.delete_cf(&cf, k);
            iter.next();
        }
        self.db
            .write(batch)
            .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e.to_string()))
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), ErrOf> {
        let cf = log_cf_of(&self.db)?;
        let mut iter = self.db.raw_iterator_cf(&cf);
        iter.seek_to_first();
        let mut batch = rocksdb::WriteBatch::default();
        while iter.valid() {
            let k = iter.key().map(|k| k.to_vec()).unwrap_or_default();
            if log_index_from_key(&k) <= log_id.index {
                batch.delete_cf(&cf, k);
            }
            iter.next();
        }
        self.db
            .write(batch)
            .map_err(|e| io_err(ErrorSubject::Logs, ErrorVerb::Write, e.to_string()))?;
        self.put_meta(META_LAST_PURGED, &ser(&log_id.index)?)
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
}

impl StateMachineStore {
    pub fn new(sm: Arc<Mutex<StateMachine>>, db: DbHandle) -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        Self {
            sm,
            db,
            current_snapshot: Arc::new(tokio::sync::Mutex::new(None)),
            events: tx,
        }
    }

    /// 订阅发布事件（集群 watch 用）。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PublishEvent> {
        self.events.subscribe()
    }

    fn get_meta(&self, key: &[u8]) -> Result<Option<Vec<u8>>, ErrOf> {
        let cf = meta_cf_of(&self.db)?;
        self.db
            .get_cf(&cf, key)
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Read, e.to_string()))
    }

    fn put_meta(&self, key: &[u8], value: &[u8]) -> Result<(), ErrOf> {
        let cf = meta_cf_of(&self.db)?;
        self.db
            .put_cf(&cf, key, value)
            .map_err(|e| io_err(ErrorSubject::Store, ErrorVerb::Write, e.to_string()))
    }

    fn read_last_applied(&self) -> Result<Option<LogIdOf>, ErrOf> {
        self.get_meta(META_LAST_APPLIED)?
            .map(|raw| de(&raw))
            .transpose()
    }

    fn write_last_applied(&self, id: &LogIdOf) -> Result<(), ErrOf> {
        self.put_meta(META_LAST_APPLIED, &ser(id)?)
    }

    fn read_membership(&self) -> Result<StoredMembership<NodeId, NodeInfo>, ErrOf> {
        match self.get_meta(META_MEMBERSHIP)? {
            Some(raw) => de(&raw),
            None => Ok(StoredMembership::default()),
        }
    }

    fn write_membership(&self, m: &StoredMembership<NodeId, NodeInfo>) -> Result<(), ErrOf> {
        self.put_meta(META_MEMBERSHIP, &ser(m)?)
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
