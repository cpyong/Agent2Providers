//! 出网薄封装：与其余独立包同口径的代理优先级、控制面超时、JSON/表单收发。
//!
//! 这个包只发控制面请求（OAuth、积分、项目、模型目录），真正的对话请求由
//! 网关按 `build_chat` 给出的 URL / 头 / 体发出去，所以这里不需要
//! `open_stream`、不需要 600 秒流式超时。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent2api_provider_protocol as proto;
use serde_json::Value;

/// 控制面请求超时（积分回查、项目列表、换票）。
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);

/// RFC 3986 unreserved = `A-Z a-z 0-9 - . _ ~`。
///
/// OAuth 的 `application/x-www-form-urlencoded` 和 URL 路径段都要求保留这些
/// 字符。用 `NON_ALPHANUMERIC` 会把 `client_id` 里的 `_`、`scope` 里的 `.`
/// 也编成 `%5F` / `%2E`；虽然服务端解码后等价，但既有实现可能直接比对原始
/// 串，而且 RFC 3986 明确规定 unreserved 不该编码。
const UNRESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// 编码单个 query 值 / 路径段；`/`、`?`、`&`、`=` 一律百分号化。
pub fn encode_component(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, UNRESERVED).to_string()
}

fn cache() -> &'static Mutex<HashMap<String, reqwest::Client>> {
    static CACHE: OnceLock<Mutex<HashMap<String, reqwest::Client>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 账号级代理 → 网关默认出口 → 直连。
pub fn client(proxy: Option<&proto::WireProxy>, gateway_proxy: &str) -> reqwest::Client {
    let key = match proxy {
        Some(proxy) => proxy.to_url(),
        None => gateway_proxy.trim().to_string(),
    };
    if let Ok(guard) = cache().lock() {
        if let Some(existing) = guard.get(&key) {
            return existing.clone();
        }
    }
    let built = build(&key);
    if let Ok(mut guard) = cache().lock() {
        guard.insert(key, built.clone());
    }
    built
}

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

/// JSON 请求（非 2xx 也返回，交给调用方分类）。
pub async fn request_json(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    headers: &[(String, String)],
    body: Option<&Value>,
) -> Result<(u16, Value), String> {
    let mut request = client
        .request(method, url)
        .timeout(CONTROL_TIMEOUT)
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

/// 表单请求（OAuth 的 `/oauth/token` 用 `application/x-www-form-urlencoded`）。
pub async fn request_form(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    form: &str,
) -> Result<(u16, Value), String> {
    let mut request = client
        .post(url)
        .timeout(CONTROL_TIMEOUT)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form.to_string());
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request.send().await.map_err(|error| describe(&error))?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|error| describe(&error))?;
    Ok((status, parse_json(&text)))
}

pub fn parse_json(text: &str) -> Value {
    serde_json::from_str(text.trim()).unwrap_or(Value::Null)
}

/// 传输错误里给出「该做什么」的提示，别把 reqwest 的英文原样抛给用户。
pub fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "请求 Meoo 超时，请检查网络或稍后重试".to_string()
    } else if error.is_connect() {
        "连不上 Meoo 服务，请检查网络或代理设置".to_string()
    } else {
        format!("请求 Meoo 失败：{error}")
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 控制面响应常见两种形状：裸对象，或 `{success:true,data:{…}}`。
/// 网关 `/models` 的 `data` 是数组（OpenAI 风格），不能当信封拆。
pub fn unwrap_envelope(value: Value) -> Value {
    let is_envelope = value.get("success").and_then(Value::as_bool) == Some(true)
        || value.get("success").is_none() && value.get("errCode").is_none();
    if is_envelope {
        if let Some(data) = value.get("data") {
            if data.is_object() || data.is_array() {
                return data.clone();
            }
        }
    }
    value
}

/// 从响应里取上游错误信息（控制面是 problem+json，网关是 error 对象）。
pub fn error_message(value: &Value, status: u16) -> String {
    let text = ["message", "detail", "error_description", "msg"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let nested = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let code = value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .or_else(|| value.get("code").and_then(Value::as_str));
    match (text.or(nested), code) {
        (Some(message), Some(code)) => format!("HTTP {status}: {message}（{code}）"),
        (Some(message), None) => format!("HTTP {status}: {message}"),
        (None, Some(code)) => format!("HTTP {status}（{code}）"),
        (None, None) => format!("HTTP {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unwraps_success_envelope() {
        assert_eq!(unwrap_envelope(json!({"success": true, "data": {"points": 1}})), json!({"points": 1}));
    }

    #[test]
    fn keeps_bare_object() {
        assert_eq!(unwrap_envelope(json!({"account": {"availableBalance": 20}})), json!({"account": {"availableBalance": 20}}));
    }

    #[test]
    fn does_not_unwrap_error_envelope() {
        let error = json!({"success": false, "data": {"hint": "x"}, "errCode": "bad"});
        assert_eq!(unwrap_envelope(error.clone()), error);
    }

    #[test]
    fn error_message_reads_problem_json() {
        let body = json!({"detail": "缺少 Bearer token", "code": "invalid_token"});
        assert!(error_message(&body, 401).contains("invalid_token"));
    }
}
