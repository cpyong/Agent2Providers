//! 账号凭证：**优先复用本机 opencode 登录态**，其次允许手工粘贴 Key。
//!
//! ── 本机登录态在哪（按顺序探测）──────────────────────────────
//! ```text
//!   %OPENCODE_AUTH_FILE%                                   显式覆盖（最高优先级）
//!   %XDG_DATA_HOME%\opencode\auth.json
//!   %USERPROFILE%\.local\share\opencode\auth.json          ← opencode 默认数据目录
//!   %USERPROFILE%\.config\opencode\auth.json
//!   %APPDATA%\opencode\auth.json
//!   %APPDATA%\ai.opencode.desktop\auth.json                ← 桌面版 userData
//!   %APPDATA%\ai.opencode.desktop\cli\<版本>\auth.json      ← 桌面版自带 CLI（嵌套目录）
//!   %LOCALAPPDATA%\opencode\auth.json
//!   %LOCALAPPDATA%\ai.opencode.desktop\auth.json
//!   %LOCALAPPDATA%\Programs\ai.opencode.desktop\auth.json  ← 桌面版安装目录
//!   %LOCALAPPDATA%\Programs\opencode\auth.json
//! ```
//! 文件形状（opencode 自己的 auth 存储）：
//! ```json
//! { "opencode": { "type": "api", "key": "sk-..." } }
//! { "opencode": { "type": "oauth", "access": "...", "refresh": "...", "expires": 0 } }
//! ```
//! 字段都认（`key` / `access` / `token` / `apiKey`），取第一个非空值；顶层键
//! 也兼容 `oc` / `zen` / `opencode-zen` / `providers.opencode`，以及整个文件
//! 就是一枚裸字符串的极简写法。
//!
//! ── 桌面版为什么可能读不到 ──────────────────────────────────
//! opencode 桌面版 2.x 把登录态写进 `%USERPROFILE%\.local\share\opencode\
//! opencode.db`（SQLite 的 `credential` / `account` / `control_account` 表），
//! 而不是 CLI 的 `auth.json`；Electron 自身还可能把会话放在加密的 Local
//! Storage / Cookies 里。插件是纯 Rust、不引 SQLite/DPAPI 依赖，因此**不解析**
//! 这类存储，只负责探测 + 给出明确指引（用桌面版自带 CLI 登录，或粘贴 Key）。
//!
//! ── 为什么不要求用户重填 ────────────────────────────────────
//! 用户机器上已经跑过 `opencode auth login`，Key 就在 auth.json 里；让用户
//! 再复制粘贴一遍既多余又容易粘错。UI 上「使用本机登录态」按钮直接把这条
//! 文件读进来完成添加；读不到才提示手工粘贴。
//!
//! ── 只代理免费模型 ──────────────────────────────────────────
//! 本插件不区分套餐：无论 Zen 还是 Go，都只放行目录里的免费模型（见
//! `models.rs`）。付费模型请求会在 `build_chat` 被拒，避免用户误以为在
//! 走免费额度。

use std::path::{Path, PathBuf};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::Value;

use super::{endpoints, identity};

/// 一条可用的账号凭证。
pub struct Account {
    /// 上游 Key（本机登录态或手工粘贴）。
    pub api_key: String,
    /// 上游通道。
    pub tier: endpoints::Tier,
    /// 有效基址（已去尾部斜杠）。
    pub base_url: String,
    /// Key 的稳定指纹（去重 / 会话加盐用）。
    pub key_id: String,
    /// `local`（本机登录态）或 `manual`（手工粘贴）。
    pub source: &'static str,
}

