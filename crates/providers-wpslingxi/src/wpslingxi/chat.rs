//! 会话式转发（`stateful`）：把 OpenAI Chat 请求翻成灵犀的
//! 「cowork session → credits run → agent-runtime 私有 SSE」，再翻回 OpenAI
//! `chat.completion.chunk`。
//!
//! ── 为什么必须走 `conversation` 而不是 `build_chat` ────────────
//! 上游的响应**不是** OpenAI SSE：正文在 `choices[].text`（不是
//! `delta.content`）、不发 `[DONE]`（靠 `finish_reason` 收尾）、每帧是单行
//! `data:{…}`（不能用空行分帧），而且一次通话要先建 session、再领 run，
//! 把 `x-cc-session-id` / `x-cc-run-id` 塞进请求头。协议库里没有对应的
//! `UpstreamKind` 翻译机，交给网关透传只会让客户端收到一堆乱码。所以插件
//! 自己读帧、自己翻译、自己推标准 chat SSE。
//!
//! ── run 为什么按「账号 × 模型」缓存 ──────────────────────────
//! 上游**按 run 计费**：一个 run 内多次调用只扣一次智点。每次请求都新建
//! run 会多扣；并发时各建一个也多扣。所以缓存 1 小时（不滑动续期）并对
//! 同一 key 单飞。上游说 `SQLNoRows` /「会话不存在」时丢弃缓存重建一次。
//!
//! ── usage 为什么不下发 ──────────────────────────────────────
//! 上游 `usage.total_tokens` 是**当前上下文大小**（含缓存命中），直接透传
//! 会让用量虚高几十倍。网关的计量由自己按请求做，这里不伪造一个 token 数。
//!
//! ── 工具与思考 ─────────────────────────────────────────────
//! 私有帧的 `reasoning_content` 翻成 `delta.reasoning_content`，`tool_calls`
//! 原样翻成 OpenAI 形态。工具 schema 先压平：上游见到 `anyOf` / `oneOf` /
//! `$ref` / 数组型 `type` 会让整轮回「AI服务异常」。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{serve::Cancel, ConversationSink, PluginCtx};
use serde_json::{json, Value};

use super::{client, credentials, endpoints};
use crate::http;

/// run 缓存时长（上游按 run 计费；**不滑动续期**，到期就重建）。
const RUN_TTL_MS: i64 = 60 * 60 * 1000;

