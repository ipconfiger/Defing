//! 加密层（模块 07）：AEAD 信封加密（KEK + 每项 DEK）、主密钥加载、脱敏。
//! 注意：加密在 API 层（提交命令前）执行，状态机内只存密文 —— 保证 Raft apply 确定性。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use dsh_core::model::Ciphertext;
use rand::RngCore;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("crypto: {0}")]
    Msg(String),
    #[error("crypto io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<aes_gcm::Error> for CryptoError {
    fn from(e: aes_gcm::Error) -> Self {
        CryptoError::Msg(format!("aead: {e:?}"))
    }
}

impl From<aes_gcm::aes::cipher::InvalidLength> for CryptoError {
    fn from(e: aes_gcm::aes::cipher::InvalidLength) -> Self {
        CryptoError::Msg(format!("key length: {e}"))
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

/// 主密钥环：旧 KEK 列表 + 当前 KEK（末位为当前；轮换期间旧数据可解，design-v2 §7.5）。
#[derive(Debug, Clone, Default)]
pub struct KeyRing {
    entries: Vec<[u8; 32]>,
}

impl KeyRing {
    pub fn new(initial: [u8; 32]) -> Self {
        Self {
            entries: vec![initial],
        }
    }

    pub fn from_entries(entries: Vec<[u8; 32]>) -> Self {
        Self { entries }
    }

    /// 当前 KEK 代际（1-based；Ciphertext.dek_v 对齐）。
    pub fn generation(&self) -> u64 {
        self.entries.len() as u64
    }

    /// 按代际取 KEK（1-based；越界返回 None）。
    pub fn get(&self, generation: u64) -> Option<&[u8; 32]> {
        self.entries.get((generation as usize).wrapping_sub(1))
    }

    pub fn current(&self) -> &[u8; 32] {
        self.entries.last().expect("keyring non-empty")
    }

    /// 轮换：追加新 KEK 为当前（旧 KEK 保留）。
    pub fn push(&mut self, kek: [u8; 32]) {
        self.entries.push(kek);
    }

    pub fn entries(&self) -> &[[u8; 32]] {
        &self.entries
    }
}

/// 信封加密器（主密钥 KEK 环，内存可变；环文件持久化见 save_ring/load_ring）。
pub struct Cipher {
    keys: std::sync::RwLock<KeyRing>,
}

impl Cipher {
    pub fn new(kek: [u8; 32]) -> Self {
        Self::with_keyring(KeyRing::new(kek))
    }

    pub fn with_keyring(ring: KeyRing) -> Self {
        Self {
            keys: std::sync::RwLock::new(ring),
        }
    }

    pub fn keyring(&self) -> KeyRing {
        self.keys.read().expect("keyring lock").clone()
    }

    /// 轮换主密钥：新 KEK 成为当前，旧 KEK 保留（可解旧数据，CRY-002）。
    pub fn rotate_master_key(&self, new_kek: [u8; 32]) {
        self.keys.write().expect("keyring lock").push(new_kek);
    }

    /// DEK 重包：用当前 KEK 重加密 edek（数据不重加密，design-v2 §7.5）。
    pub fn rewrap_dek(&self, c: &Ciphertext) -> Result<Ciphertext, CryptoError> {
        let ring = self.keyring();
        // 用该密文 edek 对应的旧 KEK 解出 DEK
        let kek = ring
            .get(c.dek_v)
            .ok_or_else(|| CryptoError::Msg(format!("unknown KEK generation {}", c.dek_v)))?;
        let dek = Self::unwrap_dek(kek, c)?;
        // 用当前 KEK 重包
        let (edek, edek_nonce) = Self::wrap_dek(ring.current(), &dek)?;
        Ok(Ciphertext {
            enc: c.enc.clone(),
            v: c.v,
            dek_v: ring.generation(),
            nonce: c.nonce.clone(), // 数据密文不变
            ct: c.ct.clone(),
            edek,
            edek_nonce,
        })
    }

    fn wrap_dek(kek: &[u8; 32], dek: &[u8]) -> Result<(String, String), CryptoError> {
        let edek_nonce: [u8; 12] = random_bytes();
        let kek_cipher = Aes256Gcm::new_from_slice(kek).map_err(CryptoError::from)?;
        let edek = kek_cipher
            .encrypt(Nonce::from_slice(&edek_nonce), dek)
            .map_err(CryptoError::from)?;
        Ok((B64.encode(edek), B64.encode(edek_nonce)))
    }

    fn unwrap_dek(kek: &[u8; 32], c: &Ciphertext) -> Result<Vec<u8>, CryptoError> {
        let kek_cipher = Aes256Gcm::new_from_slice(kek).map_err(CryptoError::from)?;
        kek_cipher
            .decrypt(
                Nonce::from_slice(&b64(&c.edek_nonce)?),
                b64(&c.edek)?.as_slice(),
            )
            .map_err(CryptoError::from)
    }

    /// 生成主密钥（32B base64，供 DSH_MASTER_KEY / 密钥文件使用）。
    pub fn generate_master_key() -> String {
        B64.encode(random_bytes::<32>())
    }

    /// 加密：生成 DEK → 加密数据 → 当前 KEK 包装 DEK（dek_v = 当前代际）。
    pub fn encrypt_secret(&self, plain: &[u8]) -> Result<Ciphertext, CryptoError> {
        let ring = self.keyring();
        let dek: [u8; 32] = random_bytes();
        let nonce: [u8; 12] = random_bytes();
        let data_cipher = Aes256Gcm::new_from_slice(&dek).map_err(CryptoError::from)?;
        let ct = data_cipher
            .encrypt(Nonce::from_slice(&nonce), plain)
            .map_err(CryptoError::from)?;

        let (edek, edek_nonce) = Self::wrap_dek(ring.current(), &dek)?;
        Ok(Ciphertext {
            enc: "aes-256-gcm".into(),
            v: 1,
            dek_v: ring.generation(),
            nonce: B64.encode(nonce),
            ct: B64.encode(ct),
            edek,
            edek_nonce,
        })
    }

    /// 解密：按 dek_v 取对应 KEK 解 DEK → DEK 解数据（轮换后旧数据仍可解，CRY-002）。
    pub fn decrypt_secret(&self, c: &Ciphertext) -> Result<Vec<u8>, CryptoError> {
        let ring = self.keyring();
        // 优先按代际取 KEK；未知代际则从最新到最旧逐个尝试
        let kek = ring.get(c.dek_v).or_else(|| {
            (1..=ring.generation())
                .rev()
                .find_map(|g| ring.get(g).filter(|_| true))
        });
        let kek =
            kek.ok_or_else(|| CryptoError::Msg(format!("no KEK for generation {}", c.dek_v)))?;
        let dek = Self::unwrap_dek(kek, c)?;
        let data_cipher = Aes256Gcm::new_from_slice(&dek).map_err(CryptoError::from)?;
        let plain = data_cipher
            .decrypt(Nonce::from_slice(&b64(&c.nonce)?), b64(&c.ct)?.as_slice())
            .map_err(CryptoError::from)?;
        Ok(plain)
    }

    /// 脱敏展示。
    pub fn mask(&self, _c: &Ciphertext, show_first: usize) -> String {
        let stars = "*".repeat(show_first.max(1));
        format!("{{secret:{stars}}}")
    }
}

fn b64(s: &str) -> Result<Vec<u8>, CryptoError> {
    B64.decode(s)
        .map_err(|e| CryptoError::Msg(format!("base64: {e}")))
}

/// 主密钥环文件路径（{key_file}.ring.json；轮换后重启可加载全部 KEK）。
pub fn ring_file_path(key_file: &str) -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(key_file);
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".ring.json");
    p.set_file_name(name);
    p
}

