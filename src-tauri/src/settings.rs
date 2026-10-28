//! 用户设置：`~/.codemaker-hub-light/settings.json` 的原子读写。
//!
//! 约定（docs/settings.md、§9）：
//! - 只保存用户改过的配置；缺省值由 [`Settings::default`] 提供。
//! - 写入采用「临时文件 + fsync + rename」，避免半截文件。
//! - 代理实际端口另外落盘到 `proxy.port`（纯文本），供外部 CLI 读取。
//! - IPC 字段与前端 `src/lib/types.ts` 的 `AppSettings` 对齐（snake_case）。

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// 应用数据目录名（位于用户主目录下）。
pub const APP_DIR_NAME: &str = ".codemaker-hub-light";
/// 默认网关地址：占位值，首次使用需在「设置 → 代理与记录」中填写可访问的兼容网关。
pub const DEFAULT_GATEWAY_URL: &str = "https://api.example.com";
/// 代理默认端口；占用时回退 15722..=15800，再失败交给系统分配。
pub const DEFAULT_PROXY_PORT: u16 = 15721;
pub const PROXY_PORT_FALLBACK_END: u16 = 15800;

const SETTINGS_FILE: &str = "settings.json";
const PROXY_PORT_FILE: &str = "proxy.port";

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("无法定位用户主目录（USERPROFILE / HOME 均未设置）")]
    HomeDirUnavailable,
    #[error("设置不合法：{0}")]
    Invalid(String),
    #[error("读写设置文件失败：{0}")]
    Io(#[from] io::Error),
    #[error("解析设置文件失败：{0}")]
    Json(#[from] serde_json::Error),
}

/// 应用设置。字段名与前端 `AppSettings` 对齐（snake_case）。
///
/// 旧版 `settings.json` 里已废弃的 `quota_refresh_secs` / `validate_interval_secs` /
/// `log_level` 会被 serde 忽略（结构体不加 `deny_unknown_fields`），因此升级后
/// 无需迁移文件；这些配置此前从未被运行时读取，删除不改变行为。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// API 网关地址。
    pub gateway_url: String,
    /// 代理监听地址，只允许回环地址。
    pub proxy_bind: String,
    /// 代理首选端口。
    pub proxy_port: u16,
    /// 是否把请求/响应正文写入数据库（默认关闭，隐私优先）。
    /// 兼容旧文件里的 `recordBodies`。
    #[serde(alias = "recordBodies", alias = "recordBody")]
    pub record_body: bool,
    /// 用量数据保留天数。
    pub retention_days: u32,
    /// `aigw.cost.total_credit` → 人民币元的换算系数。
    pub credit_to_yuan: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            gateway_url: DEFAULT_GATEWAY_URL.to_string(),
            proxy_bind: "127.0.0.1".to_string(),
            proxy_port: DEFAULT_PROXY_PORT,
            record_body: false,
            retention_days: 90,
            credit_to_yuan: 0.01,
        }
    }
}

