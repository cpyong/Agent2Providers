//! 把 OpenAI `messages` 翻成本机 opencode v2 会话，再把回答翻回 OpenAI SSE。
//!
//! ── 一次请求的步骤（2026-11-10 在本机 2.2.6 上逐条实测）──────
//! ```text
//!   GET  /api/event                      订阅全局 SSE（先订阅才不会丢增量）
//!   POST /api/session                   建会话，带 model:{id,providerID}
//!   POST /api/session/{id}/prompt       {text} → 200 立即返回，回答异步产生
//!   SSE  session.text.delta             → OpenAI chat.completion.chunk
//!   SSE  session.execution.succeeded   → finish_reason:"stop" + [DONE]
//!   DELETE /api/session/{id}           清理，不在桌面端留下一堆会话
//! ```
//! SSE 是**全局**事件流，必须按 `data.sessionID` 过滤；流断开 / 超时会回退
//! 到 `GET /api/session/{id}/message` 轮询取全文，并只补推尚未发出的部分。
//!
//! ── 与纯补全端点的差别 ──────────────────────────────────────
//! v2 的 prompt 只吃一段 `text`，因此把多轮 `messages` 拍平成带角色标签的
//! 文本（同一个请求新建会话）。工具调用 / 结构化输出 / `logprobs` 这类
//! OpenAI 字段无法一一映射，本家只承诺文本对话。

use std::collections::BTreeMap;
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};
use futures_util::StreamExt;
use serde_json::{json, Value};

use super::{api, models};

/// 等回答的最长时间。
const ANSWER_TIMEOUT: Duration = Duration::from_secs(180);
/// 轮询兜底的间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(300);
/// SSE 连接建立的最长等待（拿不到就走轮询，不阻塞整次请求）。
const SSE_OPEN_TIMEOUT: Duration = Duration::from_secs(3);

/// 转发入口。
pub async fn run(
    ctx: &PluginCtx,
    req: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    let server = api::resolve(ctx, &req.account).await?;
    let control = api::control();

    let mut client_model = req
        .body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if client_model.is_empty() {
        client_model = req.wire_model.trim().to_string();
    }
    let (provider_id, model_id) = models::resolve(&client_model)?;
    let prompt = build_prompt(&req.body);
    if prompt.trim().is_empty() {
        return Err("请求里没有可发送的消息内容".to_string());
    }

    let session = create_session(&control, &server, &provider_id, &model_id).await?;

    let answer = deliver(
        &server,
        &session,
        &client_model,
        &model_id,
        &prompt,
        &mut sink,
    )
    .await;

    // 无论成功失败都清理：失败时留在桌面端的空会话比一次 DELETE 更烦人。
    let _ = api::request_json(
        &control,
        &server,
        reqwest::Method::DELETE,
        &format!("/api/session/{session}"),
        None,
    )
    .await;
    answer
}

/// 客户端 `messages` → 一段带角色标签的文本。
pub fn build_prompt(body: &Value) -> String {
    if let Some(input) = body.get("input").and_then(Value::as_str) {
        if !input.trim().is_empty() {
            return input.to_string();
        }
    }
    let Some(items) = body
        .get("messages")
        .and_then(Value::as_array)
        .or_else(|| body.get("input").and_then(Value::as_array))
    else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for item in items {
        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
        let label = match role {
            "system" | "developer" => "系统",
            "assistant" => "助手",
            "tool" => "工具",
            _ => "用户",
        };
        let text = content_text(item.get("content"));
        if text.trim().is_empty() {
            continue;
        }
        parts.push(format!("[{label}]\n{text}"));
    }
    parts.join("\n\n")
}

/// content 可能是字符串，也可能是 OpenAI 的 part 数组。
fn content_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(items) = content.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("text")
            || item.get("type").is_none()
        {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                out.push_str(text);
            } else if let Some(text) = item.as_str() {
                out.push_str(text);
            }
        }
    }
    out
}

/// 建会话，返回 sessionID。
async fn create_session(
    client: &reqwest::Client,
    server: &api::Server,
    provider_id: &str,
    model_id: &str,
) -> Result<String, String> {
    let body = json!({
        "model": { "id": model_id, "providerID": provider_id },
        "title": "agent2api"
    });
    let (status, payload) = api::request_json(
        client,
        server,
        reqwest::Method::POST,
        "/api/session",
        Some(&body),
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(format!("创建 opencode 会话失败：{}", api::probe_error(status)));
    }
    payload
        .get("data")
        .and_then(|data| data.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "创建 opencode 会话成功但响应里没有 id".to_string())
}