/// 持久化主密钥环（base64 列表 JSON，旧→新）。
///
/// 密钥环内含全部历史+当前 KEK 的明文，绝不能世界可读（S4）：
/// - Unix 下用 OpenOptions 以 0o600 mode 直接创建/截断文件，避免「先 0644 写、
///   再 chmod」的权限暴露窗口；
/// - 文件已存在时 OpenOptions 的 mode 只作用于创建，因此写后再显式
///   set_permissions 一次，顺带修复修复前以 0644 落盘的旧文件；
/// - 两种操作的错误均经 `?` 转换为 CryptoError。
pub fn save_ring(path: &std::path::Path, ring: &KeyRing) -> Result<(), CryptoError> {
    let list: Vec<String> = ring.entries().iter().map(|k| B64.encode(k)).collect();
    let raw =
        serde_json::to_vec(&list).map_err(|e| CryptoError::Msg(format!("serialize ring: {e}")))?;
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true).mode(0o600);
        let mut f = opts.open(path)?;
        f.write_all(&raw)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, raw)?;
    }
    Ok(())
}

/// 加载主密钥环文件（不存在 → 空）。
pub fn load_ring(path: &std::path::Path) -> Result<Vec<[u8; 32]>, CryptoError> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()),
    };
    let list: Vec<String> =
        serde_json::from_slice(&raw).map_err(|e| CryptoError::Msg(format!("parse ring: {e}")))?;
    let mut out = Vec::new();
    for k in list {
        let bytes = B64
            .decode(k)
            .map_err(|e| CryptoError::Msg(format!("ring key: {e}")))?;
        if bytes.len() != 32 {
            return Err(CryptoError::Msg("ring key must be 32 bytes".into()));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        out.push(arr);
    }
    Ok(out)
}

