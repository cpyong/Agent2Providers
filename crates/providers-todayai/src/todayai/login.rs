//! Today AI 手机号验证码登录（**依据客户端实现**：接口从 CN 桌面端
//! `app.asar` 的 `phone-otp/enabled.ts` 读出并逐字核对）。
//!
//! ── 链路（两步，没有窗口、没有轮询）──────────────────────────
//! ```text
//! POST {账号}/api/auth/phone-number/send-otp  {phoneNumber:"+86…"}
//!   → 204 / 200（无正文）
//! POST {账号}/api/auth/phone-number/verify    {phoneNumber:"+86…", code:"123456"}
//!   → { token: <sessionToken>, user: { id, phoneNumber, email, name } }
//! GET  {账号}/api/auth/token   Authorization: Bearer <sessionToken>
//!   → { token: <jwt>, expiresIn }
//! ```
//!
//! 桌面端在 `verify` 之后立刻调 `establish()` 换 JWT，这里跟随同一顺序：
//! 登录成功的那一刻就落一份**可用**的访问令牌，而不是等第一次转发再换。
//!
//! ── 为什么这条链路不进 `login` 的通用形态 ────────────────────
//! 协议里的 `LoginStart` 只有「开窗口 / 轮询」两种形态，而本家是**两次
//! 同步调用**：发码 → 用户填码 → 登录。`flow` 只携带手机号（发码与验证
//! 之间没有 msgid 之类的中间态，Better Auth 把验证码记录存在服务端并绑定
//! 手机号），因此这条流程是**无状态**的：插件重启、网关换进程都不影响。
//!
//! ── 手机号格式 ──────────────────────────────────────────────
//! 上游只收中国大陆号，且网页端发送的是 `+86` + 11 位（`auth.cnPhone.*`
//! 文案里写明「Mainland China phone numbers only」）。这里按同一口径
//! 归一化，容忍用户填 `86` 前缀、空格、连字符与括号。

use serde_json::{json, Value};

use super::client;
use super::credentials;

/// 发码路径（`EnabledAccountPhoneOtpClient.requestCode`）
const SEND_CODE_PATH: &str = "/api/auth/phone-number/send-otp";
/// 验证码登录路径（`EnabledAccountPhoneOtpClient.verifyCode`）
const VERIFY_PATH: &str = "/api/auth/phone-number/verify";

/// 中国大陆手机号 → E.164（`+86` + 11 位）
pub fn normalize_phone(raw: &str) -> Result<String, String> {
    let trimmed: String = raw
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '-' && *ch != '(' && *ch != ')')
        .collect();
    let digits = trimmed.strip_prefix("+86").unwrap_or(&trimmed);
    let digits = digits
        .strip_prefix("86")
        .filter(|rest| rest.len() == 11)
        .unwrap_or(digits);
    if digits.len() == 11
        && digits.starts_with('1')
        && digits
            .chars()
            .nth(1)
            .is_some_and(|ch| ('3'..='9').contains(&ch))
        && digits.chars().all(|ch| ch.is_ascii_digit())
    {
        Ok(format!("+86{digits}"))
    } else {
        Err("请填写 11 位中国大陆手机号".to_string())
    }
}

/// 6 位数字验证码
fn normalize_code(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.len() == 6 && trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        Ok(trimmed.to_string())
    } else {
        Err("请填写 6 位数字验证码".to_string())
    }
}

/// 从上游用户对象里取一个字符串字段（`phoneNumber` / `phone` 两种写法都认）
fn user_field(user: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = user
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

/// 从 `verify` 响应里取会话令牌。
///
/// Better Auth 的手机号插件回 `{token, user}`；个别版本把会话包在
/// `session` 里（桌面端的邮箱登录分支就兼容了这一形态），因此两处都读。
fn extract_session_token(payload: &Value) -> String {
    let direct = payload
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !direct.is_empty() {
        return direct.to_string();
    }
    payload
        .pointer("/session/token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 发送短信验证码。
pub async fn send_code(proxy: &str, phone: &str) -> Result<Value, String> {
    let phone = normalize_phone(phone)?;
    let client = crate::http::client(None, proxy);
    let body = json!({ "phoneNumber": &phone });
    client::post_auth(&client, SEND_CODE_PATH, &body, "发送验证码").await?;
    // 不打完整手机号（它是个人信息）：只留「发给了尾号」的可核对痕迹
    agent2api_provider_sdk::log_info!(
        "[Today AI] 📱 验证码已发送（{}）",
        credentials::mask_phone(&phone)
    );
    Ok(json!({ "phone": phone }))
}

/// 用手机号 + 验证码登录，返回一份 `NormalizedAccount`
/// （`{secret, public, identity, desktop}`）。
pub async fn login_with_code(
    proxy: &str,
    phone: &str,
    code: &str,
    name: Option<&str>,
) -> Result<agent2api_provider_protocol::NormalizedAccount, String> {
    let phone = normalize_phone(phone)?;
    let code = normalize_code(code)?;
    let client = crate::http::client(None, proxy);
    let body = json!({ "phoneNumber": &phone, "code": &code });
    let payload = client::post_auth(&client, VERIFY_PATH, &body, "验证码登录").await?;

    let session_token = extract_session_token(&payload);
    if session_token.is_empty() {
        agent2api_provider_sdk::log_warn!("[Today AI] 验证码通过但上游未返回会话令牌");
        return Err("登录成功但上游没有返回会话令牌（token）".to_string());
    }
    let user = payload.get("user").cloned().unwrap_or(Value::Null);
    let user_id = user_field(&user, &["id", "userId"]);
    let email = user_field(&user, &["email"]);
    let user_name = user_field(&user, &["name", "displayName"]);
    let user_phone = {
        let value = user_field(&user, &["phoneNumber", "phone"]);
        if value.is_empty() {
            phone.clone()
        } else {
            value
        }
    };

    // 登录成功的那一刻就把 JWT 换好：第一次转发不必再多一次往返
    let issued = client::issue_access_token(&client, &session_token).await?;
    let now = crate::http::now_ms();
    let token_expires_at = now + issued.expires_in * 1000;

    agent2api_provider_sdk::log_info!(
        "[Today AI] ✅ 登录成功（{}，userid {}）",
        credentials::mask_phone(&user_phone),
        if user_id.is_empty() { "缺失" } else { user_id.as_str() }
    );

    let display = name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            if user_name.is_empty() {
                None
            } else {
                Some(user_name.clone())
            }
        })
        .unwrap_or_else(|| credentials::default_name(&user_phone, &email, &user_id));

    // 身份与尾号先算好，再把令牌**移进** secret（避免 use-after-move）
    let identity = credentials::identity_of(&json!({
        "userId": &user_id,
        "email": &email,
        "phone": &user_phone,
        "accessToken": &issued.token,
    }));
    let token_tail = credentials::token_tail(&issued.token);

    Ok(agent2api_provider_protocol::NormalizedAccount {
        secret: json!({
            "sessionToken": session_token,
            "accessToken": issued.token,
            "tokenExpiresAt": token_expires_at,
            "userId": user_id,
            "phone": user_phone,
            "email": email,
            "expiresAt": now + credentials::SESSION_TTL_MS,
            "addedAt": now,
        }),
        public: json!({
            "name": display,
            "uid": user_id.clone(),
            "phone": user_phone.clone(),
            "email": email.clone(),
            "tokenTail": token_tail,
        }),
        identity,
        desktop: false,
    })
}