/// 入口：拿凭证 → 取 run → 打开私有 SSE → 翻译成 chat SSE。
pub async fn run(
    ctx: &PluginCtx,
    req: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    let (credential, account_proxy) = credentials::load(ctx, &req.account).await?;
    let http_client = http::client(
        req.proxy.as_ref().or(account_proxy.as_ref()),
        &ctx.env.proxy,
    );
    let model = model_of(&req);
    let title = last_user_text(&req.body);
    let body = build_body(&req.body, &model);
    let key = format!("{}|{}", credential.identity(), model);
    let cancel = sink.cancellation();
    let id = format!("chatcmpl-{}", http::unique_id());
    let created = http::now_ms() / 1000;

    let mut active = ensure_run(&http_client, &credential, &key, &model, &title).await?;
    for attempt in 0..2 {
        if cancel.is_cancelled() {
            return Err("请求已取消".to_string());
        }
        let opened = client::open_chat(&http_client, &credential, &active, &body).await?;
        match opened {
            http::Opened::Error { status, payload } => {
                let message = client::text_of(&payload);
                if status == 401 || status == 403 {
                    return Err(format!(
                        "WPS 灵犀登录态已失效（HTTP {status}），请重新粘贴 wps_sid Cookie{}",
                        suffix(&message)
                    ));
                }
                if status == 402 || client::is_credit_exhausted(&message) {
                    return Err(format!(
                        "WPS 灵犀智点不足，签到或充值后再试{}",
                        suffix(&message)
                    ));
                }
                if attempt == 0 && (status == 404 || client::is_session_expired(&message)) {
                    forget_run(&key);
                    active =
                        ensure_run(&http_client, &credential, &key, &model, &title).await?;
                    continue;
                }
                return Err(client::describe_error(status, &payload, "WPS 灵犀模型通话"));
            }
            http::Opened::Stream {
                headers: upstream_headers,
                response,
            } => {
                // 上游偶发「HTTP 200 + application/json」的错误信封。记一条日志
                // 就够：正文仍按帧解析，解析不出内容会如实报「没有正文帧」。
                if upstream_headers
                    .get("content-type")
                    .is_some_and(|value| value.contains("application/json"))
                {
                    agent2api_provider_sdk::log_warn!(
                        "[WPS 灵犀] 上游把流式请求回成了 JSON，本次可能拿不到正文"
                    );
                }
                match pump(&mut sink, response, &id, &model, created, &cancel).await? {
                    PumpOutcome::Finished => return Ok(()),
                    PumpOutcome::RunExpired(message) if attempt == 0 => {
                        // 只在**一帧正文都没推过**时才回来重建（pump 保证），
                        // 否则会重复发 head，客户端收到两条响应头。
                        agent2api_provider_sdk::log_info!(
                            "[WPS 灵犀] run 已过期，重建一次：{message}"
                        );
                        forget_run(&key);
                        active =
                            ensure_run(&http_client, &credential, &key, &model, &title).await?;
                        continue;
                    }
                    PumpOutcome::RunExpired(message) => {
                        return Err(format!("WPS 灵犀会话过期且重建后仍失败：{message}"));
                    }
                    PumpOutcome::Failed(message) => return Err(message),
                }
            }
        }
    }
    Err("WPS 灵犀会话重建后仍未能完成本次请求".to_string())
}

fn suffix(message: &str) -> String {
    if message.is_empty() {
        String::new()
    } else {
        format!("：{message}")
    }
}

/// 网关解析出的发送名优先，其次 body 里的原名，都没有就用上游的 `auto`。
fn model_of(req: &proto::ConversationRequest) -> String {
    let wire = req.wire_model.trim();
    if !wire.is_empty() {
        return wire.to_string();
    }
    let requested = req
        .body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if requested.is_empty() {
        "auto".to_string()
    } else {
        requested.to_string()
    }
}

// ─── 请求体 ────────────────────────────────────────────────

/// 最后一条 user 文本（上游只用前 60 字给 run 起标题，不是正文）。
fn last_user_text(body: &Value) -> String {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return "灵犀会话".to_string();
    };
    for item in messages.iter().rev() {
        if item.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text = message_text(item);
        if !text.trim().is_empty() {
            return text.trim().to_string();
        }
    }
    "灵犀会话".to_string()
}

fn message_text(message: &Value) -> String {
    let Some(content) = message.get("content") else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            out.push_str(text);
        }
    }
    out
}