/// 报头 → 订阅 → 发 prompt → 推增量 → 收尾。
async fn deliver(
    server: &api::Server,
    session: &str,
    client_model: &str,
    model_id: &str,
    prompt: &str,
    sink: &mut ConversationSink<'_>,
) -> Result<(), String> {
    let mut headers = BTreeMap::new();
    headers.insert("content-type".to_string(), "text/event-stream".to_string());
    sink.head(proto::StreamHead {
        status: 200,
        headers,
        upstream: proto::UpstreamKind::Chat,
    })
    .await?;

    let shown_model = if client_model.is_empty() { model_id } else { client_model };
    let completion_id = format!("chatcmpl-{}", short_id(session));
    let created = now_secs();

    // 先订阅：SSE 里没有历史回放，晚连会丢掉前面的 delta。
    let event_stream = open_event_stream(server).await;
    // 再发 prompt（HTTP 200 只代表入队成功）
    let (status, payload) = api::request_json(
        &api::control(),
        server,
        reqwest::Method::POST,
        &format!("/api/session/{session}/prompt"),
        Some(&json!({ "text": prompt })),
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "发送 opencode prompt 失败：{}",
            error_text(&payload, status)
        ));
    }

    push_chunk(
        sink,
        &frame(&completion_id, shown_model, created, json!({ "role": "assistant" }), None),
    )
    .await?;

    let mut pushed = String::new();
    let mut completed = false;
    if let Some(response) = event_stream {
        match read_deltas(
            response,
            session,
            &completion_id,
            shown_model,
            created,
            sink,
            &mut pushed,
        )
        .await
        {
            Ok(true) => completed = true,
            // 流断了 / 超时：下面用轮询拿全文，只补推没发过的部分
            Ok(false) => {}
            Err(reason) => return Err(reason),
        }
    }

    if !completed {
        let full = poll_full_text(server, session).await?;
        let remaining = full.strip_prefix(&pushed).unwrap_or(full.as_str());
        if !remaining.is_empty() {
            push_chunk(
                sink,
                &frame(
                    &completion_id,
                    shown_model,
                    created,
                    json!({ "content": remaining }),
                    None,
                ),
            )
            .await?;
        }
    }

    push_chunk(
        sink,
        &frame(
            &completion_id,
            shown_model,
            created,
            json!({}),
            Some("stop"),
        ),
    )
    .await?;
    sink.send(b"data: [DONE]\n\n").await
}

/// 打开全局 SSE；失败返回 `None`（调用方走轮询兜底）。
async fn open_event_stream(server: &api::Server) -> Option<reqwest::Response> {
    let url = format!("{}/api/event", server.base);
    let mut request = api::stream_client()
        .get(&url)
        .header("Accept", "text/event-stream");
    if let Some(auth) = &server.auth {
        request = request.header("Authorization", auth);
    }
    match tokio::time::timeout(SSE_OPEN_TIMEOUT, request.send()).await {
        Ok(Ok(response)) if response.status().is_success() => Some(response),
        _ => None,
    }
}