impl Settings {
    /// 读取设置；文件不存在时返回默认值（不创建文件）。
    pub fn load(data_dir: &Path) -> Result<Self, SettingsError> {
        let path = settings_path(data_dir);
        match fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(SettingsError::Io(err)),
        }
    }

    /// 启动期加载：解析失败或校验失败时备份原文件并回退默认值，避免"打不开应用"。
    ///
    /// 自愈而不是报错的原因：`settings.json` 是用户可见、可手改的文件，
    /// 一个手滑的 `credit_to_yuan: 0` 不该让整个应用无法启动。
    pub fn load_or_recover(data_dir: &Path) -> Self {
        let loaded = Self::load(data_dir).map(|mut settings| {
            settings.normalize();
            settings
        });
        match loaded {
            Ok(settings) => match settings.validate() {
                Ok(()) => settings,
                Err(err) => Self::backup_and_default(data_dir, &err.to_string()),
            },
            Err(err) => Self::backup_and_default(data_dir, &err.to_string()),
        }
    }

    /// 把有问题的设置文件改名留档，并回退默认值。
    fn backup_and_default(data_dir: &Path, reason: &str) -> Self {
        let path = settings_path(data_dir);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let backup = data_dir.join(format!("{SETTINGS_FILE}.bad-{stamp}"));
        let backed_up = fs::rename(&path, &backup).is_ok();
        tracing::warn!(
            reason,
            backup = %backup.display(),
            backed_up,
            "设置文件不可用，已回退默认值"
        );
        Self::default()
    }

    /// 校验设置。只允许回环绑定（docs/settings.md）。
    pub fn validate(&self) -> Result<(), SettingsError> {
        if !gateway_url_is_acceptable(&self.gateway_url) {
            return Err(SettingsError::Invalid(
                "gateway_url 必须是 https:// 地址；http:// 仅允许本机回环（本地调试）".to_string(),
            ));
        }
        if !is_loopback_bind(self.proxy_bind.trim()) {
            return Err(SettingsError::Invalid(
                "proxy_bind 只允许 127.0.0.1 / localhost / ::1".to_string(),
            ));
        }
        if self.proxy_port < 1024 {
            return Err(SettingsError::Invalid(
                "proxy_port 必须是 1024..=65535 之间的端口".to_string(),
            ));
        }
        if !(1..=3650).contains(&self.retention_days) {
            return Err(SettingsError::Invalid(
                "retention_days 必须在 1..=3650 之间".to_string(),
            ));
        }
        if !self.credit_to_yuan.is_finite() || self.credit_to_yuan <= 0.0 {
            return Err(SettingsError::Invalid(
                "credit_to_yuan 必须是大于 0 的有限数值".to_string(),
            ));
        }
        Ok(())
    }

    /// 规范化可变字段（去空格、去掉网关尾部 `/`），在保存前调用。
    pub fn normalize(&mut self) {
        self.gateway_url = self.gateway_url.trim().trim_end_matches('/').to_string();
        self.proxy_bind = self.proxy_bind.trim().to_string();
    }

    /// 合并前端提交的局部设置（未携带的字段保持原值）。
    pub fn apply_patch(&mut self, patch: SettingsPatch) {
        if let Some(value) = patch.gateway_url {
            self.gateway_url = value;
        }
        if let Some(value) = patch.proxy_bind {
            self.proxy_bind = value;
        }
        if let Some(value) = patch.proxy_port {
            self.proxy_port = value;
        }
        if let Some(value) = patch.record_body {
            self.record_body = value;
        }
        if let Some(value) = patch.retention_days {
            self.retention_days = value;
        }
        if let Some(value) = patch.credit_to_yuan {
            self.credit_to_yuan = value;
        }
    }

    /// 原子写入设置文件。
    pub fn save(&self, data_dir: &Path) -> Result<(), SettingsError> {
        ensure_dir(data_dir)?;
        let bytes = serde_json::to_vec_pretty(self)?;
        atomic_write(&settings_path(data_dir), &bytes)
    }
}

/// 前端 `settings_update` 提交的局部补丁；未出现的字段保持不变。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SettingsPatch {
    pub gateway_url: Option<String>,
    pub proxy_bind: Option<String>,
    pub proxy_port: Option<u16>,
    #[serde(alias = "recordBodies", alias = "recordBody")]
    pub record_body: Option<bool>,
    pub retention_days: Option<u32>,
    pub credit_to_yuan: Option<f64>,
}

/// 判断绑定地址是否为回环地址。
pub fn is_loopback_bind(bind: &str) -> bool {
    matches!(bind.trim(), "127.0.0.1" | "localhost" | "::1")
}

/// 网关地址是否可接受：`https://` 允许任意主机；`http://` 只允许本机回环。
///
/// 账号凭据以请求头形式发往网关，明文 http 会把 token 暴露在同网段链路上，
/// 因此除本地 mock/自测外一律要求 https（见 docs/gateway-contract.md）。
pub fn gateway_url_is_acceptable(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.starts_with("https://") {
        return true;
    }
    let Some(rest) = trimmed.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // 去掉端口（含 IPv6 的 `[::1]:8080` 形式）。
    let host = match authority.rsplit_once(':') {
        Some((head, port)) if port.chars().all(|c| c.is_ascii_digit()) => head,
        _ => authority,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// 用户主目录（Windows 优先 USERPROFILE，类 Unix 用 HOME）。
pub fn home_dir() -> Result<PathBuf, SettingsError> {
    if let Some(dir) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Err(SettingsError::HomeDirUnavailable)
}

/// 默认数据目录：`~/.codemaker-hub-light`。
pub fn default_data_dir() -> Result<PathBuf, SettingsError> {
    Ok(home_dir()?.join(APP_DIR_NAME))
}

/// 创建数据目录（幂等）。
pub fn ensure_dir(dir: &Path) -> Result<(), SettingsError> {
    fs::create_dir_all(dir)?;
    Ok(())
}

pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SETTINGS_FILE)
}

pub fn proxy_port_path(data_dir: &Path) -> PathBuf {
    data_dir.join(PROXY_PORT_FILE)
}

/// 写 `proxy.port`：内容为实际监听端口（纯文本），供 CLI 发现。
pub fn write_proxy_port(data_dir: &Path, port: u16) -> Result<(), SettingsError> {
    ensure_dir(data_dir)?;
    atomic_write(&proxy_port_path(data_dir), port.to_string().as_bytes())
}

