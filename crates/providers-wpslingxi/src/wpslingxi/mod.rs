//! WPS 灵犀（lingxi）插件实现：`Provider` trait 的装配层。
//!
//! ── 上游长什么样 ────────────────────────────────────────────
//! ```text
//!   账号面  GET  /api/public/v1/account/profile        资料（补 uid）
//!           GET  /api/aioffice/v1/sessions/plans      模型 / 模式目录
//!           GET  /api/public/v1/credits/balance       智点余额
//!           GET  /api/public/v1/tasks                 任务中心（含每日签到）
//!           POST /api/public/v1/tasks/daily_check_in/claim?date=…  签到
//!   对话面  POST /api/aioffice/v1/cowork/sessions     → session_id
//!           POST /api/public/v1/credits/runs          → run_id（按 run 计费）
//!           POST /api/public/v1/agent-runtime/chat/completions → 私有 SSE
//! ```
//! 鉴权只有 `Cookie: wps_sid=…`，外加一套固定的桌面客户端标识头；域名按
//! `lingxi.wps.cn` → `lingxi.kdocs.cn` 回落（两套账号集群）。
//!
//! ── 明确**没有**的东西（不申报就不显示入口）─────────────────
//!   - **没有续期接口**：Cookie 失效只能重新登录，`refresh = false`；
//!   - **没有官方 OAuth / 设备码 / 换票协议**：所以「网页登录」不是 Trae
//!     那种「授权页回调 → 换票」，而是插件自己拉起一个独立 Chromium 实例，
//!     用户在里面登录 WPS 账号后，从它的 CDP cookie jar 读 `wps_sid`
//!     （见 `web_login.rs`）。`web_login = true` 指的是这个入口；
//!   - **没有桌面端导入**：Cookie 没有稳定可读的落盘位置；
//!   - **没有新手礼 / 活动套餐领取**：`claims = false`；
//!   - **没有「默认模型」概念**：注入别家的模型名只会换来一次必然失败。
//!
//! ── 明确**有**的东西 ────────────────────────────────────────
//!   - 模型代理（`stateful`：私有 SSE 由插件翻译成标准 chat SSE）；
//!   - 上游模型目录 + 内置快照兜底；
//!   - 智点余额（含赠送批次到期时刻）；
//!   - 每日签到（状态回查 + 余额差双重确认，绝不把 HTTP 200 当成功）；
//!   - 浏览器登录（独立临时 profile，读完 Cookie 即关；手动粘贴保留为兜底）。

pub mod chat;
pub mod checkin;
pub mod client;
pub mod credentials;
pub mod endpoints;
pub mod models;
pub mod web_login;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

use crate::http;

/// provider id（落盘契约，不能改）。
pub const ID: &str = "wpslingxi";

pub struct WpsLingxiProvider;

