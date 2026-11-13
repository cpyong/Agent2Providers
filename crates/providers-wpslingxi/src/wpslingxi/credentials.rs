//! WPS 灵犀凭证：一份 `wps_sid` Cookie，**没有刷新接口**。
//!
//! ── 与 Today AI / Trae 的关键差别 ───────────────────────────
//! 上游登录走 WPS 账号 SSO，没有公开的设备码 / 换票协议，程序侧换不出新票。
//! 所以 `refresh = false`：Cookie 失效只能**重新粘贴**，唯一可靠的判据是
//! 上游 401，不是本地按天数猜的过期时间（猜错会把还能用的号摘掉）。
//!
//! ── 输入形态要宽松 ─────────────────────────────────────────
//! 用户从 DevTools 复制出来的可能是：
//!   · 裸 sid                       `2f8c…`
//!   · 单键                         `wps_sid=2f8c…`
//!   · 整段 Cookie                  `Cookie: wps_sid=…; uid=…; …`
//!   · 带引号的粘贴                 `"Cookie: wps_sid=…"`
//!   · 一份 JSON                   `{"cookie":"wps_sid=…; uid=…"}`
//! 一律归一成 [`Credential`]。只保留 `wps_sid` / `uid` /
//! `copilot_server_branch` 三段：其余 Cookie 与业务无关，留着只会外泄。
//!
//! ── 身份 ───────────────────────────────────────────────────
//! `uid` 优先；缺失时用 sid 的 sha256 前 16 位。**绝不落 sid 原文**：这个值
//! 会进运行缓存键与日志，露出来等于泄露登录态。sid 重新登录后会变，因此
//! 缺 uid 的身份只保证进程内稳定，不承诺跨重登合并。

use std::collections::BTreeMap;
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, StoreAccount};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{client, ID};
use crate::http;

pub const SID_KEY: &str = "wps_sid";
pub const UID_KEY: &str = "uid";
pub const BRANCH_KEY: &str = "copilot_server_branch";

/// 归一后的凭证。
#[derive(Clone, Debug)]
pub struct Credential {
    pub wps_sid: String,
    pub uid: String,
    pub branch: String,
}

impl Credential {
    /// `Cookie:` 头的值（顺序固定，便于比对与排障）。
    pub fn cookie_header(&self) -> String {
        let mut parts = vec![format!("{SID_KEY}={}", self.wps_sid)];
        if !self.uid.is_empty() {
            parts.push(format!("{UID_KEY}={}", self.uid));
        }
        if !self.branch.is_empty() {
            parts.push(format!("{BRANCH_KEY}={}", self.branch));
        }
        parts.join("; ")
    }

    /// 稳定身份（账号去重 + run 缓存键）。
    pub fn identity(&self) -> String {
        if !self.uid.is_empty() {
            return format!("{ID}-{}", self.uid);
        }
        format!("{ID}-sid-{}", fingerprint(&self.wps_sid))
    }

    pub fn is_empty(&self) -> bool {
        self.wps_sid.trim().is_empty()
    }
}

/// sid 的短指纹（16 位十六进制），不落原文。
fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 去掉首尾引号与 `Cookie:` 前缀。
///
/// 顺序必须是「先去引号 → 再去前缀 → 再去引号」：常见粘贴形态是整段被引号
/// 包住的 `"Cookie: wps_sid=…"`，先判前缀会漏掉它。
fn strip_quotes(text: &str) -> &str {
    text.trim_matches(|ch| ch == '"' || ch == '\'').trim()
}

fn strip_prefix(text: &str) -> String {
    let value = strip_quotes(text).to_string();
    if value.to_ascii_lowercase().starts_with("cookie:") {
        return strip_quotes(&value["cookie:".len()..]).to_string();
    }
    value
}

/// 整段 Cookie / 裸 sid → 只保留三个业务键的映射。
pub fn parse_cookie_input(raw: &str) -> BTreeMap<String, String> {
    let text = strip_prefix(raw);
    let mut values = BTreeMap::new();
    if text.is_empty() {
        return values;
    }
    if !text.contains('=') {
        values.insert(SID_KEY.to_string(), text);
        return values;
    }
    for segment in text.split(';') {
        let Some((name, value)) = segment.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if !value.is_empty()
            && (name == SID_KEY || name == UID_KEY || name == BRANCH_KEY)
        {
            values.insert(name.to_string(), value.to_string());
        }
    }
    values
}

/// 从取值链里挑第一个非空字符串（凭证可能被手工编辑过，宽容读取）。
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

