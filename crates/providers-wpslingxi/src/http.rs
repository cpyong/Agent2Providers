//! 出网薄封装：按「账号级代理 → 网关默认出口 → 直连」优先级建客户端，
//! 外加 JSON 收发与**打开流式响应**两条路径。
//!
//! ── 为什么会有 `open_stream` ────────────────────────────────
//! 灵犀的推理响应不是标准 OpenAI chat SSE（正文在 `choices[].text`、不发
//! `[DONE]`、每帧一行 `data:{…}`）。插件必须自己逐帧读、逐帧翻译，所以
//! 不能把响应交给网关的翻译机，也不能用「收齐文本再解析」的办法 —— 那会
//! 丢掉流式语义。这里只负责把 `reqwest::Response` 交出去，解析在
//! `wpslingxi/chat.rs`。
//!
//! ── 与其余独立包 `http.rs` 的关系 ───────────────────────────
//! 客户端缓存、代理优先级、超时口径同源；多出来的是 `Opened` 与
//! `open_stream`。合集包那份还带着给二十多家用的 `send_raw`、系统代理等，
//! 本包没有调用点，不复制。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use serde_json::Value;

/// 控制面请求的超时（目录 / 余额 / 任务 / 建 run）。
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// 模型通话的总时长上限（推理模型可能跑很久，与上游客户端同口径）。
pub const CHAT_TIMEOUT: Duration = Duration::from_secs(600);

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

/// 建一个客户端。代理 URL 非法时降级直连 —— 可用性优先。
fn build(proxy_url: &str) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        // 客户端级超时只是兜底：每个请求都会再显式给一次（流式要 600 秒，
        // 控制面只要 30 秒）。
        .timeout(CHAT_TIMEOUT);
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

/// 发一次 POST（JSON 体）并解析 JSON（**非 2xx 也返回**）。
pub async fn post_json(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    timeout: Duration,
) -> Result<(u16, Value), String> {
    request_json(
        client,
        reqwest::Method::POST,
        url,
        headers,
        Some(body),
        timeout,
    )
    .await
}

/// 通用 JSON 请求：任意方法 + 可选 JSON 体 + 逐请求超时。
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

/// 打开一条流式响应。
///
/// 成功时交出 `Response`（调用方用 `chunk()` 逐段读）；非 2xx 时把正文收齐
/// 转成 JSON 交给调用方分类 —— 上游的 402 / 429 / 404 都带业务文案，丢了
/// 就只能报一句「上游返回错误」。
pub enum Opened {
    Stream {
        headers: BTreeMap<String, String>,
        response: reqwest::Response,
    },
    Error {
        status: u16,
        payload: Value,
    },
}

pub async fn open_stream(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    timeout: Duration,
) -> Result<Opened, String> {
    let mut request = client
        .post(url)
        .timeout(timeout)
        .header("Accept", "text/event-stream")
        .header("Content-Type", "application/json")
        .body(body.to_string());
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request.send().await.map_err(|error| describe(&error))?;
    let status = response.status().as_u16();
    let mut head = BTreeMap::new();
    for (name, value) in response.headers().iter() {
        if let Ok(text) = value.to_str() {
            head.insert(name.as_str().to_string(), text.to_string());
        }
    }
    if !(200..300).contains(&status) {
        let text = response.text().await.map_err(|error| describe(&error))?;
        return Ok(Opened::Error {
            status,
            payload: parse_json(&text),
        });
    }
    Ok(Opened::Stream {
        headers: head,
        response,
    })
}

/// 空体 / 非 JSON 一律给 `Value::Null`（上游 5xx 常给 HTML）。
fn parse_json(text: &str) -> Value {
    if text.trim().is_empty() {
        return Value::Null;
    }
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// 传输错误 → 人话（保留原因，便于排障）。
pub fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "连接 WPS 灵犀上游超时".to_string()
    } else if error.is_connect() {
        format!("无法连接 WPS 灵犀上游: {error}")
    } else {
        format!("请求 WPS 灵犀上游失败: {error}")
    }
}

/// 当前毫秒时间戳。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 进程内唯一的短 id（`X-Request-Id` / `client_request_id` / `chatcmpl-`）。
///
/// 不引 uuid 依赖：时间戳 + pid + 自增序号在本进程内已经唯一，这三个用途
/// 都只要求「同一次请求不与别的请求撞车」。
pub fn unique_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:x}{:x}{:x}",
        now_ms() as u64,
        std::process::id(),
        seq
    )
}
