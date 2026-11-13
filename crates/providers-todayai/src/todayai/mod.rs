//! Today AI（todayai.cn）插件实现：账号管理（**手机号 + 短信验证码登录**）加推理转发。
//!
//! ── 上游长什么样（依据：CN 桌面端 `app.asar` v1.23.0 + 实测）───────
//! 三个平面（细节见各子模块的模块头）：
//!
//! ```text
//!   账号（Better Auth） https://auth.todayai.cn/api/auth
//!                        /phone-number/send-otp   发短信验证码
//!                        /phone-number/verify     验证码 → 会话令牌
//!                        /token                   会话令牌 → JWT
//!   助理后端             https://api.todayai.cn/v1
//!                        /llm/models              模型目录（35 个 id）
//!                        /users/me/settings       账号级 preferredModel
//!                        /v3/conversations        + /v3/messages 助理会话
//! ```
//!
//! 本家**有状态**：`api.todayai.cn` 没有 OpenAI / Anthropic 兼容的推理
//! 接口（`/v1/chat/completions` 等一律 404），真正跑推理的是助理会话
//! （v3 长轮询）。转发链由 `chat.rs` 驱动：选模型 → 建会话 → 发消息 →
//! 长轮询 → 翻译成 chat SSE。**每次请求一个新会话**，不碰用户主会话。
//!
//! ── 与 Loomy 的关键差别：**能续期** ────────────────────────
//! 手机验证码登录换来的是 Better Auth 的**会话令牌**，它可以反复去
//! `/api/auth/token` 换新的 JWT。所以 `refresh = true`：JWT 临期或上游回
//! 401 时插件自己换一枚，用户不必重登（Loomy 的 session 是终态，只能重登）。
//!
//! ── 明确**没有**的东西（不申报就不显示入口）─────────────────
//!   - **没有签到**：`checkin` 恒 false，也没有任何「每日首次使用」之类的
//!     等价动作 —— 不申报 `CheckinInfo`，签到中心不会出现这家；
//!   - **没有余额接口**：`usage` 为 false（桌面端有 `/v1/users/me`，但那是
//!     账号资料，不是余额读数，不据此虚构一张积分卡）；
//!   - **没有领取类活动**：`claims` 为 false；
//!   - **没有网页授权登录**：`web_login` 为 false，官方客户端只做手机号验证码；
//!   - **没有桌面端导入**：登录态在客户端自己的存储里，没有稳定可读的文件。
//!
//! ── 模型清单 ────────────────────────────────────────────────
//! 只认 `/v1/llm/models`；拉不到就如实报失败，**不编模型名** —— 见
//! `models.rs` 的模块头。`default_models` 为空表，与 Loomy 同一纪律。

pub mod chat;
pub mod client;
pub mod credentials;
pub mod endpoints;
pub mod login;
pub mod models;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

/// provider id（落盘契约，不能改）。
pub const ID: &str = "todayai";

pub struct TodayAiProvider;