impl Provider for WpsLingxiProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "WPS 灵犀")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 上游不是 OpenAI 兼容层：一次通话要建 session + 领 run，
                // 响应是私有 SSE（choices[].text）。插件自己读帧翻译。
                stateful: true,
                // Cookie 没有续期接口，失效只能重新登录
                refresh: false,
                model_refresh: true,
                usage: true,
                checkin: true,
                claims: false,
                // 上游没有 OAuth / 设备码：拉起独立浏览器，登录完从 CDP 读 wps_sid
                web_login: true,
                // 出站 SSE 由插件自己生成，不需要网关改写模型名 / 丢换行
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                supports_default_model: false,
                env_credentials: false,
            })
            .with_login(vec![
                // 主推：插件拉起一个独立临时浏览器窗口，登录完自动收下账号
                proto::LoginMethod::Browser {
                    button: "网页登录（打开浏览器）".to_string(),
                    hint: Some(
                        "会打开一个独立的 Chrome / Edge 窗口，用微信扫码或手机号登录 WPS 账号；\
                         登录完成后插件自动读取 wps_sid 并添加账号，不用复制 Cookie。\
                         该窗口使用临时配置目录，不碰你日常浏览器的登录态。"
                            .to_string(),
                    ),
                },
                // 兜底：无 GUI / 没有浏览器 / 远程部署时仍可手动添加
                proto::LoginMethod::Token {
                    label: "wps_sid Cookie（手动）".to_string(),
                    placeholder: "粘贴整段 Cookie，或单串 wps_sid=…".to_string(),
                    extra_fields: Vec::new(),
                },
            ])
            // 远程目录没拉到时用上游客户端 1.3.31 的内置快照（不是编的模型名）
            .with_models(models::snapshot())
            .with_account_features(proto::AccountFeatures {
                usage: true,
                // usage 会给 bonus_credits[] / purchased_credits[] 逐份读数，
                // 带 expireAt（上游给 expire_time）
                credit_detail: true,
                credit_expiry: true,
                identifier: vec!["uid".to_string()],
                // Cookie 没有官方承诺的有效期，不写 expiry 也不报 long_lived
                expiry: Vec::new(),
                long_lived: false,
                ..Default::default()
            })
            .with_checkin(proto::CheckinInfo {
                description: Some(
                    "WPS 灵犀任务中心每日签到：状态回查 + 智点余额差确认到账，发的是智点"
                        .to_string(),
                ),
                modes: vec![proto::CheckinMode::Checkin],
                ..Default::default()
            })
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 会话式转发：转调 `chat::run`。
    ///
    /// 刻意**不实现** `build_chat`：上游没有「一次请求 = 一次补全」的接口，
    /// 万一网关的 `stateful` 分流坏了，默认实现直接报错，好过往一个不存在
    /// 的路径上发请求。
    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: agent2api_provider_sdk::ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(chat::run(ctx, req, sink))
    }

    /// 上游错误分类（网关在 HTTP 非 2xx 的路径上用）。
    ///   - 401 / 403 → `TokenExpired`：本家没有续期接口，编排层最终会提示重贴；
    ///   - 402 / 429 / 智点不足文案 → `QuotaLimited`（换号 / 冷却）；
    ///   - 审核拦截 → `ContentBlocked`（不罚账号）；
    ///   - 其余 → 原样透传。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let raw = client::text_of(body);
        let detail = if raw.is_empty() {
            "上游错误".to_string()
        } else {
            raw
        };
        let message = format!("上游返回 {status}: {detail}");
        let code = body.get("code").and_then(|value| match value {
            Value::Number(number) => number.as_i64(),
            Value::String(text) => text.trim().parse::<i64>().ok(),
            _ => None,
        });
        if status == 401 || status == 403 {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 402
            || status == 429
            || client::is_credit_exhausted(&detail)
        {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status,
            };
        }
        if status >= 400
            && (detail.contains("AuditFailed")
                || detail.contains("内容审核")
                || detail.contains("暂时无法回答此问题"))
        {
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

    /// 取当前能用的凭证：`Cookie:` 头的完整值。
    ///
    /// 本家 `refresh = false`，网关不会走后台续期；这个方法主要用于握手后的
    /// 「有没有凭证」探测与排障。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credential = credentials::snapshot(ctx, account).await?;
            Ok(credential.cookie_header())
        })
    }

    /// 没有续期接口。报错比把同一个被拒的 Cookie 再交一遍更诚实。
    fn refresh_token<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async {
            Err("WPS 灵犀没有凭证续期接口：Cookie 已失效，请点「网页登录」重新登录，或重新粘贴 wps_sid".to_string())
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

    /// 刷模型目录：用指定账号（空串 = 库里第一个可用账号）的 Cookie。
    /// 「一个账号都没有」不是失败，是「用户还没添加账号」的正常状态。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let (credential, proxy) = match credentials::load(ctx, account).await {
                Ok(loaded) => loaded,
                Err(_) if account.is_empty() => {
                    agent2api_provider_sdk::log_info!(
                        "[WPS 灵犀] 模型目录刷新跳过：尚未添加账号"
                    );
                    return Ok(proto::RefreshOutcome::unchanged());
                }
                Err(error) => return Ok(proto::RefreshOutcome::failed(error)),
            };
            let http_client = http::client(proxy.as_ref(), &ctx.env.proxy);
            Ok(models::refresh(&http_client, &credential, force).await)
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

    /// 只读签到状态（界面「今日是否已签 / 连续几天」）。
    ///
    /// 默认实现转调 `claim` 会是「看一眼就替用户领一遍」，这里显式给只读版。
    fn claim_status<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _refresh: bool,
    ) -> SendFuture<'a, Value> {
        Box::pin(checkin::status(ctx, account))
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 添加账号。插件自有 UI 页只调网关 `/api/accounts`，拿不到登录 RPC
    /// 的直连路径，所以浏览器登录借 `action` 分流到同一套 flow（与 Today AI
    /// 的短信登录同款做法）；普通粘贴仍走 `credentials::normalize`。
    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            if let proto::NormalizeSource::Manual { payload } = &input.source {
                if let Some(result) = web_login::ui_dispatch(ctx, payload).await {
                    return result;
                }
            }
            credentials::normalize(ctx, input).await
        })
    }

    fn account_view(&self, secret: &Value) -> Result<proto::AccountView, String> {
        credentials::account_view(secret)
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(credentials::identity_of(secret))
    }

    // ── 登录 ──────────────────────────────────────────────────

    /// 官方没有 OAuth / 设备码协议，所以这里不是「给一个授权页地址」，而是
    /// 插件自己拉起独立浏览器窗口；用户在里面登录，插件从 CDP 读 `wps_sid`。
    /// 返回的 `url` 刻意留空：不能再让网关另开一个窗口，那个窗口里的登录态
    /// 插件读不到，用户会以为登录没生效。
    fn login_start<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(web_login::start(ctx, method))
    }

    /// 网关按 `poll_interval_ms` 来问；只有拿到经资料接口验证的 Cookie 才
    /// 生成账号，其余情况如实报「还在等」。
    fn login_poll<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(web_login::poll(ctx, flow))
    }

    /// 取消：关掉那个临时浏览器并删掉临时 profile。
    fn login_cancel<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, ()> {
        Box::pin(web_login::cancel(flow))
    }
}
