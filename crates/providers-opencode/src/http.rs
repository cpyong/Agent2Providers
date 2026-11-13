//! 出网薄封装：按「账号级代理 → 网关默认出口 → 直连」优先级建客户端，
//! 外加两个 JSON 收发函数。
//!
//! 控制面（探活 / 目录）用 20 秒超时；数据面由网关自己发请求，不经过这里。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use serde_json::Value;

/// 控制面请求的超时（探活 / 目录这类小请求）。
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);

/// 客户端缓存（键 = 代理 URL，空串 = 直连）。
fn cache() -> &'static Mutex<HashMap<String, reqwest::Client>> {
    static CACHE: OnceLock<Mutex<HashMap<String, reqwest::Client>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取一个客户端：账号级代理优先，其次网关默认出口，都没有就直连。
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
    let mut request = client
        .get(url)
        .timeout(CONTROL_TIMEOUT)
        .header("Accept", "application/json");
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
