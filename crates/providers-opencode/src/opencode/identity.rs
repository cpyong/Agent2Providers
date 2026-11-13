//! 上游身份头与会话 id 的派生。
//!
//! ── 为什么需要这一层（移植自 opencode2api 的 `identity` / `httpx`）──
//! OpenCode 的两条车道都按「客户端指纹」判请求，缺头或形状不对会被网关
//! 直接拒绝（免费车道是 403 `FreeTierError`，付费车道也会拒掉明显伪造的
//! 会话形状）。需要拼的东西：
//!
//! ```text
//!   User-Agent           v1: opencode/1.19.3 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14
//!                        v2: opencode/latest/2.1.1/cli
//!   x-opencode-client    恒 "cli"
//!   x-opencode-session   规范化会话 id（ses_ + 12 hex + 14 Base62）
//!   x-opencode-session-id 同上（子代理场景上游才区分 child）
//!   x-opencode-project   40 位 hex（git root commit 形状）
//!   x-opencode-request   v1 才发：msg_ 形状的用户消息 id
//!   x-session-affinity   v2 才发（同时发 X-Session-Id）
//! ```
//!
//! 会话 id 的编码规则照抄 opencode 客户端 `Identifier.descending()`：
//! 48 位 = `~(ms<<12 | counter)`，低 12 位因此读作 `ffc`–`fff`。上游据此
//! 判断 id 是否可信，所以不能直接拿随机 hex 冒充。
//!
//! ── 这里与 opencode2api 的一处**有意差别** ────────────────────
//! 它为了免费车道的出口配额在每个进程启动时撒盐（`bootNonce`），使重启后
//! 同一会话换新 id；本插件三条车道（含匿名免费）都用稳定派生：同一段对话
//! 跨重启落进同一个上游会话，更利于 prompt cache。出口 IP 的免费额度不由
//! 会话 id 决定，撒盐并不能绕过 429 `FreeUsageLimitError`。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::endpoints;

/// v1 车道模型请求的 User-Agent（OpenCode 1.19.3 + provider-utils 4.0.23）。
const V1_MODEL_UA: &str =
    "opencode/1.19.3 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14";

/// v1 车道目录请求的 User-Agent（不带 AI SDK 后缀）。
const V1_MODELS_UA: &str = "opencode/1.19.3";

/// v2 车道的 CLI User-Agent。
const V2_UA: &str = "opencode/latest/2.1.1/cli";

/// 匿名 `inference` 车道的 User-Agent（实测桌面版 2.2.6 自带 CLI 用这个）。
const DESKTOP_UA: &str = "opencode/latest/2.2.6/cli";

/// 匿名车道的字面量 Key。
///
/// 桌面版 CLI 在没有 `OPENCODE_API_KEY`、也没有 Console 连接时，会把
/// `provider.opencode.settings.apiKey` 写成字符串 `public`，请求因此带
/// `Authorization: Bearer public`。带上它是为了与官方客户端一致；实测
/// `429 FreeUsageLimitError` 是出口 IP 免费额度耗尽（`Retry-After` 约 14h），
/// 与这个头无关 —— 不带 Authorization 同样会拿到该 429。
const ANON_KEY: &str = "public";

/// 组装 `Authorization` 值：空 Key 用匿名字面量 `public`。
fn bearer(key: &str) -> String {
    let key = key.trim();
    if key.is_empty() {
        format!("Bearer {ANON_KEY}")
    } else {
        format!("Bearer {key}")
    }
}

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// 一次请求要带的三枚派生 id。
pub struct RequestIds {
    /// 会话亲和键（`x-opencode-session`）。
    pub session: String,
    /// 项目 id（40 位 hex）。
    pub project: String,
    /// 用户消息 id（`msg_...`，仅 v1 车道发送）。
    pub message: String,
}

