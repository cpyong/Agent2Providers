//! Today AI 凭证：**会话令牌 + 访问令牌**两层。
//!
//! ── 与 Loomy 最大的不同：本家能续期 ─────────────────────────
//! 手机验证码登录换来两样东西（依据桌面端 `phone-otp/enabled.ts` 与
//! `session-client`）：
//!
//! ```text
//! POST {账号}/api/auth/phone-number/verify {phoneNumber, code}
//!   → { token: <sessionToken>, user: {…} }        ← 会话令牌（Better Auth）
//! GET  {账号}/api/auth/token   Bearer <sessionToken>
//!   → { token: <jwt>, expiresIn }                 ← 访问令牌（模型网关认它）
//! ```
//!
//! 会话令牌可以**反复换发** JWT：所以 `refresh = true`，JWT 临期或遇到 401
//! 时插件自己换一枚，用户不必重登。会话令牌本身有寿命（Better Auth 默认
//! 7 天），到期只能重新短信登录 —— 这一列的口径是「什么时候该重登」。
//!
//! ── 插件形态下的凭证 ────────────────────────────────────────
//! `ctx.store_get` 拿到的是不透明 JSON（`secret`），形状由本模块定义：
//! ```json
//! {
//!   "sessionToken": "…", "accessToken": "…", "tokenExpiresAt": 1760000000000,
//!   "userId": "…", "phone": "+8613…", "email": "…",
//!   "expiresAt": 1760000000000, "addedAt": 1760000000000
//! }
//! ```

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, StoreAccount};
use serde_json::{json, Value};

/// 会话令牌的估算寿命（Better Auth 默认 7 天；上游不返回明确时刻）
pub const SESSION_TTL_MS: i64 = 7 * 24 * 3600 * 1000;

/// 访问令牌的提前量：临期 60 秒就换新的（换发是纯本地到账号接口的一次请求）
pub const TOKEN_EXPIRY_MARGIN_MS: i64 = 60 * 1000;

/// 「验证码已发出」的回报前缀（与 `ui/todayai/app.js` 的判据逐字一致）。
///
/// `account.normalize` 的通道只有「产出账号」与「报错」两种结果，而发码
/// 两步流程的第一步两者都不是 —— 见 [`normalize`] 里那段说明。
pub const OTP_SENT: &str = "OTP_SENT";

/// 一条 Today AI 登录态。
#[derive(Clone, Debug)]
pub struct Credentials {
    /// 会话令牌（换发 JWT 用；手工粘贴的凭证可能没有）
    pub session_token: String,
    /// 访问令牌（模型网关的 `Authorization: Bearer`）
    pub access_token: String,
    /// 访问令牌的过期时刻（毫秒；`0` = 未知，按可用处理）
    pub token_expires_at: i64,
    /// 会话令牌的估算过期时刻（毫秒）
    pub expires_at: i64,
}

impl Credentials {
    /// 访问令牌此刻是否可用（未知过期时刻时按「可用」处理 —— 真过期了
    /// 上游会给 401，网关据此走 `refresh_token`）。
    pub fn token_valid(&self) -> bool {
        if self.access_token.trim().is_empty() {
            return false;
        }
        if self.token_expires_at <= 0 {
            return true;
        }
        self.token_expires_at - TOKEN_EXPIRY_MARGIN_MS > crate::http::now_ms()
    }

    /// 会话令牌是否已过期 / 临期（只用于提示重登）
    pub fn session_expiring(&self) -> bool {
        self.expires_at > 0 && self.expires_at <= crate::http::now_ms()
    }

    /// 还能不能换发 JWT：有会话令牌，且会话本身没有过期。
    ///
    /// 手工粘贴的裸 JWT 没有会话令牌 —— 过期后只能重登，因此不参与
    /// 「后台自动续期」的排期（见 `mod.rs` 的 `token_expiring`）。
    pub fn can_refresh(&self) -> bool {
        !self.session_token.is_empty() && !self.session_expiring()
    }
}

/// 从一条取值链里挑第一个非空字符串（凭证可能被手工编辑过，宽容读取）
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

