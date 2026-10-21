//! Tauri 应用外壳。
//!
//! 这一层只做装配和转译：起引擎、把命令转成引擎消息、把引擎事件转发给前端、
//! 在有货时发提醒。**所有业务判断都在 `apw-core` 里**，这里不许出现任何
//! 「什么算有货」之类的逻辑 —— 一旦让界面层参与判断，那条核心不变量就多了
//! 一处可以被绕开的地方。
//!
//! 提醒也刻意放在这一层而不是前端：托盘模式下窗口是隐藏的，WebView 可能被
//! 系统节流甚至挂起，把「及时提醒」挂在一个会被挂起的执行环境上是不能接受的。
//!
//! 多账号的边界也在这里落地：每个账号有自己的 `Watcher`、目标列表、间隔、
//! 会话和自动加购闸门。库存查询仍然走共享的公开客户端，但状态、提醒和订单
//! 都按账号路由，A 账号的到货绝不会显示到 B 账号下面。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use apw_core::accounts::{
    Account, AccountSession, AccountStore, MAX_ACCOUNTS, MAX_COOKIE_BYTES, SessionStatus,
};
use apw_core::apple::{AppleClient, ClientConfig};
use apw_core::catalog::Catalog;
use apw_core::config::{MIN_INTERVAL_SECONDS, Settings, SettingsStore};
use apw_core::model::{Category, Product, REGIONS, Store, Target, TargetKey, region_by_locale};
use apw_core::notify::{Bark, Feishu, Multi, Notification, Notifier, Sound};
use apw_core::order::{self, OrderRecord, OrderStatus};
use apw_core::watcher::{Event, TargetState, Watcher, WatcherConfig};
use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::Mutex;

mod auto_open;
mod notification_queue;

use auto_open::AutoOpenBag;

/// 前端事件通道名。前端用 `listen("watcher://event", ...)` 订阅。
const EVENT_CHANNEL: &str = "watcher://event";
/// 订单结果通道。自动加购和手动加购都把结果发到这里。
const ORDER_CHANNEL: &str = "watcher://order";
/// 启动过程中的降级说明通道：配置读不出来之类的事必须让用户看见。
const NOTICE_CHANNEL: &str = "watcher://notice";

/// 每个账号的 Apple 登录窗口使用独立 label 和独立 WebView 数据目录。
/// 这样两个账号即使先后登录，也不会共用同一份浏览器 Cookie。
fn login_window_label(account_id: &str) -> String {
    format!("apple-login-{account_id}")
}

/// Apple 商店登录页。中国大陆使用独立的 secure 域名，其余地区走对应的
/// storefront 路径；登录完成后 Cookie 会落在该地区的 Apple 域上。
fn apple_sign_in_url(locale: &str) -> &'static str {
    match locale {
        "zh_CN" => "https://secure.www.apple.com.cn/shop/signIn",
        "zh_HK" => "https://secure.store.apple.com/hk-zh/shop/signIn",
        "zh_TW" => "https://secure.store.apple.com/tw/shop/signIn",
        "ja_JP" => "https://secure.store.apple.com/jp/shop/signIn",
        "en_SG" => "https://secure.store.apple.com/sg/shop/signIn",
        "en_AU" => "https://secure.store.apple.com/au/shop/signIn",
        "en_MY" => "https://secure.store.apple.com/my/shop/signIn",
        _ => "https://secure.store.apple.com/shop/signIn",
    }
}

/// 一次自动加购成功后，静默多久再允许下一次自动加购。
///
/// 多个目标同时到货是常态。若每个 `InStock` 都往购物袋里塞一件，用户打开
/// 付款页时看到的是一串重复商品；十分钟足够完成一次付款，也足够用户手动
/// 再加购别的型号。
const AUTO_ORDER_HOLD: Duration = Duration::from_secs(10 * 60);

/// 自动加购失败后的重试冷却。失败可能只是网络抖动，但也不能每秒重试。
const AUTO_ORDER_RETRY: Duration = Duration::from_secs(60);

/// 地区的可序列化形式。
///
/// `model::Region` 的字段都是 `&'static str`，而且界面不需要知道 `base_url`
/// 这类内部细节。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegionDto {
    title: &'static str,
    locale: &'static str,
}

/// 品类的可序列化形式。
///
/// 界面上的品类下拉框由这里驱动，而不是在前端另抄一份常量：抄一份就迟早会有
/// 一边先加了品类、另一边还蒙在鼓里。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CategoryDto {
    value: Category,
    title: &'static str,
}

/// 账号会话的展示形式。**绝不包含 Cookie 原值**，只给出数量和名字。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionDto {
    configured: bool,
    cookie_count: usize,
    cookie_names: Vec<String>,
    status: SessionStatus,
    status_label: &'static str,
    updated_at_ms: u64,
    checked_at_ms: Option<u64>,
    detail: Option<String>,
}

impl From<&AccountSession> for SessionDto {
    fn from(session: &AccountSession) -> Self {
        let mut cookie_names: Vec<String> = session
            .cookies
            .split(';')
            .filter_map(|piece| piece.trim().split_once('='))
            .map(|(name, _)| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
        cookie_names.sort();
        cookie_names.dedup();
        Self {
            configured: session.has_cookies(),
            cookie_count: cookie_names.len(),
            cookie_names,
            status: session.status,
            status_label: session.status.label(),
            updated_at_ms: session.updated_at_ms,
            checked_at_ms: session.checked_at_ms,
            detail: session.detail.clone(),
        }
    }
}

/// 一个账号的展示形式。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountDto {
    id: String,
    name: String,
    apple_id: String,
    locale: String,
    enabled: bool,
    auto_order: bool,
    interval_seconds: u64,
    target_count: usize,
    running: bool,
    session: SessionDto,
    last_order: Option<OrderRecord>,
}

/// 更新账号时允许改动的字段。没传的字段保持原值。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct AccountPatch {
    name: Option<String>,
    apple_id: Option<String>,
    locale: Option<String>,
    enabled: Option<bool>,
    auto_order: Option<bool>,
    interval_seconds: Option<u64>,
}