/// 派生一次请求的身份头所需的 id。
///
/// 会话信号优先级与 opencode2api 的 `DeriveRequestIDs` 一致：客户端自己带的
/// 会话头 → 对话开头指纹（system / instructions + 第一条 user）→ 整个 body
/// 的哈希。`scope` 是账号指纹，避免两个账号的相同开头撞进同一个上游会话。
pub fn request_ids(scope: &str, headers: &BTreeMap<String, String>, body: &Value) -> RequestIds {
    let signal = first_header(
        headers,
        &[
            "x-opencode-session",
            "x-opencode-session-id",
            "x-session-affinity",
            "x-session-id",
        ],
    );
    let signal = if signal.is_empty() {
        conversation_seed(body)
    } else {
        signal
    };
    let signal = if signal.is_empty() {
        let encoded = serde_json::to_vec(body).unwrap_or_default();
        hex(&digest(&[&encoded]))
    } else {
        signal
    };

    let session = canonical_session(scope, &signal);
    let project = project_id(scope, &session);
    let message = message_id(&session, &user_turn(body));
    RequestIds { session, project, message }
}

/// 推理请求头（`Authorization` 之外的鉴权与指纹）。
///
/// `key` 为空表示匿名免费车道：此时发 `Authorization: Bearer public`，
/// 与桌面版 CLI 的匿名字面量 Key 一致（桌面端并非「不鉴权」）。
pub fn inference_headers(key: &str, base: &str, ids: &RequestIds) -> Vec<(String, String)> {
    let mut headers = vec![
        ("x-opencode-client".to_string(), "cli".to_string()),
        ("Authorization".to_string(), bearer(key)),
    ];
    headers.push((
        "x-opencode-project".to_string(),
        ids.project.clone(),
    ));
    if endpoints::is_inference(base) {
        headers.push(("User-Agent".to_string(), DESKTOP_UA.to_string()));
        headers.push(("x-opencode-session".to_string(), ids.session.clone()));
        headers.push((
            "x-opencode-session-id".to_string(),
            ids.session.clone(),
        ));
        headers.push(("x-session-affinity".to_string(), ids.session.clone()));
        headers.push(("X-Session-Id".to_string(), ids.session.clone()));
        return headers;
    }
    if endpoints::is_v1(base) {
        headers.push(("Accept".to_string(), "*/*".to_string()));
        headers.push(("User-Agent".to_string(), V1_MODEL_UA.to_string()));
        headers.push(("x-opencode-session".to_string(), ids.session.clone()));
        headers.push((
            "x-opencode-session-id".to_string(),
            ids.session.clone(),
        ));
        headers.push(("x-opencode-request".to_string(), ids.message.clone()));
    } else {
        headers.push(("User-Agent".to_string(), V2_UA.to_string()));
        headers.push(("x-opencode-session".to_string(), ids.session.clone()));
        headers.push((
            "x-opencode-session-id".to_string(),
            ids.session.clone(),
        ));
        // v2 车道用这三个头表达「同一会话」；**不能**发 x-opencode-request，
        // 上游会以 403 FreeTierError 拒绝。
        headers.push(("x-session-affinity".to_string(), ids.session.clone()));
        headers.push(("X-Session-Id".to_string(), ids.session.clone()));
    }
    headers
}

/// 目录 / 探活请求头（UA 按车道选，不带会话头）。
pub fn models_headers(key: &str, base: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".to_string(), bearer(key)),
        ("x-opencode-client".to_string(), "cli".to_string()),
        (
            "User-Agent".to_string(),
            if endpoints::is_v1(base) {
                V1_MODELS_UA.to_string()
            } else {
                V2_UA.to_string()
            },
        ),
    ]
}

/// 一段字符串的短指纹（账号去重 / 会话加盐用）。
pub fn fingerprint(value: &str) -> String {
    hex(&digest(&[value.as_bytes()])[..8])
}

