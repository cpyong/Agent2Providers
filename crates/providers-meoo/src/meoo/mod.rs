//! Meoo 秒悟 provider。
//!
//! ── 三个平面的取舍 ──────────────────────────────────────────
//! - 数据面：**无状态** `build_chat`。AI 网关是 OpenAI Chat Completions
//!   兼容层，URL / 头 / 体交给网关自己发，插件不做 SSE 中转（少一层、少一处
//!   掉流式的风险）。
//! - 控制面：`/open/v1/cli-compat/…` 拿身份、积分、项目服务 AK；模型目录
//!   与聊天都用 AK 走 `api.meoo.host`。
//! - 账号面：浏览器 OAuth PKCE（官方 CLI 同款），也支持手填项目服务 AK。
//!
//! 「签到」的实现与证据纪律见 `checkin.rs` —— 上游没有 claim 端点，不假装
//! 有一个。

pub mod browser_login;
pub mod checkin;
pub mod client;
pub mod credentials;
pub mod endpoints;
pub mod models;
pub mod oauth;
pub mod opener;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::{json, Value};

/// provider id（落盘契约，不能改）。
pub const ID: &str = "meoo";

pub struct MeooProvider;

impl Provider for MeooProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "Meoo 秒悟")
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: false,
                // 凭证续期在 credentials::load 里按需自动完成，不走网关后台排期
                refresh: false,
                model_refresh: true,
                usage: true,
                checkin: true,
                claims: false,
                web_login: true,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                supports_default_model: false,
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Browser {
                    button: "网页登录（浏览器授权）".to_string(),
                    hint: Some(
                        "打开 Meoo 官方授权页，登录并点一次「授权」。使用官方 OAuth PKCE，\
                         不读取浏览器 Cookie；授权完成后自动收下账号与项目服务 AK。"
                            .to_string(),
                    ),
                },
                proto::LoginMethod::Token {
                    label: "项目服务 AK（手动）".to_string(),
                    placeholder: "粘贴 meoo_project_service_v1.…，或 Meoo CLI 的凭证 JSON".to_string(),
                    extra_fields: Vec::new(),
                },
            ])
            .with_models(models::snapshot())
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: false,
                credit_expiry: false,
                identifier: vec!["userId".to_string()],
                expiry: Vec::new(),
                long_lived: false,
                ..Default::default()
            })
            .with_checkin(proto::CheckinInfo {
                description: Some(
                    "Meoo 没有独立签到接口：每日首次登录由服务端赠 2000 积分。\
                     本插件做一次登录续期，并用积分余额差确认是否真的到账。"
                        .to_string(),
                ),
                modes: vec![proto::CheckinMode::Checkin],
                ..Default::default()
            })
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 标准 OpenAI Chat Completions 转发。网关会自己带 token 重试 / 换号。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let credential = credentials::load(ctx, &req.account).await?;
            let ak = client::resolve_service_ak(ctx, &credential).await?;
            let mut body = req.body.clone();
            if !req.wire_model.trim().is_empty() {
                body["model"] = json!(req.wire_model);
            }
            let headers = vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                (
                    "Accept".to_string(),
                    if req.stream {
                        "text/event-stream".to_string()
                    } else {
                        "application/json".to_string()
                    },
                ),
                ("Authorization".to_string(), format!("Bearer {ak}")),
                ("X-Meoo-Client".to_string(), "cli".to_string()),
            ];
            Ok(ChatPlan::chat(
                endpoints::url(endpoints::gateway_base(), endpoints::EP_CHAT),
                headers,
                body,
            ))
        })
    }

    /// 401 换号 / 额度不足冷却 / 审核拦截不罚号，其余透传。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let message = crate::http::error_message(body, status);
        let code = body
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .or_else(|| body.get("code").and_then(Value::as_str))
            .unwrap_or("");
        if status == 401 || status == 403 || code.contains("SERVICE_AK") || code == "invalid_token" {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 402
            || status == 429
            || code.contains("INSUFFICIENT")
            || code.contains("QUOTA")
            || message.contains("积分不足")
            || message.contains("额度不足")
        {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: None,
                status,
            };
        }
        if status >= 400 && (message.contains("内容审核") || message.contains("AuditFailed")) {
            return proto::ErrorClass::ContentBlocked {
                status,
                message,
                upstream_code: None,
            };
        }
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: None,
        }
    }

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async { Ok(models::catalog()) })
    }

    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            if account.is_empty() && ctx.store_get(ID, "").await.is_err() {
                agent2api_provider_sdk::log_info!("[Meoo] 模型目录刷新跳过：尚未添加账号");
                return Ok(proto::RefreshOutcome::unchanged());
            }
            Ok(models::refresh(ctx, account, force).await)
        })
    }

    // ── 余额 / 签到 ───────────────────────────────────────────

    fn usage<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(checkin::usage(ctx, account))
    }

    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(checkin::checkin(ctx, account))
    }

    /// 只读状态：绝不触发登录续期，否则「看一眼状态」等于签到一次。
    fn claim_status<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _refresh: bool,
    ) -> SendFuture<'a, Value> {
        Box::pin(checkin::status(ctx, account))
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 添加账号。插件自有 UI 页只调得到网关 `/api/accounts`，拿不到登录 RPC
    /// 的直连路径，所以浏览器登录借 `action` 分流到同一套 flow；手填项目
    /// 服务 AK 仍走 `credentials::normalize`。
    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            if let proto::NormalizeSource::Manual { payload } = &input.source {
                if let Some(result) = browser_login::ui_dispatch(ctx, payload).await {
                    return result;
                }
            }
            credentials::normalize(ctx, input).await
        })
    }

    fn account_view(&self, secret: &Value) -> Result<proto::AccountView, String> {
        Ok(credentials::account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(credentials::identity_of(secret))
    }

    // ── 登录 ──────────────────────────────────────────────────

    fn login_start<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(browser_login::start(ctx, method, payload))
    }

    fn login_poll<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(browser_login::poll(ctx, flow))
    }

    fn login_cancel<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, ()> {
        Box::pin(browser_login::cancel(ctx, flow))
    }
}
