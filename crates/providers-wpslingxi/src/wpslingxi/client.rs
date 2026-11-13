//! 上游请求构造：请求头、双域回落、三步 run、错误文案。
//!
//! ── 为什么所有请求都走 `call` ───────────────────────────────
//! `lingxi.wps.cn` 与 `lingxi.kdocs.cn` 是两套账号集群，同一份 `wps_sid`
//! 只在一边被认。如果只打主域，kdocs 账号的失败形态是 401/404 —— 看起来
//! 像「Cookie 过期」，实际是域不对。`call` 固定按序试两个域，并把试过的域
//! 写进错误信息，避免用户白贴一次 Cookie。
//!
//! ── 一次对话为什么是三步 ───────────────────────────────────
//! 上游**按 run 计费**：一个 run 内多次调用只扣一次智点。所以建 session →
//! 领 run 的结果要按「账号 × 模型」缓存一小时（不滑动续期），并发时还要
//! 单飞 —— 各建一个 run 就是各扣一次。缓存与单飞在 `chat.rs`，这里只负责
//! 「怎么建」。

use std::time::Duration;

use serde_json::{json, Value};

use super::{credentials::Credential, endpoints};
use crate::http;

/// 所有域都判定「登录态失效」时错误带这个前缀，供调用方给出「重新粘贴
/// Cookie」的提示，而不是让用户以为网络坏了。
pub const AUTH_ERROR_PREFIX: &str = "AUTH:";

/// 账号面 / 对话面的公共请求头（桌面客户端伪装）。
///
/// `X-Request-Id` 每次新建；设备号是**固定值**（见 `endpoints::DEVICE_ID`）。
pub fn headers(credential: &Credential, accept: &str) -> Vec<(String, String)> {
    vec![
        ("Cookie".to_string(), credential.cookie_header()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), accept.to_string()),
        ("X-Client-Type".to_string(), endpoints::CLIENT_TYPE.to_string()),
        ("X-Client-Lang".to_string(), endpoints::CLIENT_LANG.to_string()),
        (
            "X-Cc-Agent-Type".to_string(),
            endpoints::CLIENT_TYPE.to_string(),
        ),
        ("X-Request-Id".to_string(), http::unique_id()),
        ("x-kso-app-name".to_string(), endpoints::APP_NAME.to_string()),
        (
            "x-kso-app-version".to_string(),
            endpoints::APP_VERSION.to_string(),
        ),
        (
            "x-kso-device-id".to_string(),
            endpoints::DEVICE_ID.to_string(),
        ),
    ]
}