/// 丢弃「孤立的 tool 消息」（没有紧邻声明其 id 的 assistant）。
///
/// 上游对这类历史一律 400；只丢弃、不重排、不改 content。
fn normalize_messages(messages: &Value) -> Vec<Value> {
    let Some(items) = messages.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut declared: Vec<String> = Vec::new();
    for item in items {
        let role = item.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "assistant" {
            declared = item
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|calls| {
                    calls
                        .iter()
                        .filter_map(|call| call.get("id").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
        } else if role == "tool" {
            let id = item
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let Some(index) = declared.iter().position(|value| value == id) else {
                continue;
            };
            declared.remove(index);
        } else {
            declared.clear();
        }
        out.push(item.clone());
    }
    out
}

/// OpenAI 入站 → agent-runtime 请求体。
pub fn build_body(payload: &Value, model: &str) -> Value {
    let mut body = json!({
        "stream": true,
        "model": model,
        "messages": normalize_messages(&payload.get("messages").cloned().unwrap_or(json!([]))),
    });
    let Some(object) = body.as_object_mut() else {
        return body;
    };
    if let Some(tools) = payload.get("tools").and_then(Value::as_array) {
        let usable: Vec<Value> = tools
            .iter()
            .filter_map(|tool| tool_of(tool))
            .collect();
        if !usable.is_empty() {
            object.insert("tools".to_string(), Value::Array(usable));
            if let Some(choice) = tool_choice(payload.get("tool_choice")) {
                object.insert("tool_choice".to_string(), Value::String(choice));
            }
        }
    }
    if let Some(temperature) = payload.get("temperature").and_then(Value::as_f64) {
        object.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(max_tokens) = payload
        .get("max_tokens")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
    {
        // 顶层 max_tokens 上游不理，必须包进 base_llm_arguments。
        object.insert(
            "base_llm_arguments".to_string(),
            json!({ "max_tokens": max_tokens }),
        );
    }
    if let Some(effort) = payload
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        // 顶层与 llm_arguments 各写一份（实测两处都读）。
        object.insert("reasoning_effort".to_string(), json!(effort));
        object.insert("llm_arguments".to_string(), json!({ "reasoning_effort": effort }));
    }
    Value::Object(object.clone())
}

/// 只保留 `type=function` 的工具定义；其余类型原样发过去会让整轮失败。
fn tool_of(tool: &Value) -> Option<Value> {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    let mut out = tool.clone();
    let function = tool.get("function")?.clone();
    let mut function = function;
    if let Some(object) = function.as_object_mut() {
        let schema = sanitize_schema(object.get("parameters").unwrap_or(&Value::Null), &Value::Null, 0);
        object.insert("parameters".to_string(), schema);
    }
    if let Some(object) = out.as_object_mut() {
        object.insert("function".to_string(), function);
    }
    Some(out)
}

/// 对象形态的 `tool_choice` 一律降级成 `required`（上游不支持点名函数）。
fn tool_choice(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Object(_) => Some("required".to_string()),
        _ => None,
    }
}

/// 把任意 JSON Schema 压成上游能吃的朴素子集。
///
/// `anyOf` / `oneOf` 选「最具体」的一支，`$ref` 展开本地指针（循环引用降级
/// 成 string），数组型 `type` 取第一个非 null，其余元数据保留。
fn sanitize_schema(schema: &Value, defs: &Value, depth: u32) -> Value {
    if depth > 24 {
        return json!({ "type": "string" });
    }
    let Some(node) = schema.as_object() else {
        return json!({ "type": "string" });
    };
    let mut merged_defs = defs.as_object().cloned().unwrap_or_default();
    if let Some(local) = schema.get("$defs").and_then(Value::as_object) {
        merged_defs.extend(local.clone());
    }
    if let Some(local) = schema.get("definitions").and_then(Value::as_object) {
        merged_defs.extend(local.clone());
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if let Some(path) = reference.strip_prefix("#/") {
            let mut target = Value::Object(merged_defs.clone());
            for token in path.split('/') {
                let key = token.replace("~1", "/").replace("~0", "~");
                target = target.get(&key).cloned().unwrap_or(Value::Null);
            }
            if target.is_object() {
                return sanitize_schema(&target, &Value::Object(merged_defs), depth + 1);
            }
        }
        return json!({ "type": "string" });
    }
    for branch_key in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(branch_key).and_then(Value::as_array) {
            let chosen = branches
                .iter()
                .filter(|item| item.is_object())
                .max_by_key(|item| {
                    let rank = match item.get("type").and_then(Value::as_str) {
                        Some("object") => 3,
                        Some("array") => 2,
                        _ => 1,
                    };
                    (rank, item.as_object().map(|map| map.len()).unwrap_or(0))
                })
                .cloned();
            return match chosen {
                Some(value) => sanitize_schema(&value, &Value::Object(merged_defs), depth + 1),
                None => json!({ "type": "string" }),
            };
        }
    }
    let mut out = serde_json::Map::new();
    for (key, value) in node {
        if matches!(key.as_str(), "$defs" | "definitions" | "allOf" | "not" | "if" | "then" | "else" | "$id" | "$schema" | "$ref") {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    let kind = match out.get("type") {
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(|kind| kind.as_str())
            .find(|kind| *kind != "null")
            .unwrap_or("string")
            .to_string(),
        Some(Value::String(kind)) => kind.clone(),
        _ => "string".to_string(),
    };
    out.insert("type".to_string(), Value::String(kind));
    if let Some(properties) = out.get("properties").cloned() {
        if let Some(map) = properties.as_object() {
            let mut next = serde_json::Map::new();
            for (name, value) in map {
                next.insert(
                    name.clone(),
                    sanitize_schema(value, &Value::Object(merged_defs.clone()), depth + 1),
                );
            }
            out.insert("properties".to_string(), Value::Object(next));
        }
    }
    if let Some(items) = out.get("items").cloned() {
        out.insert(
            "items".to_string(),
            sanitize_schema(&items, &Value::Object(merged_defs), depth + 1),
        );
    }
    Value::Object(out)
}

// ─── run 缓存与单飞 ────────────────────────────────────────

struct CachedRun {
    run: client::Run,
    created_ms: i64,
}

fn runs() -> &'static Mutex<HashMap<String, CachedRun>> {
    static RUNS: OnceLock<Mutex<HashMap<String, CachedRun>>> = OnceLock::new();
    RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn run_locks() -> &'static Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_run(key: &str) -> Option<client::Run> {
    let mut guard = runs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = http::now_ms();
    if guard
        .get(key)
        .is_some_and(|cached| now - cached.created_ms >= RUN_TTL_MS)
    {
        guard.remove(key);
    }
    guard.get(key).map(|cached| cached.run.clone())
}

fn forget_run(key: &str) {
    if let Ok(mut guard) = runs().lock() {
        guard.remove(key);
    }
}

fn lock_for(key: &str) -> Result<Arc<tokio::sync::Mutex<()>>, String> {
    let mut guard = run_locks()
        .lock()
        .map_err(|_| "WPS 灵犀 run 锁表已损坏".to_string())?;
    Ok(guard
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone())
}

/// 拿一个可用 run：命中缓存就用，否则「建会话 + 领 run」，同 key 并发单飞。
async fn ensure_run(
    http_client: &reqwest::Client,
    credential: &credentials::Credential,
    key: &str,
    model: &str,
    title: &str,
) -> Result<client::Run, String> {
    if let Some(cached) = cached_run(key) {
        return Ok(cached);
    }
    let lock = lock_for(key)?;
    let _serialized = lock.lock().await;
    if let Some(cached) = cached_run(key) {
        return Ok(cached);
    }
    let mut last = String::new();
    for base in endpoints::bases() {
        match create_run(http_client, credential, &base, model, title).await {
            Ok(run) => {
                agent2api_provider_sdk::log_debug!(
                    "[WPS 灵犀] 新 run：{}（group {}，域 {}）",
                    run.run_id,
                    run.group_id,
                    run.base
                );
                if let Ok(mut guard) = runs().lock() {
                    guard.insert(
                        key.to_string(),
                        CachedRun {
                            run: run.clone(),
                            created_ms: http::now_ms(),
                        },
                    );
                }
                return Ok(run);
            }
            Err(error) => last = error,
        }
    }
    Err(format!(
        "建立 WPS 灵犀会话失败（已试 {} 个域）：{last}",
        endpoints::bases().len()
    ))
}

async fn create_run(
    http_client: &reqwest::Client,
    credential: &credentials::Credential,
    base: &str,
    model: &str,
    title: &str,
) -> Result<client::Run, String> {
    let session_id = client::create_session(http_client, credential, base).await?;
    client::create_run(http_client, credential, base, &session_id, model, title).await
}

// ─── 出站：私有 SSE → OpenAI chat SSE ──────────────────────

enum PumpOutcome {
    Finished,
    /// 一帧正文都没推过就发现 run 过期：允许重建一次。
    RunExpired(String),
    Failed(String),
}

struct Translated {
    frames: Vec<String>,
    finished: bool,
}

/// 流式读帧并逐帧推送。
///
/// `head` 是**懒发**的：第一帧正文要出去时才发。这样 run 过期重建时不会
/// 出现「同一条会话两条 head」。上游每帧是单行 `data:{…}`（偶尔连着多行），
/// 这里按 `\n` 切行、逐行解析，不用空行分帧 —— 那会吞帧。
async fn pump(
    sink: &mut ConversationSink<'_>,
    mut response: reqwest::Response,
    id: &str,
    model: &str,
    created: i64,
    cancel: &Arc<Cancel>,
) -> Result<PumpOutcome, String> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut head_sent = false;
    let mut started = false;
    let mut got_finish = false;
    'outer: loop {
        if cancel.is_cancelled() {
            return Ok(PumpOutcome::Failed("请求已取消".to_string()));
        }
        let chunk = response
            .chunk()
            .await
            .map_err(|error| http::describe(&error))?;
        let Some(bytes) = chunk else {
            break;
        };
        buffer.extend_from_slice(&bytes);
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=position).collect();
            let text = String::from_utf8_lossy(&line);
            let trimmed = text.trim();
            if trimmed.is_empty()
                || trimmed.starts_with(':')
                || trimmed.starts_with("event:")
                || trimmed.starts_with("id:")
            {
                continue;
            }
            let data = trimmed.strip_prefix("data:").unwrap_or(trimmed).trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let frame: Value = match serde_json::from_str::<Value>(data) {
                Ok(value) if value.is_object() => value,
                _ => continue, // 心跳 / 坏行：跳过，不把整条流判死
            };
            if let Some((code, message)) = frame_error(&frame) {
                let detail = if message.is_empty() {
                    format!("上游返回 {code}")
                } else {
                    message
                };
                if !started && client::is_session_expired(&detail) {
                    return Ok(PumpOutcome::RunExpired(detail));
                }
                if client::is_credit_exhausted(&detail) {
                    return Ok(PumpOutcome::Failed(format!(
                        "WPS 灵犀智点不足：{detail}"
                    )));
                }
                return Ok(PumpOutcome::Failed(format!(
                    "WPS 灵犀上游返回错误：{detail}"
                )));
            }
            let translated = translate_frame(&frame, id, model, created);
            for frame_line in &translated.frames {
                if !started {
                    // 第一帧先给 role（网关聚合非流式响应时也认这一帧）
                    let role = chunk_frame(
                        id,
                        model,
                        created,
                        json!({ "role": "assistant", "content": "" }),
                        None,
                    );
                    send_chunk(sink, &mut head_sent, &role).await?;
                }
                send_chunk(sink, &mut head_sent, frame_line).await?;
                started = true;
            }
            if translated.finished {
                got_finish = true;
                break 'outer;
            }
        }
    }
    // EOF 时可能还剩半行（上游偶尔不发尾换行）
    if !buffer.is_empty() {
        let text = String::from_utf8_lossy(&buffer);
        let trimmed = text.trim();
        let data = trimmed.strip_prefix("data:").unwrap_or(trimmed).trim();
        if !data.is_empty() && data != "[DONE]" {
            if let Ok(frame) = serde_json::from_str::<Value>(data) {
                if frame.is_object() {
                    let translated = translate_frame(&frame, id, model, created);
                    for frame_line in &translated.frames {
                        send_chunk(sink, &mut head_sent, frame_line).await?;
                        started = true;
                    }
                    got_finish = got_finish || translated.finished;
                }
            }
        }
    }
    if !started {
        return Ok(PumpOutcome::Failed(
            "WPS 灵犀上游流在完成事件之前结束（没有收到任何正文帧）".to_string(),
        ));
    }
    if !head_sent {
        return Ok(PumpOutcome::Failed(
            "WPS 灵犀上游没有给出可翻译的正文帧".to_string(),
        ));
    }
    // 上游靠 finish_reason 收尾、不发 [DONE]；没给 finish_reason 时要补一帧，
    // 否则客户端会一直等。已经给过就不再补，避免两帧 finish_reason。
    if !got_finish {
        let finish = chunk_frame(id, model, created, json!({}), Some("stop"));
        send_chunk(sink, &mut head_sent, &finish).await?;
    }
    send_chunk(sink, &mut head_sent, "data: [DONE]\n\n").await?;
    Ok(PumpOutcome::Finished)
}

