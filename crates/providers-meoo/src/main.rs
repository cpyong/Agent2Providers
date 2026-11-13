//! Meoo 秒悟 独立插件包（一家一包）。
//!
//! ── 这个包为什么存在 ────────────────────────────────────────
//! Meoo 的两套接口是分开的：
//!   - 控制面 `https://meoo.com/open/v1/cli-compat/…`：OAuth access token，
//!     拿身份、积分、项目、项目服务 AK；
//!   - AI 网关 `https://api.meoo.host/meoo-ai/compatible-mode/v1`：项目服务 AK，
//!     OpenAI Chat Completions 兼容（`/models`、`/chat/completions`）。
//! 网关把插件当独立子进程拉起，握手只用 `serve_blocking` 一条语句；
//! 其余全部在 `meoo` 模块里。
//!
//! ── 能力与边界（不吹牛）────────────────────────────────────
//! - 登录：官方 OAuth PKCE（`cli.compat`），浏览器点一次授权，本机 loopback
//!   收 code。也支持手填项目服务 AK（此时只能聊天 / 拉模型，查不了积分）。
//! - 模型：网关 `/models` 在线目录 + 内置兜底清单；不编造模型名。
//! - 对话：无状态 `build_chat`，交给网关发出去，插件不做流式中转。
//! - 签到：**上游没有独立「签到 / 领取」接口**。官方文档写的是「所有用户
//!   每日首次登录系统免费获赠 2000 积分」，即服务端在登录时发放。本包按
//!   WPS 灵犀那套取证纪律实现：领前读积分 → 用 refresh_token 走一次登录续期
//!   → 领后回读积分，只有余额真的涨了才报 `Claimed`，HTTP 200 一律不算数。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 编译期禁止 unwrap / expect / panic（clippy lint）。所有错误走 `Result`
//! 的 `String` 通道，文案要告诉用户下一步该做什么。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod http;
mod meoo;

use agent2api_provider_sdk::BoxProvider;

fn main() {
    let providers: Vec<BoxProvider> = vec![Box::new(meoo::MeooProvider)];
    agent2api_provider_sdk::serve_blocking(providers);
}
