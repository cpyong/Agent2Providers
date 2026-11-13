//! 本机 opencode v2 API 的连接信息、自动发现与 HTTP 薄封装。
//!
//! ── 这不是 OpenAI 兼容端点 ──────────────────────────────────
//! opencode 桌面端 / `opencode serve` 起的是自有 v2 API：模型、会话、消息
//! 都在 `/api/*` 下，前端页面挂在 `/`（未匹配路径一律回落 SPA）。把截图里
//! 那个 `http://127.0.0.1:26894` 填进「自定义上游基址」拼
//! `/v1/chat/completions` 只会拿到一屏 HTML，这是两条完全不同的协议。
//!
//! ── 服务地址与密码在哪 ──────────────────────────────────────
//! ```text
//!   %USERPROFILE%\.local\state\opencode\service.json
//!     {"id":"…","version":"2.2.6","url":"http://127.0.0.1:28894","pid":…,
//!      "password":"…"}
//!   %USERPROFILE%\.config\opencode\service.json   {"password":"…"}（无 url）
//! ```
//! 端口是**随机分配**且每次启动都可能变（截图里的 26894 现在是 28894），
//! 所以优先读 state 文件而不是写死端口。Basic auth 用户名默认 `opencode`
//! （`OPENCODE_SERVER_USERNAME` 可覆盖），密码为空表示服务端没开鉴权。

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::Value;

use super::ID;

/// 控制面超时（模型 / 会话 / 消息这类短请求）。
const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);

/// 一条到本机服务的连接。
#[derive(Clone, Debug)]
pub struct Server {
    /// 形如 `http://127.0.0.1:28894`，不带尾部斜杠。
    pub base: String,
    /// `Basic …`；`None` = 服务端没设密码。
    pub auth: Option<String>,
}

