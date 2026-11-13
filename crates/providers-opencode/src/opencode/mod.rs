//! OpenCode 免费模型代理插件：把 openCode 里可以免费使用的模型暴露成标准
//! OpenAI 接口，供任意客户端调用。
//!
//! ── 代理对象与接入方式（依据：opencode2api）───────────────────
//! ```text
//!   Zen  https://opencode.ai/zen       POST /v1/chat/completions   v1 请求形状
//!   Go   https://opencode.ai/zen/go    POST /v1/chat/completions   v2 请求形状
//!   目录  https://models.opencode.ai/api.json（公开，筛免费模型）
//! ```
//! 上游是 OpenAI Chat 协议，所以本家是**无状态**转发：网关把客户端请求体
//! 做好角色归一后交给 `build_chat`，插件只回答「发到哪、带什么头、body 是
//! 什么」，流式 SSE 由网关翻译 / 透传。请求体原样透传，工具调用完整保留。
//!
//! ── 凭据：复用本机 opencode 登录态 ──────────────────────────
//! 优先读 `auth.json` 里的 `opencode` 项（见 `account.rs` 的探测路径），
//! 用户跑过一次 `opencode auth login` 就无需再填；读不到时才要求手工粘贴
//! API Key。
//!
//! ── 只放行免费模型 ──────────────────────────────────────────
//! `models()` 只返回免费集合；`build_chat` 对非免费模型直接报错，避免用户
//! 以为在消耗免费额度（判定规则见 `models.rs`）。
//!
//! ── 明确**没有**的东西 ──────────────────────────────────────
//!   - **没有续期**：Key 长期有效，`refresh = false`；
//!   - **没有签到 / 领取 / 余额**：上游没有对应动作或没有通用读数接口。
//!
//! ── 匿名车道（有额度限制，不保证随时可用）────────────────────
//! 默认就是匿名免费通道（`opencode.ai/inference`），凭据可以留空：此时请求
//! 带桌面版同款的字面量 `Authorization: Bearer public`（见 `identity.rs`），
//! 不是「不鉴权」。手工填 Key 时该 Key 优先。
//!
//! 2026-11-10 实测：该通道 `POST /v1/chat/completions` 对免费模型返回
//! `429 FreeUsageLimitError`（出口 IP 额度耗尽，Retry-After 约 14 小时），
//! 本机 opencode 桌面端日志里也有同样错误。因此本家**不能**作为稳定来源；
//! 需要随时可用请装同一个包里的 `opencode-local`（走本机 opencode v2 API）。
//! 本家保留给「有 Zen / Go Key」或「额度已恢复」的场景。

pub mod account;
pub mod endpoints;
pub mod identity;
pub mod models;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

/// provider id（落盘契约，不能改）。
pub const ID: &str = "opencode";

pub struct OpenCodeProvider;