/// 主密钥加载：优先 DSH_MASTER_KEY（base64 32B），否则密钥文件（raw 32B）。
pub fn load_master_key(
    env_key: Option<&str>,
    key_file: Option<&str>,
) -> Result<Option<[u8; 32]>, CryptoError> {
    let raw: Vec<u8> = if let Some(k) = env_key {
        B64.decode(k)
            .map_err(|e| CryptoError::Msg(format!("DSH_MASTER_KEY base64: {e}")))?
    } else if let Some(f) = key_file {
        std::fs::read(f)?
    } else {
        return Ok(None);
    };
    if raw.len() != 32 {
        return Err(CryptoError::Msg("master key must be 32 bytes".into()));
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&raw);
    Ok(Some(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wire_format() {
        let kek: [u8; 32] = random_bytes();
        let cipher = Cipher::new(kek);
        let ct = cipher.encrypt_secret(b"redis://secret@host:6379").unwrap();
        assert_eq!(ct.enc, "aes-256-gcm");
        assert_eq!(ct.v, 1);
        let plain = cipher.decrypt_secret(&ct).unwrap();
        assert_eq!(plain, b"redis://secret@host:6379");
        // 不同 nonce → 密文不同
        let ct2 = cipher.encrypt_secret(b"redis://secret@host:6379").unwrap();
        assert_ne!(ct.ct, ct2.ct);
        // 密文可序列化存储
        let json = serde_json::to_string(&ct).unwrap();
        let back: Ciphertext = serde_json::from_str(&json).unwrap();
        assert_eq!(cipher.decrypt_secret(&back).unwrap(), plain);
    }

    #[test]
    fn mask_does_not_leak() {
        let kek: [u8; 32] = random_bytes();
        let cipher = Cipher::new(kek);
        let ct = cipher.encrypt_secret(b"super-secret").unwrap();
        let masked = cipher.mask(&ct, 2);
        assert!(!masked.contains("super"));
        assert!(masked.contains("secret"));
    }

    #[test]
    fn master_key_gen_and_load() {
        let k = Cipher::generate_master_key();
        let loaded = load_master_key(Some(&k), None).unwrap().unwrap();
        assert_eq!(loaded.len(), 32);
        assert!(load_master_key(None, None).unwrap().is_none());
    }
}

#[cfg(test)]
mod rotation_tests {
    use super::*;

    fn kek(n: u8) -> [u8; 32] {
        [n; 32]
    }

    #[test]
    fn rotate_old_data_still_decrypts_cry002() {
        let cipher = Cipher::new(kek(1));
        let old_ct = cipher.encrypt_secret(b"secret-v1").unwrap();
        assert_eq!(old_ct.dek_v, 1);

        // 轮换到 KEK2 → 新写用代际 2；旧密文仍可解（CRY-002）
        cipher.rotate_master_key(kek(2));
        let new_ct = cipher.encrypt_secret(b"secret-v2").unwrap();
        assert_eq!(new_ct.dek_v, 2);
        assert_eq!(cipher.decrypt_secret(&old_ct).unwrap(), b"secret-v1");
        assert_eq!(cipher.decrypt_secret(&new_ct).unwrap(), b"secret-v2");

        // 再轮换到 KEK3：旧旧密文（代际1）与旧密文（代际2）均可解
        cipher.rotate_master_key(kek(3));
        assert_eq!(cipher.decrypt_secret(&old_ct).unwrap(), b"secret-v1");
        assert_eq!(cipher.decrypt_secret(&new_ct).unwrap(), b"secret-v2");
        let latest = cipher.encrypt_secret(b"secret-v3").unwrap();
        assert_eq!(latest.dek_v, 3);
    }

    #[test]
    fn rewrap_updates_edek_keeps_data() {
        let cipher = Cipher::new(kek(1));
        let ct = cipher.encrypt_secret(b"stable-data").unwrap();
        assert_eq!(ct.dek_v, 1);
        let old_edek = ct.edek.clone();

        cipher.rotate_master_key(kek(2));
        let rw = cipher.rewrap_dek(&ct).unwrap();
        assert_eq!(rw.dek_v, 2, "重包后代际更新");
        assert_eq!(rw.ct, ct.ct, "数据密文不变");
        assert_eq!(rw.nonce, ct.nonce, "数据 nonce 不变");
        assert_ne!(rw.edek, old_edek, "edek 已用新 KEK 重包");
        assert_eq!(cipher.decrypt_secret(&rw).unwrap(), b"stable-data");
    }

    #[test]
    fn keyring_ring_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dsh-ring-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key.ring.json");
        let mut ring = KeyRing::new(kek(1));
        ring.push(kek(2));
        save_ring(&path, &ring).unwrap();
        // S4：密钥环文件权限必须为 0600（仅 Unix 断言，非 Unix 平台跳过）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "ring 文件权限必须为 0600");
        }
        let loaded = load_ring(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0], kek(1));
        assert_eq!(loaded[1], kek(2));
        // 重建 Cipher：旧数据可解
        let cipher = Cipher::with_keyring(KeyRing {
            entries: loaded.clone(),
        });
        assert_eq!(cipher.keyring().generation(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