fn pick(record: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = record
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

fn env_value(name: &str) -> String {
    std::env::var(name).unwrap_or_default().trim().to_string()
}

fn env_path(name: &str) -> Option<PathBuf> {
    let value = env_value(name);
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

/// 简化 base64（Basic auth 用；不引额外依赖）。
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let a = bytes[index] as u32;
        let b = if index + 1 < bytes.len() { bytes[index + 1] as u32 } else { 0 };
        let c = if index + 2 < bytes.len() { bytes[index + 2] as u32 } else { 0 };
        let triple = (a << 16) | (b << 8) | c;
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        out.push(if index + 1 < bytes.len() {
            TABLE[((triple >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if index + 2 < bytes.len() {
            TABLE[(triple & 63) as usize] as char
        } else {
            '='
        });
        index += 3;
    }
    out
}

fn basic_auth(username: &str, password: &str) -> String {
    format!("Basic {}", base64(format!("{username}:{password}").as_bytes()))
}

/// `service.json` 的候选路径（按优先级）。
fn service_candidates() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(state) = env_path("XDG_STATE_HOME") {
        paths.push(state.join("opencode").join("service.json"));
    }
    for name in ["USERPROFILE", "HOME"] {
        if let Some(home) = env_path(name) {
            push_unique(&mut paths, home.join(".local").join("state").join("opencode").join("service.json"));
            push_unique(&mut paths, home.join(".config").join("opencode").join("service.json"));
        }
    }
    if let Some(appdata) = env_path("APPDATA") {
        push_unique(&mut paths, appdata.join("opencode").join("service.json"));
    }
    if let Some(local) = env_path("LOCALAPPDATA") {
        push_unique(&mut paths, local.join("opencode").join("service.json"));
    }
    paths
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|item| item == &path) {
        paths.push(path);
    }
}

/// 自动发现到的连接信息（都可能为空）。
#[derive(Default, Debug)]
pub struct Discovered {
    pub url: String,
    pub password: String,
    pub username: String,
}

/// 读环境变量 / `service.json`，拿服务地址与密码。
pub fn discover() -> Discovered {
    let mut found = Discovered {
        url: env_value("OPENCODE_SERVER_URL"),
        password: env_value("OPENCODE_SERVER_PASSWORD"),
        username: env_value("OPENCODE_SERVER_USERNAME"),
    };
    for path in service_candidates() {
        if !path.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if found.url.is_empty() {
            found.url = pick(&parsed, &["url", "baseUrl", "base_url"]);
        }
        if found.password.is_empty() {
            found.password = pick(&parsed, &["password", "pass", "token"]);
        }
        if !found.url.is_empty() && !found.password.is_empty() {
            break;
        }
    }
    found
}

/// 从账号 secret 还原连接（缺字段时用自动发现补齐）。
pub fn from_secret(secret: &Value) -> Result<Server, String> {
    let found = discover();
    let raw = pick(secret, &["baseUrl", "base_url", "url", "serverUrl"]);
    // 网关弹窗把「令牌」放在 token 里；看起来像 URL 时也当成服务地址
    let token = pick(secret, &["token"]);
    let base = if !raw.is_empty() {
        raw
    } else if token.starts_with("http://") || token.starts_with("https://") {
        token
    } else {
        found.url
    };
    if base.is_empty() {
        return Err(
            "没有找到本机 opencode 服务地址。请启动 opencode 桌面端（设置 → 服务器里能看到连接地址），或在插件页手填该地址"
                .to_string(),
        );
    }
    let username = {
        let from_secret = pick(secret, &["username", "user"]);
        if from_secret.is_empty() {
            if found.username.is_empty() { "opencode".to_string() } else { found.username }
        } else {
            from_secret
        }
    };
    let password = {
        let from_secret = pick(secret, &["password", "pass"]);
        if from_secret.is_empty() { found.password } else { from_secret }
    };
    let auth = if password.is_empty() {
        None
    } else {
        Some(basic_auth(&username, &password))
    };
    Ok(Server {
        base: base.trim().trim_end_matches('/').to_string(),
        auth,
    })
}

/// 控制面客户端：直连本机（**不走任何代理**）+ 20 秒超时。
pub fn control() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(CONTROL_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

/// 事件流客户端：不能有整体超时（超时会把长流掐断）。
pub fn stream_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .use_rustls_tls()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

/// 发一次 v2 API 请求，返回 `(status, json)`；非 2xx 也交调用方判定。
pub async fn request_json(
    client: &reqwest::Client,
    server: &Server,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
) -> Result<(u16, Value), String> {
    let url = format!("{}{path}", server.base);
    let mut request = client
        .request(method, &url)
        .header("Accept", "application/json");
    if let Some(auth) = &server.auth {
        request = request.header("Authorization", auth);
    }
    if let Some(payload) = body {
        request = request.header("Content-Type", "application/json").json(payload);
    }
    let response = request.send().await.map_err(|error| describe(&error))?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|error| describe(&error))?;
    let value = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    Ok((status, value))
}

fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "连接本机 opencode 服务超时".to_string()
    } else if error.is_connect() {
        format!("无法连接本机 opencode 服务（{}）：桌面端或 `opencode serve` 是否在运行？", server_hint(error))
    } else {
        format!("请求本机 opencode 服务失败: {error}")
    }
}

fn server_hint(error: &reqwest::Error) -> String {
    error
        .url()
        .map(|url| url.to_string())
        .unwrap_or_else(|| "地址未知".to_string())
}

/// 探活失败的可读文案。
pub fn probe_error(status: u16) -> String {
    match status {
        401 => "本机 opencode 服务要求密码，但密码不对（或用户名不是 opencode）；可从 ~/.local/state/opencode/service.json 复制 password".to_string(),
        404 => "这个地址不是 opencode v2 API（/api/model 返回 404）；请确认填的是设置 → 服务器里的地址，而不是带 /v1 的网关地址".to_string(),
        503 => "本机 opencode 服务尚未就绪（HTTP 503），请稍后重试".to_string(),
        _ => format!("本机 opencode 服务探活失败（HTTP {status}）"),
    }
}

