//! WPS 灵犀（lingxi）独立插件包。
//!
//! ── 为什么单独一个包 ────────────────────────────────────────
//! 与 `providers-todayai` / `providers-opencode` 同一口径：一家一包、独立构建
//! 与发布。上游私有协议（三步 run + 自定义 SSE）改动频繁，独立发版不影响合集包。
//! 包里的 provider id 是 `wpslingxi`，**同时只该装一个**提供这个 id 的包。
//!
//! ── 目录约定 ────────────────────────────────────────────────
//! ```text
//!   src/main.rs                入口：注册一家 provider，交给 SDK 的 serve 主循环
//!   src/http.rs                出网薄封装（按代理缓存客户端 + JSON / SSE 收发）
//!   src/wpslingxi/
//!     mod.rs                   Provider trait 实现（能力申报 / 路由）
//!     endpoints.rs             上游域名、路径、桌面端标识、环境变量覆盖
//!     credentials.rs           wps_sid Cookie 归一、账号视图、身份
//!     client.rs                请求头、双域回落、三步 run、错误文案
//!     models.rs                模型目录（plans + models）与内置快照
//!     chat.rs                  会话式转发：run 缓存 + 私有 SSE → OpenAI chat SSE
//!     checkin.rs               智点余额、任务中心状态、每日签到
//!     web_login.rs             浏览器登录：独立 Chromium + CDP 读 wps_sid
//!   ui/wpslingxi/              账号页（浏览器登录 / 签到 / 模型清单）
//!   pack.json                  插件清单
//! ```
//!
//! ── 与其余独立包共用不了的部分 ───────────────────────────────
//! `http.rs` 逻辑同源但多一条「打开流式响应」的路径（本家上游不是标准
//! OpenAI SSE，插件必须自己读帧并翻译），所以没有合并进 SDK —— SDK 是第三方
//! 插件也要编译的契约面，不该为一家上游的私有协议扩大它。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod http;
mod wpslingxi;

use agent2api_provider_sdk::BoxProvider;

fn main() {
    let providers: Vec<BoxProvider> = vec![Box::new(wpslingxi::WpsLingxiProvider)];
    agent2api_provider_sdk::serve_blocking(providers);
}
