//! 上游地址、车道与免费模型目录地址。
//!
//! ── 车道（来自桌面版 CLI 2.2.6 二进制与实测）──────────────
//! ```text
//!   anon     https://opencode.ai/inference/openai/v1   匿名免费，无需 Key
//!   zen      https://opencode.ai/zen                   v1 请求形状，Zen Key
//!   go       https://opencode.ai/zen/go                v2 请求形状，Go 套餐
//!   目录     {anon}/models                            公开，无需鉴权
//! ```
//! 匿名车道是桌面版的默认通道：桌面端没有登录入口，免费模型直接可用。
//! 实测桌面版 CLI 在无登录态时把 `apiKey` 写成字面量 `public`，请求带
//! `Authorization: Bearer public`。
//!
//! ── 2026-11-10 实测：匿名车道有出口 IP 免费额度 ───────────────
//! `GET /v1/models` 恒 200，但 `POST /v1/chat/completions` 对免费模型返回
//! `429 FreeUsageLimitError`（`Retry-After` 约 14 小时，文案是 "Free models
//! work in OpenCode"）；本机 opencode 桌面端日志里也出现过同一条错误。
//! 也就是说这条车道**并非随时可用**：额度耗尽时换 Key / 换账号都没用。
//! 需要稳定可用请改用本包的 `opencode-local` provider（走本机 v2 API）。
//!
//! 三条车道都只代理「免费模型」：清单与判定都取自匿名车道的 `/v1/models`，
//! 见 `models.rs`。

/// 匿名免费通道（桌面版默认，无需 Key）。
pub const ANON_BASE: &str = "https://opencode.ai/inference/openai/v1";

/// Zen 通道（使用 API Key 时的默认车道）。
pub const ZEN_BASE: &str = "https://opencode.ai/zen";

/// Go 套餐通道。
pub const GO_BASE: &str = "https://opencode.ai/zen/go";

/// 免费模型目录（公开 JSON，形状是 OpenAI `data[]` + 每项 `cost.text.*.price`）。
///
/// 必须与真实转发车道一致：旧版用全站能力目录 `models.opencode.ai/api.json`，
/// 筛出的模型有一大半在这条车道上并不存在（见 `models.rs` 的实测说明）。
pub const ANON_MODELS_URL: &str = "https://opencode.ai/inference/openai/v1/models";

/// 上游通道。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// 匿名免费：不带 Key，受出口 IP 的免费额度约束。
    Anonymous,
    /// Zen Key。
    Zen,
    /// Go 套餐 Key。
    Go,
}

impl Tier {
    /// 落盘值（`secret.tier`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anonymous => "anon",
            Self::Zen => "zen",
            Self::Go => "go",
        }
    }

    /// 界面显示名。
    pub fn label(self) -> &'static str {
        match self {
            Self::Anonymous => "匿名免费",
            Self::Zen => "Zen",
            Self::Go => "Go",
        }
    }

    /// 默认基址。
    pub fn default_base(self) -> &'static str {
        match self {
            Self::Anonymous => ANON_BASE,
            Self::Zen => ZEN_BASE,
            Self::Go => GO_BASE,
        }
    }

    /// 这条车道是否需要 Key。
    pub fn needs_key(self) -> bool {
        !matches!(self, Self::Anonymous)
    }
}

/// 解析落盘的 `tier`；空串 / 未知值按 Zen 处理。
pub fn parse_tier(raw: &str) -> Tier {
    match raw.trim().to_ascii_lowercase().as_str() {
        "go" => Tier::Go,
        "anon" | "anonymous" | "free" => Tier::Anonymous,
        _ => Tier::Zen,
    }
}

/// 有效基址：显式 `baseUrl` 优先，否则按车道取默认值（去掉尾部斜杠）。
pub fn base_url(tier: Tier, override_url: &str) -> String {
    let raw = override_url.trim();
    let value = if raw.is_empty() { tier.default_base() } else { raw };
    value.trim_end_matches('/').to_string()
}

/// 这个基址是不是 v1 车道（与 opencode2api 的 `IsV1Upstream` 一致）。
pub fn is_v1(base: &str) -> bool {
    let trimmed = base.trim().trim_end_matches('/').to_ascii_lowercase();
    trimmed == ZEN_BASE || trimmed.ends_with("/zen")
}

/// 是不是匿名 `inference` 车道（UA / 头集合与 Zen 车道不同）。
pub fn is_inference(base: &str) -> bool {
    base.to_ascii_lowercase().contains("/inference")
}

/// 聊天端点：基址本身已带 `/v1` 时直接拼 `/chat/completions`。
pub fn chat_url(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{trimmed}/chat/completions")
    } else {
        format!("{trimmed}/v1/chat/completions")
    }
}

/// 模型清单端点：基址已带 `/v1` 时拼 `/models`，否则拼 `/v1/models`。
pub fn models_url(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{trimmed}/models")
    } else {
        format!("{trimmed}/v1/models")
    }
}

/// 是不是官方三个基址之一。
///
/// 自定义基址（例如本机 `zen-gate` 网关、或第三方的 OpenAI 兼容聚合）
/// 的模型集合与官方目录不同，免费判定要改按上游 `/v1/models` 走。
pub fn is_official(base: &str) -> bool {
    let trimmed = base.trim().trim_end_matches('/');
    trimmed == ANON_BASE || trimmed == ZEN_BASE || trimmed == GO_BASE
}
