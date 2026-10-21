//! 多账号配置：账号档案、会话 Cookie 与独立监控目标。
//!
//! 这里刻意不把账号塞进 `settings.v2.json`。会话 Cookie 是敏感信息，和普通的
//! 界面偏好混在一个文件里，用户发日志、同步配置、备份设置时都更容易把它们带出去。
//! 账号单独一份 `accounts.v1.json`，并且沿用设置模块的原则：**读不出来是错误，
//! 不是空配置**；写入必须原子，损坏文件先留档再让用户决定怎么处理。
//!
//! 账号是监控与下单的边界：每个账号有自己的目标列表、地区、查询间隔、会话和
//! 是否自动加购。库存查询本身仍然是公开接口，但状态、提醒和后续加购都必须
//! 跟着账号走，不能把 A 账号的库存状态显示到 B 账号下面。

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::config::{DEFAULT_INTERVAL_SECONDS, MIN_INTERVAL_SECONDS};
use crate::model::{REGIONS, Target, region_by_locale};
use crate::order::OrderRecord;

/// 账号文件最多允许多大。正常账号不到 1 KB，200 个账号也远小于这个数。
pub const MAX_ACCOUNTS_BYTES: usize = 2 << 20;

/// 允许保存的账号数量上限。超过这个数多半是文件被写坏了，而不是真实用法。
pub const MAX_ACCOUNTS: usize = 200;

/// 单个账号允许保存的 Cookie 头长度上限。正常会话只有几 KB；超过这个数
/// 多半是粘错了整页内容，拒绝比写进账号文件后让整份配置读不出来要好。
pub const MAX_COOKIE_BYTES: usize = 32 << 10;

const APP_DIR: &str = "apple-pickup-watcher";
const ACCOUNTS_FILE: &str = "accounts.v1.json";

/// 账号会话的可用状态。
///
/// 和库存状态一样，「不知道」不能折叠成「已登录」。`Guest` 表示 Cookie 只够
/// 建立购物袋会话，没有登录 Apple 账户；这种状态下自动加购仍然可用，但付款链接
/// 只在那份浏览器会话里有效，界面上必须说清楚。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// 还没导入任何 Cookie。
    #[default]
    NotConfigured,
    /// 还没校验过。
    Unknown,
    /// Cookie 能用，但没有登录 Apple 账户。
    Guest,
    /// Cookie 能用，而且已经登录 Apple 账户。
    SignedIn,
    /// Cookie 已失效，需要重新导入。
    Expired,
}

impl SessionStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotConfigured => "未配置会话",
            Self::Unknown => "会话待校验",
            Self::Guest => "访客会话",
            Self::SignedIn => "已登录",
            Self::Expired => "会话已失效",
        }
    }

    /// 这份会话能不能发起自动加购。访客会话也可以加购，只是付款链接的
    /// 可转移性更差，界面会额外提示。
    pub fn can_order(self) -> bool {
        matches!(self, Self::Guest | Self::SignedIn | Self::Unknown)
    }
}

/// 一个 Apple 账号的会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountSession {
    /// `name=value; name2=value2` 形式的 Cookie 头。只保存在本地账号文件里，
    /// 不写日志、不在界面上回显原值。
    pub cookies: String,
    pub updated_at_ms: u64,
    pub checked_at_ms: Option<u64>,
    pub status: SessionStatus,
    pub detail: Option<String>,
}

impl Default for AccountSession {
    fn default() -> Self {
        Self {
            cookies: String::new(),
            updated_at_ms: 0,
            checked_at_ms: None,
            status: SessionStatus::NotConfigured,
            detail: None,
        }
    }
}

impl AccountSession {
    pub fn has_cookies(&self) -> bool {
        !self.cookies.trim().is_empty()
    }

    /// 把用户粘贴的 Cookie 头整理成稳定写法。
    ///
    /// 只接受 `name=value` 形式的片段：从浏览器开发者工具复制时经常带着
    /// `Path`、`Domain` 之类的属性，那些属性属于浏览器，不属于这里。重名的
    /// Cookie 保留最后一次出现的值，并把顺序稳定下来。
    pub fn set_cookies(&mut self, raw: &str) {
        let mut ordered: Vec<(String, String)> = Vec::new();
        for piece in raw.split(';') {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            let Some((name, value)) = piece.split_once('=') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
                continue;
            }
            // 从开发者工具复制 Cookie 时常常连带 Path / Domain / Expires 这些
            // 属性；它们属于浏览器，不属于要发出去的 Cookie 头。
            if is_cookie_attribute(name) {
                continue;
            }
            let value = value.trim().replace(['\r', '\n'], "");
            if let Some(existing) = ordered.iter_mut().find(|(n, _)| n == name) {
                existing.1 = value;
            } else {
                ordered.push((name.to_string(), value));
            }
        }