fn pick(record: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = record
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

/// 匿名账号的固定标识（不需要 Key，也不参与 Key 去重）。
const ANON_ID: &str = "anonymous";

/// 从账号的 `secret` 里解析出网所需的全部字段。
///
/// `apiKey` 为空即匿名免费账号：默认走官方 `inference` 车道（请求带
/// `Authorization: Bearer public`）；如果账号里存了自定义 `baseUrl`，匿名
/// 也照它走（例如本机 `zen-gate` 网关），不再强制官方基址。
pub fn from_secret(secret: &Value) -> Result<Account, String> {
    let api_key = pick(secret, &["apiKey", "accessToken", "token", "key", "api_key"]);
    if api_key.is_empty() {
        return Ok(Account {
            api_key: String::new(),
            tier: endpoints::Tier::Anonymous,
            base_url: endpoints::base_url(
                endpoints::Tier::Anonymous,
                &pick(secret, &["baseUrl", "base_url"]),
            ),
            key_id: ANON_ID.to_string(),
            source: "anonymous",
        });
    }
    let tier = endpoints::parse_tier(&pick(secret, &["tier", "channel", "plan"]));
    let base_url = endpoints::base_url(tier, &pick(secret, &["baseUrl", "base_url"]));
    let key_id = identity::fingerprint(&api_key);
    let source = match pick(secret, &["source"]).as_str() {
        "local" => "local",
        "anonymous" => "anonymous",
        _ => "manual",
    };
    Ok(Account {
        api_key,
        tier,
        base_url,
        key_id,
        source,
    })
}

/// 读取一个非空的环境变量为路径。
fn env_path(name: &str) -> Option<PathBuf> {
    let raw = std::env::var(name).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// 保序去重地追加一条候选路径。
fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|item| item == &path) {
        paths.push(path);
    }
}

/// 桌面版自带 CLI 的版本目录（`<userData>\cli\<版本>\`），新版本在前。
fn desktop_cli_dirs(data_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("cli")) else {
        return Vec::new();
    };
    let mut dirs: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| (entry.file_name().to_string_lossy().to_string(), entry.path()))
        .collect();
    // 目录名形如 `2.2.6-x64`，字符串降序即可挑出最新版本
    dirs.sort_by(|left, right| right.0.cmp(&left.0));
    dirs.into_iter().map(|(_, path)| path).collect()
}

/// 本机 auth.json 的候选路径（按优先级）。
fn auth_candidates() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = env_path("OPENCODE_AUTH_FILE") {
        paths.push(explicit);
    }
    if let Some(data) = env_path("XDG_DATA_HOME") {
        push_unique(&mut paths, data.join("opencode").join("auth.json"));
    }
    let mut homes: Vec<PathBuf> = Vec::new();
    for name in ["USERPROFILE", "HOME"] {
        if let Some(home) = env_path(name) {
            if !homes.iter().any(|item| item == &home) {
                homes.push(home);
            }
        }
    }
    for home in &homes {
        push_unique(
            &mut paths,
            home.join(".local").join("share").join("opencode").join("auth.json"),
        );
        push_unique(&mut paths, home.join(".config").join("opencode").join("auth.json"));
    }
    if let Some(appdata) = env_path("APPDATA") {
        push_unique(&mut paths, appdata.join("opencode").join("auth.json"));
        // 桌面版 Electron userData：auth.json 可能直接落在根目录，
        // 也可能被自带 CLI 写到 cli/<版本>/ 下
        let desktop = appdata.join("ai.opencode.desktop");
        push_unique(&mut paths, desktop.join("auth.json"));
        for dir in desktop_cli_dirs(&desktop) {
            push_unique(&mut paths, dir.join("auth.json"));
        }
    }
    if let Some(local) = env_path("LOCALAPPDATA") {
        push_unique(&mut paths, local.join("opencode").join("auth.json"));
        push_unique(&mut paths, local.join("ai.opencode.desktop").join("auth.json"));
        push_unique(
            &mut paths,
            local.join("Programs").join("ai.opencode.desktop").join("auth.json"),
        );
        push_unique(&mut paths, local.join("Programs").join("opencode").join("auth.json"));
    }
    paths
}

