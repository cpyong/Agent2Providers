//! WPS 灵犀上游地址、路径与桌面端标识。
//!
//! ── 协议来源（2026-11 公开实现取证）──────────────────────────
//! `RobbsLuo/Coding2API` 的 `src/provider/lingxi/{client,credential,events}.py`
//! 与 `TECHNICAL.md` §5.8、`xfyang728/dsh-router-wpslingxi` 的 README、
//! WPS 官方社区《不开电脑灵犀也能签到领智点》（bbs.wps.cn/topic/100885）。
//! 三条链路：
//!
//! ```text
//!   账号面（两个域都试，主域 → kdocs 回落域）
//!     GET  /api/public/v1/account/profile        → data.id / data.name
//!     GET  /api/aioffice/v1/sessions/plans      → data.plans[] / data.models[]
//!     GET  /api/public/v1/credits/balance       → data.total_balance / bonus_credits[]
//!     GET  /api/public/v1/tasks                 → data.tasks[]（task_key=daily_check_in）
//!     POST /api/public/v1/tasks/daily_check_in/claim?date=YYYY-MM-DD   body {}
//!
//!   对话面（一次对话三步，按 run 计费）
//!     POST /api/aioffice/v1/cowork/sessions     → data.session_id
//!     POST /api/public/v1/credits/runs          → data.run_id / group_id
//!     POST /api/public/v1/agent-runtime/chat/completions  → 私有 SSE
//! ```
//!
//! ── 为什么必须两个域都试 ────────────────────────────────────
//! `lingxi.wps.cn` 与 `lingxi.kdocs.cn` 是两套账号集群：kdocs 账号的
//! `wps_sid` 只在自己的域上被认。只打主域的表现是 401/404，看上去像
//! 「Cookie 过期」，实际是域不对。回落顺序固定，失败信息里带上试过的域。
//!
//! ── 为什么要伪装桌面端 ─────────────────────────────────────
//! `X-Client-Type: lingxi_claw` + 三个 `x-kso-*` 是桌面客户端 1.3.31 的固定
//! 常量。设备号必须是**稳定值**：每次随机换设备号会被上游当成新设备。
//! 这些是客户端内置常量，不是用户凭证，所以写死为默认值。

/// 主域
pub const DEFAULT_BASE: &str = "https://lingxi.wps.cn";
/// kdocs 账号集群的回落域
pub const DEFAULT_FALLBACK_BASE: &str = "https://lingxi.kdocs.cn";

pub const EP_PROFILE: &str = "/api/public/v1/account/profile";
pub const EP_PLANS: &str = "/api/aioffice/v1/sessions/plans";
pub const EP_BALANCE: &str = "/api/public/v1/credits/balance";
pub const EP_TASKS: &str = "/api/public/v1/tasks";
pub const EP_CLAIM: &str = "/api/public/v1/tasks/daily_check_in/claim";
pub const EP_SESSIONS: &str = "/api/aioffice/v1/cowork/sessions";
pub const EP_RUNS: &str = "/api/public/v1/credits/runs";
pub const EP_CHAT: &str = "/api/public/v1/agent-runtime/chat/completions";

/// 上游客户端类型（原生工具调用只在这条通路上给）
pub const CLIENT_TYPE: &str = "lingxi_claw";
pub const CLIENT_LANG: &str = "zh-CN";

/// 桌面客户端标识（安装包 1.3.31）
pub const APP_NAME: &str = "win-lingxi-desktop";
pub const APP_VERSION: &str = "1.3.31";

/// 稳定设备号：`sha256("coding2api-lingxi-device")` 的十六进制。
///
/// 上游按设备号识别客户端；换号会被当成新设备。这里照抄公开实现的取值，
/// 不引入 sha2 只为在每次请求时重算一遍同一个常量。
pub const DEVICE_ID: &str = "c68b3ae649d03e1618d15b1d964c8716853d2320434cc66988a68e7a2170baf4";

/// 建 cowork 会话时的环境标识（上游只用它给会话起工作目录名）
pub const DEVICE_NAME: &str = "agent2api";
pub const WORK_SPACE_DIR: &str = "agent2api";

/// 读环境变量覆盖（空值视为未设置），去掉尾部斜杠。
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

/// 本次要尝试的域（去重，主域在前）。
///
/// `WPSLINGXI_BASE_URL` 覆盖主域；`WPSLINGXI_FALLBACK_BASE_URL` 设成与主域
/// 相同即可关掉回落（自托管 / 抓包调试时用）。
pub fn bases() -> Vec<String> {
    let primary = env_override("WPSLINGXI_BASE_URL")
        .or_else(|| env_override("LINGXI_BASE_URL"))
        .unwrap_or_else(|| DEFAULT_BASE.to_string());
    let fallback = env_override("WPSLINGXI_FALLBACK_BASE_URL")
        .unwrap_or_else(|| DEFAULT_FALLBACK_BASE.to_string());
    if fallback == primary {
        vec![primary]
    } else {
        vec![primary, fallback]
    }
}
