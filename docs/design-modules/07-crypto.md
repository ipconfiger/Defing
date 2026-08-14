# 模块 07 —— 加密层（dsh-crypto）

> 依据：design-v2 §7、schema/storage.v1.schema.json（Ciphertext）
> 版本：v1.0 ｜ 状态：开发就绪

## 1. 职责与边界
- 职责：AEAD 加解密、信封加密（KEK/DEK）、密文 wire 格式编解码、主密钥加载、轮换、脱敏。
- 不做：TLS（传输层）；KMS 插件（企业版接口预留）；业务规则。

## 2. 密钥层次与格式（design-v2 §7.2/§7.3）

```
trait KeyProvider: Send + Sync {
    fn kek(&self) -> Result<&[u8; 32]>;          // 主密钥（仅内存）
    fn kek_version(&self) -> u64;
}
enum KeySource { Env, File(PathBuf), Kms(Box<dyn KmsProvider>) }   // Kms 为企业版 trait

struct Ciphertext {                              // 对应 schema Ciphertext
    enc: Algorithm,                              // aes-256-gcm | chacha20-poly1305
    v: u32,                                      // 恒 1
    dek_v: u64,                                  // DEK 版本（轮换用）
    nonce: [u8; 12], ct: Vec<u8>,
    edek: Vec<u8>, edek_nonce: [u8; 12],         // KEK 加密的 DEK
}
```

## 3. 加解密 API

```
pub struct Cipher { keys: RwLock<KeyRing> }       // 当前 KEK + 旧 KEK 列表（轮换期间）
impl Cipher {
    fn encrypt_secret(&self, plain: &[u8]) -> Result<SecretBox>;   // 生成 DEK → AEAD → 包装 edek
    fn decrypt_secret(&self, sb: &SecretBox) -> Result<Vec<u8>>;   // 按 dek_v 选 KEK 解密
    fn rotate_master_key(&self, new_kek: [u8; 32]) -> Result<()>;  // 换 KEK；触发重包任务（模块 11）
    fn rewrap_dek(&self, sb: &SecretBox) -> Result<SecretBox>;     // 用当前 KEK 重包 edek
    fn mask(&self, sb: &SecretBox, show_first: usize) -> String;   // 脱敏展示
}
```

## 4. 主密钥加载与启动检查
- 来源：DSH_MASTER_KEY（base64 32B）/ --master-key-file（raw 32B 或 PEM）/ 口令（Argon2id，企业版）。
- 启动检查：存在 secret item 或共享项时无主密钥 → 拒绝启动；`--allow-no-master-key` 开发模式。
- 生成：`dsh admin gen-master-key` 输出 base64 + 保存指引（权限 0400）。

## 5. 轮换流程（不中断服务）
1. `dsh admin rotate-master-key`：加载新 KEK → 加入 KeyRing（新写用新 KEK）。
2. 后台任务（模块 11）遍历 secret 值：rewrap_dek 更新 edek（数据不重加密）。
3. 全部重包完成 → 旧 KEK 从 KeyRing 移除。
4. 轮换期间旧数据可解（KeyRing 保留旧 KEK 列表）。

## 6. 脱敏与审计
- mask：`show_first` 字符 + 掩码（如 `redis://***@host`）；管理面/导出默认脱敏。
- reveal=true 需会话 + 审计（模块 10）；解密/导出触发审计。

## 7. 测试要点（对应 design-v3 §5）
- CRY-001 磁盘无明文（写入后扫描存储目录） ｜ CRY-002 轮换后旧数据可解
- CRY-003 脱敏正确性；KAT 向量（aes-gcm 官方测试向量）；密文编解码往返（schema golden）

## 8. 任务清单
□ KeyProvider/KeySource □ Cipher（encrypt/decrypt/rewrap/mask） □ 密文 wire 编解码
□ 主密钥加载与启动检查 □ gen-master-key CLI □ rotate 流程 + KeyRing
□ KAT 向量测试 □ 磁盘无明文扫描测试
