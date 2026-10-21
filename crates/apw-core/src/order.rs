//! 账号会话下的购物袋操作。
//!
//! 库存判断仍由 [`crate::watcher`] 负责；这里只处理一件事：**用某个账号已经
//! 导入的浏览器会话，把商品加入该账号的 Apple 购物袋**，并给出用户自己去付款
//! 的地址。付款这一步刻意不自动化 —— 它涉及支付方式、验证码和账单地址，
//! 任何一步失败都不该由一个后台程序替用户猜。
//!
//! # 会话是怎么起作用的
//!
//! Apple 的购物袋挂在会话 Cookie（`dssid2`）上，而不是挂在某个可分享的订单号上。
//! 因此自动加购能够成立的前提是：用户从**准备用来付款的同一个浏览器**里复制
//! Cookie。只要浏览器和本程序用的是同一份会话，本程序加进去的商品就会出现在
//! 用户打开购物袋链接时看到的那个袋子里。界面和文档必须把这一点说清楚，不能
//! 让人误以为链接本身可以跨会话分享。
//!
//! # 失败必须是失败
//!
//! 和库存判定一样，这里不存在「大概是成功」：没有拿到 `addedToBag` 或明确的
//! `responseCode: SUCCESS`，就是失败，并带上 Apple 的原始说明。绝不能因为
//! HTTP 200 就把失败当成已下单。

use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::accounts::{SessionStatus, now_ms};
use crate::apple::RequestProfile;
use crate::model::{Region, Target, region_by_locale};

/// Apple 返回的响应都不大；超过这个数说明拿到的不是加购结果。
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

/// 加购是一次用户能感知到的交互，超时要短到让失败能被看见。
const ORDER_TIMEOUT: Duration = Duration::from_secs(25);

/// 购物袋状态接口使用的公开 apikey，和 Apple 商店前端一致。
const BAG_STATUS_API_KEY: &str = "SJHJUH4YFCTTPD4F4";

/// 一次加购的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BagOutcome {
    /// Apple 回报的购物袋商品数；没有返回时为 `None`。
    pub bag_quantity: Option<u32>,
    /// Apple 的原始提示，或本程序补的说明。
    pub message: String,
    /// 用户打开后可以继续结账的购物袋地址。
    pub payment_url: String,
    pub added_at_ms: u64,
}

/// 一次加购尝试的记录，随账号持久化。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderRecord {
    pub id: String,
    pub account_id: String,
    pub part_number: String,
    pub product_name: String,
    pub store_title: String,
    pub locale: String,
    pub status: OrderStatus,
    pub message: String,
    /// 即使加购失败也会给出购物袋地址，用户仍可手动检查。
    pub payment_url: String,
    pub bag_quantity: Option<u32>,
    pub attempted_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// 商品已经在账号的购物袋里，等待用户付款。
    Added,
    /// 没有加入购物袋，原因见 `message`。
    Failed,
}

impl OrderRecord {
    pub fn added(account_id: &str, target: &Target, outcome: &BagOutcome) -> Self {
        Self {
            id: new_order_id(),
            account_id: account_id.to_string(),
            part_number: target.part_number.clone(),
            product_name: target.product_name.clone(),
            store_title: target.store_title.clone(),
            locale: target.locale.clone(),
            status: OrderStatus::Added,
            message: outcome.message.clone(),
            payment_url: outcome.payment_url.clone(),
            bag_quantity: outcome.bag_quantity,
            attempted_at_ms: outcome.added_at_ms,
        }
    }

    pub fn failed(account_id: &str, target: &Target, message: impl Into<String>) -> Self {
        Self {
            id: new_order_id(),
            account_id: account_id.to_string(),
            part_number: target.part_number.clone(),
            product_name: target.product_name.clone(),
            store_title: target.store_title.clone(),
            locale: target.locale.clone(),
            status: OrderStatus::Failed,
            message: message.into(),
            payment_url: region_by_locale(&target.locale)
                .map(Region::bag_url)
                .unwrap_or_default(),
            bag_quantity: None,
            attempted_at_ms: now_ms(),
        }
    }
}

/// 会话校验结果。校验失败只代表「不知道」，绝不折叠成「已失效」——
/// 网络不通和 Apple 明确拒绝是两回事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCheck {
    pub status: SessionStatus,
    pub detail: Option<String>,
}