impl Provider for OpenCodeProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "OpenCode")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 上游就是 OpenAI Chat：一次请求一次回答
                stateful: false,
                // Key 长期有效，上游没有 refresh 接口
                refresh: false,
                model_refresh: true,
                // 免费模型没有余额读数
                usage: false,
                checkin: false,
                claims: false,
                // 只有「匿名免费」/「粘贴 Key」两种入口，不开授权窗口
                web_login: false,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                // 免费目录是全局公开数据，刷新不需要账号
                refresh_uses_account: false,
                // 只认免费目录里的模型：注入别家的默认模型名会换来一次拒绝
                supports_default_model: false,
                // account.rs 确实实现了环境变量 / auth.json 旁路，申报要与实现一致
                env_credentials: true,
            })
            .with_login(vec![
                // 首选：匿名免费通道，不需要任何凭据
                proto::LoginMethod::Desktop {
                    button: "匿名免费使用（无需登录）".into(),
                    hint: Some(
                        "走 inference 匿名通道，额度按出口 IP 计算，耗尽时会返回 429 且换账号无效。\
                         想要稳定可用请在「模型/提供商」里改用本包附带的 OpenCode 本地服务（opencode-local）。\
                         有本机 auth.json 时会自动优先使用"
                            .into(),
                    ),
                },
                // 可选覆盖：自己的 Zen / Go Key
                proto::LoginMethod::Token {
                    label: "OpenCode API Key（可选）".into(),
                    placeholder: "opencode.ai/auth 创建的 Key".into(),
                    extra_fields: vec![],
                },
            ])
            // 目录从实际转发车道的 /v1/models 拉，见 `models.rs`
            .with_models(vec![])
            .with_account_features(proto::AccountFeatures {
                usage: false,
                // Key 本身没有到期时刻
                long_lived: true,
                identifier: vec!["uid".to_string()],
                ..Default::default()
            })
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 构造 `POST {base}/v1/chat/completions`。
    ///
    /// body **原样透传**（含 `stream`）：OpenCode 的 Zen / Go 通道接受标准
    /// OpenAI 请求体，模型名由网关的 `wire_model` 规则写进 body。这里只做两
    /// 件事：拒绝非免费模型，补鉴权与车道指纹头（见 `identity.rs`）。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            // 用网关解析出的「发送名」判定：客户端可能请求的是别名，
            // `model_rules` 映射后的 `wire_model` 才是上游认识的名字。
            let model = if req.wire_model.trim().is_empty() {
                req.body
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string()
            } else {
                req.wire_model.trim().to_string()
            };

            let found = ctx
                .store_get(ID, &req.account)
                .await
                .map_err(|error| format!("读取 OpenCode 账号失败：{error}"))?;
            let account = account::from_secret(&found.secret)?;

            // 官方基址按「与转发同一数据源」的免费清单判定；自定义基址按上游
            // 自己的 `/v1/models` 判定（聚合网关的 `kilo-auto/free` 等不在官方
            // 清单里）。账号级代理在这里必须透传，否则自定义网关走不到。
            if !model.is_empty() {
                let allowed = if endpoints::is_official(&account.base_url) {
                    models::is_free_model(&model)
                } else {
                    let client = crate::http::client(req.proxy.as_ref(), &ctx.env.proxy);
                    models::allowed_for_custom(
                        &client,
                        &account.api_key,
                        &account.base_url,
                        &model,
                    )
                    .await
                };
                if !allowed {
                    return Err(format!(
                        "OpenCode 插件只代理免费模型，`{model}` 不在可用清单中（刷新模型清单后重试）"
                    ));
                }
            }

            let ids = identity::request_ids(&account.key_id, &req.headers, &req.body);

            // body 原样透传；只在网关解析出「发送名」且 body 里还不是它时
            // 补一次 model 字段，确保请求落到目录里那个模型。
            let mut body = req.body;
            if !req.wire_model.trim().is_empty() {
                if let Value::Object(map) = &mut body {
                    map.insert(
                        "model".to_string(),
                        Value::String(req.wire_model.trim().to_string()),
                    );
                }
            }

            let headers =
                identity::inference_headers(&account.api_key, &account.base_url, &ids);
            // 匿名车道的基址自带 `/v1`，Zen / Go 的基址不带，由 `chat_url` 统一。
            Ok(ChatPlan::chat(
                endpoints::chat_url(&account.base_url),
                headers,
                body,
            ))
        })
    }

    /// 上游错误分类。
    ///
    ///   - `401` → `Fatal`：API Key 无续期能力，归成「凭证失效」只会让网关
    ///     走一次注定失败的刷新重试，最后仍然把 401 交给用户；
    ///   - `429 FreeUsageLimitError` → `Fatal`：匿名免费额度按**出口 IP**计，
    ///     换账号 / 换 Key 毫无意义，归成 `QuotaLimited` 只会触发一轮注定
    ///     失败的冷却与轮换；
    ///   - `403` / 其它 `429` → `QuotaLimited`：Key 无权访问该通道 / 套餐额度
    ///     用完 / 频率限制，换一个账号确实可能成功；
    ///   - 其余 → 原样透传。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let detail = error_message(body);
        let detail = if detail.is_empty() { "上游错误".to_string() } else { detail };
        let message = format!("OpenCode 返回 {status}: {detail}");
        let code = upstream_code(body);
        if is_free_usage_limit(body) {
            return proto::ErrorClass::Fatal {
                status,
                message: format!(
                    "{message}（匿名免费额度按出口 IP 计，已用尽；换账号无效，请等待额度重置或改用 opencode-local）"
                ),
                upstream_code: code,
            };
        }
        if status == 403 || status == 429 {
            return proto::ErrorClass::QuotaLimited {
                // 上游没有给结构化的恢复时刻（429 的 Retry-After 不在 classify
                // 的入参里），交给网关的限额标记给兜底冷却。
                reset_at: None,
                message,
                upstream_code: code,
                status,
            };
        }
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: code,
        }
    }

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async { Ok(models::snapshot()) })
    }

    /// 拉取公开的免费模型目录。
    ///
    /// 目录不依赖账号（`refresh_uses_account = false`），所以即使还没添加
    /// 账号，`/v1/models` 也能先把免费清单暴露给客户端。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        _account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let client = crate::http::client(None, &ctx.env.proxy);
            Ok(models::refresh(&client, force).await)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(account::normalize(ctx, input))
    }

    fn account_view(&self, secret: &Value) -> Result<proto::AccountView, String> {
        account::account_view(secret)
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(account::identity_of(secret))
    }
}

/// 从上游错误体里取一句可读文案。
///
/// OpenCode 的网关有两种形状：OpenAI 风格 `{error:{message,type,code}}`，
/// 以及 `{message}`。两种都认，缺失时返回空串（由调用方补「上游错误」）。
fn error_message(body: &Value) -> String {
    let error = body.get("error");
    let candidate = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .or_else(|| error.and_then(Value::as_str))
        .or_else(|| body.get("message").and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or("");
    candidate.to_string()
}

/// 是不是「匿名免费额度耗尽」。
///
/// 上游形状：`{"type":"error","error":{"type":"FreeUsageLimitError",
/// "message":"Free usage limit reached. …"}}`。按 type 优先、文案兜底，
/// 避免把普通的 429 频率限制也当成额度耗尽。
fn is_free_usage_limit(body: &Value) -> bool {
    let error = body.get("error");
    let kind = error
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        .or_else(|| body.get("type").and_then(Value::as_str))
        .unwrap_or("");
    if kind.eq_ignore_ascii_case("FreeUsageLimitError") {
        return true;
    }
    error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .or_else(|| body.get("message").and_then(Value::as_str))
        .is_some_and(|text| text.to_ascii_lowercase().contains("free usage limit"))
}

/// 上游业务码（数字或可转成数字的字符串）。
fn upstream_code(body: &Value) -> Option<i64> {
    let error = body.get("error");
    let value = error
        .and_then(|value| value.get("code"))
        .or_else(|| body.get("code"));
    let Some(value) = value else {
        return None;
    };
    if let Some(code) = value.as_i64() {
        return Some(code);
    }
    value
        .as_str()
        .and_then(|text| text.trim().parse::<i64>().ok())
}