/// 规范化一条账号：解析地址 / 密码 → 探活 → 落盘。
pub async fn normalize(
    _ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let (payload, source) = match &input.source {
        proto::NormalizeSource::Manual { payload } => (payload.clone(), "manual"),
        proto::NormalizeSource::FileText { content, .. } => {
            let trimmed = content.trim();
            let parsed = serde_json::from_str::<Value>(trimmed).unwrap_or(Value::Null);
            (
                if parsed.is_object() {
                    parsed
                } else {
                    serde_json::json!({ "baseUrl": trimmed })
                },
                "file",
            )
        }
        proto::NormalizeSource::Desktop => (Value::Object(Default::default()), "local"),
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err("本机服务连接不需要二进制文件".to_string());
        }
    };

    let server = from_secret(&payload)?;
    let client = control();
    let (status, body) =
        request_json(&client, &server, reqwest::Method::GET, "/api/model", None).await?;
    if !(200..300).contains(&status) {
        return Err(probe_error(status));
    }
    let count = super::models::store(&body);
    if count == 0 {
        return Err("本机 opencode 服务没有列出任何可用模型".to_string());
    }

    let name = input.name.clone().or_else(|| {
        let value = pick(&payload, &["name"]);
        if value.is_empty() { None } else { Some(value) }
    });
    // 自动发现时**不要把地址/端口写进凭证**：opencode 每次启动都换随机
    // 端口，落一份快照会让桌面端重启后这个账号永久连不上。留空即可，
    // 运行时 `from_secret` 每次重新读 service.json。手填地址则按用户意图
    // 固定下来。密码同理：留空时实时读取，避免密码轮换后要重加账号。
    let explicit_base = pick(&payload, &["baseUrl", "base_url", "url", "serverUrl"]);
    let stored_base = if explicit_base.is_empty() { String::new() } else { server.base.clone() };
    let display = name.unwrap_or_else(|| {
        if stored_base.is_empty() {
            "本机 OpenCode（自动发现）".to_string()
        } else {
            format!("本机 OpenCode · {}", server.base)
        }
    });
    let uid = short_fingerprint(&server.base);
    let secret = serde_json::json!({
        "baseUrl": stored_base,
        "username": pick(&payload, &["username", "user"]),
        "password": pick(&payload, &["password", "pass"]),
        "source": source,
    });
    Ok(proto::NormalizedAccount {
        secret,
        public: serde_json::json!({
            "name": display,
            "uid": uid,
            "plan": "本地服务",
            "source": if source == "local" { "自动发现" } else { "手填" },
            "longLived": true,
        }),
        identity: format!("{ID}:{uid}"),
        desktop: false,
    })
}

fn short_fingerprint(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let sum = Sha256::digest(text.as_bytes());
    sum[..5]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

/// 解析这次请求要用哪条连接：优先指定账号 → 队首账号 → 自动发现。
pub async fn resolve(ctx: &PluginCtx, account: &str) -> Result<Server, String> {
    if !account.trim().is_empty() {
        if let Ok(found) = ctx.store_get(ID, account).await {
            return from_secret(&found.secret);
        }
    }
    if let Ok(found) = ctx.store_get(ID, "").await {
        return from_secret(&found.secret);
    }
    from_secret(&Value::Object(Default::default()))
}

/// 公开视图（不泄露密码）。
pub fn account_view(secret: &Value) -> Result<proto::AccountView, String> {
    let server = from_secret(secret)?;
    let stored = pick(secret, &["name"]);
    Ok(serde_json::json!({
        "name": if stored.is_empty() { format!("本机 OpenCode · {}", server.base) } else { stored },
        "uid": short_fingerprint(&server.base),
        "plan": "本地服务",
        "source": if pick(secret, &["source"]) == "local" { "自动发现" } else { "手填" },
        "longLived": true,
    }))
}

pub fn identity_of(secret: &Value) -> String {
    let base = pick(secret, &["baseUrl", "base_url", "url", "serverUrl"]);
    let found = discover();
    format!("{ID}:{}", short_fingerprint(if base.is_empty() { &found.url } else { &base }))
}

#[cfg(test)]
mod tests {
    use super::{base64, basic_auth};
    use serde_json::json;

    #[test]
    fn basic_auth_matches_standard_base64() {
        assert_eq!(base64(b"opencode:secret"), "b3BlbmNvZGU6c2VjcmV0");
        assert_eq!(basic_auth("opencode", "secret"), "Basic b3BlbmNvZGU6c2VjcmV0");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
    }

    #[test]
    fn from_secret_prefers_explicit_url() {
        let parsed = super::from_secret(&json!({
            "baseUrl": "http://127.0.0.1:12345/",
            "username": "admin",
            "password": "pw"
        }));
        assert!(parsed.is_ok());
        if let Ok(server) = parsed {
            assert_eq!(server.base, "http://127.0.0.1:12345");
            assert_eq!(server.auth, Some("Basic YWRtaW46cHc=".to_string()));
        }
    }
}