/// 加购过程中的失败。
#[derive(Debug, thiserror::Error)]
pub enum OrderError {
    #[error("这个账号还没有导入会话 Cookie")]
    NoSession,

    #[error("构造 Apple 会话失败：{0}")]
    Session(String),

    #[error("没有拿到 Apple 的 atb 令牌，通常是 Cookie 已失效；请重新从浏览器导入")]
    NoAtbToken,

    #[error("Apple 返回 HTTP {status}：{detail}")]
    Http { status: u16, detail: String },

    #[error("Apple 拒绝加入购物袋：{0}")]
    Rejected(String),

    #[error("Apple 返回了无法识别的加购结果：{0}")]
    Schema(String),

    #[error("网络请求失败：{0}")]
    Transport(String),
}

/// 用账号会话检查购物袋接口是否还能用。
///
/// 判定规则：
/// - HTTP 2xx：会话可用；有 `myacinfo` / `dssid` 时视为已登录，否则是访客会话。
/// - HTTP 401 / 403：Apple 明确拒绝了这份会话，标记为失效。
/// - 其他情况（超时、5xx、541）：**未知**，让用户重试或换网络，不替用户下结论。
pub async fn check_session(region: &Region, cookies: &str, proxies: &[String]) -> SessionCheck {
    if cookies.trim().is_empty() {
        return SessionCheck {
            status: SessionStatus::NotConfigured,
            detail: Some("还没有导入 Cookie".into()),
        };
    }

    let status_url = format!("{}/shop/bag/status", region.base_url);
    let mut last = None;
    for proxy in routes(proxies) {
        let (client, jar) = match build_client(region, cookies, proxy) {
            Ok(pair) => pair,
            Err(err) => {
                last = Some(err.to_string());
                continue;
            }
        };
        let Ok(mut url) = Url::parse(&status_url) else {
            return SessionCheck {
                status: SessionStatus::Unknown,
                detail: Some("购物袋状态地址无效".into()),
            };
        };
        url.query_pairs_mut()
            .append_pair("apikey", BAG_STATUS_API_KEY)
            .append_pair("storefront", storefront(region.locale));

        let response = client
            .get(url)
            .headers(json_headers(region, Some(&region.bag_url())))
            .send()
            .await;
        match response {
            Ok(response) => {
                let http_status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                if (200..300).contains(&http_status) {
                    let base = Url::parse(region.base_url).ok();
                    let signed_in = base.as_ref().is_some_and(|base| {
                        cookie_value(&jar, base, "myacinfo").is_some()
                            || cookie_value(&jar, base, "dssid").is_some()
                            || cookie_value(&jar, base, "dssid2").is_some()
                    });
                    let session_status = if signed_in {
                        SessionStatus::SignedIn
                    } else {
                        SessionStatus::Guest
                    };
                    let detail = match bag_items(&body) {
                        Some(items) => {
                            format!("购物袋接口可用（HTTP {http_status}，当前 {items} 件商品）")
                        }
                        None => format!("购物袋接口可用（HTTP {http_status}）"),
                    };
                    return SessionCheck {
                        status: session_status,
                        detail: Some(detail),
                    };
                }
                if http_status == 401 || http_status == 403 {
                    return SessionCheck {
                        status: SessionStatus::Expired,
                        detail: Some(format!(
                            "Apple 拒绝了这份会话（HTTP {http_status}），请重新导入 Cookie"
                        )),
                    };
                }
                last = Some(format!("购物袋接口返回 HTTP {http_status}"));
            }
            Err(err) => last = Some(format!("网络请求失败：{err}")),
        }
    }

    SessionCheck {
        status: SessionStatus::Unknown,
        detail: last,
    }
}

/// 把商品加入账号的购物袋。
///
/// 先直接连接；失败时依次尝试设置里的代理。每个出口都会重新建立自己的
/// Cookie 罐，绝不把某个代理上的会话串到另一个出口。
pub async fn add_to_bag(
    region: &Region,
    part_number: &str,
    cookies: &str,
    proxies: &[String],
) -> Result<BagOutcome, OrderError> {
    if cookies.trim().is_empty() {
        return Err(OrderError::NoSession);
    }
    let part = part_number.trim();
    if part.is_empty() {
        return Err(OrderError::Rejected("零件号为空".into()));
    }

    let mut last = None;
    for proxy in routes(proxies) {
        match add_via(region, part, cookies, proxy).await {
            Ok(outcome) => return Ok(outcome),
            Err(err) => last = Some(err),
        }
    }
    Err(last.unwrap_or_else(|| OrderError::Transport("没有可用的出口线路".into())))
}