/// 帧内业务错误（`code != Success` / `result != ok`）→ 码与文案。
fn frame_error(frame: &Value) -> Option<(String, String)> {
    let code = frame
        .get("code")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !code.is_empty() && !code.eq_ignore_ascii_case("success") {
        return Some((code.to_string(), client::text_of(frame)));
    }
    let result = frame
        .get("result")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !result.is_empty() && !result.eq_ignore_ascii_case("ok") {
        return Some((result.to_string(), client::text_of(frame)));
    }
    None
}

/// 一帧私有 SSE → 若干条 OpenAI `chat.completion.chunk`。
///
/// `usage` 故意不翻：上游那是上下文大小，不是本次消耗。
fn translate_frame(frame: &Value, id: &str, model: &str, created: i64) -> Translated {
    let choice = frame
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .filter(|choice| choice.is_object());
    let Some(choice) = choice else {
        return Translated { frames: Vec::new(), finished: false };
    };
    let mut delta = serde_json::Map::new();
    if let Some(text) = choice.get("text").and_then(Value::as_str) {
        if !text.is_empty() {
            delta.insert("content".to_string(), json!(text));
        }
    }
    if let Some(reasoning) = choice.get("reasoning_content").and_then(Value::as_str) {
        if !reasoning.is_empty() {
            delta.insert("reasoning_content".to_string(), json!(reasoning));
        }
    }
    if let Some(calls) = choice.get("tool_calls").and_then(Value::as_array) {
        let mapped: Vec<Value> = calls.iter().filter_map(map_tool_call).collect();
        if !mapped.is_empty() {
            delta.insert("tool_calls".to_string(), Value::Array(mapped));
        }
    }
    let finish = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let mut frames = Vec::new();
    if !delta.is_empty() {
        frames.push(chunk_frame(id, model, created, Value::Object(delta), None));
    }
    if let Some(reason) = finish {
        frames.push(chunk_frame(
            id,
            model,
            created,
            json!({}),
            Some(reason),
        ));
    }
    Translated {
        frames,
        finished: finish.is_some(),
    }
}

