//! `opencode-local`：把**本机 opencode 桌面端 / `opencode serve`** 提供的
//! 免费模型代理成标准 OpenAI Chat 接口。
//!
//! ── 为什么需要第二家 provider ────────────────────────────────
//! 同包的 `opencode`（云端直连）走 `opencode.ai/inference` 匿名车道，实测
//! `POST /v1/chat/completions` 会返回 `429 FreeUsageLimitError`（出口 IP 免费
//! 额度耗尽，`Retry-After` 约 14 小时），换 Key / 换账号都无效。而本机
//! opencode 客户端通过自己的 v2 API 发起会话时，同样的免费模型能正常回答
//! （2026-11-10 实测 `iceman-free` 返回 PONG）。所以稳定可用的路是：
//!
//! ```text
//!   OpenAI 客户端 → agent2api → 本插件 → http://127.0.0.1:<随机端口>/api/*
//!                                      → 本机 opencode → 模型
//! ```
//!
//! v2 API 是**多步会话协议**（建会话 → 发 prompt → SSE 事件 → 收尾），不是
//! 一次 POST 就能拿到补全，因此 `stateful = true`，走 `conversation`。
//! 能力位是整家静态的，不能按账号切换，所以必须注册成独立的 provider id。
//!
//! ── 使用前提 ────────────────────────────────────────────────
//! 本机 opencode 桌面端在运行（设置 → 服务器里能看到连接地址）；插件会
//! 自动从 `~/.local/state/opencode/service.json` 读地址与密码，也支持手填。
//! 端口每次启动都可能变，别写死截图里的 26894。

pub mod api;
pub mod chat;
pub mod models;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{
    ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture,
};
use serde_json::Value;

/// provider id（落盘契约，不能改）。
pub const ID: &str = "opencode-local";

pub struct OpenCodeLocalProvider;

impl Provider for OpenCodeLocalProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "OpenCode 本地服务")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 建会话 → prompt → SSE → 清理，是典型的多步会话协议
                stateful: true,
                // 服务密码长期有效，没有 refresh 接口
                refresh: false,
                model_refresh: true,
                usage: false,
                checkin: false,
                claims: false,
                web_login: false,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                // 允许一个账号都没有时靠自动发现 service.json 转发
                anonymous_default_session: true,
                system_proxy_when_unset: false,
                // 目录来自本机服务，不依赖某条账号记录
                refresh_uses_account: false,
                // 目录只认本机服务列出的模型，注入别家默认名会换来一次拒绝
                supports_default_model: false,
                env_credentials: true,
            })
            .with_login(vec![
                proto::LoginMethod::Desktop {
                    button: "连接本机 OpenCode 服务（自动发现）".into(),
                    hint: Some(
                        "自动读取 ~/.local/state/opencode/service.json 里的地址与密码；\
                         请先启动 opencode 桌面端，端口每次启动可能变化"
                            .into(),
                    ),
                },
                proto::LoginMethod::Token {
                    label: "服务地址（设置 → 服务器里的连接地址）".into(),
                    placeholder: "http://127.0.0.1:28894".into(),
                    extra_fields: vec![proto::LoginField {
                        key: "password".to_string(),
                        label: "服务密码（留空自动读取）".to_string(),
                        placeholder: "service.json 里的 password".to_string(),
                        optional: true,
                        multiline: false,
                    }],
                },
            ])
            .with_models(vec![])
            .with_account_features(proto::AccountFeatures {
                usage: false,
                long_lived: true,
                identifier: vec!["uid".to_string()],
                ..Default::default()
            })
    }

    // ── 数据面 ────────────────────────────────────────────────

    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(chat::run(ctx, req, sink))
    }

    /// 本机服务的错误一律原样透传：没有「换个账号就能成功」的语义。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("本机 OpenCode 服务返回 {status}"));
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
        Box::pin(async { Ok(models::snapshot()) })
    }

    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let server = api::resolve(ctx, account).await?;
            Ok(models::refresh(&server, force).await)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(api::normalize(ctx, input))
    }

    fn account_view(&self, secret: &Value) -> Result<proto::AccountView, String> {
        api::account_view(secret)
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(api::identity_of(secret))
    }
}