/// 临时文件 + fsync + rename 的原子写。
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SettingsError> {
    let mut tmp_os = path.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);

    let write_result = (|| -> io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(SettingsError::Io(err));
    }

    // Windows 的 rename 覆盖已存在文件；失败则清理临时文件。
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(SettingsError::Io(err));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_are_valid() {
        Settings::default().validate().expect("默认设置必须合法");
    }

    #[test]
    fn rejects_non_loopback_bind() {
        let settings = Settings {
            proxy_bind: "0.0.0.0".to_string(),
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn rejects_bad_gateway() {
        let settings = Settings {
            gateway_url: "api.example.test".to_string(),
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        let mut settings = Settings {
            gateway_url: " https://example.test/ ".to_string(),
            ..Settings::default()
        };
        settings.normalize();
        assert_eq!(settings.gateway_url, "https://example.test");
    }

    fn temp_settings_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chl-settings-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("临时目录应可创建");
        dir
    }

    /// 启动期自愈：坏 JSON 不能让应用打不开，必须备份 + 回默认值。
    #[test]
    fn corrupt_settings_recover_to_defaults_with_backup() {
        let dir = temp_settings_dir();
        fs::write(settings_path(&dir), b"{ this is not json").expect("写入坏文件应成功");

        let settings = Settings::load_or_recover(&dir);

        assert_eq!(settings.credit_to_yuan, Settings::default().credit_to_yuan);
        assert!(!settings_path(&dir).exists(), "坏文件应被移走");
        let backups: Vec<String> = fs::read_dir(&dir)
            .expect("目录应可读")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("settings.json.bad-"))
            .collect();
        assert_eq!(backups.len(), 1, "应留下一个备份文件：{backups:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 合法 JSON 但校验失败（手改出的 0 折算、非回环明文网关）同样自愈。
    #[test]
    fn invalid_settings_recover_to_defaults() {
        let dir = temp_settings_dir();
        let bad = Settings {
            credit_to_yuan: 0.0,
            gateway_url: "http://intranet.example.com".to_string(),
            ..Settings::default()
        };
        fs::write(
            settings_path(&dir),
            serde_json::to_vec(&bad).expect("序列化应成功"),
        )
        .expect("写入设置应成功");

        let recovered = Settings::load_or_recover(&dir);

        assert_eq!(recovered.gateway_url, DEFAULT_GATEWAY_URL);
        assert_eq!(recovered.credit_to_yuan, Settings::default().credit_to_yuan);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_gateway_allowed_only_for_loopback() {
        assert!(gateway_url_is_acceptable("https://api.example.test"));
        assert!(gateway_url_is_acceptable("http://127.0.0.1:15721"));
        assert!(gateway_url_is_acceptable("http://localhost:8080"));
        assert!(gateway_url_is_acceptable("http://[::1]:8080"));

        assert!(!gateway_url_is_acceptable("http://api.example.test"));
        assert!(!gateway_url_is_acceptable("http://10.0.0.5:8080"));
        assert!(!gateway_url_is_acceptable("api.example.test"));
        assert!(!gateway_url_is_acceptable("ftp://example.com"));
    }

    #[test]
    fn patch_only_changes_present_fields() {
        let mut settings = Settings {
            credit_to_yuan: 0.5,
            ..Settings::default()
        };
        let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
            "proxy_port": 16000,
            "gateway_url": "https://example.test/",
            "record_body": true,
            "retention_days": 7
        }))
        .expect("前端补丁应可反序列化");
        settings.apply_patch(patch);
        settings.normalize();

        assert_eq!(settings.proxy_port, 16000);
        assert_eq!(settings.gateway_url, "https://example.test");
        assert!(settings.record_body);
        assert_eq!(settings.retention_days, 7);
        // 未提交的字段保持原值。
        assert_eq!(settings.credit_to_yuan, 0.5);
    }

    /// 旧版 settings.json 里已删除的三个配置项（保存但从不读取）必须被忽略，
    /// 升级不要求用户手改文件。
    #[test]
    fn legacy_removed_fields_are_ignored() {
        let dir = temp_settings_dir();
        fs::write(
            settings_path(&dir),
            br#"{
                "gateway_url": "https://example.test/",
                "quota_refresh_secs": 300,
                "validate_interval_secs": 600,
                "log_level": "debug"
            }"#,
        )
        .expect("写入旧版设置应成功");

        let settings = Settings::load(&dir).expect("旧字段应被忽略");

        assert_eq!(settings.gateway_url, "https://example.test/");
        assert!(settings.validate().is_ok(), "迁移后的设置应合法");
        let _ = fs::remove_dir_all(&dir);
    }
}