/// 逐帧读 SSE，把 `session.text.delta` 推成 OpenAI chunk。
///
/// 返回 `true` = 收到 `session.execution.succeeded`；`false` = 流结束 / 超时，
/// 需要轮询补全文。
async fn read_deltas(
    response: reqwest::Response,
    session: &str,
    completion_id: &str,
    model: &str,
    created: i64,
    sink: &mut ConversationSink<'_>,
    pushed: &mut String,
) -> Result<bool, String> {
    let mut source = response.bytes_stream();
    let mut buffer = String::new();
    loop {
        let next = tokio::time::timeout(ANSWER_TIMEOUT, source.next()).await;
        let chunk = match next {
            Err(_) | Ok(None) | Ok(Some(Err(_))) => return Ok(false),
            Ok(Some(Ok(chunk))) => chunk,
        };
        buffer.push_str(&String::from_utf8_lossy(chunk.as_ref()));
        while let Some(end) = buffer.find("\n\n") {
            let frame_text: String = buffer.drain(..end + 2).collect();
            for line in frame_text.lines() {
                let Some(raw) = line.strip_prefix("data:") else {
                    continue;
                };
                let raw = raw.trim();
                if raw.is_empty() || raw == "[DONE]" {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(raw) else {
                    continue;
                };
                let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
                if kind.is_empty() {
                    continue;
                }
                let data = event.get("data").unwrap_or(&Value::Null);
                let sid = data.get("sessionID").and_then(Value::as_str).unwrap_or("");
                if !sid.is_empty() && sid != session {
                    continue;
                }
                match kind {
                    "session.text.delta" => {
                        let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
                        if delta.is_empty() {
                            continue;
                        }
                        push_chunk(
                            sink,
                            &frame(
                                completion_id,
                                model,
                                created,
                                json!({ "content": delta }),
                                None,
                            ),
                        )
                        .await?;
                        pushed.push_str(delta);
                    }
                    "session.execution.succeeded" => return Ok(true),
                    "session.execution.failed" | "session.execution.interrupted"
                    | "session.step.failed" => {
                        return Err(format!(
                            "opencode 会话执行失败：{}",
                            event_text(data)
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 轮询 `message` 列表，等 `idle` 后取本轮 assistant 的正文。
async fn poll_full_text(server: &api::Server, session: &str) -> Result<String, String> {
    let poll = async {
        loop {
            let (status, payload) = api::request_json(
                &api::control(),
                server,
                reqwest::Method::GET,
                &format!("/api/session/{session}/message?order=desc&limit=12"),
                None,
            )
            .await?;
            if !(200..300).contains(&status) {
                return Err(format!("读取 opencode 会话消息失败：{}", api::probe_error(status)));
            }
            let items = payload
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut idle: Option<&str> = None;
            let mut text = String::new();
            for item in &items {
                match item.get("type").and_then(Value::as_str).unwrap_or("") {
                    "idle" => {
                        if idle.is_none() {
                            idle = Some(
                                item.get("outcome")
                                    .and_then(Value::as_str)
                                    .unwrap_or("succeeded"),
                            );
                        }
                    }
                    "assistant" if text.is_empty() => {
                        text = assistant_text(item);
                    }
                    "error" => return Err(format!("opencode 会话报错：{}", event_text(item))),
                    _ => {}
                }
            }
            if let Some(outcome) = idle {
                if outcome != "succeeded" {
                    return Err(format!("opencode 会话结束状态：{outcome}"));
                }
                return Ok(text);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    };
    match tokio::time::timeout(ANSWER_TIMEOUT, poll).await {
        Ok(result) => result,
        Err(_) => Err("等待 opencode 回答超时（180 秒）".to_string()),
    }
}

/// assistant 消息里的 `content: [{type:"text", text:"…"}]`。
fn assistant_text(message: &Value) -> String {
    let Some(items) = message.get("content").and_then(Value::as_array) else {
        return message
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
    };
    let mut out = String::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                out.push_str(text);
            }
        }
    }
    out
}

/// 一帧 OpenAI `chat.completion.chunk`。
fn frame(
    id: &str,
    model: &str,
    created: i64,
    delta: Value,
    finish: Option<&str>,
) -> String {
    let choice = match finish {
        Some(reason) => json!({ "index": 0, "delta": delta, "finish_reason": reason }),
        None => json!({ "index": 0, "delta": delta, "finish_reason": Value::Null }),
    };
    let payload = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [choice]
    });
    format!("data: {payload}\n\n")
}

async fn push_chunk(sink: &mut ConversationSink<'_>, chunk: &str) -> Result<(), String> {
    sink.send(chunk.as_bytes()).await
}

fn event_text(data: &Value) -> String {
    for key in ["message", "error", "reason", "text"] {
        if let Some(text) = data.get(key).and_then(Value::as_str) {
            if !text.trim().is_empty() {
                return text.to_string();
            }
        }
    }
    "上游没有给出原因".to_string()
}

fn error_text(payload: &Value, status: u16) -> String {
    let detail = event_text(payload.get("error").unwrap_or(payload));
    if detail == "上游没有给出原因" {
        format!("HTTP {status}")
    } else {
        detail
    }
}

fn short_id(session: &str) -> String {
    session
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .rev()
        .take(16)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{assistant_text, build_prompt, frame};
    use serde_json::json;

    #[test]
    fn flattens_multi_turn_messages() {
        let prompt = build_prompt(&json!({
            "messages": [
                { "role": "system", "content": "You are terse." },
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [{"type":"text","text":"hello"}] },
                { "role": "user", "content": "again" }
            ]
        }));
        assert!(prompt.contains("[系统]\nYou are terse."));
        assert!(prompt.contains("[助手]\nhello"));
        assert!(prompt.ends_with("[用户]\nagain"));
    }

    #[test]
    fn extracts_assistant_content() {
        let message = json!({
            "type": "assistant",
            "content": [
                { "type": "text", "text": "PONG" },
                { "type": "reasoning", "text": "ignored" }
            ]
        });
        assert_eq!(assistant_text(&message), "PONG");
    }

    #[test]
    fn frames_are_openai_sse() {
        let chunk = frame("chatcmpl-x", "iceman-free", 1, json!({"content":"P"}), None);
        assert!(chunk.starts_with("data: {"));
        assert!(chunk.ends_with("\n\n"));
        assert!(chunk.contains("\"chat.completion.chunk\""));
    }
}