        self.cookies = ordered
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        self.updated_at_ms = now_ms();
        self.checked_at_ms = None;
        self.status = if self.cookies.is_empty() {
            SessionStatus::NotConfigured
        } else {
            SessionStatus::Unknown
        };
        self.detail = None;
    }

    pub fn clear(&mut self) {
        self.cookies.clear();
        self.updated_at_ms = 0;
        self.checked_at_ms = None;
        self.status = SessionStatus::NotConfigured;
        self.detail = None;
    }

    /// 记录一次校验结果。细节里可能带 HTTP 状态码，但不带 Cookie 值。
    pub fn record_check(&mut self, status: SessionStatus, detail: Option<String>) {
        self.status = status;
        self.checked_at_ms = Some(now_ms());
        self.detail = detail;
    }
}

/// Cookie 头里不会出现的属性名；复制整行 `Set-Cookie` 时要把它们剔掉。
fn is_cookie_attribute(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "path"
            | "domain"
            | "expires"
            | "max-age"
            | "secure"
            | "httponly"
            | "samesite"
            | "priority"
            | "partitioned"
    )
}

/// 一个被独立监控的账号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Account {
    /// 稳定标识，供前端选中、事件路由和订单记录使用。
    pub id: String,
    /// 用户自己起的备注名。
    pub name: String,
    /// Apple 账户邮箱，仅用于展示和帮助用户区分账号，不参与登录。
    pub apple_id: String,
    /// 这个账号监控的地区。
    pub locale: String,
    /// 是否参与监控。关闭后保留配置，但不会轮询，也不会下单。
    pub enabled: bool,
    /// 有货时是否自动把商品加入该账号的购物袋。
    pub auto_order: bool,
    /// 这个账号的查询间隔（秒）。
    pub interval_seconds: u64,
    /// 这个账号独立监控的目标列表。
    pub targets: Vec<Target>,
    pub session: AccountSession,
    /// 最近一次自动加购或手动加购的结果。付款链接会随账号一起持久化，
    /// 这样用户关闭程序后仍能从界面上找回。
    pub last_order: Option<OrderRecord>,
    pub created_at_ms: u64,
}

impl Default for Account {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            apple_id: String::new(),
            locale: REGIONS.first().map_or("zh_CN", |r| r.locale).to_string(),
            enabled: true,
            auto_order: false,
            interval_seconds: DEFAULT_INTERVAL_SECONDS,
            targets: Vec::new(),
            session: AccountSession::default(),
            last_order: None,
            created_at_ms: 0,
        }
    }
}

impl Account {
    /// 新建一个账号，分配稳定 id。
    pub fn new(name: impl Into<String>, locale: impl Into<String>) -> Self {
        let mut account = Self {
            id: new_id(),
            name: name.into(),
            locale: locale.into(),
            created_at_ms: now_ms(),
            ..Self::default()
        };
        account.normalize();
        account
    }

    /// 收敛非法字段。只修正能安全修正的东西，不擅自清空用户的目标或会话。
    pub fn normalize(&mut self) {
        if self.id.trim().is_empty() {
            self.id = new_id();
        }
        if region_by_locale(&self.locale).is_none() {
            self.locale = REGIONS.first().map_or("zh_CN", |r| r.locale).to_string();
        }
        if self.interval_seconds < MIN_INTERVAL_SECONDS {
            self.interval_seconds = DEFAULT_INTERVAL_SECONDS;
        }

        // 缺地区、门店或零件号的目标发不出请求，留着只会在界面上永远显示未知。
        // 去重时新建 Vec，和 Settings::normalize 的理由相同：不在原数组上就地压缩。
        let mut seen = HashSet::new();
        self.targets = self
            .targets
            .drain(..)
            .filter(|t| {
                !t.locale.trim().is_empty()
                    && !t.store_number.trim().is_empty()
                    && !t.part_number.trim().is_empty()
            })
            .filter(|t| seen.insert(t.key()))
            .collect();

        self.name = self.name.trim().to_string();
        self.apple_id = self.apple_id.trim().to_string();
        if self.name.is_empty() {
            self.name = if self.apple_id.is_empty() {
                "未命名账号".to_string()
            } else {
                self.apple_id.clone()
            };
        }

        self.session.cookies = self.session.cookies.trim().to_string();
        if self.session.cookies.is_empty() {
            self.session.cookies.clear();
            if self.session.status != SessionStatus::NotConfigured {
                self.session.status = SessionStatus::NotConfigured;
                self.session.detail = None;
            }
        } else if self.session.status == SessionStatus::NotConfigured {
            self.session.status = SessionStatus::Unknown;
        }

        if self.created_at_ms == 0 {
            self.created_at_ms = now_ms();
        }
    }
}