/// 从 `secret` 解析凭证（**唯一入口**）。
pub fn from_secret(account: &StoreAccount) -> Result<Credentials, String> {
    let secret = &account.secret;
    let session_token = pick(secret, &["sessionToken", "session", "session_token"]);
    let access_token = pick(secret, &["accessToken", "token", "access_token"]);
    if session_token.is_empty() && access_token.is_empty() {
        return Err(
            "Today AI 账号缺少凭证（sessionToken / accessToken），请重新登录或粘贴令牌"
                .to_string(),
        );
    }
    let token_expires_at = secret
        .get("tokenExpiresAt")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let explicit_expiry = secret.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    let added_at = secret.get("addedAt").and_then(Value::as_i64).unwrap_or(0);
    let expires_at = if explicit_expiry > 0 {
        explicit_expiry
    } else if added_at > 0 {
        added_at + SESSION_TTL_MS
    } else {
        0
    };
    Ok(Credentials {
        session_token,
        access_token,
        token_expires_at,
        expires_at,
    })
}

/// 取一条账号的凭证（`account` 空串 = 本家队首账号）
pub async fn snapshot(ctx: &PluginCtx, account: &str) -> Result<Credentials, String> {
    let found = ctx
        .store_get(super::ID, account)
        .await
        .map_err(|error| format!("读取 Today AI 账号失败：{error}"))?;
    from_secret(&found)
}

/// 拿一枚**当前可用**的访问令牌（临期主动换发）。
pub async fn access_token(ctx: &PluginCtx, account: &str) -> Result<String, String> {
    let found = ctx
        .store_get(super::ID, account)
        .await
        .map_err(|error| format!("读取 Today AI 账号失败：{error}"))?;
    let credentials = from_secret(&found)?;
    if credentials.token_valid() {
        return Ok(credentials.access_token);
    }
    reissue(ctx, &found, &credentials).await
}

/// 无条件换发一枚访问令牌（网关在 401 之后走这条）。
pub async fn refresh_token(ctx: &PluginCtx, account: &str) -> Result<String, String> {
    let found = ctx
        .store_get(super::ID, account)
        .await
        .map_err(|error| format!("读取 Today AI 账号失败：{error}"))?;
    let credentials = from_secret(&found)?;
    reissue(ctx, &found, &credentials).await
}

/// 用会话令牌换一枚新的 JWT，并**尽力**写回账号记录。
///
/// ── 写回失败为什么不报错 ────────────────────────────────────
/// 令牌已经拿到了，本次请求可以正常发出。写回只是让下一次少换一次；
/// 并发换发时「比较再写」可能落到 `false`（别的请求先写了），那不是错误。
async fn reissue(
    ctx: &PluginCtx,
    found: &StoreAccount,
    credentials: &Credentials,
) -> Result<String, String> {
    if credentials.session_token.is_empty() {
        return Err(
            "Today AI 账号里的访问令牌已过期，且没有登录会话可续期，请重新用手机号登录"
                .to_string(),
        );
    }
    let client = crate::http::client(None, &ctx.env.proxy);
    let issued = super::client::issue_access_token(&client, &credentials.session_token).await?;
    let expires_at = crate::http::now_ms() + issued.expires_in * 1000;
    let next = crate::store::with_fields(
        &found.secret,
        &[
            ("accessToken", Value::String(issued.token.clone())),
            ("tokenExpiresAt", Value::from(expires_at)),
        ],
    );
    match ctx.store_update(&found.id, &found.secret, &next).await {
        Ok(true) => {}
        Ok(false) => agent2api_provider_sdk::log_info!(
            "[Today AI] 新令牌未能写回（另一处已先更新），本次请求继续使用新令牌"
        ),
        Err(error) => agent2api_provider_sdk::log_warn!(
            "[Today AI] 新令牌写回失败（{error}），本次请求继续使用新令牌"
        ),
    }
    Ok(issued.token)
}

/// 手机号脱敏（保留前 3 后 4）。按 `char` 切，避免多字节字符被切成半个。
pub fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    if chars.len() < 7 {
        return phone.to_string();
    }
    let head: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

/// 令牌尾 4 字符（公开形态的展示字段）
pub fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

/// 截断到 `max` 个**字符**（不是字节）
fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    value.chars().take(max).collect()
}

/// 默认展示名：脱敏手机号 → 邮箱 → userId → 通用名
pub fn default_name(phone: &str, email: &str, user_id: &str) -> String {
    if phone.chars().count() >= 7 {
        format!("Today AI {}", mask_phone(phone))
    } else if !email.is_empty() {
        format!("Today AI {email}")
    } else if !user_id.is_empty() {
        format!("Today AI {user_id}")
    } else {
        "Today AI 账号".to_string()
    }
}

