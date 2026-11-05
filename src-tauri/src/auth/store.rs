//! 加密凭据存储：`~/.codemaker-hub-light/auth.bin`。
//!
//! 文件格式（docs/authentication.md）：
//! ```text
//! offset 0  4 bytes  "CHL1"      magic
//! offset 4  1 byte   version     = 1
//! offset 5  12 bytes nonce       AES-256-GCM nonce
//! offset 17 n bytes  ciphertext  AES-256-GCM(JSON(Credentials))，AAD = magic+version
//! ```
//!
//! 主密钥存在 OS 凭据库（Windows Credential Manager）：
//! service `codemaker-hub-light`、user `master-key`，内容为 32 字节密钥的 hex。
//! 取不到主密钥时**不**明文降级，直接返回错误。
//!
//! 并发：`auth.lock` 独占文件锁；写入为「临时文件 + fsync + rename」原子替换。
//! 安全：凭据结构不实现 `Serialize` 给前端；`Debug` 输出全部脱敏；日志不打印内容。

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 4] = b"CHL1";
const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = MAGIC.len() + 1;

const AUTH_FILE: &str = "auth.bin";
const LOCK_FILE: &str = "auth.lock";
const KEYRING_SERVICE: &str = "codemaker-hub-light";
const KEYRING_USER: &str = "master-key";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("凭据文件读写失败：{0}")]
    Io(#[from] io::Error),
    #[error("凭据序列化失败：{0}")]
    Json(#[from] serde_json::Error),
    #[error("凭据加密失败：{0}")]
    Crypto(String),
    #[error("凭据文件损坏、版本不支持或主密钥不匹配")]
    DecryptFailed,
    #[error("系统凭据库不可用：{0}")]
    Keyring(String),
    #[error("系统凭据库中没有主密钥，无法解密已有凭据文件")]
    KeyringMissing,
    /// 注销时凭据文件与主密钥没能全部删除（两侧都试过，不短路）。
    ///
    /// 主密钥**先删**，因此即使文件残留，那份密文也已不可解密（下次启动报
    /// [`Self::KeyringMissing`] 而不是静默自动登录）；这里仍要报错，因为
    /// "磁盘上留着可用凭据"是安全承诺的失败，必须让用户看见。
    #[error("凭据未完全删除（文件：{file}；主密钥：{key}）")]
    DeleteIncomplete { file: String, key: String },
    /// 阻塞线程池任务失败（`spawn_blocking` 的 JoinError）。
    ///
    /// 生产配置是 `panic = "abort"`，正常走不到这里；保留显式错误而不是 unwrap。
    #[error("凭据存储任务失败：{0}")]
    Join(String),
}

/// 落盘的凭据记录（只在内存与加密文件中存在，绝不形成日志/前端/DB 明文）。
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    /// 续期 key：`GET /get_auth_key` 返回的 128 字符值（**不是**深链里的 SSO token）。
    /// 旧版本可能存成 225 字符的 `access_sso_token`，续期成功后会原地升级。
    #[serde(default)]
    pub auth_key: Option<String>,
    pub user: String,
    #[serde(default)]
    pub server_url: String,
    #[serde(default)]
    pub issued_at: i64,
    #[serde(default)]
    pub expires_at: i64,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("access_token", &"<redacted>")
            .field("auth_key", &self.auth_key.as_ref().map(|_| "<redacted>"))
            .field("user", &self.user)
            .field("server_url", &self.server_url)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Credentials {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn auth_key(&self) -> Option<&str> {
        self.auth_key.as_deref()
    }
}

/// 主密钥来源；生产用系统凭据库，测试注入固定密钥。
pub trait MasterKeySource: Send + Sync {
    fn get(&self) -> Result<Option<[u8; KEY_LEN]>, StoreError>;
    fn get_or_create(&self) -> Result<[u8; KEY_LEN], StoreError>;
    fn delete(&self) -> Result<(), StoreError>;
}

/// OS 凭据库中的主密钥（Windows Credential Manager / macOS Keychain / Linux keyring）。
pub struct KeyringMasterKey {
    service: String,
    user: String,
}

impl KeyringMasterKey {
    pub fn new() -> Self {
        Self {
            service: KEYRING_SERVICE.to_string(),
            user: KEYRING_USER.to_string(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(&self.service, &self.user)
            .map_err(|err| StoreError::Keyring(err.to_string()))
    }
}

impl Default for KeyringMasterKey {
    fn default() -> Self {
        Self::new()
    }
}

impl MasterKeySource for KeyringMasterKey {
    fn get(&self) -> Result<Option<[u8; KEY_LEN]>, StoreError> {
        let entry = self.entry()?;
        match entry.get_password() {
            Ok(hex_key) => decode_key(&hex_key).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(StoreError::Keyring(err.to_string())),
        }
    }

    fn get_or_create(&self) -> Result<[u8; KEY_LEN], StoreError> {
        if let Some(key) = self.get()? {
            return Ok(key);
        }
        let key = random_key()?;
        let entry = self.entry()?;
        entry
            .set_password(&encode_key(&key))
            .map_err(|err| StoreError::Keyring(err.to_string()))?;
        Ok(key)
    }

    fn delete(&self) -> Result<(), StoreError> {
        let entry = self.entry()?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(StoreError::Keyring(err.to_string())),
        }
    }
}

/// 加密凭据文件。
///
/// `Clone` 只复制路径与 `Arc` 主密钥来源，供 `spawn_blocking` 把整段阻塞 IO 搬走；
/// 同步方法保留给测试与内部实现，生产调用方应使用 `*_async`。
#[derive(Clone)]
pub struct CredentialStore {
    path: PathBuf,
    lock_path: PathBuf,
    key_source: Arc<dyn MasterKeySource>,
    /// 写盘代次：`delete()` 每次自增，`save_if` 写盘前比对。
    ///
    /// 这把"注销之后才拿到锁的写盘"变成 no-op：`fs2` 排他锁把 `save_if` 与
    /// `delete` 串行化，所以"先比代次再写"这一步在锁内完成就不可能被插队。
    /// 为什么不用"提交失败就把刚写的文件删掉"：续期与"注销后重新登录"可以并发，
    /// 补删会误删新会话刚写出的凭据（且无法写出确定性测试）。
    generation: Arc<AtomicU64>,
}

impl CredentialStore {
    /// 生产构造：主密钥来自 OS 凭据库。
    pub fn new(data_dir: &Path) -> Self {
        Self::with_key_source(data_dir, Box::new(KeyringMasterKey::new()))
    }

    /// 测试构造：注入主密钥来源，避免污染真实凭据库。
    pub fn with_key_source(data_dir: &Path, key_source: Box<dyn MasterKeySource>) -> Self {
        Self {
            path: data_dir.join(AUTH_FILE),
            lock_path: data_dir.join(LOCK_FILE),
            key_source: Arc::from(key_source),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 当前写盘代次。要在**发盘之前**取一次，然后交给 [`Self::save_if`]。
    pub fn credential_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    /// 读取凭据；文件不存在返回 `None`，主密钥缺失返回明确错误（不会生成新密钥）。
    pub fn load(&self) -> Result<Option<Credentials>, StoreError> {
        self.locked(|| {
            let bytes = match fs::read(&self.path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(err) => return Err(StoreError::Io(err)),
            };
            let key = self.key_source.get()?.ok_or(StoreError::KeyringMissing)?;
            decrypt(&key, &bytes).map(Some)
        })
    }

    /// 加密写盘（原子替换）。
    pub fn save(&self, credentials: &Credentials) -> Result<(), StoreError> {
        self.locked(|| self.write_locked(credentials))
    }

    /// 带代次守卫的写盘：注销之后到达的写盘直接放弃（返回 `false`，一个字节都不写）。
    ///
    /// 调用方要在**发起写盘之前**取 [`Self::credential_generation`]。比对与写入都在
    /// `auth.lock` 内完成，而 `delete()` 也在同一把锁内自增代次，因此不存在
    /// "检查通过、然后注销、然后写回去" 的窗口。
    pub fn save_if(&self, generation: u64, credentials: &Credentials) -> Result<bool, StoreError> {
        self.locked(|| {
            if self.credential_generation() != generation {
                return Ok(false);
            }
            self.write_locked(credentials)?;
            Ok(true)
        })
    }

    /// 写入实体（必须在 [`Self::locked`] 内调用）。
    fn write_locked(&self, credentials: &Credentials) -> Result<(), StoreError> {
        let key = self.key_source.get_or_create()?;
        let plaintext = serde_json::to_vec(credentials)?;
        let blob = encrypt(&key, &plaintext)?;
        atomic_write(&self.path, &blob)
    }

    /// 删除凭据文件与主密钥。
    ///
    /// 两条纪律：
    /// - **先删主密钥**，且两侧都尝试、不短路：主密钥没了，残留密文就不可解密，
    ///   下次启动是"加载失败 + 需要重新登录"，而不是静默自动登录成上一个人。
    /// - 无论成败都推进写盘代次：注销这件事已经发生，任何在途续期都不能再写回来。
    pub fn delete(&self) -> Result<(), StoreError> {
        self.locked(|| {
            let key_result = self.key_source.delete();
            let file_result = match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(StoreError::Io(err)),
            };
            // 即使在锁内获取，代次也必须先推进再返回：调用方会依据失败做重试，
            // 重试期间到达的写盘同样必须作废。
            self.generation.fetch_add(1, Ordering::AcqRel);
            match (file_result, key_result) {
                (Ok(()), Ok(())) => Ok(()),
                (file, key) => Err(StoreError::DeleteIncomplete {
                    file: delete_state(file),
                    key: delete_state(key),
                }),
            }
        })
    }

    fn locked<T>(&self, action: impl FnOnce() -> Result<T, StoreError>) -> Result<T, StoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)?;
        lock.lock_exclusive()?;
        let result = action();
        let _ = FileExt::unlock(&lock);
        result
    }

    fn clone_for_blocking(&self) -> Self {
        self.clone()
    }

    /// 异步读盘：`fs2` 排他锁 + `fs::read` + keyring 取主密钥 + AES-GCM 解密
    /// 全部在阻塞线程池执行，避免占住 Tokio worker。
    pub async fn load_async(&self) -> Result<Option<Credentials>, StoreError> {
        let store = self.clone_for_blocking();
        tokio::task::spawn_blocking(move || store.load())
            .await
            .map_err(|err| StoreError::Join(err.to_string()))?
    }

    /// 异步写盘：临时文件 + fsync + rename 与主密钥创建都在阻塞线程池执行。
    pub async fn save_async(&self, credentials: &Credentials) -> Result<(), StoreError> {
        let store = self.clone_for_blocking();
        let credentials = credentials.clone();
        tokio::task::spawn_blocking(move || store.save(&credentials))
            .await
            .map_err(|err| StoreError::Join(err.to_string()))?
    }

    /// 带代次守卫的异步写盘；`Ok(false)` 表示会话已作废、没有写入。
    pub async fn save_async_if(
        &self,
        generation: u64,
        credentials: &Credentials,
    ) -> Result<bool, StoreError> {
        let store = self.clone_for_blocking();
        let credentials = credentials.clone();
        tokio::task::spawn_blocking(move || store.save_if(generation, &credentials))
            .await
            .map_err(|err| StoreError::Join(err.to_string()))?
    }

    /// 异步删除凭据文件与主密钥。
    pub async fn delete_async(&self) -> Result<(), StoreError> {
        let store = self.clone_for_blocking();
        tokio::task::spawn_blocking(move || store.delete())
            .await
            .map_err(|err| StoreError::Join(err.to_string()))?
    }
}

fn random_key() -> Result<[u8; KEY_LEN], StoreError> {
    let mut key = [0u8; KEY_LEN];
    getrandom::getrandom(&mut key)
        .map_err(|err| StoreError::Crypto(format!("无法生成主密钥：{err}")))?;
    Ok(key)
}

fn encode_key(key: &[u8; KEY_LEN]) -> String {
    let mut out = String::with_capacity(KEY_LEN * 2);
    for byte in key {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn decode_key(encoded: &str) -> Result<[u8; KEY_LEN], StoreError> {
    let encoded = encoded.trim();
    if encoded.len() != KEY_LEN * 2 {
        return Err(StoreError::Keyring("主密钥长度不是 32 字节".to_string()));
    }
    let mut key = [0u8; KEY_LEN];
    for (index, byte) in key.iter_mut().enumerate() {
        let slice = encoded
            .get(index * 2..index * 2 + 2)
            .ok_or_else(|| StoreError::Keyring("主密钥编码无效".to_string()))?;
        *byte = u8::from_str_radix(slice, 16)
            .map_err(|_| StoreError::Keyring("主密钥编码无效".to_string()))?;
    }
    Ok(key)
}

fn cipher(key: &[u8; KEY_LEN]) -> Result<Aes256Gcm, StoreError> {
    Aes256Gcm::new_from_slice(key).map_err(|err| StoreError::Crypto(err.to_string()))
}

fn encrypt(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes)
        .map_err(|err| StoreError::Crypto(format!("无法生成 nonce：{err}")))?;

    let header = [MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], VERSION];
    let ciphertext = cipher(key)?
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: plaintext,
                aad: &header,
            },
        )
        .map_err(|_| StoreError::Crypto("AES-GCM 加密失败".to_string()))?;

    let mut blob = Vec::with_capacity(HEADER_LEN + NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&header);
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