/// 账号读写过程中的失败。
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("定位用户配置目录失败：系统未提供配置目录位置")]
    NoConfigDir,

    #[error("{action} {} 失败：{source}", .path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("账号文件 {} 超过 {limit} 字节上限", .path.display())]
    TooLarge { path: PathBuf, limit: usize },

    #[error("解析账号文件 {} 失败：{source}", .path.display())]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("编码账号文件失败：{source}")]
    Encode { source: serde_json::Error },

    #[error("账号数量 {count} 超过上限 {limit}")]
    TooMany { count: usize, limit: usize },
}

impl AccountError {
    fn io(action: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    }
}

/// 账号文件存储。和 [`crate::config::SettingsStore`] 一样，构造时只算路径。
#[derive(Debug, Clone)]
pub struct AccountStore {
    path: PathBuf,
}

impl AccountStore {
    pub fn new() -> Result<Self, AccountError> {
        let dir = dirs::config_dir().ok_or(AccountError::NoConfigDir)?;
        Ok(Self {
            path: dir.join(APP_DIR).join(ACCOUNTS_FILE),
        })
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 文件不存在返回 `Ok(None)`；存在但读不出来返回错误。
    pub fn load(&self) -> Result<Option<Vec<Account>>, AccountError> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(AccountError::io("打开账号文件", &self.path, source)),
        };

        let data = read_capped(file, MAX_ACCOUNTS_BYTES)
            .map_err(|source| AccountError::io("读取账号文件", &self.path, source))?
            .ok_or_else(|| AccountError::TooLarge {
                path: self.path.clone(),
                limit: MAX_ACCOUNTS_BYTES,
            })?;

        let mut accounts: Vec<Account> =
            serde_json::from_slice(&data).map_err(|source| AccountError::Malformed {
                path: self.path.clone(),
                source,
            })?;

        let mut seen = HashSet::new();
        accounts.retain_mut(|account| {
            account.normalize();
            seen.insert(account.id.clone())
        });

