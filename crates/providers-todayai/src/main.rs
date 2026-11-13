//! Today AI 独立插件包。
//!
//! ── 为什么单独一个包 ────────────────────────────────────────
//! 这个包**只**包含 Today AI 一家，与官方合集包（`crates/providers`，二十多家
//! 一起编译）分开构建、分开发布、分开安装。好处是：
//!   1. 上游契约变化时只重发这一家，用户装的合集包不受影响；
//!   2. 单个二进制更小（不需要为一家拉进整个依赖树里的其余实现）；
//!   3. 两家包的 provider id 都是 `todayai`，但**同时只该装一个** ——
//!      网关按 provider id 认实现，重复安装会出现两份同名实现。
//!
//! ── 目录约定 ────────────────────────────────────────────────
//! ```text
//!   src/
//!     main.rs      入口：注册一家 provider，交给 SDK 的 serve 主循环
//!     http.rs      出网薄封装（按代理缓存客户端 + JSON 收发）
//!     store.rs     secret 的写回辅助（真正的账号库在网关侧）
//!     todayai/     实现本体（mod / client / credentials / endpoints / login / models）
//!   pack.json      插件清单（网关按它安装与启动）
//! ```
//!
//! ── 为什么把 `http.rs` / `store.rs` 各留一份 ─────────────────
//! 合集包里的同名模块是那个二进制 crate 的私有模块，不能跨 crate 复用；
//! 而把两家的公共设施提进 `provider-sdk` 会扩大契约面（SDK 是第三方插件
//! 也要编译的库）。这里的用量很小 —— 一个按代理缓存的 `reqwest::Client`、
//! 两个 JSON 收发函数、一个 `with_fields` —— 各自留一份比改契约便宜。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod http;
mod store;
mod todayai;

use agent2api_provider_sdk::BoxProvider;

fn main() {
    let providers: Vec<BoxProvider> = vec![Box::new(todayai::TodayAiProvider)];
    agent2api_provider_sdk::serve_blocking(providers);
}