fn decrypt(key: &[u8; KEY_LEN], blob: &[u8]) -> Result<Credentials, StoreError> {
    if blob.len() < HEADER_LEN + NONCE_LEN + TAG_LEN {
        return Err(StoreError::DecryptFailed);
    }
    if &blob[..MAGIC.len()] != MAGIC {
        return Err(StoreError::DecryptFailed);
    }
    if blob[MAGIC.len()] != VERSION {
        return Err(StoreError::DecryptFailed);
    }

    let header = &blob[..HEADER_LEN];
    let nonce = &blob[HEADER_LEN..HEADER_LEN + NONCE_LEN];
    let ciphertext = &blob[HEADER_LEN + NONCE_LEN..];

    let plaintext = cipher(key)?
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: header,
            },
        )
        .map_err(|_| StoreError::DecryptFailed)?;

    Ok(serde_json::from_slice(&plaintext)?)
}

/// 把单侧删除结果压成一句可读状态，供 [`StoreError::DeleteIncomplete`] 展示。
fn delete_state(result: Result<(), StoreError>) -> String {
    match result {
        Ok(()) => "ok".to_string(),
        Err(err) => err.to_string(),
    }
}

/// 临时文件 + fsync + rename 的原子写；Unix 上权限 0600。
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut tmp_os = path.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);

    let write_result = (|| -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(StoreError::Io(err));
    }

    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(StoreError::Io(err));
    }
    Ok(())
}

