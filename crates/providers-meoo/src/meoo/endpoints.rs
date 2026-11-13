//! Meoo 的两套域名与全部端点常量。
//!
//! ── 事实来源 ────────────────────────────────────────────────
//! 官方开源 CLI `@aliyun-meoo/cli` 0.5.8（`dist/lib/oauth.js`、`config.js`、
//! `apiClient.js`）+ 公开集成 dsh-meoo 的模型网关配置：
//!   - OAuth：`https://meoo.com/oauth/authorize` / `/oauth/token`，
//!     PKCE S256，scope `cli.compat`，client_id 按域名选正式/测试；
//!   - 控制面：`/open/v1/cli-compat/…`，Bearer = OAuth access token；
//!   - AI 网关：`https://api.meoo.host/meoo-ai/compatible-mode/v1`，
//!     Bearer = 项目服务 AK（`meoo_project_service_v1.*`）。
//! 官网前端 0.0.102 里的 `/api/v1/agent/models` 需要网页 Cookie，本插件走
//! OAuth，不用那条。
//!
//! ── 环境变量覆盖只认本机 ────────────────────────────────────
//! `MEOO_API_BASE` / `MEOO_GATEWAY_BASE` 是离线冒烟测试用的钩子。刻意**只
//! 接受 http 的 127.0.0.1 / localhost**：否则一个环境变量就能把令牌和 AK
//! 发到任意地址，这种钩子不该带进正式包。

use std::sync::OnceLock;

pub const DEFAULT_API_BASE: &str = "https://meoo.com";
pub const DEFAULT_GATEWAY_BASE: &str = "https://api.meoo.host/meoo-ai/compatible-mode/v1";

pub const OAUTH_AUTHORIZE: &str = "/oauth/authorize";
pub const OAUTH_TOKEN: &str = "/oauth/token";
pub const OAUTH_CALLBACK_PATH: &str = "/oauth/callback";
pub const OAUTH_SCOPE: &str = "cli.compat";
pub const CLIENT_ID_PRODUCTION: &str = "meoo_app_tdWlRbpvAxN1EjmZ8XICxdTh";
pub const CLIENT_ID_TEST: &str = "meoo_app_SFNvmHUFNePxyyfGjCd9hyaE";

pub const EP_IDENTITY: &str = "/open/v1/cli-compat/account/identity";
pub const EP_POINTS: &str = "/open/v1/cli-compat/account/points";
pub const EP_ENTITLEMENT: &str = "/open/v1/cli-compat/account/entitlement";
pub const EP_PROJECTS: &str = "/open/v1/cli-compat/projects?page_size=50";
pub const EP_MODELS: &str = "/models";
pub const EP_CHAT: &str = "/chat/completions";

/// 每日登录赠送积分（官方文档 `docs.meoo.com/coindesc`，四档套餐都是 2000）。
pub const DAILY_LOGIN_GRANT: f64 = 2_000.0;

fn env_override(key: &str) -> Option<String> {
    let value = std::env::var(key).ok()?.trim().trim_end_matches('/').to_string();
    if value.is_empty() {
        return None;
    }
    // 只放行本机 loopback 的 http，且路径里不能塞 userinfo / 查询串。
    let rest = value.strip_prefix("http://")?;
    let host = rest.split('/').next().unwrap_or("");
    let name = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    let loopback = matches!(name.as_str(), "127.0.0.1" | "localhost" | "[::1]");
    let clean = !rest.contains('@') && !rest.contains('?') && !rest.contains('#');
    if loopback && clean {
        Some(value)
    } else {
        None
    }
}

fn cached(key: &str, fallback: &str, slot: &'static OnceLock<String>) -> &'static str {
    slot.get_or_init(|| env_override(key).unwrap_or_else(|| fallback.to_string()))
        .as_str()
}

pub fn api_base() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    cached("MEOO_API_BASE", DEFAULT_API_BASE, &BASE)
}

pub fn gateway_base() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    cached("MEOO_GATEWAY_BASE", DEFAULT_GATEWAY_BASE, &BASE)
}

/// 正式 client id 只在 `meoo.com` 上用；其它域名（含本机假上游）用测试 id。
pub fn client_id(api_base: &str) -> &'static str {
    let rest = api_base.split("://").nth(1).unwrap_or("");
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if host == "meoo.com" {
        CLIENT_ID_PRODUCTION
    } else {
        CLIENT_ID_TEST
    }
}

fn encode_path(value: &str) -> String {
    crate::http::encode_component(value)
}

pub fn project_cloud_environment(project_url_id: &str) -> String {
    format!(
        "/open/v1/cli-compat/projects/{}/cloud/environment",
        encode_path(project_url_id)
    )
}

pub fn project_cloud(project_url_id: &str) -> String {
    format!(
        "/open/v1/cli-compat/projects/{}/cloud",
        encode_path(project_url_id)
    )
}

pub fn url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_client_id_only_on_official_host() {
        assert_eq!(client_id("https://meoo.com"), CLIENT_ID_PRODUCTION);
        assert_eq!(client_id("https://meoo.com/"), CLIENT_ID_PRODUCTION);
        assert_eq!(client_id("http://127.0.0.1:8080"), CLIENT_ID_TEST);
        assert_eq!(client_id("https://gray-meoo.aliyun-inc.com"), CLIENT_ID_TEST);
    }

    #[test]
    fn loopback_override_is_accepted_and_remote_is_rejected() {
        assert!(env_override("MEOO_NO_SUCH_VAR").is_none());
        // 只测纯函数：用本地变量模拟，不碰进程级环境变量（并行单测会互相污染）
        assert!(is_loopback("http://127.0.0.1:1234"));
        assert!(is_loopback("http://localhost:1234"));
        assert!(!is_loopback("https://meoo.com"));
        assert!(!is_loopback("http://evil.example.com"));
        assert!(!is_loopback("http://user@127.0.0.1:1234"));
    }

    #[test]
    fn project_paths_are_escaped() {
        assert_eq!(
            project_cloud_environment("abc/../x"),
            "/open/v1/cli-compat/projects/abc%2F..%2Fx/cloud/environment"
        );
        assert!(project_cloud("p 1").contains("p%201"));
    }

    #[test]
    fn url_joins_without_double_slash() {
        assert_eq!(url("https://api.meoo.host/v1/", "/models"), "https://api.meoo.host/v1/models");
    }

    /// 与 `env_override` 同一判据，抽出来给单测用（避免改进程环境变量）。
    fn is_loopback(value: &str) -> bool {
        let Some(rest) = value.strip_prefix("http://") else {
            return false;
        };
        let host = rest.split('/').next().unwrap_or("");
        let name = host.split(':').next().unwrap_or("").to_ascii_lowercase();
        let loopback = matches!(name.as_str(), "127.0.0.1" | "localhost" | "[::1]");
        loopback && !rest.contains('@') && !rest.contains('?') && !rest.contains('#')
    }
}
