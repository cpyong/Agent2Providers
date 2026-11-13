//! Today AI（todayai.cn）上游地址与客户端标识（环境变量可覆盖）。
//!
//! ── 上游长什么样（依据：Today 桌面端 CN 版 `app.asar` v1.23.0）──────
//! 三个平面（细节见各子模块的模块头）：
//!
//! ```text
//!   账号（Better Auth） https://auth.todayai.cn/api/auth
//!                        手机号验证码：/phone-number/{send-otp,verify}
//!                        换访问令牌：  /token        （Bearer = 会话令牌）
//!   访问令牌             {账号基址}/token  的等价物，JWT，audience = API 基址
//!   助理后端             https://api.todayai.cn/v1
//!                        目录 / 设置 / 会话（Bearer JWT；**不是** OpenAI 兼容层）
//! ```
//!
//! ── 为什么要带客户端标识头 ──────────────────────────────────
//! 桌面端的每一次出站请求都由 `AccountHeaders.build()` 统一补三个头：
//! `X-App-Version`（安装包版本）、`X-Client-Platform`（`windows` / `macos` /
//! `linux`，**访问网关时带 `-client` 后缀**）、以及账号接口的 `Origin`
//! （网页来源）。这些不是用户凭证，属于「客户端内置常量」，因此内置为默认值、
//! 环境变量可覆盖，改上游版本不必重新发版。

/// 账号（Better Auth）默认基址
pub const DEFAULT_AUTH_BASE_URL: &str = "https://auth.todayai.cn";

/// API 基址（目录 / 设置 / 助理会话）
pub const DEFAULT_API_BASE_URL: &str = "https://api.todayai.cn";

/// 网页来源（账号接口的 `Origin` 头；桌面端 `webOrigin`）
pub const DEFAULT_WEB_ORIGIN: &str = "https://todayai.cn";

/// 客户端版本（桌面端 `X-App-Version`；取自 CN 安装包 1.23.0）
pub const DEFAULT_APP_VERSION: &str = "1.23.0";

/// 读环境变量覆盖（空值视为未设置），去掉尾部斜杠
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

/// 账号（Better Auth）基址
pub fn auth_base_url() -> String {
    env_override("TODAYAI_AUTH_BASE_URL")
        .or_else(|| env_override("TODAY_AUTH_BASE_URL"))
        .unwrap_or_else(|| DEFAULT_AUTH_BASE_URL.to_string())
}

/// 模型网关基址（不含 `/v1`）
pub fn api_base_url() -> String {
    env_override("TODAYAI_API_BASE_URL")
        .or_else(|| env_override("TODAY_API_BASE_URL"))
        .unwrap_or_else(|| DEFAULT_API_BASE_URL.to_string())
}

/// 网页来源（`Origin` 头）
pub fn web_origin() -> String {
    env_override("TODAYAI_WEB_ORIGIN").unwrap_or_else(|| DEFAULT_WEB_ORIGIN.to_string())
}

/// 客户端版本（`X-App-Version`）
pub fn app_version() -> String {
    env_override("TODAYAI_APP_VERSION").unwrap_or_else(|| DEFAULT_APP_VERSION.to_string())
}

/// 账号接口的 `X-Client-Platform`（桌面端映射表：`windows-client → windows`）
pub fn auth_client_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// 模型网关的 `X-Client-Platform`（桌面端用未映射的平台名，带 `-client`）
pub fn api_client_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows-client"
    } else if cfg!(target_os = "macos") {
        "macos-client"
    } else {
        "linux-client"
    }
}
