//! RocksDB 存储实现（模块 02）：列族划分 + Store trait 实现 + 备份。
//! 注意：rust-rocksdb 0.23 中 cf_handle 返回 ColumnFamilyRef（=&ColumnFamily），
//! 迭代器 key()/value() 返回 Option<&[u8]>。

use std::path::Path;
use std::sync::Arc;

use dsh_core::error::{Error, ErrorKind};
use dsh_core::store::Store;

/// 列族。
pub const CF_STATE: &str = "default"; // 状态机 KV（p/、sh/、idx/、sess/、audit/）
pub const CF_RAFT_LOG: &str = "raft-log";
pub const CF_RAFT_META: &str = "raft-meta";
pub const CF_SNAPSHOTS: &str = "snapshots";

/// RocksDB 打开选项。
pub struct OpenOptions {
    pub dir: String,
    pub max_open_files: i32,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            dir: "./data".into(),
            max_open_files: 512,
        }
    }
}

/// RocksDB Store（状态机 KV 全部落在 default 列族）。
pub struct RocksStore {
    db: Arc<rocksdb::DBWithThreadMode<rocksdb::SingleThreaded>>,
}

impl RocksStore {
    pub fn open(opts: &OpenOptions) -> Result<Self, Error> {
        let dir = Path::new(&opts.dir);
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::new(ErrorKind::Storage, format!("create dir: {e}")))?;
        let mut db_opts = rocksdb::Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        db_opts.set_atomic_flush(true);
        db_opts.set_max_open_files(opts.max_open_files);
        let cfs = [CF_STATE, CF_RAFT_LOG, CF_RAFT_META, CF_SNAPSHOTS];
        let db = rocksdb::DB::open_cf(&db_opts, dir, cfs)
            .map_err(|e| Error::new(ErrorKind::Storage, format!("open rocksdb: {e}")))?;
        Ok(Self { db: Arc::new(db) })
    }

    /// 供 Raft 日志/元数据访问（模块 03 使用）。
    pub fn raw(&self) -> Arc<rocksdb::DBWithThreadMode<rocksdb::SingleThreaded>> {
        self.db.clone()
    }

    fn cf_handle(&self, cf: &str) -> Result<rocksdb::ColumnFamilyRef<'_>, Error> {
        self.db
            .cf_handle(cf)
            .ok_or_else(|| Error::new(ErrorKind::Storage, format!("unknown cf {cf}")))
    }
}

impl Store for RocksStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let cf = self.cf_handle(CF_STATE)?;
        self.db
            .get_cf(&cf, key)
            .map_err(|e| Error::new(ErrorKind::Storage, format!("get: {e}")))
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        let cf = self.cf_handle(CF_STATE)?;
        self.db
            .put_cf(&cf, key, value)
            .map_err(|e| Error::new(ErrorKind::Storage, format!("put: {e}")))
    }

    fn delete(&self, key: &[u8]) -> Result<(), Error> {
        let cf = self.cf_handle(CF_STATE)?;
        self.db
            .delete_cf(&cf, key)
            .map_err(|e| Error::new(ErrorKind::Storage, format!("delete: {e}")))
    }

    fn get_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Error> {
        let cf = self.cf_handle(CF_STATE)?;
        let mut iter = self.db.raw_iterator_cf(&cf);
        iter.seek(prefix);
        let mut out = Vec::new();
        while iter.valid() {
            let k = iter.key().map(|k| k.to_vec()).unwrap_or_default();
            let v = iter.value().map(|v| v.to_vec()).unwrap_or_default();
            if !k.starts_with(prefix) {
                break;
            }
            out.push((k, v));
            iter.next();
        }
        Ok(out)
    }

    fn put_batch(&self, pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<(), Error> {
        let cf = self.cf_handle(CF_STATE)?;
        let mut batch = rocksdb::WriteBatch::default();
        for (k, v) in pairs {
            batch.put_cf(&cf, k, v);
        }
        self.db
            .write(batch)
            .map_err(|e| Error::new(ErrorKind::Storage, format!("batch: {e}")))
    }

    fn flush(&self) -> Result<(), Error> {
        let cf = self.cf_handle(CF_STATE)?;
        self.db
            .flush_cf(&cf)
            .map_err(|e| Error::new(ErrorKind::Storage, format!("flush: {e}")))
    }
}

/// 备份：RocksDB checkpoint（design-v2 §6 快照/备份）。
pub fn create_checkpoint(store: &RocksStore, path: &Path) -> Result<(), Error> {
    let db = store.raw();
    let ckpt = rocksdb::checkpoint::Checkpoint::new(&db)
        .map_err(|e| Error::new(ErrorKind::Storage, format!("checkpoint init: {e}")))?;
    ckpt.create_checkpoint(path)
        .map_err(|e| Error::new(ErrorKind::Storage, format!("checkpoint: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("dsh-storage-{tag}-{}-{n}", std::process::id()))
    }

    #[test]
    fn open_put_get_prefix() {
        let dir = tmpdir("basic");
        let _ = std::fs::remove_dir_all(&dir);
        let store = RocksStore::open(&OpenOptions {
            dir: dir.display().to_string(),
            max_open_files: 64,
        })
        .unwrap();
        store.put(b"p/x", b"1").unwrap();
        store.put(b"p/x/struct", b"2").unwrap();
        store.put(b"p/y", b"3").unwrap();
        assert_eq!(store.get(b"p/x").unwrap().unwrap(), b"1");
        let rows = store.get_prefix(b"p/x").unwrap();
        assert_eq!(rows.len(), 2);
        store
            .put_batch(&[
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
            ])
            .unwrap();
        assert_eq!(store.get(b"a").unwrap().unwrap(), b"1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_reopen_keeps_data() {
        let dir = tmpdir("crash");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let store = RocksStore::open(&OpenOptions {
                dir: dir.display().to_string(),
                max_open_files: 64,
            })
            .unwrap();
            store.put(b"k", b"v").unwrap();
            store.flush().unwrap();
        }
        {
            let store = RocksStore::open(&OpenOptions {
                dir: dir.display().to_string(),
                max_open_files: 64,
            })
            .unwrap();
            assert_eq!(store.get(b"k").unwrap().unwrap(), b"v");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_works() {
        let dir = tmpdir("ckpt");
        let ckpt_dir = tmpdir("ckpt-out");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&ckpt_dir);
        let store = RocksStore::open(&OpenOptions {
            dir: dir.display().to_string(),
            max_open_files: 64,
        })
        .unwrap();
        store.put(b"k", b"v").unwrap();
        store.flush().unwrap();
        create_checkpoint(&store, &ckpt_dir).unwrap();
        // checkpoint 可独立打开并读到数据
        let restored = RocksStore::open(&OpenOptions {
            dir: ckpt_dir.display().to_string(),
            max_open_files: 64,
        })
        .unwrap();
        assert_eq!(restored.get(b"k").unwrap().unwrap(), b"v");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&ckpt_dir);
    }
}