/// 自动加购闸门。每个账号一份。
#[derive(Debug, Default)]
struct AutoOrderGate {
    /// 本轮有货期间已经自动加购过的目标。目标离开有货后移除。
    ordered: HashSet<TargetKey>,
    /// 成功后十分钟内不再自动加购；失败后一分钟内不重试。
    hold_until: Option<Instant>,
}

/// 账号的运行实例：独立的监控引擎和自动加购闸门。
#[derive(Clone)]
struct AccountRuntime {
    watcher: Watcher,
    gate: Arc<Mutex<AutoOrderGate>>,
}

struct AppState {
    /// 账号的内存副本。写盘失败不回滚内存，和设置的处理方式保持一致。
    accounts: RwLock<Vec<Account>>,
    /// 为 `None` 表示账号配置不可持久化。
    account_store: Option<AccountStore>,
    /// 账号 id → 独立监控实例。增删账号时同步维护。
    runtimes: RwLock<HashMap<String, AccountRuntime>>,
    /// 和引擎共用的 Apple 客户端句柄，改代理设置时用它换线路，不用重启。
    client: AppleClient,
    catalog: Catalog,
    http: reqwest::Client,
    /// 设置的内存副本。写盘失败不该让界面卡住，所以内存副本是权威的展示来源。
    settings: RwLock<Settings>,
    /// 为 `None` 表示配置不可持久化（目录不可写，或上次读取失败已放弃写盘）。
    store: Option<SettingsStore>,
}