/// 从一条 OAuth / API 记录里取 token。
fn entry_key(entry: &Value, label: &str) -> Result<Option<String>, String> {
    if let Some(token) = entry.as_str() {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_string()));
        }
        return Ok(None);
    }
    let token = pick(entry, &["key", "access", "token", "apiKey", "api_key"]);
    if !token.is_empty() {
        return Ok(Some(token));
    }
    if pick(entry, &["type"]) == "oauth" {
        return Err(format!(
            "{label} 是 OAuth 登录项但缺少 access token，请重新执行 `opencode auth login`"
        ));
    }
    Ok(None)
}

/// 从 auth.json 内容里提取 opencode 的 Key。
///
/// `Ok(None)` 表示这条文件里没有 opencode 登录项（继续看下一个候选）。
fn extract_key(parsed: &Value) -> Result<Option<String>, String> {
    if let Some(raw) = parsed.as_str() {
        let token = raw.trim();
        return Ok(if token.is_empty() {
            None
        } else {
            Some(token.to_string())
        });
    }
    let direct = pick(parsed, &["apiKey", "accessToken", "token", "key", "api_key"]);
    if !direct.is_empty() {
        return Ok(Some(direct));
    }
    for name in ["opencode", "oc", "zen", "opencode-zen"] {
        if let Some(entry) = parsed.get(name) {
            if let Some(found) = entry_key(entry, name)? {
                return Ok(Some(found));
            }
        }
    }
    if let Some(provider) = parsed.get("providers").and_then(|value| value.get("opencode")) {
        if let Some(found) = entry_key(provider, "providers.opencode")? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// 桌面版线索：装了桌面版却拿不到 auth.json 时，解释原因并给出可行做法。
fn desktop_hint() -> Option<String> {
    let data_dir = env_path("APPDATA").map(|dir| dir.join("ai.opencode.desktop"));
    let app_dir = env_path("LOCALAPPDATA")
        .map(|dir| dir.join("Programs").join("ai.opencode.desktop"))
        .filter(|dir| dir.is_dir());
    let has_data = data_dir.as_ref().is_some_and(|dir| dir.is_dir());
    if !has_data && app_dir.is_none() {
        return None;
    }

    let db = [env_path("XDG_DATA_HOME"), env_path("USERPROFILE"), env_path("HOME")]
        .into_iter()
        .flatten()
        .map(|base| base.join(".local").join("share").join("opencode").join("opencode.db"))
        .find(|path| path.is_file());
    let cli = data_dir
        .as_deref()
        .and_then(|dir| desktop_cli_dirs(dir).into_iter().next())
        .map(|dir| dir.join("opencode-cli.exe"))
        .filter(|path| path.is_file());

    let mut parts: Vec<String> = Vec::new();
    if has_data {
        parts.push(format!(
            "另检测到 opencode 桌面版数据目录 {}",
            data_dir.as_ref().map(|dir| dir.display().to_string()).unwrap_or_default()
        ));
    }
    if let Some(dir) = &app_dir {
        parts.push(format!("安装目录 {}", dir.display()));
    }
    if let Some(db) = &db {
        parts.push(format!(
            "登录态在 {} 的 SQLite（credential / account 表）或 Electron 加密存储里，插件不解析",
            db.display()
        ));
    }
    match &cli {
        Some(path) => parts.push(format!(
            "可用桌面版自带 CLI 执行 `\"{}\" auth login` 生成 auth.json 后重试",
            path.display()
        )),
        None => parts.push("可用桌面版自带 CLI 执行 `opencode auth login` 生成 auth.json 后重试".to_string()),
    }
    parts.push("或在插件页手工粘贴 OpenCode API Key".to_string());
    Some(parts.join("；"))
}

/// 从本机 auth.json 里取 `opencode` 的 Key。
///
/// 返回 `(key, 实际读取的路径)`，路径只用于提示，不写进 secret。
fn read_local_key() -> Result<(String, String), String> {
    let candidates = auth_candidates();
    let mut seen_any = false;
    let mut seen_path = String::new();
    for path in &candidates {
        if !path.is_file() {
            continue;
        }
        seen_any = true;
        if seen_path.is_empty() {
            seen_path = path.display().to_string();
        }
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("读取 {} 失败：{error}", path.display()))?;
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|error| format!("解析 {} 失败：{error}", path.display()))?;
        match extract_key(&parsed)? {
            Some(token) => return Ok((token, path.display().to_string())),
            None => continue,
        }
    }
    let mut message = if seen_any {
        format!(
            "本机 {seen_path} 里没有可用的 `opencode` 登录项；请先执行 `opencode auth login`，或手工粘贴 API Key"
        )
    } else {
        format!(
            "没有找到本机 opencode 登录态（已探测 {}）；请先执行 `opencode auth login`，或手工粘贴 API Key",
            candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" / ")
        )
    };
    if let Some(hint) = desktop_hint() {
        message.push('；');
        message.push_str(&hint);
    }
    Err(message)
}