/// 从 `secret` 解析凭证（唯一入口）。
pub fn from_secret(secret: &Value) -> Result<Credential, String> {
    let source = pick(secret, &[SID_KEY, "cookie", "raw", "wpsSid"]);
    let pairs = parse_cookie_input(&source);
    let wps_sid = pairs.get(SID_KEY).cloned().unwrap_or_default();
    let explicit_uid = pick(secret, &[UID_KEY, "userId", "userid"]);
    let explicit_branch = pick(secret, &[BRANCH_KEY, "branch"]);
    let credential = Credential {
        wps_sid,
        uid: if explicit_uid.is_empty() {
            pairs.get(UID_KEY).cloned().unwrap_or_default()
        } else {
            explicit_uid
        },
        branch: if explicit_branch.is_empty() {
            pairs.get(BRANCH_KEY).cloned().unwrap_or_default()
        } else {
            explicit_branch
        },
    };
    if credential.is_empty() {
        return Err(
            "WPS 灵犀账号缺少 wps_sid Cookie，请重新粘贴（从 lingxi.kdocs.cn 的 DevTools → Network → tasks 请求复制）"
                .to_string(),
        );
    }
    if credential.wps_sid.chars().count() > 8192 {
        return Err("Cookie 过长（超过 8192 字符，多半是粘错了内容）".to_string());
    }
    Ok(credential)
}

/// 读一条账号的凭证（`account` 空串 = 本家队首账号）。
pub async fn snapshot(ctx: &PluginCtx, account: &str) -> Result<Credential, String> {
    let found = ctx
        .store_get(ID, account)
        .await
        .map_err(|error| format!("读取 WPS 灵犀账号失败：{error}"))?;
    from_secret(&found.secret)
}

/// 读一条账号，同时带出网关解析好的账号级代理。
///
/// `usage` / `checkin` 这两条控制面链路拿不到 `ConversationRequest.proxy`，
/// 只能从账号记录里取；漏了它会让「账号配了代理」在签到时静默失效。
pub async fn load(
    ctx: &PluginCtx,
    account: &str,
) -> Result<(Credential, Option<proto::WireProxy>), String> {
    let found = ctx
        .store_get(ID, account)
        .await
        .map_err(|error| format!("读取 WPS 灵犀账号失败：{error}"))?;
    let credential = from_secret(&found.secret)?;
    Ok((credential, proxy_of(&found)))
}

fn proxy_of(found: &StoreAccount) -> Option<proto::WireProxy> {
    use agent2api_provider_sdk::StoreProxy;
    match found.proxy.as_ref() {
        Some(StoreProxy::Resolved { proxy }) => Some(proxy.clone()),
        Some(StoreProxy::Static { config }) => {
            serde_json::from_value(config.clone()).ok()
        }
        _ => None,
    }
}

/// 令牌尾 4 字符（公开视图里的展示字段；sid 只露尾号）。
fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

/// 默认展示名：uid → sid 尾号。
fn default_name(uid: &str, sid_tail: &str) -> String {
    if !uid.is_empty() {
        format!("WPS 灵犀 {uid}")
    } else if !sid_tail.is_empty() {
        format!("WPS 灵犀 ····{sid_tail}")
    } else {
        "WPS 灵犀账号".to_string()
    }
}