impl Provider for TodayAiProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "Today AI")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // **有状态**：上游没有 OpenAI 兼容的推理接口，只有助理会话
                // （v3）。转发链由 `chat.rs` 驱动：选模型 → 建会话 → 发消息
                // → 长轮询 → 翻译成 chat SSE。
                stateful: true,
                // 会话令牌可反复换发 JWT（见 `credentials.rs`）
                refresh: true,
                model_refresh: true,
                // 没有可验证的余额 / 积分读数，不申报
                usage: false,
                // **没有签到**（也刻意留空 `checkin` 申报）
                checkin: false,
                // 没有新手礼 / 每日任务这类领取动作
                claims: false,
                // 只有手机验证码登录，不开授权窗口
                web_login: false,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                // 不认「默认模型」这个概念：注入别家的模型名只会换来一次 404
                supports_default_model: false,
                // 没有环境变量 / 默认登录态旁路
                env_credentials: false,
            })
            .with_login(vec![proto::LoginMethod::Sms {
                hint: Some(
                    "Fill in a mainland-China phone number, then the 6-digit SMS code".into(),
                ),
            }])
            // 本家没有内置兜底清单（见 `models.rs` 的模块头）
            .with_models(vec![])
            // 账号行：
            //   `usage: false` —— 上游没有公开的余额读数接口；
            //   `identifier: uid` —— 公开视图里的上游用户 id；
            //   `expiry: expiresAt` —— 会话令牌的估算寿命（Better Auth 默认
            //     7 天）。JWT 会由插件自动续期，所以这一列是「什么时候该重登」。
            //   **不申报 `long_lived`**：会话令牌有固定寿命。
            .with_account_features(proto::AccountFeatures {
                usage: false,
                identifier: vec!["uid".to_string()],
                expiry: vec!["expiresAt".to_string()],
                long_lived: false,
                ..Default::default()
            })
        // 不在 `with_checkin` 里申报任何东西：本家**没有签到**
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 会话式转发：转调 `chat::run`。
    ///
    /// ── 为什么不是 `build_chat` ────────────────────────────────
    /// 上游没有「一次请求 = 一次补全」的接口（`/v1/chat/completions` 是 404，
    /// 见 `chat.rs` 的模块头）。这里刻意**不实现** `build_chat`：万一网关的
    /// `stateful` 分流坏了，默认实现会直接报错，而不是往一个不存在的路径上
    /// 发请求。
    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: agent2api_provider_sdk::ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(chat::run(ctx, req, sink))
    }

    /// 上游错误分类：
    ///   - HTTP 401 / 403（网关的 `Missing or invalid Authorization header` /
    ///     `Token introspection failed` 都是 401）→ `TokenExpired`，编排层会
    ///     用会话令牌换一枚新的 JWT 再重试一次；
    ///   - HTTP 429 → `QuotaLimited`（上游没有给结构化恢复时间）；
    ///   - 命中内容策略文案 → `ContentBlocked`（不罚账号）；
    ///   - 其余 → 原样透传。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let raw = error_message(body);
        let raw = if raw.is_empty() { "上游错误" } else { raw.as_str() };
        let message = format!("上游返回 {status}: {raw}");
        if status == 401 || status == 403 {
            return proto::ErrorClass::TokenExpired { message };
        }
        let code = body.get("code").and_then(Value::as_i64);
        if status == 429 {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status,
            };
        }
        if status >= 400 && is_content_block(raw) {
            return proto::ErrorClass::ContentBlocked {
                status,
                message,
                upstream_code: code,
            };
        }
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: code,
        }
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取一枚**当前可用**的访问令牌：临期（60 秒内）就先用会话令牌换一枚。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move { credentials::access_token(ctx, account).await })
    }

    /// 401 之后的**强制**换发：不看临期窗口，直接用会话令牌换一枚。
    ///
    /// 没有会话令牌（手工粘贴的裸 JWT）时如实报错 —— 回落到「再读一次账号」
    /// 只会把同一个被拒的令牌再交一遍。
    fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move { credentials::refresh_token(ctx, account).await })
    }

    /// 访问令牌临期、且**还有会话令牌可换发**时才算「需要续期」。
    ///
    /// 会话令牌本身过期后刷不动，这里返回 false（与 Raccoon 同口径：
    /// 判不出来 / 救不回来就不要让维护任务白跑）。
    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            let Ok(credentials) = credentials::snapshot(ctx, account).await else {
                return Ok(false);
            };
            Ok(credentials.can_refresh() && !credentials.token_valid())
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async { Ok(models::catalog()) })
    }

    /// 刷模型目录：用指定账号（空串 = 库里第一个可用账号）的访问令牌。
    ///
    /// 「一个账号都没有」不是失败 —— 那是「用户还没添加账号」的正常状态，
    /// 返回 `unchanged()`。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let found = match ctx.store_get(ID, account).await {
                Ok(found) => found,
                Err(_) if account.is_empty() => {
                    agent2api_provider_sdk::log_info!(
                        "[Today AI] 模型目录刷新跳过：尚未添加账号"
                    );
                    return Ok(proto::RefreshOutcome::unchanged());
                }
                Err(error) => {
                    return Ok(proto::RefreshOutcome::failed(format!(
                        "读取 Today AI 账号失败：{error}"
                    )))
                }
            };
            let credentials = match credentials::from_secret(&found) {
                Ok(credentials) => credentials,
                Err(reason) => return Ok(proto::RefreshOutcome::failed(reason)),
            };
            // 目录请求也走同一条换发链：临期令牌先换，避免一次必然的 401
            let token = if credentials.token_valid() {
                credentials.access_token
            } else {
                match credentials::refresh_token(ctx, account).await {
                    Ok(token) => token,
                    Err(reason) => return Ok(proto::RefreshOutcome::failed(reason)),
                }
            };
            let client = crate::http::client(None, &ctx.env.proxy);
            Ok(models::refresh(&client, &token, force).await)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(credentials::normalize(ctx, input))
    }

    fn account_view(&self, secret: &Value) -> Result<proto::AccountView, String> {
        credentials::account_view(secret)
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(credentials::identity_of(secret))
    }

    // ── 登录（手机号验证码，两步）───────────────────────────────

    /// `method = "sms"`，`payload = {phone}`：发码，把手机号编成 `flow`。
    ///
    /// `url` 为空 + `poll_interval_ms = 0`：这是「用户还要填一个码」的流程，
    /// 网关不该开窗口、也不该轮询，等用户提交后走 `login.callback`。
    ///
    /// 与 Loomy 不同，这里**没有 msgid**：Better Auth 把验证码记录存在服务端
    /// 并绑定手机号，因此这条流程是无状态的（插件重启也不影响）。
    fn login_start<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            if method != "sms" {
                return Err(format!("Today AI 不支持「{method}」登录方式"));
            }
            let phone = payload.get("phone").and_then(Value::as_str).unwrap_or("");
            let normalized = login::normalize_phone(phone)?;
            login::send_code(&ctx.env.proxy, &normalized).await?;
            Ok(proto::LoginStart {
                flow: normalized,
                url: String::new(),
                instructions: "Fill in the 6-digit code sent to your phone".to_string(),
                poll_interval_ms: 0,
                external_browser: false,
                allowed_hosts: Vec::new(),
            })
        })
    }

    /// 用户提交验证码：`flow` 是 `login.start` 给的那串（手机号），
    /// `code` 是短信码。成功后直接返回一条可落库的账号（含已换好的 JWT）。
    fn login_callback<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            let phone = callback.flow.trim();
            if phone.is_empty() {
                return Ok(proto::LoginPoll::Failed {
                    message: "登录会话已失效：请重新获取验证码".to_string(),
                });
            }
            // 网关给的 `code` 优先；有些客户端把整条回调串塞进来，取最后一个
            // `code=` 参数兜底。
            let mut code = callback.code.trim().to_string();
            if code.is_empty() {
                code = query_param(&callback.url, "code");
            }
            match login::login_with_code(&ctx.env.proxy, phone, &code, None).await {
                Ok(account) => Ok(proto::LoginPoll::Done { account }),
                Err(message) => Ok(proto::LoginPoll::Failed { message }),
            }
        })
    }

    /// 管理动作：短信登录的两步（**同步形态**，面板直接调）。
    ///
    /// ```text
    ///   login.sms-send    {phone}          → {phone}
    ///   login.sms-verify  {phone, code}    → NormalizedAccount（网关落库）
    /// ```
    ///
    /// 两条路共用 `login::send_code` / `login::login_with_code`，与
    /// `login.start` / `login.callback` 不存在「两套实现」。
    fn admin_action<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        _account: &'a str,
        action: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            let phone = payload.get("phone").and_then(Value::as_str).unwrap_or("");
            let code = payload.get("code").and_then(Value::as_str).unwrap_or("");
            match action {
                "login.sms-send" | "login.send-code" => {
                    login::send_code(&ctx.env.proxy, phone).await
                }
                "login.sms-verify" | "login.verify-code" => {
                    let account =
                        login::login_with_code(&ctx.env.proxy, phone, code, None).await?;
                    serde_json::to_value(account)
                        .map_err(|error| format!("登录结果无法序列化: {error}"))
                }
                other => Err(format!(
                    "Today AI 不认识管理动作「{other}」（可用：login.sms-send / login.sms-verify）"
                )),
            }
        })
    }
}

/// 从 URL 查询串里取一个参数（没有则空串）。
fn query_param(url: &str, key: &str) -> String {
    let Some(query) = url.split_once('?').map(|(_, rest)| rest) else {
        return String::new();
    };
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name == key {
            return value.trim().to_string();
        }
    }
    String::new()
}

/// 上游错误文案：兼容 `{message}` 与 OpenAI 的 `{error:{message}}` 两种形状。
fn error_message(body: &Value) -> String {
    for pointer in ["/message", "/error/message", "/msg", "/detail"] {
        let text = body
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !text.is_empty() {
            return text.to_string();
        }
    }
    String::new()
}

/// 内容策略拦截的文案特征（OpenAI 生态的标准错误码；匹配前转小写）。
///
/// 这类拦截**不罚账号**：换中性提示词重试可能就过了，冷却账号没有意义。
const CONTENT_BLOCK_PATTERNS: &[&str] = &[
    "content_policy_violation",
    "content policy",
    "content_filter",
    "content management policy",
];

fn is_content_block(text: &str) -> bool {
    let lowered = text.to_lowercase();
    CONTENT_BLOCK_PATTERNS
        .iter()
        .any(|pattern| lowered.contains(pattern))
}