fn map_tool_call(call: &Value) -> Option<Value> {
    if !call.is_object() {
        return None;
    }
    let mut out = serde_json::Map::new();
    if let Some(index) = call.get("index").and_then(Value::as_u64) {
        out.insert("index".to_string(), json!(index));
    }
    if let Some(id) = call.get("id").and_then(Value::as_str) {
        out.insert("id".to_string(), json!(id));
    }
    out.insert("type".to_string(), json!("function"));
    if let Some(function) = call.get("function").filter(|value| value.is_object()) {
        let mut piece = serde_json::Map::new();
        if let Some(name) = function.get("name").and_then(Value::as_str) {
            piece.insert("name".to_string(), json!(name));
        }
        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
            piece.insert("arguments".to_string(), json!(arguments));
        }
        out.insert("function".to_string(), Value::Object(piece));
    }
    (!out.is_empty()).then(|| Value::Object(out))
}

fn chunk_frame(id: &str, model: &str, created: i64, delta: Value, finish: Option<&str>) -> String {
    let frame = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish,
        }],
    });
    format!("data: {frame}\n\n")
}

/// 推一段字节；第一次推之前补发 `head`。
async fn send_chunk(
    sink: &mut ConversationSink<'_>,
    head_sent: &mut bool,
    frame: &str,
) -> Result<(), String> {
    if !*head_sent {
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_string(), "text/event-stream".to_string());
        sink.head(proto::StreamHead {
            status: 200,
            headers,
            upstream: proto::UpstreamKind::Chat,
        })
        .await?;
        *head_sent = true;
    }
    sink.send(frame.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orphan_tool_messages_are_dropped() {
        let messages = json!([
            { "role": "system", "content": "sys" },
            { "role": "assistant", "content": "", "tool_calls": [{ "id": "call-1" }] },
            { "role": "tool", "tool_call_id": "call-1", "content": "ok" },
            { "role": "tool", "tool_call_id": "orphan", "content": "drop" },
            { "role": "user", "content": "hi" },
        ]);
        let out = normalize_messages(&messages);
        assert_eq!(out.len(), 4);
        assert_eq!(out[2]["tool_call_id"], "call-1");
    }

    #[test]
    fn body_puts_max_tokens_and_reasoning_where_upstream_reads_them() {
        let payload = json!({
            "model": "GLM-5.3",
            "messages": [{ "role": "user", "content": "写个快排" }],
            "max_tokens": 2048,
            "reasoning_effort": "high",
            "temperature": 0.3,
            "tools": [{
                "type": "function",
                "function": {
                    "name": "search",
                    "parameters": { "anyOf": [
                        { "type": "object", "properties": { "q": { "type": "string" } } },
                        { "type": "string" },
                    ] },
                },
            }],
        });
        let body = build_body(&payload, "GLM-5.3");
        assert_eq!(body["stream"], true);
        assert_eq!(body["base_llm_arguments"]["max_tokens"], 2048);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["llm_arguments"]["reasoning_effort"], "high");
        assert!(body.get("max_tokens").is_none());
        let params = &body["tools"][0]["function"]["parameters"];
        assert_eq!(params["type"], "object");
        assert_eq!(params["properties"]["q"]["type"], "string");
        assert!(params.get("anyOf").is_none());
    }

    #[test]
    fn private_frame_becomes_openai_chunks() {
        let frame = json!({
            "code": "Success",
            "choices": [{
                "text": "你好",
                "reasoning_content": "先想一想",
                "tool_calls": [{
                    "index": 0,
                    "id": "call-1",
                    "function": { "name": "search", "arguments": "{\"q\":\"x\"}" },
                }],
                "finish_reason": "stop",
            }],
            "usage": { "total_tokens": 999_999 },
        });
        assert!(frame_error(&frame).is_none());
        let translated = translate_frame(&frame, "chatcmpl-1", "GLM-5.3", 1);
        assert!(translated.finished);
        assert_eq!(translated.frames.len(), 2);
        let first: Value = serde_json::from_str(
            translated.frames[0]
                .strip_prefix("data: ")
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(first["choices"][0]["delta"]["content"], "你好");
        assert_eq!(first["choices"][0]["delta"]["reasoning_content"], "先想一想");
        assert_eq!(first["choices"][0]["delta"]["tool_calls"][0]["type"], "function");
        // 上游 total_tokens 是上下文大小，绝不能进 usage
        assert!(first.get("usage").is_none());
        let second: Value = serde_json::from_str(
            translated.frames[1]
                .strip_prefix("data: ")
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(second["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn business_error_inside_http_200_is_not_success() {
        let frame = json!({ "code": "AuditFailed", "message": "内容审核拦截" });
        let (code, message) = frame_error(&frame).unwrap();
        assert_eq!(code, "AuditFailed");
        assert_eq!(message, "内容审核拦截");
        assert!(frame_error(&json!({ "result": "ok", "code": "Success" })).is_none());
    }
}