/// 规范化一条账号（粘贴 Cookie / 文本文件）。
///
/// ── 为什么做一次「尽力而为」的资料探测 ──────────────────────
/// `uid` 是稳定去重身份，而用户常常只粘到 `wps_sid` 一段。加账号时打一次
/// `GET /api/public/v1/account/profile`（每域 8 秒超时）补出 uid 与昵称；失败
/// **不拦**：账号照常落库，身份退化成 sid 指纹。这样「添加账号」在断网时
/// 仍可用，联网时能拿到更稳的身份。
pub async fn normalize(
    ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let (source_text, payload_uid, payload_name) = match &input.source {
        proto::NormalizeSource::Manual { payload } => (
            pick(
                payload,
                &["cookie", SID_KEY, "raw", "wpsSid", "token"],
            ),
            pick(payload, &[UID_KEY, "userId", "userid"]),
            payload
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        ),
        proto::NormalizeSource::FileText { content, .. } => {
            let text = content.trim();
            match serde_json::from_str::<Value>(text) {
                Ok(parsed) if parsed.is_object() => (
                    pick(&parsed, &["cookie", SID_KEY, "raw", "wpsSid"]),
                    pick(&parsed, &[UID_KEY, "userId", "userid"]),
                    None,
                ),
                _ => (text.to_string(), String::new(), None),
            }
        }
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err(
                "WPS 灵犀不支持二进制文件导入，请粘贴包含 wps_sid 的 Cookie 文本".to_string(),
            )
        }
        proto::NormalizeSource::Desktop => {
            return Err(
                "WPS 灵犀不支持导入桌面端登录态（Cookie 没有稳定可读的落盘位置），请从浏览器 DevTools 复制"
                    .to_string(),
            )
        }
    };

    let pairs = parse_cookie_input(&source_text);
    let sid = pairs.get(SID_KEY).cloned().unwrap_or_default();
    if sid.is_empty() {
        return Err("没有找到 wps_sid：请粘贴整段 Cookie 或单串 wps_sid=…".to_string());
    }
    let uid = if payload_uid.is_empty() {
        pairs.get(UID_KEY).cloned().unwrap_or_default()
    } else {
        payload_uid
    };
    let branch = pairs.get(BRANCH_KEY).cloned().unwrap_or_default();
    let credential = Credential {
        wps_sid: sid.clone(),
        uid: uid.clone(),
        branch: branch.clone(),
    };

    // 尽力而为的资料探测：只为补 uid / 昵称，失败不影响落库。
    let proxy = input.proxy.clone();
    let http_client = http::client(proxy.as_ref(), &ctx.env.proxy);
    let mut resolved_uid = uid.clone();
    let mut profile_name = String::new();
    if resolved_uid.is_empty() {
        if let Ok(profile) = client::profile(&http_client, &credential, Duration::from_secs(8)).await {
            let id = pick(&profile, &["id", "uid", "userId", "user_id"]);
            let name = pick(&profile, &["name", "nickname", "nickName"]);
            if !id.is_empty() {
                resolved_uid = id;
            }
            profile_name = name;
        }
    }

    let tail = token_tail(&sid);
    let display = input
        .name
        .clone()
        .or(payload_name)
        .or_else(|| {
            if profile_name.is_empty() {
                None
            } else {
                Some(format!("WPS 灵犀 {profile_name}"))
            }
        })
        .unwrap_or_else(|| default_name(&resolved_uid, &tail));
    let identity = identity_of(&json!({ "uid": &resolved_uid, "wps_sid": &sid }));
    let mut secret = json!({
        "wps_sid": sid,
        "uid": resolved_uid,
        "addedAt": http::now_ms(),
    });
    if !branch.is_empty() {
        if let Some(object) = secret.as_object_mut() {
            object.insert("copilot_server_branch".to_string(), Value::String(branch));
        }
    }
    Ok(proto::NormalizedAccount {
        secret,
        public: json!({
            "name": display,
            "uid": resolved_uid,
            "tokenTail": tail,
        }),
        identity,
        desktop: false,
    })
}

/// 账号的稳定身份（去重用）。
pub fn identity_of(secret: &Value) -> String {
    let uid = pick(secret, &[UID_KEY, "userId", "userid"]);
    if !uid.is_empty() {
        return format!("{ID}-{uid}");
    }
    let source = pick(secret, &[SID_KEY, "cookie", "raw", "wpsSid"]);
    let sid = parse_cookie_input(&source)
        .get(SID_KEY)
        .cloned()
        .unwrap_or_default();
    format!("{ID}-sid-{}", fingerprint(&sid))
}

/// 账号的公开视图（凭证更新后刷新界面用）。
pub fn account_view(secret: &Value) -> Result<Value, String> {
    let credential = from_secret(secret)?;
    let uid = pick(secret, &[UID_KEY, "userId", "userid"]);
    let tail = token_tail(&credential.wps_sid);
    Ok(json!({
        "name": default_name(&uid, &tail),
        "uid": uid,
        "tokenTail": tail,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_sid_is_a_credential() {
        let pairs = parse_cookie_input("2f8cabc");
        assert_eq!(pairs.get(SID_KEY).map(String::as_str), Some("2f8cabc"));
    }

    #[test]
    fn whole_cookie_with_quotes_and_prefix_keeps_only_business_keys() {
        let raw = "\"Cookie: wps_sid=sid-123; uid=42; copilot_server_branch=cn; csrf=drop-me\"";
        let pairs = parse_cookie_input(raw);
        assert_eq!(pairs.get(SID_KEY).map(String::as_str), Some("sid-123"));
        assert_eq!(pairs.get(UID_KEY).map(String::as_str), Some("42"));
        assert_eq!(
            pairs.get(BRANCH_KEY).map(String::as_str),
            Some("cn")
        );
        assert_eq!(pairs.len(), 3);
        assert!(!pairs.contains_key("csrf"));
    }

    #[test]
    fn json_cookie_and_missing_sid() {
        let secret = json!({ "cookie": "wps_sid=abc; uid=7", "addedAt": 1 });
        let credential = from_secret(&secret).unwrap();
        assert_eq!(credential.cookie_header(), "wps_sid=abc; uid=7");
        assert_eq!(credential.identity(), "wpslingxi-7");
        assert!(from_secret(&json!({ "uid": "7" })).is_err());
    }

    #[test]
    fn identity_never_leaks_the_sid() {
        let secret = json!({ "wps_sid": "super-secret-cookie-value" });
        let identity = identity_of(&secret);
        assert!(identity.starts_with("wpslingxi-sid-"));
        assert!(!identity.contains("super-secret"));
        assert_eq!(identity, identity_of(&secret));
        let view = account_view(&secret).unwrap();
        assert_eq!(view["tokenTail"], "alue");
        assert!(view.get("wps_sid").is_none());
    }
}