/// 规范化一条账号（手填 / 文件导入）。
///
/// ── 身份从哪来（去重靠它）──────────────────────────────────
/// 优先 `userId`（稳定），其次邮箱，最后手机号；三者都没有时退化成令牌尾号
/// （只粘贴了令牌的场景）。尾号在换发 JWT 后会变，所以那种账号的去重能力
/// 弱一些 —— 但先让用户能把账号存进来，比在添加阶段直接拒绝更有用。
///
/// ── 为什么 normalize 阶段不探活 ──────────────────────────
/// 唯一能验证凭证的方式是换一次令牌 —— 那会把「添加账号」变成一次必然
/// 出网的调用，而手填令牌的场景常常就是想「先存着」。可用性由第一次转发
/// 或第一次续期如实报出。
pub async fn normalize(
    _ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    // ── 手机验证码登录的两步（插件页 `ui/todayai` 的唯一入口）──────
    //
    // ── 为什么要挤进 normalize ──────────────────────────────────
    // 本家的账号只能靠手机号 + 短信验证码拿到，而网关 3.3.0 的
    // 「添加账号」弹窗只认它自己内置的那十几家（`BUILTIN_CONFIGS`）：
    // 第三方插件家一律显示「账号添加功能即将上线」。于是添加表单由本插件
    // 自带的页面渲染（`ui/todayai/index.html`）。
    //
    // 而插件页面能触达插件进程的通道只有一条 —— `POST /api/accounts`
    // （网关 `add_account` → `account.normalize`）。`login.start` 走不通：
    // 网关固定用 `method="browser"` + 空 payload 调它（见 `rpc.rs`），
    // 拿不到手机号；`admin.action` 那条 RPC 也没有任何 HTTP 路由暴露给页面。
    //
    // ── 两步怎么表达 ────────────────────────────────────────────
    //   action = "sms-send"   { phone }          发码
    //   action = "sms-login"  { phone, code }    验证码换账号
    //
    // 「发码」那一步没有账号可返回，而 normalize 的产出被网关当作
    // 「一条可落库的账号」。用一个带 `OTP_SENT` 前缀的错误回报 —— 页面
    // 把它当成功（见 app.js 的 `isOtpSent`），网关则什么都不落库。这是
    // 协议里唯一不需要额外进程 / 端口就能跑通的口径。
    if let proto::NormalizeSource::Manual { payload } = &input.source {
        if let Some(action) = payload.get("action").and_then(Value::as_str) {
            let proxy = input
                .proxy
                .as_ref()
                .map(proto::WireProxy::to_url)
                .unwrap_or_default();
            let phone = payload.get("phone").and_then(Value::as_str).unwrap_or("");
            match action {
                "sms-send" => {
                    let sent = super::login::send_code(&proxy, phone).await?;
                    let normalized = sent.get("phone").and_then(Value::as_str).unwrap_or("");
                    return Err(format!(
                        "{OTP_SENT} 验证码已发送至 {}，请查收短信后填写验证码",
                        mask_phone(normalized)
                    ));
                }
                "sms-login" => {
                    let code = payload.get("code").and_then(Value::as_str).unwrap_or("");
                    let name = payload
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    return super::login::login_with_code(&proxy, phone, code, name).await;
                }
                _ => {}
            }
        }
    }
    let (session_token, access_token, user_id, phone, email, payload_name) = match &input.source {
        proto::NormalizeSource::Manual { payload } => (
            pick(payload, &["sessionToken", "session", "session_token"]),
            pick(payload, &["accessToken", "token", "access_token"]),
            pick(payload, &["userId", "userid", "user_id", "uid"]),
            pick(payload, &["phone", "phoneNumber"]),
            pick(payload, &["email"]),
            payload
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        ),
        proto::NormalizeSource::FileText { content, .. } => {
            let text = content.trim();
            // 文本文件既可能是裸 JWT，也可能是一份 JSON
            match serde_json::from_str::<Value>(text) {
                Ok(parsed) if parsed.is_object() => (
                    pick(&parsed, &["sessionToken", "session", "session_token"]),
                    pick(&parsed, &["accessToken", "token", "access_token"]),
                    pick(&parsed, &["userId", "userid", "user_id", "uid"]),
                    pick(&parsed, &["phone", "phoneNumber"]),
                    pick(&parsed, &["email"]),
                    None,
                ),
                _ => (String::new(), text.to_string(), String::new(), String::new(), String::new(), None),
            }
        }
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err("Today AI 不支持二进制文件导入，请粘贴令牌".to_string())
        }
        proto::NormalizeSource::Desktop => {
            return Err(
                "Today AI 不支持导入桌面端登录态（登录态不在可读文件里），请用手机号登录或粘贴令牌"
                    .to_string(),
            )
        }
    };
    if session_token.is_empty() && access_token.is_empty() {
        return Err("缺少令牌（sessionToken / accessToken）".to_string());
    }
    if access_token.chars().count() > 8192 || session_token.chars().count() > 8192 {
        return Err("令牌过长（超过 8192 字符，多半是粘错了内容）".to_string());
    }
    let user_id = truncate_chars(&user_id, 256);
    let phone = truncate_chars(&phone, 256);
    let email = truncate_chars(&email, 256);
    let now = crate::http::now_ms();
    // 只给了令牌、没有任何身份字段时不拦：身份退化成令牌尾号（见 `identity_of`）。
    // 这样的账号在界面上没有 uid / 手机号可显示，但转发照常可用。
    let identity = identity_of(&json!({
        "userId": &user_id,
        "phone": &phone,
        "email": &email,
        "accessToken": &access_token,
        "sessionToken": &session_token,
    }));
    let display = input
        .name
        .clone()
        .or(payload_name)
        .unwrap_or_else(|| default_name(&phone, &email, &user_id));
    // 尾号先算好，再把两个令牌**移进** secret（避免 use-after-move）
    let tail = token_tail(if access_token.is_empty() {
        &session_token
    } else {
        &access_token
    });
    Ok(proto::NormalizedAccount {
        secret: json!({
            "sessionToken": session_token,
            "accessToken": access_token,
            // 手填的访问令牌不知道寿命：0 = 按可用处理，真过期由 401 驱动续期
            "tokenExpiresAt": 0,
            "userId": user_id,
            "phone": phone,
            "email": email,
            "expiresAt": now + SESSION_TTL_MS,
            "addedAt": now,
        }),
        public: json!({
            "name": display,
            "uid": user_id.clone(),
            "phone": phone.clone(),
            "email": email.clone(),
            "tokenTail": tail,
        }),
        identity,
        desktop: false,
    })
}