/// 用账号现有 Cookie 建立一个浏览器式的请求客户端。
fn build_client(
    region: &Region,
    cookies: &str,
    proxy: Option<&str>,
) -> Result<(reqwest::Client, Arc<Jar>), OrderError> {
    let base = Url::parse(region.base_url).map_err(|err| OrderError::Session(err.to_string()))?;
    let profile = RequestProfile::chrome();
    let jar = Arc::new(Jar::default());
    for piece in cookies.split(';') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        // 解析失败的单条 Cookie 会被 cookie_store 丢掉；后面拿不到 atb 令牌时
        // 会明确报错，而不是带着半份会话继续。
        jar.add_cookie_str(piece, &base);
    }

    let mut builder = reqwest::Client::builder()
        .timeout(ORDER_TIMEOUT)
        .connect_timeout(Duration::from_secs(8))
        .cookie_provider(jar.clone())
        .user_agent(profile.user_agent);
    if let Some(proxy) = proxy {
        let parsed = reqwest::Proxy::all(proxy).map_err(|err| {
            OrderError::Session(format!("代理地址无效 {}：{err}", redact_proxy(proxy)))
        })?;
        builder = builder.proxy(parsed);
    }
    let client = builder
        .build()
        .map_err(|err| OrderError::Session(format!("构造 HTTP 客户端失败：{err}")))?;
    Ok((client, jar))
}

