//! 出网薄封装：按「账号级代理 → 网关默认出口 → 直连」优先级建客户端，
//! 外加两个 JSON 收发函数。
//!
//! ── 为什么收在一处 ──────────────────────────────────────────
//! 三个消费者（`todayai/client.rs` / `credentials.rs` / `models.rs`）共用同一套
//! 超时与代理口径。各写一份会让「这个请求到底走没走账号代理」变成一个需要
//! 逐个函数确认的问题。
//!
//! ── 与合集包 `crates/providers/src/http.rs` 的关系 ───────────
//! 逻辑同源，但这里只保留本家真正用到的几个函数（`client` / `get_json` /
//! `post_json` / `request_json` / `now_ms`）。合集包那份还带着给其余二十多家
//! 用的 `send_raw`、SSE 与系统代理客户端 —— 那些在本包里没有调用点。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use serde_json::Value;

/// 控制面请求的超时（发码 / 换令牌 / 目录这类小请求）。
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);

/// 客户端缓存（键 = 代理 URL，空串 = 直连）。
fn cache() -> &'static Mutex<HashMap<String, reqwest::Client>> {
    static CACHE: OnceLock<Mutex<HashMap<String, reqwest::Client>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取一个客户端：账号级代理优先，其次网关默认出口，都没有就直连。
///
/// `gateway_proxy` 传 `ctx.env.proxy`（`""` = 直连，`"direct"` = 显式直连，
/// 其余是代理 URL）。代理 URL 非法时降级直连 —— 可用性优先。
pub fn client(proxy: Option<&proto::WireProxy>, gateway_proxy: &str) -> reqwest::Client {
    let key = match proxy {
        Some(proxy) => proxy.to_url(),
        None => gateway_proxy.trim().to_string(),
    };
    if let Ok(guard) = cache().lock() {
        if let Some(client) = guard.get(&key) {
            return client.clone();
        }
    }
    let built = build(&key);
    if let Ok(mut guard) = cache().lock() {
        guard.insert(key, built.clone());
    }
    built
}

/// 建一个客户端（代理 URL 为空 / `direct` = 直连）。
fn build(proxy_url: &str) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(CONTROL_TIMEOUT);
    let trimmed = proxy_url.trim();
    if trimmed.is_empty() || trimmed == "direct" {
        // 显式 no_proxy：本家的口径是「直连就是直连」，不被机器系统代理改写
        builder = builder.no_proxy();
    } else {
        match reqwest::Proxy::all(trimmed) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(_) => builder = builder.no_proxy(),
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// 发一次 GET 并解析 JSON（**非 2xx 也返回**，判定权在调用方）。
pub async fn get_json(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<(u16, Value), String> {
    request_json(
        client,
        reqwest::Method::GET,
        url,
        headers,
        None,
        CONTROL_TIMEOUT,
    )
    .await
}

/// 发一次 POST（JSON 体）并解析 JSON 响应（**非 2xx 也返回**）。
pub async fn post_json(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<(u16, Value), String> {
    request_json(
        client,
        reqwest::Method::POST,
        url,
        headers,
        Some(body),
        CONTROL_TIMEOUT,
    )
    .await
}

/// 通用请求：任意方法 + 可选 JSON 体 + 逐请求超时。
///
/// ── 为什么超时要逐请求给 ────────────────────────────────────
/// 控制面（发码 / 换令牌 / 目录）20 秒够；会话面的一次长轮询本身就能挂
/// 25 秒，用同一个客户端超时会把**正常**的长轮询当成超时错误。客户端仍是
/// 缓存里那一份（代理口径只有一处），超时按请求覆盖。
pub async fn request_json(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    headers: &[(String, String)],
    body: Option<&Value>,
    timeout: Duration,
) -> Result<(u16, Value), String> {
    let mut request = client
        .request(method, url)
        .timeout(timeout)
        .header("Accept", "application/json");
    if let Some(body) = body {
        request = request
            .header("Content-Type", "application/json")
            .body(body.to_string());
    }
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request.send().await.map_err(|error| describe(&error))?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|error| describe(&error))?;
    Ok((status, parse_json(&text)))
}

/// 空体 / 非 JSON 一律给 `Value::Null`。
fn parse_json(text: &str) -> Value {
    if text.trim().is_empty() {
        return Value::Null;
    }
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// 传输错误 → 人话（保留原因，便于排障）。
pub fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "连接上游超时".to_string()
    } else if error.is_connect() {
        format!("无法连接上游: {error}")
    } else {
        format!("请求上游失败: {error}")
    }
}

/// 当前毫秒时间戳。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
