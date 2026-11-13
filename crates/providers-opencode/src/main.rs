//! OpenCode 独立插件包。
//!
//! ── 与 todayai 包同构 ───────────────────────────────────────
//! 一个 provider 一家包：单独构建、单独发布、单独安装。provider id 是
//! `opencode`，与合集包里的任何一家都不冲突。
//!
//! ── 目录约定 ────────────────────────────────────────────────
//! ```text
//!   src/
//!     main.rs       入口：注册两家 provider，交给 SDK 的 serve 主循环
//!     http.rs       云端出网薄封装（按代理缓存客户端 + JSON 收发）
//!     opencode/     云端直连实现（mod / account / endpoints / identity / models）
//!     local/        本机 v2 API 实现（mod / api / chat / models）
//!   ui/opencode/    自带账号页，同时管理两家的账号
//!   pack.json       插件清单（网关按它安装与启动）
//! ```
//!
//! ── 为什么是两家 ────────────────────────────────────────────
//! `opencode` 走云端匿名 inference（有出口 IP 免费额度，可能 429）；
//! `opencode-local` 走本机桌面端 v2 API（实测可用）。后者是**有状态会话**
//! 协议，`stateful` 是整家静态能力位，不能按账号切换，只能注册成第二家。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod http;
mod local;
mod opencode;

use agent2api_provider_sdk::BoxProvider;

fn main() {
    let providers: Vec<BoxProvider> = vec![
        // 稳定可用的一家排在前面
        Box::new(local::OpenCodeLocalProvider),
        Box::new(opencode::OpenCodeProvider),
    ];
    agent2api_provider_sdk::serve_blocking(providers);
}