/// 环境变量里的 Key（便于容器 / CI 场景）。
fn env_key() -> String {
    for name in ["OPENCODE_API_KEY", "OPENCODE_ZEN_KEY"] {
        let value = std::env::var(name).unwrap_or_default();
        if !value.trim().is_empty() {
            return value.trim().to_string();
        }
    }
    String::new()
}

/// 依次尝试环境变量、本机 auth.json；都拿不到返回空串（→ 匿名免费车道）。
fn env_then_local() -> String {
    let env = env_key();
    if !env.is_empty() {
        return env;
    }
    read_local_key().map(|(key, _)| key).unwrap_or_default()
}

/// 公开视图里的来源文案。
fn source_label(api_key: &str, source: &str) -> &'static str {
    if api_key.is_empty() {
        "匿名免费"
    } else if source == "local" {
        "本机登录态"
    } else {
        "API Key"
    }
}

fn default_name(tier: endpoints::Tier, tail: &str) -> String {
    if tier == endpoints::Tier::Anonymous {
        return "OpenCode 匿名免费".to_string();
    }
    format!("OpenCode {} · {tail}", tier.label())
}

fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

fn identity_key(tier: endpoints::Tier, key_id: &str) -> String {
    format!("opencode:{}:{key_id}", tier.as_str())
}

fn probe_error(status: u16) -> String {
    match status {
        401 => "OpenCode Key 无效或已过期；本机登录态请重新执行 `opencode auth login`".to_string(),
        403 => "该 Key 无权访问 OpenCode 通道（可能是通道选错或套餐不包含免费模型）".to_string(),
        404 => "OpenCode 模型目录返回 404，请检查自定义基址是否正确".to_string(),
        429 => "OpenCode 拒绝了这次探活（HTTP 429），请稍后再试".to_string(),
        _ => format!("OpenCode 探活失败（HTTP {status}）"),
    }
}