/// 一次带双域回落的账号面请求，返回「哪个域成功了」与完整响应体。
///
/// 注意返回的是**完整信封**（含 `data` / `result`），不是 `data`：签到领取
/// 的 `result` 在顶层，任务与余额的列表在 `data` 里，调用方各自取。
pub async fn call(
    client: &reqwest::Client,
    credential: &Credential,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<(String, Value), String> {
    let bases = endpoints::bases();
    let mut last = String::new();
    let mut auth_failures = 0usize;
    for base in &bases {
        let url = format!("{base}{path}");
        let request_headers = headers(credential, "application/json");
        let result = http::request_json(
            client,
            method.clone(),
            &url,
            &request_headers,
            body,
            timeout,
        )
        .await;
        match result {
            Ok((status, payload)) if (200..300).contains(&status) => {
                return Ok((base.clone(), payload));
            }
            Ok((status, payload)) => {
                if status == 401 || status == 403 {
                    auth_failures += 1;
                }
                last = describe_error(status, &payload, &format!("{method} {path}"));
            }
            Err(error) => last = error,
        }
    }
    if auth_failures == bases.len() {
        return Err(format!(
            "{AUTH_ERROR_PREFIX}WPS 灵犀登录态已失效（试过的域都返回 401/403），请重新粘贴 wps_sid Cookie"
        ));
    }
    Err(format!("{last}（已试：{}）", bases.join(" / ")))
}

/// 账号资料：`data.id` / `data.name`（补 uid 与昵称，失败可忽略）。
pub async fn profile(
    client: &reqwest::Client,
    credential: &Credential,
    timeout: Duration,
) -> Result<Value, String> {
    let (_, payload) = call(
        client,
        credential,
        reqwest::Method::GET,
        endpoints::EP_PROFILE,
        None,
        timeout,
    )
    .await?;
    Ok(payload
        .get("data")
        .cloned()
        .filter(|value| value.is_object())
        .unwrap_or_else(|| json!({})))
}

/// 智点余额信封里的 `data`：`total_balance` / `bonus_credits[]` /
/// `purchased_credits[]` / `bonus_summary.total_value`。
pub async fn balance(
    client: &reqwest::Client,
    credential: &Credential,
) -> Result<Value, String> {
    let (_, payload) = call(
        client,
        credential,
        reqwest::Method::GET,
        endpoints::EP_BALANCE,
        None,
        http::CONTROL_TIMEOUT,
    )
    .await?;
    Ok(payload
        .get("data")
        .cloned()
        .filter(|value| value.is_object())
        .unwrap_or_else(|| json!({})))
}

/// 任务中心列表：`data.tasks[]`。
pub async fn tasks(
    client: &reqwest::Client,
    credential: &Credential,
) -> Result<Vec<Value>, String> {
    let (_, payload) = call(
        client,
        credential,
        reqwest::Method::GET,
        endpoints::EP_TASKS,
        None,
        http::CONTROL_TIMEOUT,
    )
    .await?;
    Ok(payload
        .get("data")
        .and_then(|data| data.get("tasks"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// 领取任务（`date` 为空时不带查询串）。请求体恒 `{}`。
pub async fn claim(
    client: &reqwest::Client,
    credential: &Credential,
    date: &str,
) -> Result<Value, String> {
    let path = if date.is_empty() {
        endpoints::EP_CLAIM.to_string()
    } else {
        format!("{}?date={}", endpoints::EP_CLAIM, encode_date(date))
    };
    let empty = json!({});
    let (_, payload) = call(
        client,
        credential,
        reqwest::Method::POST,
        &path,
        Some(&empty),
        http::CONTROL_TIMEOUT,
    )
    .await?;
    Ok(payload)
}

/// 签到窗口日期只允许数字与连字符，避免把用户数据拼进查询串。
fn encode_date(date: &str) -> String {
    date.chars()
        .filter(|ch| ch.is_ascii_digit() || *ch == '-')
        .collect()
}

/// 一次计费 run（按账号 × 模型缓存，见 `chat.rs`）。
#[derive(Clone, Debug)]
pub struct Run {
    pub base: String,
    pub session_id: String,
    pub run_id: String,
    pub group_id: String,
}

/// 第一步：`POST /api/aioffice/v1/cowork/sessions`。
pub async fn create_session(
    client: &reqwest::Client,
    credential: &Credential,
    base: &str,
) -> Result<String, String> {
    let body = json!({
        "session_type": "cowork_client",
        "agent_version": 2,
        "env": {
            "device_id": endpoints::DEVICE_ID,
            "device_name": endpoints::DEVICE_NAME,
            "work_space_dir": endpoints::WORK_SPACE_DIR,
        },
    });
    let url = format!("{base}{}", endpoints::EP_SESSIONS);
    let (status, payload) = http::post_json(
        client,
        &url,
        &headers(credential, "application/json"),
        &body,
        http::CONTROL_TIMEOUT,
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(describe_error(status, &payload, "创建灵犀 cowork 会话"));
    }
    payload
        .get("data")
        .and_then(|data| data.get("session_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "创建灵犀会话失败：上游没有返回 data.session_id".to_string())
}

/// 第二步：`POST /api/public/v1/credits/runs`。
///
/// `agent_input.text` 只取最后一条 user 文本的前 60 字 —— 上游用它给 run
/// 起标题，不是正文；正文在第三条的 `messages` 里。
pub async fn create_run(
    client: &reqwest::Client,
    credential: &Credential,
    base: &str,
    session_id: &str,
    model: &str,
    title: &str,
) -> Result<Run, String> {
    let body = json!({
        "client_request_id": http::unique_id(),
        "session_id": session_id,
        "model_provider": "lingxi",
        "model_id": model,
        "agent_input": {
            "text": title.chars().take(60).collect::<String>(),
            "files": [],
            "file_ids": [],
            "expect_model": model,
            "refs": [],
            "permission_mode": "default",
            "current_mode": "agent",
            "browser_plugin_enabled": false,
        },
    });
    let url = format!("{base}{}", endpoints::EP_RUNS);
    let (status, payload) = http::post_json(
        client,
        &url,
        &headers(credential, "application/json"),
        &body,
        http::CONTROL_TIMEOUT,
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(describe_error(status, &payload, "领取灵犀 credits run"));
    }
    let data = payload.get("data");
    let run_id = data
        .and_then(|data| data.get("run_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "领取灵犀 run 失败：上游没有返回 data.run_id".to_string())?;
    let group_id = data
        .and_then(|data| data.get("group_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| run_id.clone());
    agent2api_provider_sdk::log_debug!(
        "[WPS 灵犀] run 就绪：session={session_id} run={run_id} group={group_id}"
    );
    Ok(Run {
        base: base.to_string(),
        session_id: session_id.to_string(),
        run_id,
        group_id,
    })
}

/// 第三步：打开 `agent-runtime/chat/completions` 的私有 SSE 流。
pub async fn open_chat(
    client: &reqwest::Client,
    credential: &Credential,
    run: &Run,
    body: &Value,
) -> Result<http::Opened, String> {
    let mut request_headers = headers(credential, "text/event-stream");
    request_headers.push(("x-cc-session-id".to_string(), run.session_id.clone()));
    request_headers.push(("x-cc-run-id".to_string(), run.run_id.clone()));
    let url = format!("{}{}", run.base, endpoints::EP_CHAT);
    http::open_stream(client, &url, &request_headers, body, http::CHAT_TIMEOUT).await
}

/// 上游错误体里挖一条人类可读的话（顺序照抄公开实现）。
pub fn text_of(payload: &Value) -> String {
    for key in ["hint", "msg", "message", "detail", "error_message"] {
        if let Some(text) = payload.get(key).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.chars().take(300).collect();
            }
        }
    }
    if let Some(data) = payload.get("data").filter(|value| value.is_object()) {
        let inner = text_of(data);
        if !inner.is_empty() {
            return inner;
        }
    }
    String::new()
}

/// 一次失败响应 → 人话。保留状态码与上游原话，不猜业务码含义。
pub fn describe_error(status: u16, payload: &Value, action: &str) -> String {
    let detail = text_of(payload);
    if detail.is_empty() {
        format!("{action}失败：上游返回 {status}（没有给出文案）")
    } else {
        format!("{action}失败：上游返回 {status}：{detail}")
    }
}

/// run / 会话失效：丢弃缓存重建一次即可，不是账号故障。
pub fn is_session_expired(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    ["sqlnorows", "会话不存在", "session not found", "数据不存在", "run 不存在"]
        .iter()
        .any(|pattern| lowered.contains(&pattern.to_ascii_lowercase()))
}

/// 智点耗尽（与 HTTP 402 同义，出现在 HTTP 200 的帧里）。
pub fn is_credit_exhausted(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    [
        "insufficient credits",
        "credit is not enough",
        "积分不足",
        "余额不足",
        "智点不足",
    ]
    .iter()
    .any(|pattern| lowered.contains(pattern))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::credentials::Credential;

    #[test]
    fn headers_carry_cookie_and_stable_desktop_identity() {
        let credential = Credential {
            wps_sid: "sid-1".to_string(),
            uid: "7".to_string(),
            branch: String::new(),
        };
        let head = headers(&credential, "application/json");
        let get = |name: &str| {
            head.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("Cookie"), Some("wps_sid=sid-1; uid=7"));
        assert_eq!(get("X-Client-Type"), Some("lingxi_claw"));
        assert_eq!(get("x-kso-device-id"), Some(endpoints::DEVICE_ID));
        assert_eq!(get("Accept"), Some("application/json"));
    }

    #[test]
    fn error_text_and_session_expiry_patterns() {
        assert_eq!(text_of(&json!({ "hint": "请稍后再试" })), "请稍后再试");
        assert_eq!(
            text_of(&json!({ "data": { "message": "内层文案" } })),
            "内层文案"
        );
        assert!(is_session_expired("SQLNoRows: 会话不存在"));
        assert!(!is_session_expired("模型不存在"));
        assert!(is_credit_exhausted("智点不足，请先签到"));
        assert_eq!(encode_date("2026-11-11"), "2026-11-11");
        assert_eq!(encode_date("2026/11/11';drop"), "20261111");
    }
}