/// 账号的稳定身份（去重用）。
pub fn identity_of(secret: &Value) -> String {
    let user_id = pick(secret, &["userId", "userid", "user_id", "uid"]);
    if !user_id.is_empty() {
        return format!("todayai-{user_id}");
    }
    let email = pick(secret, &["email"]);
    if !email.is_empty() {
        return format!("todayai-{email}");
    }
    let phone = pick(secret, &["phone", "phoneNumber"]);
    if !phone.is_empty() {
        return format!("todayai-{phone}");
    }
    let token = pick(secret, &["accessToken", "token", "sessionToken", "session"]);
    format!("todayai-token-{}", token_fingerprint(&token))
}

/// 令牌的稳定指纹（**只粘贴了令牌**时的去重身份）。
///
/// ── 为什么不能用 `token_tail` ────────────────────────────────
/// 尾 4 字符只有 4 个字符的区分度，而「粘贴令牌」恰好是唯一一条**没有**
/// uid / 手机号 / 邮箱的入口：两枚不同的令牌尾号相同时，网关会把第二个
/// 账号当成第一个的更新，用户看到的是「加了新账号，旧的凭空变了」。
/// 身份一旦相撞就会静默覆盖凭证 —— 这类故障比多出一条重复记录难查得多。
///
/// ── 为什么是 FNV-1a 而不是 sha256 ────────────────────────────
/// 这里只要「同一串恒定、不同串几乎不相撞、不进 URL 太长的短串」。为这一处
/// 引入 `sha2` 会让插件包的依赖表多一项（而它已经间接带了 rustls 的哈希栈，
/// 但那是传递依赖，不该被当成自己的契约）。FNV-1a 64 位在这条用途上足够：
/// 碰撞需要构造，而且撞了也只是两个手工粘贴的账号合并，不涉及安全边界。
fn token_fingerprint(token: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in token.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 账号的公开视图（凭证更新后刷新界面用）。
pub fn account_view(secret: &Value) -> Result<Value, String> {
    let access_token = pick(secret, &["accessToken", "token", "access_token"]);
    let session_token = pick(secret, &["sessionToken", "session", "session_token"]);
    if access_token.is_empty() && session_token.is_empty() {
        return Err("Today AI 凭证里没有令牌".to_string());
    }
    let user_id = pick(secret, &["userId", "userid", "user_id", "uid"]);
    let phone = pick(secret, &["phone", "phoneNumber"]);
    let email = pick(secret, &["email"]);
    Ok(json!({
        "name": default_name(&phone, &email, &user_id),
        "uid": user_id,
        "phone": phone,
        "email": email,
        "tokenTail": token_tail(if access_token.is_empty() { &session_token } else { &access_token }),
        "expiresAt": secret.get("expiresAt").cloned().unwrap_or(Value::Null),
    }))
}