fn first_header(headers: &BTreeMap<String, String>, keys: &[&str]) -> String {
    for key in keys {
        let value = headers
            .get(*key)
            .map(|value| value.trim())
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

/// 对话开头指纹：system / instructions + 第一条 user 内容。
fn conversation_seed(body: &Value) -> String {
    let mut prelude: Vec<Value> = Vec::new();
    for field in ["system", "instructions"] {
        if let Some(value) = body.get(field) {
            if !value.is_null() {
                prelude.push(value.clone());
            }
        }
    }
    let Some(first) = first_user_turn(body, &mut prelude) else {
        return String::new();
    };
    prelude.push(first);
    serde_json::to_string(&prelude).unwrap_or_default()
}

fn first_user_turn(body: &Value, prelude: &mut Vec<Value>) -> Option<Value> {
    if let Some(input) = body.get("input").and_then(Value::as_str) {
        if !input.is_empty() {
            return Some(Value::String(input.to_string()));
        }
    }
    for field in ["messages", "input"] {
        let Some(items) = body.get(field).and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("");
            match role {
                "system" | "developer" => {
                    if let Some(content) = item.get("content") {
                        prelude.push(content.clone());
                    }
                }
                "user" => {
                    if let Some(content) = item.get("content") {
                        return Some(content.clone());
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// 规范化会话 id：官方形状原样保留，其余重编码成 `ses_` + 12 hex + 14 Base62。
fn canonical_session(scope: &str, signal: &str) -> String {
    if is_official_session(signal) {
        return signal.to_string();
    }
    let sum = digest(&[b"ses\x00", scope.as_bytes(), &[0], signal.as_bytes()]);

    // 时间部分：当天 UTC 零点往前推 0–30 天（与 opencode2api 同一窗口），
    // 再按 descending 布局取反。哈希只决定窗口内的偏移，绝不直接当时间戳。
    let window_ms: u64 = 30 * 24 * 60 * 60 * 1000;
    let offset = u32::from_be_bytes([sum[0], sum[1], sum[2], sum[3]]) as u64 % window_ms;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let midnight = now_ms / 86_400_000 * 86_400_000;
    let base = midnight.saturating_sub(offset);
    let current = (base << 12) | 1;
    let inverted = (!current) & 0xFF_FFFF_FFFF_FF;
    let time_part = format!("{inverted:012x}");

    let value = sum[6..16]
        .iter()
        .fold(0u128, |acc, byte| (acc << 8) | u128::from(*byte));
    format!("ses_{time_part}{}", base62_fixed(value, 14))
}

/// 官方会话形状判断：`ses_` + 12 位小写 hex（低 12 位落在 ffc–fff）+ 14 位
/// Base62，总长 **30**。
///
/// 旧版把长度写成 18，导致所有真实的 opencode 会话 id（例如
/// `ses_e3b7e1c73ffeOD2KkReRPNYJM7`）都被判为「非官方」再哈希一遍。
fn is_official_session(signal: &str) -> bool {
    // 全程按字节判定：会话信号可能来自任意客户端头，按 `str` 切分会落在
    // 多字节字符中间。
    let bytes = signal.as_bytes();
    if bytes.len() != 30 || !signal.starts_with("ses_") {
        return false;
    }
    let time = &bytes[4..16];
    let random = &bytes[16..30];
    let hex_char = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
    let time_ok = time[..9].iter().all(|byte| hex_char(*byte))
        && time[9] == b'f'
        && (b'c'..=b'f').contains(&time[10])
        && hex_char(time[11]);
    time_ok && random.iter().all(|byte| byte.is_ascii_alphanumeric())
}

/// 40 位 hex 的项目 id（git root commit 形状），按账号 + 会话稳定派生。
fn project_id(scope: &str, session: &str) -> String {
    let sum = digest(&[b"prj\x00", scope.as_bytes(), session.as_bytes()]);
    hex(&sum[..20])
}

/// 用户消息 id（`msg_` + 12 hex + 14 Base62），同一会话的同一轮稳定复用。
fn message_id(session: &str, turn: &str) -> String {
    let key = if turn.is_empty() {
        format!("{session}\x00#latest")
    } else {
        format!("{session}\x00{turn}")
    };
    let now = crate::http::now_ms();
    if let Ok(mut guard) = messages().lock() {
        if let Some(entry) = guard.get(&key) {
            // 6 小时内同一轮复用同一枚 id（工具循环的多次上游调用共享它）
            if now - entry.seen < 6 * 60 * 60 * 1000 {
                return entry.id.clone();
            }
        }
        let id = ascending_message_id(session, turn);
        if guard.len() > 4096 {
            guard.clear();
        }
        guard.insert(
            key,
            MessageEntry {
                id: id.clone(),
                seen: now,
            },
        );
        return id;
    }
    ascending_message_id(session, turn)
}

struct MessageEntry {
    id: String,
    seen: i64,
}

fn messages() -> &'static Mutex<HashMap<String, MessageEntry>> {
    static MESSAGES: OnceLock<Mutex<HashMap<String, MessageEntry>>> = OnceLock::new();
    MESSAGES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 全局递增计数（同一毫秒内多次调用也不撞 id）。
fn counter() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed) % 4095 + 1
}

fn ascending_message_id(session: &str, turn: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let value = (now << 12) | counter();
    let time_part = format!("{:012x}", value & 0xFF_FFFF_FFFF_FF);
    let seed = format!("{session}\x00{turn}\x00{value}");
    let sum = digest(&[seed.as_bytes()]);
    let random = sum[..10]
        .iter()
        .fold(0u128, |acc, byte| (acc << 8) | u128::from(*byte));
    format!("msg_{time_part}{}", base62_fixed(random, 14))
}

/// 用户轮次指纹：user 消息条数 + 最后一条 user 内容的哈希。
fn user_turn(body: &Value) -> String {
    if let Some(input) = body.get("input").and_then(Value::as_str) {
        if !input.is_empty() {
            return format!("1:{}", short_hash(&Value::String(input.to_string())));
        }
    }
    let mut count = 0usize;
    let mut last: Option<&Value> = None;
    for field in ["messages", "input"] {
        let Some(items) = body.get(field).and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            if item.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            if carries_tool_result(item.get("content")) {
                continue;
            }
            count += 1;
            last = item.get("content");
        }
    }
    match last {
        Some(content) if count > 0 => format!("{count}:{}", short_hash(content)),
        _ => String::new(),
    }
}

/// Anthropic 线上工具结果走 user 消息，这类消息属于工具循环而非新一轮。
fn carries_tool_result(content: Option<&Value>) -> bool {
    let Some(blocks) = content.and_then(Value::as_array) else {
        return false;
    };
    blocks.iter().any(|block| {
        block.get("type").and_then(Value::as_str) == Some("tool_result")
    })
}

fn short_hash(value: &Value) -> String {
    let encoded = serde_json::to_vec(value).unwrap_or_default();
    hex(&digest(&[&encoded])[..8])
}

fn base62_fixed(mut value: u128, width: usize) -> String {
    let mut out = vec![b'0'; width];
    for index in (0..width).rev() {
        out[index] = BASE62[(value % 62) as usize];
        value /= 62;
    }
    String::from_utf8(out).unwrap_or_default()
}

fn digest(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::is_official_session;

    #[test]
    fn recognizes_real_session_ids() {
        // 本机 opencode v2 `/api/session` 里的真实 id：ses_ + 12 hex + 14 Base62
        assert!(is_official_session("ses_e3b7e1c73ffeOD2KkReRPNYJM7"));
        assert!(is_official_session("ses_e3b2f4c93ffeGfHACJ6kn23MQy"));
    }

    #[test]
    fn rejects_other_shapes() {
        assert!(!is_official_session("ses_e3b7e1c73ffe"));
        assert!(!is_official_session("hello"));
        assert!(!is_official_session("ses_ZZZZZZZZZZZZZZZZZZZZZZZZZZ"));
    }
}
