//! Today AI 两种出站请求的薄封装：账号接口（Better Auth）与模型网关。
//!
//! ── 为什么收在一处 ──────────────────────────────────────────
//! 三处消费者（login / credentials / models）共用同一套
//! 「客户端标识头 + 错误翻译 + 令牌换发」口径；各写一份会让
//! 「哪个码算验证码错、哪个码算会话失效」这类判据分叉。
//!
//! ── 错误口径（依据桌面端 `AccountErrorMapper` 与实测）──────────
//! Better Auth 用 HTTP 状态码 + `{message, code}` 表达结果：
//!   - `400` + `INVALID_OTP` / `OTP_EXPIRED`：验证码错 / 过期；
//!   - `403` + `TOO_MANY_ATTEMPTS`：尝试次数过多；
//!   - `400` + `VALIDATION_ERROR`：字段格式不对（手机号不合法走这条）；
//!   - `401` + `UNAUTHORIZED`：会话令牌无效 / 过期；
//!   - `5xx`（正文可能是空的）：上游验证码投递或自身故障。
//!
//! 最后一条是实测结论：从中国大陆以外发起时，`send-otp` 对**任意**手机号
//! （含合法号与非法号）都回 `500` 且正文为空 —— 而 `get-session` /
//! `/token` 同一时刻正常。所以 500 不能翻译成「手机号不对」，只能如实说
//! 「上游没发出去」，让用户换网络环境重试。

use serde_json::Value;

use super::endpoints;
use crate::http;

/// 换发访问令牌的兜底寿命（秒；与桌面端 `BETTER_AUTH_JWT_FALLBACK_TTL_SECONDS` 同值）
pub const FALLBACK_TOKEN_TTL_SECONDS: i64 = 540;

/// 一次令牌换发的结果
pub struct IssuedToken {
    /// JWT（模型网关的 `Authorization: Bearer`）
    pub token: String,
    /// 上游声明的寿命（秒）
    pub expires_in: i64,
}

/// 拼账号接口的完整地址
fn auth_url(path: &str) -> String {
    format!("{}{path}", endpoints::auth_base_url())
}

/// 账号接口的公共请求头（**不含** `Content-Type`：`post_json` 自己会加）
///
/// 与桌面端 `AccountHeaders.build({includeOrigin: true})` 逐项一致。
pub fn auth_headers() -> Vec<(String, String)> {
    vec![
        ("Origin".to_string(), endpoints::web_origin()),
        ("X-App-Version".to_string(), endpoints::app_version()),
        (
            "X-Client-Platform".to_string(),
            endpoints::auth_client_platform().to_string(),
        ),
    ]
}

/// 模型网关的公共请求头（`Authorization` 由调用方补）
///
/// 桌面端对 `target: 'api'` 的请求只加版本与平台两个头，**不加 `Origin`**。
pub fn api_headers() -> Vec<(String, String)> {
    vec![
        ("X-App-Version".to_string(), endpoints::app_version()),
        (
            "X-Client-Platform".to_string(),
            endpoints::api_client_platform().to_string(),
        ),
    ]
}

/// 上游错误码（字符串形态；缺失时为空串）
pub fn error_code(payload: &Value) -> String {
    payload
        .get("code")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 上游文案（`message`）
pub fn upstream_message(payload: &Value) -> String {
    payload
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 把一次失败响应翻译成人话。
///
/// `what` 是这一步在做什么（「发送验证码」/「验证码登录」…），拼在句首。
pub fn describe_error(status: u16, payload: &Value, what: &str) -> String {
    let code = error_code(payload);
    let message = upstream_message(payload);
    let known = match code.as_str() {
        "INVALID_OTP" => Some("验证码不正确".to_string()),
        "OTP_EXPIRED" => Some("验证码已过期，请重新获取".to_string()),
        "TOO_MANY_ATTEMPTS" => Some("尝试次数过多，请稍后再试".to_string()),
        "VALIDATION_ERROR" => Some(if message.is_empty() {
            "手机号格式不正确".to_string()
        } else {
            format!("手机号格式不正确（上游原文：{message}）")
        }),
        "UNAUTHORIZED" | "INVALID_SESSION_TOKEN" => {
            Some("登录会话已失效，请重新用手机号登录".to_string())
        }
        "REGISTRATION_RESTRICTED" | "SIGN_UP_DISABLED" => {
            Some("上游未开放新账号注册".to_string())
        }
        _ => None,
    };
    if let Some(text) = known {
        return format!("{what}失败：{text}");
    }
    if status >= 500 {
        return format!(
            "{what}失败：上游没有正常响应（HTTP {status}）。\
             若身处中国大陆以外，验证码投递可能被上游拒绝，请换网络环境后重试"
        );
    }
    let detail = if message.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}：{message}")
    };
    format!("{what}失败：{detail}")
}

/// 向账号接口发一次 POST
pub async fn post_auth(
    client: &reqwest::Client,
    path: &str,
    body: &Value,
    what: &str,
) -> Result<Value, String> {
    let (status, payload) = http::post_json(client, &auth_url(path), &auth_headers(), body).await?;
    if !(200..300).contains(&status) {
        return Err(describe_error(status, &payload, what));
    }
    Ok(payload)
}

/// 用**会话令牌**换一枚访问令牌（JWT）。
///
/// ```text
/// GET {账号基址}/api/auth/token
/// Authorization: Bearer <sessionToken>
///   → { "token": "<jwt>", "expiresIn": 3600 }
/// ```
///
/// 这是桌面端 `AccountSessionTokenClient.issue()` 的同款调用。会话令牌来自
/// 手机验证码登录，本身可以反复换发 JWT —— 所以本家**有续期能力**
/// （与 Loomy 那种「14 天到期只能重登」不同）。
pub async fn issue_access_token(
    client: &reqwest::Client,
    session_token: &str,
) -> Result<IssuedToken, String> {
    let session_token = session_token.trim();
    if session_token.is_empty() {
        return Err(
            "Today AI 账号缺少登录会话（sessionToken），无法续期，请重新用手机号登录".to_string(),
        );
    }
    let mut headers = auth_headers();
    headers.push((
        "Authorization".to_string(),
        format!("Bearer {session_token}"),
    ));
    let (status, payload) =
        http::get_json(client, &auth_url("/api/auth/token"), &headers).await?;
    if !(200..300).contains(&status) {
        return Err(describe_error(status, &payload, "换取访问令牌"));
    }
    let token = payload
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        return Err("换取访问令牌失败：上游没有返回 token".to_string());
    }
    let declared = payload
        .get("expiresIn")
        .or_else(|| payload.get("expires_in"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let expires_in = if declared > 0 {
        declared
    } else {
        FALLBACK_TOKEN_TTL_SECONDS
    };
    Ok(IssuedToken { token, expires_in })
}