async fn add_via(
    region: &Region,
    part: &str,
    cookies: &str,
    proxy: Option<&str>,
) -> Result<BagOutcome, OrderError> {
    let (client, jar) = build_client(region, cookies, proxy)?;
    let base = Url::parse(region.base_url).map_err(|err| OrderError::Session(err.to_string()))?;
    let product_url = region.default_buy_page_url();

    // 购买页负责把地区、语言和商店会话的 Cookie 种下来。页面本身的 2xx 不重要，
    // 连接失败才是真的发不出去。
    client
        .get(&product_url)
        .headers(navigation_headers(region, None))
        .send()
        .await
        .map_err(|err| OrderError::Transport(err.to_string()))?
        .bytes()
        .await
        .map_err(|err| OrderError::Transport(err.to_string()))?;

    // atb 令牌是 `/shop/bag/add` 的 CSRF 凭据。没有它 Apple 会返回 CSRF_ERROR。
    let mut beacon = Url::parse(&format!("{}/shop/beacon/atb", region.base_url))
        .map_err(|err| OrderError::Session(err.to_string()))?;
    beacon.query_pairs_mut().append_pair("product", part);
    client
        .get(beacon)
        .headers(beacon_headers(region, &product_url))
        .send()
        .await
        .map_err(|err| OrderError::Transport(err.to_string()))?
        .bytes()
        .await
        .map_err(|err| OrderError::Transport(err.to_string()))?;

    let token = cookie_value(&jar, &base, "as_atb")
        .as_deref()
        .and_then(atb_token)
        .ok_or(OrderError::NoAtbToken)?;

    let mut last = None;
    for url in add_urls(region, part, &token) {
        let response = match client
            .get(url)
            .headers(json_headers(region, Some(&product_url)))
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                last = Some(OrderError::Transport(err.to_string()));
                break;
            }
        };
        let (status, body) = read_body(response).await?;
        match interpret_add(status, &body, region) {
            Ok(outcome) => return Ok(outcome),
            Err(err) => {
                // 路径不对才换下一个候选；CSRF、被拦或明确的业务错误换路径也没用。
                if matches!(
                    err,
                    OrderError::Http {
                        status: 404 | 405,
                        ..
                    }
                ) {
                    last = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(last.unwrap_or_else(|| OrderError::Rejected("Apple 没有接受加购接口".into())))
}

/// 解析加购响应。只有明确的成功才返回 `Ok`。
fn interpret_add(status: u16, body: &str, region: &Region) -> Result<BagOutcome, OrderError> {
    if !(200..300).contains(&status) {
        return Err(OrderError::Http {
            status,
            detail: safe_detail(body),
        });
    }
    let value: Value =
        serde_json::from_str(body).map_err(|_| OrderError::Schema(safe_detail(body)))?;
    let code = value
        .get("responseCode")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let added = value
        .get("addedToBag")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .map(safe_detail)
        .filter(|message| !message.is_empty());

    if added || code.eq_ignore_ascii_case("SUCCESS") {
        return Ok(BagOutcome {
            bag_quantity: value
                .get("bagQuantity")
                .and_then(Value::as_u64)
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
            message: message.unwrap_or_else(|| "已加入购物袋，请打开付款链接完成付款".into()),
            payment_url: region.bag_url(),
            added_at_ms: now_ms(),
        });
    }

    if code.eq_ignore_ascii_case("CSRF_ERROR") {
        return Err(OrderError::NoAtbToken);
    }
    Err(OrderError::Rejected(
        message.unwrap_or_else(|| format!("响应码 {code}")),
    ))
}

/// 候选加购地址。中国大陆商店固定在 `/cn/` 前缀下；其他地区先试站点根路径。
fn add_urls(region: &Region, part: &str, token: &str) -> Vec<Url> {
    let storefront = storefront(region.locale);
    let mut paths = Vec::new();
    if region.locale == "zh_CN" {
        paths.push(format!("{}/{storefront}/shop/bag/add", region.base_url));
    }
    paths.push(format!("{}/shop/bag/add", region.base_url));
    if region.locale != "zh_CN" {
        paths.push(format!("{}/{storefront}/shop/bag/add", region.base_url));
    }

    paths
        .into_iter()
        .filter_map(|path| {
            let mut url = Url::parse(&path).ok()?;
            url.query_pairs_mut()
                .append_pair("product", part)
                .append_pair("atbtoken", token)
                .append_pair("storefront", storefront);
            Some(url)
        })
        .collect()
}

async fn read_body(response: reqwest::Response) -> Result<(u16, String), OrderError> {
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(OrderError::Schema("Apple 返回的响应过大".into()));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|err| OrderError::Transport(err.to_string()))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(OrderError::Schema("Apple 返回的响应过大".into()));
    }
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

fn routes<'a>(proxies: &'a [String]) -> impl Iterator<Item = Option<&'a str>> + 'a {
    std::iter::once(None).chain(proxies.iter().map(|proxy| Some(proxy.as_str())))
}