        if accounts.len() > MAX_ACCOUNTS {
            return Err(AccountError::TooMany {
                count: accounts.len(),
                limit: MAX_ACCOUNTS,
            });
        }
        Ok(Some(accounts))
    }

    /// 原子写入。收切片而不是 `&mut Vec`，规范化只发生在内部副本上。
    pub fn save(&self, accounts: &[Account]) -> Result<(), AccountError> {
        if accounts.len() > MAX_ACCOUNTS {
            return Err(AccountError::TooMany {
                count: accounts.len(),
                limit: MAX_ACCOUNTS,
            });
        }
        let mut normalized: Vec<Account> = accounts.to_vec();
        for account in &mut normalized {
            account.normalize();
        }
        let data = serde_json::to_vec_pretty(&normalized)
            .map_err(|source| AccountError::Encode { source })?;
        if data.len() > MAX_ACCOUNTS_BYTES {
            return Err(AccountError::TooLarge {
                path: self.path.clone(),
                limit: MAX_ACCOUNTS_BYTES,
            });
        }
        write_atomically(&self.path, &data)
    }

    /// 把读不出来的账号文件改名留档。
    pub fn preserve_corrupted(&self) -> Result<Option<PathBuf>, AccountError> {
        match fs::symlink_metadata(&self.path) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(AccountError::io("检查账号文件", &self.path, source)),
        }

        let stamp = now_ms();
        let mut candidate = parent_dir(&self.path).join(format!("{ACCOUNTS_FILE}.corrupt-{stamp}"));
        let mut seq = 1u32;
        while candidate.symlink_metadata().is_ok() {
            candidate =
                parent_dir(&self.path).join(format!("{ACCOUNTS_FILE}.corrupt-{stamp}-{seq}"));
            seq = seq.saturating_add(1);
        }
        fs::rename(&self.path, &candidate)
            .map_err(|source| AccountError::io("留档损坏的账号文件", &self.path, source))?;
        Ok(Some(candidate))
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn new_id() -> String {
    let mut rng = rand::rng();
    let n: u64 = rng.random();
    format!("acct_{n:016x}")
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

fn read_capped<R: Read>(reader: R, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let mut capped = reader.take(limit as u64 + 1);
    capped.read_to_end(&mut buf)?;
    if buf.len() > limit {
        return Ok(None);
    }
    Ok(Some(buf))
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn write_atomically(path: &Path, data: &[u8]) -> Result<(), AccountError> {
    let dir = parent_dir(path);
    fs::create_dir_all(dir).map_err(|e| AccountError::io("创建账号目录", dir, e))?;

    let mut guard = None;
    let mut last = None;
    for _ in 0..32 {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!(".apw-accounts-{}-{seq}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                guard = Some((
                    TempFile {
                        path: candidate,
                        armed: true,
                    },
                    file,
                ));
                break;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(AccountError::io("创建临时文件于", dir, e)),
        }
    }

    let Some((mut temp, mut file)) = guard else {
        let source = last
            .unwrap_or_else(|| std::io::Error::new(ErrorKind::AlreadyExists, "临时文件名反复冲突"));
        return Err(AccountError::io("创建临时文件于", dir, source));
    };

    file.write_all(data)
        .map_err(|e| AccountError::io("写入临时文件", &temp.path, e))?;
    file.sync_all()
        .map_err(|e| AccountError::io("刷盘临时文件", &temp.path, e))?;
    drop(file);

    fs::rename(&temp.path, path).map_err(|e| AccountError::io("替换账号文件", path, e))?;
    temp.armed = false;

    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 目标(store: &str, part: &str) -> Target {
        Target {
            locale: "zh_CN".into(),
            store_number: store.into(),
            store_title: "上海-环球港".into(),
            part_number: part.into(),
            product_name: "iPhone 17".into(),
            companion_part: None,
            pickup_location: None,
        }
    }

    #[test]
    fn 新账号自动补齐标识与名称() {
        let mut a = Account::default();
        a.normalize();
        assert!(a.id.starts_with("acct_"));
        assert_eq!(a.name, "未命名账号");
        assert_eq!(a.locale, "zh_CN");
        assert_eq!(a.interval_seconds, DEFAULT_INTERVAL_SECONDS);
        assert!(a.enabled);
    }

    #[test]
    fn 会话_cookie_只保留键值对并去重() {
        let mut s = AccountSession::default();
        s.set_cookies(" a=1; Path=/; b=2; a=3; bad; c=x=y ");
        assert_eq!(s.cookies, "a=3; b=2; c=x=y");
        assert_eq!(s.status, SessionStatus::Unknown);
        assert!(s.has_cookies());

        s.clear();
        assert_eq!(s.status, SessionStatus::NotConfigured);
        assert!(!s.has_cookies());
    }

    #[test]
    fn 账号目标去重且丢掉发不出请求的项() {
        let mut a = Account::default();
        a.targets = vec![
            目标("R683", "MG724CH/A"),
            目标("R683", "MG724CH/A"),
            目标("", "MG724CH/A"),
        ];
        a.normalize();
        assert_eq!(a.targets.len(), 1);
    }

    #[test]
    fn 账号文件可以往返保存() {
        let path = std::env::temp_dir().join(format!(
            "apw-accounts-test-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        let store = AccountStore::at(path.clone());
        let mut a = Account::new("测试账号", "zh_CN");
        a.targets = vec![目标("R683", "MG724CH/A")];
        a.session.set_cookies("dssid2=abc; as_dc=ucp4");

        store.save(std::slice::from_ref(&a)).expect("保存应当成功");
        let loaded = store.load().expect("读取应当成功").expect("文件应当存在");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].targets.len(), 1);
        assert_eq!(loaded[0].session.cookies, "dssid2=abc; as_dc=ucp4");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn 缺失的账号文件不等于空账号列表() {
        let path = std::env::temp_dir().join(format!(
            "apw-accounts-missing-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        let store = AccountStore::at(path);
        assert!(store.load().expect("缺失不是错误").is_none());
    }

    #[test]
    fn 超过上限的账号文件不会被写盘也不会覆盖旧文件() {
        let path = std::env::temp_dir().join(format!(
            "apw-accounts-too-large-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        let store = AccountStore::at(path.clone());
        let small = Account::new("小账号", "zh_CN");
        store
            .save(std::slice::from_ref(&small))
            .expect("保存应当成功");
        let before = fs::read(&path).expect("旧文件应当存在");

        let mut huge = Account::new("超大账号", "zh_CN");
        huge.session.cookies = format!("a={}", "x".repeat(MAX_ACCOUNTS_BYTES));
        let err = store
            .save(std::slice::from_ref(&huge))
            .expect_err("必须拒绝");
        assert!(matches!(err, AccountError::TooLarge { .. }));
        assert_eq!(fs::read(&path).expect("旧文件应当保留"), before);
        let _ = fs::remove_file(path);
    }
}