impl AppState {
    fn settings_snapshot(&self) -> Settings {
        self.settings
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// 更新内存副本并尝试落盘。落盘失败只报错，不回滚内存 ——
    /// 用户的操作已经生效了，没道理因为磁盘问题把界面弹回去。
    fn put_settings(&self, next: Settings) -> Result<(), String> {
        let mut guard = self.settings.write().unwrap_or_else(|e| e.into_inner());
        *guard = next;
        let to_save = guard.clone();
        drop(guard);

        match &self.store {
            Some(store) => store.save(&to_save).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    fn accounts_snapshot(&self) -> Vec<Account> {
        self.accounts
            .read()
            .map(|accounts| accounts.clone())
            .unwrap_or_else(|err| err.into_inner().clone())
    }

    fn account(&self, id: &str) -> Option<Account> {
        self.accounts_snapshot()
            .into_iter()
            .find(|account| account.id == id)
    }

    fn first_account_id(&self) -> Option<String> {
        self.accounts_snapshot()
            .first()
            .map(|account| account.id.clone())
    }

    fn runtime(&self, id: &str) -> Option<AccountRuntime> {
        self.runtimes
            .read()
            .ok()
            .and_then(|runtimes| runtimes.get(id).cloned())
    }

    fn save_accounts(&self, accounts: &[Account]) -> Result<(), String> {
        match &self.account_store {
            Some(store) => store.save(accounts).map_err(|err| err.to_string()),
            None => Ok(()),
        }
    }

    /// 仅替换内存里某个账号，并整体落盘。
    fn mutate_account<F>(&self, id: &str, mutate: F) -> Result<Account, String>
    where
        F: FnOnce(&mut Account),
    {
        let (updated, snapshot) = {
            let mut accounts = self.accounts.write().unwrap_or_else(|err| err.into_inner());
            let account = accounts
                .iter_mut()
                .find(|account| account.id == id)
                .ok_or_else(|| "账号不存在".to_string())?;
            mutate(account);
            account.normalize();
            (account.clone(), accounts.clone())
        };
        self.save_accounts(&snapshot)?;
        Ok(updated)
    }

    fn record_order(&self, account_id: &str, order: OrderRecord) {
        let snapshot = {
            let mut accounts = self.accounts.write().unwrap_or_else(|err| err.into_inner());
            let Some(account) = accounts.iter_mut().find(|account| account.id == account_id) else {
                return;
            };
            account.last_order = Some(order);
            accounts.clone()
        };
        let _ = self.save_accounts(&snapshot);
    }

    /// 目标离开有货后，允许它下一次到货时重新自动加购。
    fn forget_out_of_stock(&self, account_id: &str, snapshot: &[TargetState]) {
        let Some(runtime) = self.runtime(account_id) else {
            return;
        };
        let Ok(mut gate) = runtime.gate.try_lock() else {
            return;
        };
        let in_stock: HashSet<TargetKey> = snapshot
            .iter()
            .filter(|state| state.availability.is_in_stock())
            .map(|state| state.target.key())
            .collect();
        gate.ordered.retain(|key| in_stock.contains(key));
    }

    /// 为一个账号起独立监控引擎，并把它的事件接到统一的事件泵上。
    fn start_runtime(&self, app: &AppHandle, account: &Account) -> Result<(), String> {
        if self
            .runtimes
            .read()
            .map(|runtimes| runtimes.contains_key(&account.id))
            .unwrap_or(false)
        {
            return Ok(());
        }

        let interval = Duration::from_secs(account.interval_seconds);
        let (watcher, events, engine) = Watcher::new(
            self.client.clone(),
            WatcherConfig {
                interval,
                ..WatcherConfig::default()
            },
        );
        tauri::async_runtime::spawn(engine);

        {
            let watcher = watcher.clone();
            let targets = account.targets.clone();
            tauri::async_runtime::spawn(async move {
                watcher.set_targets(targets).await;
                watcher.set_interval(interval).await;
            });
        }

        self.runtimes
            .write()
            .unwrap_or_else(|err| err.into_inner())
            .insert(
                account.id.clone(),
                AccountRuntime {
                    watcher,
                    gate: Arc::new(Mutex::new(AutoOrderGate::default())),
                },
            );

        tauri::async_runtime::spawn(pump_account_events(app.clone(), account.id.clone(), events));
        Ok(())
    }
}

#[tauri::command]
fn list_regions() -> Vec<RegionDto> {
    REGIONS
        .iter()
        .map(|r| RegionDto {
            title: r.title,
            locale: r.locale,
        })
        .collect()
}

#[tauri::command]
fn list_categories() -> Vec<CategoryDto> {
    Category::ALL
        .iter()
        .map(|c| CategoryDto {
            value: *c,
            title: c.title(),
        })
        .collect()
}

#[tauri::command]
fn list_stores(state: tauri::State<'_, AppState>, locale: String) -> Result<Vec<Store>, String> {
    state.catalog.stores(&locale).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_products(
    state: tauri::State<'_, AppState>,
    locale: String,
) -> Result<Vec<Product>, String> {
    state.catalog.products(&locale).map_err(|e| e.to_string())
}

/// 从 Apple 官网抓最新型号，替换该地区该品类的内存副本，返回抓到的型号数。
///
/// `category` 为 `None` 时抓该地区的全部购买页。界面传的是当前选中的品类：
/// 一次只抓那几页，用户想看新出的 Mac 不必等 iPhone、iPad、Watch 一起抓完。
#[tauri::command]
async fn refresh_products(
    state: tauri::State<'_, AppState>,
    locale: String,
    category: Option<Category>,
) -> Result<usize, String> {
    let region = region_by_locale(&locale).ok_or_else(|| format!("认不出地区 {locale}"))?;
    state
        .catalog
        .refresh_products(region, category, &state.http)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state.settings_snapshot()
}

#[tauri::command]
async fn save_settings(
    state: tauri::State<'_, AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    let mut next = settings;
    next.normalize();

    // 目标和间隔属于账号，由账号命令单独更新。这里只保存全局设置，
    // 避免旧版界面在只改推送地址时把某个账号的监控列表一并覆盖。
    // 代理变了就换线路，立即生效；地址解析不了就把错误报回界面，其余设置照存。
    if next.proxies != state.settings_snapshot().proxies {
        state
            .client
            .set_proxies(&next.proxies)
            .await
            .map_err(|e| format!("代理设置未生效：{e}"))?;
    }

    state.put_settings(next.clone())?;
    Ok(next)
}

/// 用界面上的推送地址列表整体替换现有的。
///
/// 每个地址是 Bark 还是飞书由 [`Settings::set_push_urls`] 按长相判断，界面不自己猜。
/// 只动推送地址：不经过 [`save_settings`]，免得顺带把引擎的目标、间隔和代理
/// 也重新下发一遍。
#[tauri::command]
async fn set_push_urls(
    state: tauri::State<'_, AppState>,
    urls: Vec<String>,
) -> Result<Settings, String> {
    let mut next = state.settings_snapshot();
    next.set_push_urls(&urls);
    next.normalize();
    state.put_settings(next.clone())?;
    Ok(next)
}

// ---- 多账号。

async fn account_dto(state: &AppState, account: &Account) -> AccountDto {
    let running = match state.runtime(&account.id) {
        Some(runtime) => runtime.watcher.is_running().await,
        None => false,
    };
    AccountDto {
        id: account.id.clone(),
        name: account.name.clone(),
        apple_id: account.apple_id.clone(),
        locale: account.locale.clone(),
        enabled: account.enabled,
        auto_order: account.auto_order,
        interval_seconds: account.interval_seconds,
        target_count: account.targets.len(),
        running,
        session: SessionDto::from(&account.session),
        last_order: account.last_order.clone(),
    }
}

#[tauri::command]
async fn list_accounts(state: tauri::State<'_, AppState>) -> Result<Vec<AccountDto>, String> {
    let accounts = state.accounts_snapshot();
    let mut out = Vec::with_capacity(accounts.len());
    for account in &accounts {
        out.push(account_dto(&state, account).await);
    }
    Ok(out)
}

#[tauri::command]
async fn create_account(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    name: String,
    apple_id: String,
    locale: String,
) -> Result<AccountDto, String> {
    let mut account = Account::new(name, locale);
    account.apple_id = apple_id;
    account.normalize();

    let snapshot = {
        let mut accounts = state
            .accounts
            .write()
            .unwrap_or_else(|err| err.into_inner());
        if accounts.len() >= MAX_ACCOUNTS {
            return Err(format!("账号数量已达上限 {MAX_ACCOUNTS}"));
        }
        accounts.push(account.clone());
        accounts.clone()
    };
    state.save_accounts(&snapshot)?;
    state.start_runtime(&app, &account)?;
    Ok(account_dto(&state, &account).await)
}

#[tauri::command]
async fn update_account(
    state: tauri::State<'_, AppState>,
    account_id: String,
    patch: AccountPatch,
) -> Result<AccountDto, String> {
    let updated = state.mutate_account(&account_id, |account| {
        if let Some(name) = patch.name {
            account.name = name;
        }
        if let Some(apple_id) = patch.apple_id {
            account.apple_id = apple_id;
        }
        if let Some(locale) = patch.locale {
            account.locale = locale;
        }
        if let Some(enabled) = patch.enabled {
            account.enabled = enabled;
        }
        if let Some(auto_order) = patch.auto_order {
            account.auto_order = auto_order;
        }
        if let Some(interval_seconds) = patch.interval_seconds {
            account.interval_seconds = interval_seconds.max(MIN_INTERVAL_SECONDS);
        }
    })?;

    if let Some(runtime) = state.runtime(&account_id) {
        runtime
            .watcher
            .set_interval(Duration::from_secs(updated.interval_seconds))
            .await;
        if !updated.enabled {
            runtime.watcher.stop().await;
        }
    }

    Ok(account_dto(&state, &updated).await)
}

#[tauri::command]
async fn delete_account(
    state: tauri::State<'_, AppState>,
    account_id: String,
) -> Result<(), String> {
    let runtime = state
        .runtimes
        .write()
        .unwrap_or_else(|err| err.into_inner())
        .remove(&account_id);
    if let Some(runtime) = runtime {
        runtime.watcher.stop().await;
    }

    let snapshot = {
        let mut accounts = state
            .accounts
            .write()
            .unwrap_or_else(|err| err.into_inner());
        accounts.retain(|account| account.id != account_id);
        accounts.clone()
    };
    state.save_accounts(&snapshot)
}

/// 保存账号会话。只接受 `name=value` 片段，Cookie 属性会被丢掉；
/// 原值只落本地账号文件，不写日志、不在事件里回传。
#[tauri::command]
async fn set_account_session(
    state: tauri::State<'_, AppState>,
    account_id: String,
    cookies: String,
) -> Result<AccountDto, String> {
    persist_account_session(&state, &account_id, &cookies).await
}

async fn persist_account_session(
    state: &AppState,
    account_id: &str,
    cookies: &str,
) -> Result<AccountDto, String> {
    let raw = cookies.trim().to_string();
    if raw.is_empty() {
        // 空输入不能被当成「清空会话」：界面上的保存按钮很容易在空文本框时被
        // 误触，那会把用户辛苦导入的会话悄悄删掉。清空必须走显式命令。
        return Err("Cookie 不能为空；如需清空会话，请使用「清除会话」".into());
    }
    if raw.len() > MAX_COOKIE_BYTES {
        return Err(format!(
            "Cookie 内容过长（{} 字节，上限 {MAX_COOKIE_BYTES} 字节）。请只复制 Cookie 请求头，不要粘贴整页内容",
            raw.len()
        ));
    }
    let mut probe = state
        .account(account_id)
        .ok_or_else(|| "账号不存在".to_string())?;
    probe.session.set_cookies(&raw);
    if !probe.session.has_cookies() {
        return Err("没有解析出有效的 Cookie，请粘贴 `name=value; name2=value2` 形式的内容".into());
    }

    let updated = state.mutate_account(account_id, |account| {
        account.session.set_cookies(&raw);
    })?;
    Ok(account_dto(state, &updated).await)
}

/// 打开该账号专属的 Apple 登录窗口。
///
/// WebView 数据目录按账号隔离：Cookie 只在本地磁盘上，读取后由
/// `save_account_login_cookies` 复制进账号会话文件，供 Rust 侧的库存/加购请求使用。
#[tauri::command]
async fn open_account_login(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    account_id: String,
) -> Result<(), String> {
    let account = state
        .account(&account_id)
        .ok_or_else(|| "账号不存在".to_string())?;
    let region = region_by_locale(&account.locale)
        .ok_or_else(|| format!("认不出地区 {}", account.locale))?;
    let label = login_window_label(&account_id);

    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }

    let data_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|err| format!("找不到应用数据目录：{err}"))?
        .join("login-webviews")
        .join(&account_id);
    std::fs::create_dir_all(&data_dir).map_err(|err| format!("创建登录数据目录失败：{err}"))?;

    let url = apple_sign_in_url(region.locale)
        .parse()
        .map_err(|err| format!("构造 Apple 登录地址失败：{err}"))?;
    WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(url))
        .title(format!("登录 Apple · {}", account.name))
        .inner_size(1120.0, 800.0)
        .min_inner_size(760.0, 560.0)
        .data_directory(data_dir)
        .build()
        .map_err(|err| format!("打开 Apple 登录窗口失败：{err}"))?;
    Ok(())
}

/// 从该账号的登录窗口读取 Apple 商店 Cookie，并保存到账号会话。
///
/// `cookies_for_url` 只返回会发给目标商店域（含 `/shop` 路径）的 Cookie，
/// 避免把其他站点的项混进请求头。Windows 上读取 Cookie 可能等待 WebView2，
/// 因此放到阻塞线程里执行。
#[tauri::command]
async fn save_account_login_cookies(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    account_id: String,
) -> Result<AccountDto, String> {
    let account = state
        .account(&account_id)
        .ok_or_else(|| "账号不存在".to_string())?;
    let region = region_by_locale(&account.locale)
        .ok_or_else(|| format!("认不出地区 {}", account.locale))?;
    let label = login_window_label(&account_id);
    let window = app
        .get_webview_window(&label)
        .ok_or_else(|| "登录窗口没有打开；请先点击「登录 Apple」并完成登录".to_string())?;
    let url = format!("{}/shop/bag", region.base_url)
        .parse()
        .map_err(|err| format!("构造 Cookie 读取地址失败：{err}"))?;

    let cookies = tauri::async_runtime::spawn_blocking(move || window.cookies_for_url(url))
        .await
        .map_err(|err| format!("读取登录 Cookie 失败：{err}"))?
        .map_err(|err| format!("读取登录 Cookie 失败：{err}"))?;

    let mut seen = HashSet::new();
    let raw = cookies
        .into_iter()
        .filter(|cookie| !cookie.name().trim().is_empty())
        .filter(|cookie| seen.insert(cookie.name().to_string()))
        .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
        .collect::<Vec<_>>()
        .join("; ");

    if raw.is_empty() {
        return Err(
            "登录窗口里还没有读到 Apple Cookie。请确认已登录成功，并停留在 Apple 商店页面后再保存"
                .into(),
        );
    }
    persist_account_session(&state, &account_id, &raw).await
}

#[tauri::command]
async fn clear_account_session(
    state: tauri::State<'_, AppState>,
    account_id: String,
) -> Result<AccountDto, String> {
    let updated = state.mutate_account(&account_id, |account| account.session.clear())?;
    Ok(account_dto(&state, &updated).await)
}

/// 用账号会话访问一次购物袋状态接口，验证 Cookie 是否还能用。
#[tauri::command]
async fn check_account_session(
    state: tauri::State<'_, AppState>,
    account_id: String,
) -> Result<AccountDto, String> {
    let account = state
        .account(&account_id)
        .ok_or_else(|| "账号不存在".to_string())?;
    let region = region_by_locale(&account.locale)
        .ok_or_else(|| format!("认不出地区 {}", account.locale))?;
    let proxies = state.settings_snapshot().proxies;
    let check = order::check_session(region, &account.session.cookies, &proxies).await;

    let updated = state.mutate_account(&account_id, |account| {
        account
            .session
            .record_check(check.status, check.detail.clone());
    })?;
    Ok(account_dto(&state, &updated).await)
}

#[tauri::command]
async fn set_account_targets(
    state: tauri::State<'_, AppState>,
    account_id: String,
    targets: Vec<Target>,
) -> Result<Vec<TargetState>, String> {
    let mut targets = targets;
    state.catalog.attach_companions(&mut targets);
    state.catalog.attach_locations(&mut targets);

    let updated = state.mutate_account(&account_id, |account| {
        account.targets = targets.clone();
    })?;
    if let Some(runtime) = state.runtime(&account_id) {
        runtime.watcher.set_targets(updated.targets.clone()).await;
        Ok(runtime.watcher.snapshot().await)
    } else {
        Err("账号运行实例不存在".into())
    }
}

#[tauri::command]
async fn set_account_interval(
    state: tauri::State<'_, AppState>,
    account_id: String,
    seconds: u64,
) -> Result<u64, String> {
    let secs = seconds.max(MIN_INTERVAL_SECONDS);
    let updated = state.mutate_account(&account_id, |account| {
        account.interval_seconds = secs;
    })?;
    if let Some(runtime) = state.runtime(&account_id) {
        runtime
            .watcher
            .set_interval(Duration::from_secs(updated.interval_seconds))
            .await;
    }
    Ok(updated.interval_seconds)
}

#[tauri::command]
async fn set_account_locale(
    state: tauri::State<'_, AppState>,
    account_id: String,
    locale: String,
) -> Result<AccountDto, String> {
    let updated = state.mutate_account(&account_id, |account| {
        account.locale = locale;
    })?;
    Ok(account_dto(&state, &updated).await)
}

#[tauri::command]
async fn get_account_snapshot(
    state: tauri::State<'_, AppState>,
    account_id: String,
) -> Result<Vec<TargetState>, String> {
    match state.runtime(&account_id) {
        Some(runtime) => Ok(runtime.watcher.snapshot().await),
        None => Ok(Vec::new()),
    }
}

#[tauri::command]
async fn start_account(
    state: tauri::State<'_, AppState>,
    account_id: String,
) -> Result<(), String> {
    let account = state
        .account(&account_id)
        .ok_or_else(|| "账号不存在".to_string())?;
    if !account.enabled {
        return Err("账号已停用，先在账号面板里启用".into());
    }
    let runtime = state
        .runtime(&account_id)
        .ok_or_else(|| "账号运行实例不存在".to_string())?;
    runtime.watcher.start().await;
    Ok(())
}

#[tauri::command]
async fn stop_account(state: tauri::State<'_, AppState>, account_id: String) -> Result<(), String> {
    if let Some(runtime) = state.runtime(&account_id) {
        runtime.watcher.stop().await;
    }
    Ok(())
}

#[tauri::command]
async fn start_all(state: tauri::State<'_, AppState>) -> Result<(), String> {
    for account in state.accounts_snapshot() {
        if !account.enabled {
            continue;
        }
        if let Some(runtime) = state.runtime(&account.id) {
            runtime.watcher.start().await;
        }
    }
    Ok(())
}

#[tauri::command]
async fn stop_all(state: tauri::State<'_, AppState>) -> Result<(), String> {
    for account in state.accounts_snapshot() {
        if let Some(runtime) = state.runtime(&account.id) {
            runtime.watcher.stop().await;
        }
    }
    Ok(())
}

/// 手动把某个目标加入账号购物袋。不传目标时优先选第一个有货项。
#[tauri::command]
async fn place_order_now(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    account_id: String,
    target: Option<Target>,
) -> Result<OrderRecord, String> {
    let target = match target {
        Some(target) => target,
        None => {
            let snapshot = match state.runtime(&account_id) {
                Some(runtime) => runtime.watcher.snapshot().await,
                None => Vec::new(),
            };
            snapshot
                .into_iter()
                .find(|state| state.availability.is_in_stock())
                .map(|state| state.target)
                .or_else(|| {
                    state
                        .account(&account_id)
                        .and_then(|account| account.targets.first().cloned())
                })
                .ok_or_else(|| "这个账号还没有监控目标".to_string())?
        }
    };
    perform_order(&app, &account_id, &target).await
}

// ---- 兼容旧版界面的命令：默认操作第一个账号。

#[tauri::command]
async fn get_snapshot(state: tauri::State<'_, AppState>) -> Result<Vec<TargetState>, String> {
    match state.first_account_id() {
        Some(account_id) => get_account_snapshot(state, account_id).await,
        None => Ok(Vec::new()),
    }
}

#[tauri::command]
async fn set_targets(
    state: tauri::State<'_, AppState>,
    targets: Vec<Target>,
) -> Result<Vec<TargetState>, String> {
    let account_id = state
        .first_account_id()
        .ok_or_else(|| "还没有账号".to_string())?;
    let rows = set_account_targets(state.clone(), account_id, targets.clone()).await?;
    let mut next = state.settings_snapshot();
    next.targets = targets;
    state.put_settings(next)?;
    Ok(rows)
}

#[tauri::command]
async fn set_interval(state: tauri::State<'_, AppState>, seconds: u64) -> Result<u64, String> {
    let Some(account_id) = state.first_account_id() else {
        return Err("还没有账号".into());
    };
    let applied = set_account_interval(state.clone(), account_id, seconds).await?;
    let mut next = state.settings_snapshot();
    next.interval_seconds = applied;
    state.put_settings(next)?;
    Ok(applied)
}

#[tauri::command]
async fn start_watching(state: tauri::State<'_, AppState>) -> Result<(), String> {
    match state.first_account_id() {
        Some(account_id) => start_account(state, account_id).await,
        None => Ok(()),
    }
}

#[tauri::command]
async fn stop_watching(state: tauri::State<'_, AppState>) -> Result<(), String> {
    match state.first_account_id() {
        Some(account_id) => stop_account(state, account_id).await,
        None => Ok(()),
    }
}

#[tauri::command]
async fn is_running(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let Some(account_id) = state.first_account_id() else {
        return Ok(false);
    };
    match state.runtime(&account_id) {
        Some(runtime) => Ok(runtime.watcher.is_running().await),
        None => Ok(false),
    }
}

/// 一个待安装的更新。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    version: String,
    current_version: String,
    notes: Option<String>,
}