fn cookie_value(jar: &Jar, url: &Url, name: &str) -> Option<String> {
    let header = jar.cookies(url)?.to_str().ok()?.to_string();
    header
        .split(';')
        .filter_map(|piece| piece.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
}

/// `as_atb` 的值形如 `1.0|<date>|<token>`，第三段才是加购需要的令牌。
pub fn atb_token(raw: &str) -> Option<String> {
    let mut parts = raw.split('|');
    parts.next()?;
    parts.next()?;
    let token = parts.next()?.trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn bag_items(body: &str) -> Option<u64> {
    let value: Value = serde_json::from_str(body).ok()?;
    value.get("items").and_then(Value::as_u64).or_else(|| {
        value
            .get("body")
            .and_then(|body| body.get("items"))
            .and_then(Value::as_u64)
    })
}

/// Apple 商店的 storefront 短代码。
pub fn storefront(locale: &str) -> &'static str {
    match locale {
        "zh_CN" => "cn",
        "zh_HK" => "hk",
        "zh_TW" => "tw",
        "ja_JP" => "jp",
        "en_SG" => "sg",
        "en_AU" => "au",
        "en_MY" => "my",
        _ => "cn",
    }
}

fn base_headers(region: &Region) -> HeaderMap {
    let profile = RequestProfile::chrome();
    let mut headers = HeaderMap::new();
    put(&mut headers, "user-agent", &profile.user_agent);
    put(&mut headers, "accept-language", region.accept_language());
    if let Some(hints) = profile.client_hints {
        put(&mut headers, "sec-ch-ua", &hints.ua);
        put(&mut headers, "sec-ch-ua-mobile", &hints.mobile);
        put(&mut headers, "sec-ch-ua-platform", &hints.platform);
    }
    headers
}

fn navigation_headers(region: &Region, referer: Option<&str>) -> HeaderMap {
    let mut headers = base_headers(region);
    put(
        &mut headers,
        "accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
    );
    put(&mut headers, "sec-fetch-site", "none");
    put(&mut headers, "sec-fetch-mode", "navigate");
    put(&mut headers, "sec-fetch-dest", "document");
    put(&mut headers, "upgrade-insecure-requests", "1");
    if let Some(referer) = referer {
        put(&mut headers, "referer", referer);
    }
    headers
}

fn beacon_headers(region: &Region, referer: &str) -> HeaderMap {
    let mut headers = base_headers(region);
    put(
        &mut headers,
        "accept",
        "image/avif,image/webp,image/apng,image/*,*/*;q=0.8",
    );
    put(&mut headers, "sec-fetch-site", "same-origin");
    put(&mut headers, "sec-fetch-mode", "no-cors");
    put(&mut headers, "sec-fetch-dest", "image");
    put(&mut headers, "referer", referer);
    headers
}

fn json_headers(region: &Region, referer: Option<&str>) -> HeaderMap {
    let mut headers = base_headers(region);
    put(
        &mut headers,
        "accept",
        "application/json, text/javascript, */*; q=0.01",
    );
    put(&mut headers, "x-requested-with", "XMLHttpRequest");
    put(&mut headers, "sec-fetch-site", "same-origin");
    put(&mut headers, "sec-fetch-mode", "cors");
    put(&mut headers, "sec-fetch-dest", "empty");
    if let Some(referer) = referer {
        put(&mut headers, "referer", referer);
    }
    headers
}

fn put(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

fn safe_detail(raw: &str) -> String {
    let one_line: String = raw
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(300)
        .collect();
    let trimmed = one_line.trim();
    if trimmed.is_empty() {
        "（响应为空）".into()
    } else {
        trimmed.to_string()
    }
}

fn redact_proxy(proxy: &str) -> String {
    match Url::parse(proxy) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.to_string()
        }
        Err(_) => "<无法解析的代理地址>".into(),
    }
}

fn new_order_id() -> String {
    use rand::Rng;
    let n: u64 = rand::rng().random();
    format!("order_{n:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cn() -> &'static Region {
        region_by_locale("zh_CN").expect("必须有中国大陆")
    }

    #[test]
    fn atb_令牌取第三段() {
        assert_eq!(
            atb_token("1.0|2026-10-21|abc-def"),
            Some("abc-def".to_string())
        );
        assert_eq!(atb_token("1.0|2026-10-21|"), None);
        assert_eq!(atb_token("broken"), None);
    }

    #[test]
    fn 成功响应解析出购物袋数量与付款地址() {
        let outcome = interpret_add(
            200,
            r#"{"addedToBag":true,"removedFromBag":false,"responseCode":"SUCCESS","bagQuantity":1,"message":"产品已添加到购物袋"}"#,
            cn(),
        )
        .expect("明确成功应当解析成功");
        assert_eq!(outcome.bag_quantity, Some(1));
        assert_eq!(outcome.payment_url, "https://www.apple.com.cn/shop/bag");
        assert!(outcome.message.contains("购物袋"));
    }

    #[test]
    fn csrf_错误不能被当成成功() {
        let err = interpret_add(
            200,
            r#"{"addedToBag":false,"responseCode":"CSRF_ERROR","message":"bad token"}"#,
            cn(),
        )
        .expect_err("CSRF 必须是失败");
        assert!(matches!(err, OrderError::NoAtbToken));
    }

    #[test]
    fn 每种地区都有对应的_storefront() {
        for region in crate::model::REGIONS {
            let code = storefront(region.locale);
            assert_eq!(code.len(), 2, "{} 的 storefront 不对", region.locale);
        }
    }

    #[test]
    fn 加购地址会转义零件号里的斜杠() {
        let urls = add_urls(cn(), "MG724CH/A", "token");
        assert!(urls[0].as_str().contains("product=MG724CH%2FA"));
        assert!(urls[0].as_str().contains("atbtoken=token"));
        assert!(urls[0].as_str().contains("/cn/shop/bag/add"));
    }
}