/// 规范化一条账号：解析来源 → 探活 → 生成公开视图与稳定身份。
pub async fn normalize(
    ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    // 1) 取 Key 与来源标记
    let (api_key, source, payload_name, tier_raw, base_override) = match &input.source {
        proto::NormalizeSource::Manual { payload } => {
            let pasted = pick(payload, &["apiKey", "accessToken", "token", "key", "api_key"]);
            let (key, source) = if pasted.is_empty() {
                // 没填 Key：先看环境变量、再看本机 auth.json；
                // 都没有就拿空 Key 走匿名免费车道，不再阻断添加
                (env_then_local(), "local")
            } else {
                (pasted, "manual")
            };
            (
                key,
                source,
                payload
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                pick(payload, &["tier", "channel"]),
                pick(payload, &["baseUrl", "base_url"]),
            )
        }
        // 桌面端导入：优先本机登录态，没有就落到匿名免费车道
        proto::NormalizeSource::Desktop => (
            env_then_local(),
            "local",
            None,
            String::new(),
            String::new(),
        ),
        proto::NormalizeSource::FileText { content, .. } => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return Err("文件是空的，请粘贴 OpenCode API Key".to_string());
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(parsed) => (
                    pick(&parsed, &["apiKey", "accessToken", "token", "key", "api_key"]),
                    "manual",
                    None,
                    pick(&parsed, &["tier", "channel"]),
                    pick(&parsed, &["baseUrl", "base_url"]),
                ),
                Err(_) => (trimmed.to_string(), "manual", None, String::new(), String::new()),
            }
        }
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err("OpenCode 不支持二进制文件导入，请使用本机登录态或粘贴 API Key".to_string());
        }
    };

    // 空 Key = 匿名免费车道（桌面版默认形态，不需要登录）
    let tier = if api_key.is_empty() {
        endpoints::Tier::Anonymous
    } else {
        endpoints::parse_tier(&tier_raw)
    };
    let base_url = endpoints::base_url(tier, &base_override);

    // 2) 探活：只有需要 Key 的车道才校验。匿名车道按出口 IP 计免费额度，
    //    429 只代表额度用尽、不代表账号不可用，因此不在这里拦。
    if tier.needs_key() {
        let client = crate::http::client(input.proxy.as_ref(), &ctx.env.proxy);
        // 与 `endpoints::chat_url` 同一口径：基址自带 `/v1` 时不能拼成
        // `/v1/v1/models`（旧版手拼 URL 会 404）。
        let (status, _payload) = crate::http::get_json(
            &client,
            &endpoints::models_url(&base_url),
            &identity::models_headers(&api_key, &base_url),
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(probe_error(status));
        }
    }

    // 3) 落盘 + 公开视图
    let key_id = if api_key.is_empty() {
        ANON_ID.to_string()
    } else {
        identity::fingerprint(&api_key)
    };
    let tail = if api_key.is_empty() {
        String::new()
    } else {
        token_tail(&api_key)
    };
    let display = input
        .name
        .clone()
        .or(payload_name)
        .unwrap_or_else(|| default_name(tier, &tail));

    let mut secret = serde_json::json!({
        "apiKey": api_key,
        "tier": tier.as_str(),
        "keyId": key_id,
        "name": display.clone(),
        "source": source,
    });
    if !base_override.trim().is_empty() {
        secret["baseUrl"] = Value::String(base_override.trim().trim_end_matches('/').to_string());
    }

    Ok(proto::NormalizedAccount {
        secret,
        public: serde_json::json!({
            "name": display,
            "uid": key_id.clone(),
            "tokenTail": tail,
            "plan": tier.label(),
            "source": source_label(&api_key, source),
            "longLived": true,
        }),
        identity: identity_key(tier, &key_id),
        desktop: false,
    })
}

/// 从凭证重新生成公开视图。
pub fn account_view(secret: &Value) -> Result<proto::AccountView, String> {
    let account = from_secret(secret)?;
    let tail = token_tail(&account.api_key);
    let stored_name = pick(secret, &["name"]);
    let display = if stored_name.is_empty() {
        default_name(account.tier, &tail)
    } else {
        stored_name
    };
    Ok(serde_json::json!({
        "name": display,
        "uid": account.key_id,
        "tokenTail": tail,
        "plan": account.tier.label(),
        "source": source_label(&account.api_key, account.source),
        "longLived": true,
    }))
}

/// 稳定身份：同一通道下的同一把 Key 视为同一账号。
pub fn identity_of(secret: &Value) -> String {
    let api_key = pick(secret, &["apiKey", "accessToken", "token", "key", "api_key"]);
    if api_key.is_empty() {
        return identity_key(endpoints::Tier::Anonymous, ANON_ID);
    }
    let tier = endpoints::parse_tier(&pick(secret, &["tier", "channel", "plan"]));
    identity_key(tier, &identity::fingerprint(&api_key))
}