/// 查询有没有新版本。返回 `None` 表示已经是最新。
///
/// **刻意不做静默自动安装**：给用户装东西这件事应该由用户点头。何况这是个会在
/// 抢购当口挂着的程序，自作主张地下载、替换、重启，正好会赶上最不该被打断的时刻。
#[tauri::command]
async fn check_for_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(Some(UpdateInfo {
            version: update.version.clone(),
            current_version: update.current_version.clone(),
            notes: update.body.clone(),
        })),
        Ok(None) => Ok(None),
        // 检查更新失败不是错误状态，只是这次没查到 —— 网络不通、GitHub 抽风都
        // 会走到这里，没必要弹给用户看，写进日志即可。
        Err(err) => Err(err.to_string()),
    }
}

/// 下载并安装更新。安装完成后需要重启应用才生效。
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "已经是最新版本".to_string())?;

    let handle = app.clone();
    update
        .download_and_install(
            move |downloaded, total| {
                // 进度只发给界面，不做任何决策。
                let _ = handle.emit(
                    "watcher://update-progress",
                    serde_json::json!({ "downloaded": downloaded, "total": total }),
                );
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 手动试一次提醒，让用户在真正抢购之前确认铃声和推送都通。
#[tauri::command]
async fn test_notify(app: AppHandle) -> Result<(), String> {
    dispatch_notification(
        &app,
        // 标题刻意和真实到货提醒一样带「有货了」：飞书自定义机器人的关键词
        // 安全模式只放行含关键词的消息，测试文案若不含，用户会误以为配置坏了。
        Notification::new("有货了（测试）", "如果你看到并听到了这条，说明提醒是通的"),
    )
    .await
    .map_err(|e| e.to_string())
}

/// 发出一条提醒：系统通知 + 提示音 + Bark，按用户设置取舍。
async fn dispatch_notification(
    app: &AppHandle,
    notification: Notification,
) -> Result<(), apw_core::notify::NotifyError> {
    let settings = match app.try_state::<AppState>() {
        Some(state) => state.settings_snapshot(),
        None => return Ok(()),
    };

    // 系统通知总是发。它是最轻量也最可靠的一条，没有理由让用户关掉它之后
    // 就完全收不到东西。
    emit_system_notification(app, &notification);

    let mut channels = Multi::new();
    if settings.sound_enabled {
        channels.push(Sound::embedded());
    }
    if !settings.bark_url.trim().is_empty() {
        let http = app
            .try_state::<AppState>()
            .map(|s| s.http.clone())
            .unwrap_or_default();
        // Bark 每次现构造：地址是用户随时可改的设置项，缓存实例会在改完地址后
        // 继续往旧地址推。共享的 http 客户端一并传进去，连接池仍然复用。
        // 用户可以填多个地址（分号分隔），每个地址一条渠道，并发推送、各自报错。
        for bark in Bark::from_list(&settings.bark_url, http) {
            channels.push(bark);
        }
    }
    if !settings.feishu_webhook.trim().is_empty() {
        let http = app
            .try_state::<AppState>()
            .map(|s| s.http.clone())
            .unwrap_or_default();
        // 飞书与 Bark 同样的构造规则：地址随时可改、支持分号分隔多个、
        // 共享连接池，渠道各自报错。
        for feishu in Feishu::from_list(&settings.feishu_webhook, http) {
            channels.push(feishu);
        }
    }
    if channels.is_empty() {
        return Ok(());
    }
    channels.notify(&notification).await
}

fn emit_system_notification(app: &AppHandle, n: &Notification) {
    use tauri_plugin_notification::NotificationExt;
    // 通知发不出去（用户在系统里关了权限）不该影响其他渠道，也不该让流程中断。
    if let Err(err) = app
        .notification()
        .builder()
        .title(&n.title)
        .body(&n.body)
        .show()
    {
        let _ = app.emit(NOTICE_CHANNEL, format!("系统通知发送失败：{err}"));
    }
}

/// 用账号会话完成一次加购，并产出一条可持久化的订单记录。
///
/// 加购失败不是命令错误：界面需要看到失败原因，也可能需要重试，所以这里
/// 返回 `Ok(OrderRecord)`，只有账号/地区都不存在时才返回 `Err`。
async fn perform_order(
    app: &AppHandle,
    account_id: &str,
    target: &Target,
) -> Result<OrderRecord, String> {
    let state = app.state::<AppState>();
    let account = state
        .account(account_id)
        .ok_or_else(|| "账号不存在".to_string())?;
    let region =
        region_by_locale(&target.locale).ok_or_else(|| format!("认不出地区 {}", target.locale))?;

    let record = if !account.session.has_cookies() {
        OrderRecord::failed(
            account_id,
            target,
            "这个账号还没有导入会话 Cookie；打开「账号」面板粘贴浏览器 Cookie 后再试。",
        )
    } else {
        let proxies = state.settings_snapshot().proxies;
        match order::add_to_bag(
            region,
            &target.part_number,
            &account.session.cookies,
            &proxies,
        )
        .await
        {
            Ok(outcome) => OrderRecord::added(account_id, target, &outcome),
            Err(err) => OrderRecord::failed(account_id, target, err.to_string()),
        }
    };

    state.record_order(account_id, record.clone());
    let _ = app.emit(
        ORDER_CHANNEL,
        serde_json::json!({ "accountId": account_id, "order": record }),
    );

    let notification = match record.status {
        OrderStatus::Added => Notification::new(
            "已加入购物袋",
            format!(
                "{}\n{}\n请打开付款链接完成付款（需使用导入 Cookie 的同一浏览器会话）",
                record.product_name, record.message
            ),
        )
        .with_url(record.payment_url.clone()),
        OrderStatus::Failed => Notification::new(
            "自动加购失败",
            format!("{}\n{}", record.product_name, record.message),
        ),
    };
    let _ = dispatch_notification(app, notification).await;

    Ok(record)
}

/// 自动加购的节流闸门。多个目标同时到货时只处理第一个。
async fn try_auto_order(app: AppHandle, account_id: String, target: Target) {
    let state = app.state::<AppState>();
    let Some(runtime) = state.runtime(&account_id) else {
        return;
    };
    let mut gate = runtime.gate.lock().await;
    if let Some(hold_until) = gate.hold_until
        && Instant::now() < hold_until
    {
        return;
    }
    let key = target.key();
    if gate.ordered.contains(&key) {
        return;
    }
    let Some(account) = state.account(&account_id) else {
        return;
    };
    if !account.enabled || !account.auto_order || !account.session.has_cookies() {
        return;
    }
    gate.ordered.insert(key.clone());

    let result = perform_order(&app, &account_id, &target).await;
    let added = matches!(result, Ok(ref record) if record.status == OrderStatus::Added);
    gate.hold_until = Some(
        Instant::now()
            + if added {
                AUTO_ORDER_HOLD
            } else {
                AUTO_ORDER_RETRY
            },
    );
    if !added {
        gate.ordered.remove(&key);
    }
}

/// 消费某个账号的引擎事件：转发给前端，并在有货时发提醒/自动加购。
async fn pump_account_events(
    app: AppHandle,
    account_id: String,
    mut events: tokio::sync::mpsc::Receiver<Event>,
) {
    let mut auto_open_bag = AutoOpenBag::default();
    let (notifications, pending) = notification_queue::channel();
    let notification_app = app.clone();
    tauri::async_runtime::spawn(notification_queue::run(pending, move |notification| {
        let app = notification_app.clone();
        async move {
            if let Err(err) = dispatch_notification(&app, notification).await {
                let _ = app.emit(NOTICE_CHANNEL, format!("发送提醒时出错：{err}"));
            }
        }
    }));

    while let Some(event) = events.recv().await {
        // 先原样转发，外加账号 id。前端拿到的事件流与引擎发出的完全一致，
        // 中间少一层可能出错的翻译；账号 id 只用于路由。
        let _ = app.emit(
            EVENT_CHANNEL,
            serde_json::json!({ "accountId": account_id, "event": event }),
        );

        match &event {
            Event::RunStateChanged { running } => {
                auto_open_bag.on_run_state_changed(*running);
            }
            Event::CycleComplete { snapshot, .. } => {
                if let Some(state) = app.try_state::<AppState>() {
                    state.forget_out_of_stock(&account_id, snapshot);
                }
            }
            Event::InStock {
                state: target_state,
            } => {
                let target = &target_state.target;
                let account = app
                    .try_state::<AppState>()
                    .and_then(|state| state.account(&account_id));
                let account_name = account
                    .as_ref()
                    .map_or_else(|| "账号".to_string(), |account| account.name.clone());

                let notification = Notification::new(
                    "有货了",
                    format!(
                        "{account_name}\n{} {}",
                        target.store_title, target.product_name
                    ),
                );
                let notification = match region_by_locale(&target.locale) {
                    Some(region) => notification.with_url(region.bag_url()),
                    None => notification,
                };

                let auto_handles_order = account.as_ref().is_some_and(|account| {
                    account.enabled && account.auto_order && account.session.has_cookies()
                });
                if !auto_handles_order {
                    let settings = app
                        .try_state::<AppState>()
                        .map(|state| state.settings_snapshot())
                        .unwrap_or_default();
                    use tauri_plugin_opener::OpenerExt;
                    if let Err(err) = auto_open_bag.open_if_needed(
                        settings.open_bag_on_hit,
                        &target.locale,
                        |url| app.opener().open_url(url, None::<&str>),
                    ) {
                        let _ = app.emit(NOTICE_CHANNEL, format!("打开购物袋失败：{err}"));
                    }
                }

                // 不逐项等待网络推送，否则15个Bark超时会让快照和暂停状态迟到150秒。
                // 队列满时保留背压，不丢提醒，也不无限创建后台任务。
                if notifications.send(notification).await.is_err() {
                    let _ = app.emit(NOTICE_CHANNEL, "提醒发送任务已停止，无法发送到货提醒");
                }

                if auto_handles_order {
                    let order_app = app.clone();
                    let order_account = account_id.clone();
                    let order_target = target.clone();
                    tauri::async_runtime::spawn(async move {
                        try_auto_order(order_app, order_account, order_target).await;
                    });
                }
            }
            _ => {}
        }
    }
}

/// 建系统托盘。
///
/// 关窗口时不退出而是收进托盘：这个工具的正常用法就是挂上几个小时等发售，
/// 让它一直占着一个窗口和 Dock 图标没有道理。
fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(
            app.default_window_icon().cloned().ok_or_else(|| {
                tauri::Error::AssetNotFound("默认窗口图标缺失，无法建立托盘".into())
            })?,
        )
        .tooltip("Apple Pickup Watcher")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => reveal_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn reveal_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// 载入设置。读不出来时放弃写盘并留档原文件。
///
/// 这一条是刻意的：读不到旧配置**不等于**用户没有配置。若照常写盘，界面初始化
/// 时的几次控件赋值就会把仅存的那份原子替换成一份空的默认配置，监控列表再也
/// 找不回来。Go 版正是这么丢过数据。
fn load_settings(notices: &mut Vec<String>) -> (Settings, Option<SettingsStore>) {
    let store = match SettingsStore::new() {
        Ok(s) => s,
        Err(err) => {
            notices.push(format!("配置目录不可用，本次运行的设置不会被保存：{err}"));
            return (Settings::default(), None);
        }
    };

    // 「新版配置文件还不存在」才是首次运行的判据。
    //
    // 不能用「目标列表为空」代替：用户删光目标后保存的是一份合法的空目标配置，
    // 而旧版 settings.json 是刻意保留不删的（为了能回退），于是下次启动会把他
    // 亲手删掉的目标连同 locale、Bark 地址一起原样倒回来。
    let first_run = store.path().symlink_metadata().is_err();

    match store.load() {
        Ok(settings) => {
            if first_run
                && let Some(legacy) = store.import_legacy()
                && !legacy.targets.is_empty()
            {
                notices.push(format!(
                    "已从旧版设置迁移了 {} 条监控目标。",
                    legacy.targets.len()
                ));
                // 立刻落盘。否则用户不改任何设置时新版文件一直不存在，
                // 每次启动都要重迁一遍，用户删掉的目标也会一直复活。
                if let Err(err) = store.save(&legacy) {
                    notices.push(format!("迁移结果暂时没能保存：{err}"));
                }
                return (legacy, Some(store));
            }
            (settings, Some(store))
        }
        Err(err) => {
            match store.preserve_corrupted() {
                Ok(Some(path)) => {
                    notices.push(format!("读取设置失败，原文件已备份到 {}", path.display()))
                }
                Ok(None) => {}
                Err(backup_err) => {
                    notices.push(format!("读取设置失败，且无法备份原文件：{backup_err}"));
                }
            }
            notices.push(format!(
                "读取设置失败，已回退到默认设置，并且本次运行不会覆盖磁盘上的配置：{err}"
            ));
            (Settings::default(), None)
        }
    }
}

/// 载入账号。首次运行时把旧版设置里的目标迁进一个默认账号。
///
/// 旧版 `settings.v2.json` 原样保留，不做改写；账号迁移只写 `accounts.v1.json`。
fn load_accounts(
    notices: &mut Vec<String>,
    settings: &Settings,
) -> (Vec<Account>, Option<AccountStore>) {
    let default_account = || {
        let mut account = Account::new("默认账号", settings.locale.clone());
        account.interval_seconds = settings.interval_seconds;
        account.targets = settings.targets.clone();
        account
    };

    let store = match AccountStore::new() {
        Ok(store) => store,
        Err(err) => {
            notices.push(format!(
                "账号配置目录不可用，本次运行的账号不会被保存：{err}"
            ));
            return (vec![default_account()], None);
        }
    };

    match store.load() {
        Ok(Some(mut accounts)) => {
            if accounts.is_empty() {
                let account = default_account();
                if let Err(err) = store.save(std::slice::from_ref(&account)) {
                    notices.push(format!("保存默认账号失败：{err}"));
                }
                accounts.push(account);
            }
            (accounts, Some(store))
        }
        Ok(None) => {
            let account = default_account();
            if let Err(err) = store.save(std::slice::from_ref(&account)) {
                notices.push(format!("保存默认账号失败：{err}"));
            }
            if !settings.targets.is_empty() {
                notices.push(format!(
                    "已把旧版设置里的 {} 个监控目标迁入「默认账号」。",
                    settings.targets.len()
                ));
            }
            (vec![account], Some(store))
        }
        Err(err) => {
            match store.preserve_corrupted() {
                Ok(Some(path)) => {
                    notices.push(format!("读取账号失败，原文件已备份到 {}", path.display()))
                }
                Ok(None) => {}
                Err(backup_err) => {
                    notices.push(format!("读取账号失败，且无法备份原文件：{backup_err}"));
                }
            }
            notices.push(format!(
                "读取账号失败，已回退到默认账号；本次运行不会覆盖原账号文件：{err}"
            ));
            (vec![default_account()], Some(store))
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let mut notices = Vec::new();
            let (mut settings, store) = load_settings(&mut notices);

            // 旧版本保存的 Apple Watch 目标没有搭档表带，查出来会是假的
            // 「无货」；启动时按目录补齐，下次写盘就带上了。
            let catalog = Catalog::new();
            catalog.attach_companions(&mut settings.targets);
            catalog.attach_locations(&mut settings.targets);

            let (mut accounts, account_store) = load_accounts(&mut notices, &settings);
            for account in &mut accounts {
                catalog.attach_companions(&mut account.targets);
                catalog.attach_locations(&mut account.targets);
            }

            let client = AppleClient::new(ClientConfig {
                proxies: settings.proxies.clone(),
                ..ClientConfig::default()
            })
            .map_err(|e| format!("构造 Apple 客户端失败：{e}"))?;

            app.manage(AppState {
                accounts: RwLock::new(accounts.clone()),
                account_store,
                runtimes: RwLock::new(HashMap::new()),
                client,
                catalog,
                http: reqwest::Client::new(),
                settings: RwLock::new(settings),
                store,
            });

            let handle: AppHandle = app.handle().clone();
            for account in &accounts {
                if let Err(err) = app.state::<AppState>().start_runtime(&handle, account) {
                    notices.push(format!("账号「{}」的监控实例启动失败：{err}", account.name));
                }
            }

            if let Err(err) = setup_tray(&handle) {
                // 托盘建不起来只是少一项能力，不该让程序起不来。
                notices.push(format!("系统托盘不可用：{err}"));
            }

            if !notices.is_empty() {
                // 界面还没订阅上，稍等一下再发。这些是降级说明，用户必须看见 ——
                // 只写到 stderr 是没用的，用户是双击图标启动的。
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    for notice in notices {
                        let _ = handle.emit(NOTICE_CHANNEL, notice);
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // 收进托盘而不是退出。真要退出走托盘菜单里的「退出」。
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_regions,
            list_categories,
            list_stores,
            list_products,
            refresh_products,
            get_settings,
            save_settings,
            set_push_urls,
            list_accounts,
            create_account,
            update_account,
            delete_account,
            set_account_session,
            open_account_login,
            save_account_login_cookies,
            clear_account_session,
            check_account_session,
            set_account_targets,
            set_account_interval,
            set_account_locale,
            get_account_snapshot,
            start_account,
            stop_account,
            start_all,
            stop_all,
            place_order_now,
            get_snapshot,
            set_targets,
            set_interval,
            start_watching,
            stop_watching,
            is_running,
            test_notify,
            check_for_update,
            install_update,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}