/// 仅测试：固定主密钥来源。
#[cfg(test)]
pub(crate) struct StaticKey(pub [u8; KEY_LEN]);

#[cfg(test)]
impl MasterKeySource for StaticKey {
    fn get(&self) -> Result<Option<[u8; KEY_LEN]>, StoreError> {
        Ok(Some(self.0))
    }

    fn get_or_create(&self) -> Result<[u8; KEY_LEN], StoreError> {
        Ok(self.0)
    }

    fn delete(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chl-store-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("临时目录应可创建");
        dir
    }

    fn sample() -> Credentials {
        Credentials {
            access_token: "test-access-token-plaintext".to_string(),
            auth_key: Some("test-auth-key-plaintext".to_string()),
            user: "test-user".to_string(),
            server_url: "https://api.example.test".to_string(),
            issued_at: 1_700_000_000,
            expires_at: 1_700_086_400,
        }
    }

    fn store(dir: &Path, key_byte: u8) -> CredentialStore {
        CredentialStore::with_key_source(dir, Box::new(StaticKey([key_byte; KEY_LEN])))
    }

    #[test]
    fn roundtrip_and_no_plaintext_on_disk() {
        let dir = temp_dir("roundtrip");
        let store = store(&dir, 7);
        store.save(&sample()).expect("保存应成功");

        let blob = fs::read(store.path()).expect("文件应可读");
        assert_eq!(&blob[..4], MAGIC);
        assert_eq!(blob[4], VERSION);
        assert!(!blob
            .windows("test-access-token-plaintext".len())
            .any(|window| window == b"test-access-token-plaintext"));

        let loaded = store.load().expect("读取应成功").expect("应有凭据");
        assert_eq!(loaded.access_token(), "test-access-token-plaintext");
        assert_eq!(loaded.auth_key(), Some("test-auth-key-plaintext"));
        assert_eq!(loaded.user, "test-user");
        assert_eq!(loaded.expires_at, 1_700_086_400);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_key_cannot_decrypt() {
        let dir = temp_dir("wrong-key");
        store(&dir, 1).save(&sample()).expect("保存应成功");
        let error = store(&dir, 2).load().expect_err("错误主密钥应解密失败");
        assert!(matches!(error, StoreError::DecryptFailed));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_file_is_rejected() {
        let dir = temp_dir("tamper");
        let store = store(&dir, 3);
        store.save(&sample()).expect("保存应成功");
        let mut blob = fs::read(store.path()).expect("文件应可读");
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        fs::write(store.path(), &blob).expect("篡改写回应成功");
        assert!(matches!(
            store.load().expect_err("篡改应失败"),
            StoreError::DecryptFailed
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_is_none() {
        let dir = temp_dir("missing");
        let store = store(&dir, 4);
        assert!(store.load().expect("读取应成功").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_master_key_is_explicit_error() {
        struct NoKey;
        impl MasterKeySource for NoKey {
            fn get(&self) -> Result<Option<[u8; KEY_LEN]>, StoreError> {
                Ok(None)
            }
            fn get_or_create(&self) -> Result<[u8; KEY_LEN], StoreError> {
                Err(StoreError::KeyringMissing)
            }
            fn delete(&self) -> Result<(), StoreError> {
                Ok(())
            }
        }
        let dir = temp_dir("nokey");
        let store = CredentialStore::with_key_source(&dir, Box::new(NoKey));
        // 先用手写文件制造“存在凭据文件”的场景。
        fs::write(store.path(), b"CHL1\x01").expect("写文件应成功");
        assert!(matches!(
            store.load().expect_err("缺主密钥应报错"),
            StoreError::KeyringMissing
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_file() {
        let dir = temp_dir("delete");
        let store = store(&dir, 5);
        store.save(&sample()).expect("保存应成功");
        assert!(store.exists());
        store.delete().expect("删除应成功");
        assert!(!store.exists());
        assert!(store.load().expect("读取应成功").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_redacts_secrets() {
        let text = format!("{:?}", sample());
        assert!(!text.contains("test-access-token-plaintext"));
        assert!(!text.contains("test-auth-key-plaintext"));
        assert!(text.contains("<redacted>"));
        assert!(text.contains("test-user"));
    }

    #[test]
    fn key_hex_roundtrip() {
        let key = [0xabu8; KEY_LEN];
        let hex = encode_key(&key);
        assert_eq!(hex.len(), KEY_LEN * 2);
        assert_eq!(decode_key(&hex).expect("解码应成功"), key);
        assert!(decode_key("zz").is_err());
    }

    // -----------------------------------------------------------------------
    // 注销屏障：先删主密钥、两侧都试、写盘代次守卫
    // -----------------------------------------------------------------------

    /// 记录 `delete()` 调用的主密钥来源，用来证明"文件删不掉也照样删主密钥"。
    struct SpyKey {
        deleted: Arc<AtomicUsize>,
    }

    impl MasterKeySource for SpyKey {
        fn get(&self) -> Result<Option<[u8; KEY_LEN]>, StoreError> {
            Ok(Some([3u8; KEY_LEN]))
        }

        fn get_or_create(&self) -> Result<[u8; KEY_LEN], StoreError> {
            Ok([3u8; KEY_LEN])
        }

        fn delete(&self) -> Result<(), StoreError> {
            self.deleted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// 用同名目录占住 `auth.bin`，让 `fs::remove_file` 必然失败（Windows/Linux 一致）。
    fn occupy_auth_path(store: &CredentialStore) {
        if store.path().is_file() {
            fs::remove_file(store.path()).expect("先删掉真实文件");
        }
        fs::create_dir_all(store.path()).expect("目录应可创建");
    }

    /// 文件删不掉时主密钥仍必须被删：残留密文不可解密 ⇒ 下次启动需要重新登录。
    #[test]
    fn delete_removes_key_even_when_file_cannot_be_removed() {
        let dir = temp_dir("delete-partial");
        let deleted = Arc::new(AtomicUsize::new(0));
        let store = CredentialStore::with_key_source(
            &dir,
            Box::new(SpyKey {
                deleted: deleted.clone(),
            }),
        );
        occupy_auth_path(&store);

        let err = store.delete().expect_err("文件删不掉必须报错");
        assert!(
            matches!(err, StoreError::DeleteIncomplete { .. }),
            "应是部分删除失败：{err}"
        );
        assert_eq!(
            deleted.load(Ordering::SeqCst),
            1,
            "主密钥必须照样尝试删除（不能因文件失败而短路）"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 注销之前取到的代次，写盘必须被丢弃——这是"续期把凭据写回来"的正解。
    #[test]
    fn save_after_delete_is_a_noop() {
        let dir = temp_dir("generation");
        let store = store(&dir, 6);
        let generation = store.credential_generation();
        assert!(store
            .save_if(generation, &sample())
            .expect("首次写盘应成功"));
        assert!(store.exists());

        store.delete().expect("删除应成功");
        assert!(!store.exists());
        assert_ne!(
            store.credential_generation(),
            generation,
            "删除必须推进代次"
        );

        assert!(
            !store
                .save_if(generation, &sample())
                .expect("放弃写入不是错误"),
            "旧代次的写盘必须被放弃"
        );
        assert!(!store.exists(), "注销后到达的写盘不得复活凭据");

        // 新会话重新取代次后可以正常写盘（登录不会被守卫挡住）。
        let fresh = store.credential_generation();
        assert!(store.save_if(fresh, &sample()).expect("新代次应可写"));
        assert!(store.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 即使删除只成功一半，代次也必须推进：否则重试窗口里的写盘仍会复活凭据。
    #[test]
    fn delete_bumps_generation_even_on_partial_failure() {
        let dir = temp_dir("generation-partial");
        let store = store(&dir, 8);
        let generation = store.credential_generation();
        assert!(store.save_if(generation, &sample()).expect("保存应成功"));

        occupy_auth_path(&store);
        assert!(store.delete().is_err(), "文件删不掉应报错");
        assert_ne!(
            store.credential_generation(),
            generation,
            "部分失败也必须推进代次"
        );
        assert!(
            !store
                .save_if(generation, &sample())
                .expect("放弃写入不是错误"),
            "部分失败后的旧代次写盘同样必须被放弃"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
